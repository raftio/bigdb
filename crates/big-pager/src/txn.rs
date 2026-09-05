// Copyright 2026 Bany
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! `ReadTxn` / `WriteTxn`. Pure copy-on-write: no WAL, no checkpoint.

use crate::error::{Result, StoreError};
use crate::freelist::Freelist;
use crate::pager::{Pager, PagerMut};
use crate::roots::RootRecords;
use crate::snapshot::{Snapshot, SnapshotId, SnapshotRegistry};
use crate::Store;
use big_page::{FragmentKey, MetaPage, Page, Pgno, TxnId};
use std::collections::BTreeMap;
use std::sync::Arc;

/// A page in a write transaction may still be in RAM rather than on disk.
pub enum TxnPage<'a, P: Pager + 'a> {
    Dirty(&'a Page),
    Clean(P::Ref<'a>),
}

impl<P: Pager> core::ops::Deref for TxnPage<'_, P> {
    type Target = Page;

    fn deref(&self) -> &Page {
        match self {
            Self::Dirty(p) => p,
            Self::Clean(r) => r,
        }
    }
}

/// A reader takes no lock and waits for nobody; the tree under its root is immutable.
pub struct ReadTxn<'db, P: Pager> {
    store: &'db Store<P>,
    txn_id: TxnId,
    roots: Arc<RootRecords>,
    /// Captured with the roots, not fetched later.
    ///
    /// A commit publishes new roots and a new catalog under one lock; reading them in two
    /// steps can land between the two and pair fresh roots with a stale schema, which shows
    /// up as a just-written fragment being invisible rather than as an error.
    catalog: Arc<Vec<Vec<u8>>>,
}

impl<'db, P: Pager> ReadTxn<'db, P> {
    pub(crate) fn new(
        store: &'db Store<P>,
        txn_id: TxnId,
        roots: Arc<RootRecords>,
        catalog: Arc<Vec<Vec<u8>>>,
    ) -> Self {
        store.register_reader(txn_id);
        Self { store, txn_id, roots, catalog }
    }

    pub fn txn_id(&self) -> TxnId {
        self.txn_id
    }

    /// The reference is tied to `&self`, so a page cannot be reclaimed while a slice points into it.
    pub fn read(&self, pgno: Pgno) -> Result<P::Ref<'_>> {
        self.store.pager().read(pgno)
    }

    pub fn root(&self, key: &FragmentKey) -> Option<Pgno> {
        self.roots.get(key)
    }

    pub fn roots(&self) -> &RootRecords {
        &self.roots
    }

    /// Opaque catalog records as of this reader's transaction.
    pub fn catalog(&self) -> &Arc<Vec<Vec<u8>>> {
        &self.catalog
    }
}

impl<P: Pager> Drop for ReadTxn<'_, P> {
    fn drop(&mut self) {
        self.store.release_reader(self.txn_id);
    }
}

/// A write transaction is itself a readable pager: its own dirty pages first, then the file.
/// That is what lets the b-tree descend through pages this transaction has not committed yet.
impl<'db, P: PagerMut> crate::Pager for WriteTxn<'db, P> {
    type Ref<'a>
        = TxnPage<'a, P>
    where
        Self: 'a;

    fn read(&self, pgno: Pgno) -> Result<TxnPage<'_, P>> {
        WriteTxn::read(self, pgno)
    }

    fn page_count(&self) -> u64 {
        self.next_pgno
    }

    fn capacity(&self) -> Option<u64> {
        crate::Pager::capacity(self.store.pager())
    }

    /// A page this transaction is still building has to be checked for real every time.
    ///
    /// The backing pager's memo is keyed by page number, and a number this transaction
    /// allocated may have been carrying a memo from whatever lived there before it was freed.
    /// Only a page that is still exactly as the file has it may use that memo.
    fn verify_bitmap(&self, pgno: Pgno, page: &Page, expected: u32) -> bool {
        match self.dirty.contains_key(&pgno) {
            true => big_page::bitmap_page_checksum(page) == expected,
            false => crate::Pager::verify_bitmap(self.store.pager(), pgno, page, expected),
        }
    }
}

/// One writer at a time; readers keep running normally alongside it.
/// Which of the metadata chains a transaction changed.
///
/// The freelist is absent because a commit always changes it: it records the pages this very
/// commit is recycling.
#[derive(Clone, Copy, Default, Debug)]
pub struct DirtyChains {
    pub roots: bool,
    pub catalog: bool,
    pub snapshots: bool,
}

pub struct WriteTxn<'db, P: PagerMut> {
    store: &'db Store<P>,
    _excl: std::sync::MutexGuard<'db, ()>,
    txn_id: TxnId,
    /// Reclaim floor: a page replaced at a txn <= horizon is invisible to every live reader.
    horizon: TxnId,
    base: MetaPage,
    dirty: BTreeMap<Pgno, Page>,
    freelist: Freelist,
    roots: RootRecords,
    snapshots: SnapshotRegistry,
    catalog: Vec<Vec<u8>>,
    next_pgno: u64,
    /// Pages this transaction both allocated and freed.
    ///
    /// Nothing outside can reach them: they were never named by a committed meta page, so no
    /// reader can be holding one and the horizon has nothing to say about them. Handing them
    /// straight back is what stops a large transaction growing the file by its own churn -
    /// every page a copy-on-write rewrite abandons mid-transaction would otherwise be dead
    /// weight until the *next* transaction could reclaim it.
    scratch: Vec<Pgno>,
    /// Which metadata chains this transaction actually changed.
    ///
    /// A commit rewrites a chain wholesale, so a chain nobody touched costs its entire length
    /// in pages for no reason. The catalog is the expensive one: 128 bytes per fragment, and
    /// most transactions do not alter a single byte of it.
    dirty_chains: DirtyChains,
    /// Lowest page number this transaction allocated from the tail, so `free` can tell its own
    /// pages from ones it inherited.
    tail_floor: u64,
    committed: bool,
}

impl<'db, P: PagerMut> WriteTxn<'db, P> {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        store: &'db Store<P>,
        excl: std::sync::MutexGuard<'db, ()>,
        base: MetaPage,
        horizon: TxnId,
        freelist: Freelist,
        roots: RootRecords,
        snapshots: SnapshotRegistry,
        catalog: Vec<Vec<u8>>,
    ) -> Self {
        let next_pgno = store.pager().page_count();
        Self {
            store,
            _excl: excl,
            txn_id: base.txn_id + 1,
            horizon,
            base,
            dirty: BTreeMap::new(),
            freelist,
            roots,
            snapshots,
            catalog,
            next_pgno,
            scratch: Vec::new(),
            dirty_chains: DirtyChains::default(),
            // Everything at or above this was allocated by this transaction; everything below
            // it existed before and belongs to the horizon's rules.
            tail_floor: next_pgno,
            committed: false,
        }
    }

    pub fn txn_id(&self) -> TxnId {
        self.txn_id
    }

    pub fn read(&self, pgno: Pgno) -> Result<TxnPage<'_, P>> {
        match self.dirty.get(&pgno) {
            Some(p) => Ok(TxnPage::Dirty(p)),
            None => Ok(TxnPage::Clean(self.store.pager().read(pgno)?)),
        }
    }

    /// Prefers reusing a freed page; only grows the file once none is available.
    pub fn alloc(&mut self) -> Result<Pgno> {
        // This transaction's own discards first: they are free in every sense, and reusing one
        // keeps the file from growing by work that is already undone.
        if let Some(p) = self.scratch.pop() {
            self.dirty.remove(&p);
            return Ok(p);
        }
        if let Some(p) = self.freelist.alloc(self.horizon) {
            return Ok(p);
        }
        self.alloc_tail()
    }

    /// Allocates from the file tail without touching the freelist, which is what breaks the
    /// otherwise circular dependency when the freelist itself needs pages.
    pub fn alloc_tail(&mut self) -> Result<Pgno> {
        if let Some(cap) = self.store.pager().capacity() {
            if self.next_pgno >= cap {
                return Err(StoreError::MapSizeExhausted {
                    need: self.next_pgno + 1,
                    mapsize: cap,
                });
            }
        }
        let p = self.next_pgno as Pgno;
        self.next_pgno += 1;
        Ok(p)
    }

    pub fn free(&mut self, pgno: Pgno) {
        // A page from beyond the tail floor was allocated by this transaction, so it cannot be
        // in any committed tree and no reader can be looking at it. It goes to `scratch`, which
        // `alloc` empties first and `commit` gives back - see `Freelist::absorb_scratch`.
        if pgno as u64 >= self.tail_floor {
            self.dirty.remove(&pgno);
            self.scratch.push(pgno);
            return;
        }
        self.freelist.push(pgno, self.txn_id);
    }

    /// A modified page gets a NEW pgno; the old one is left completely intact.
    pub fn cow(&mut self, pgno: Pgno) -> Result<Pgno> {
        let content = (*self.read(pgno)?).clone();
        let new = self.alloc()?;
        self.dirty.insert(new, content);
        self.free(pgno);
        Ok(new)
    }

    pub fn write(&mut self, pgno: Pgno, page: Page) -> Result<()> {
        if (pgno as u64) >= self.next_pgno {
            return Err(StoreError::UnallocatedPage(pgno));
        }
        self.dirty.insert(pgno, page);
        Ok(())
    }

    pub fn root(&self, key: &FragmentKey) -> Option<Pgno> {
        self.roots.get(key)
    }

    pub fn set_root(&mut self, key: FragmentKey, root: Pgno) {
        self.dirty_chains.roots = true;
        self.roots.set(key, root);
    }

    pub fn remove_root(&mut self, key: &FragmentKey) -> Option<Pgno> {
        self.dirty_chains.roots = true;
        self.roots.remove(key)
    }

    pub fn roots(&self) -> &RootRecords {
        &self.roots
    }

    pub fn snapshots(&self) -> &SnapshotRegistry {
        &self.snapshots
    }

    /// Pins the state as of *before* this transaction; its root records are the ones on disk.
    pub fn create_snapshot(&mut self, expires_at: u64, name: &str, pinned: bool) -> Snapshot {
        self.dirty_chains.snapshots = true;
        let root = self.base.root_records.unwrap_or(0);
        self.snapshots.create(self.base.txn_id, root, expires_at, name, pinned)
    }

    pub fn drop_snapshot(&mut self, id: SnapshotId) -> Option<Snapshot> {
        self.dirty_chains.snapshots = true;
        self.snapshots.remove(id)
    }

    pub fn expire_snapshots(&mut self, now: u64) -> Vec<Snapshot> {
        self.dirty_chains.snapshots = true;
        self.snapshots.expire(now)
    }

    /// Replaces the opaque catalog blob. Written in the same commit as everything else, so a
    /// schema change and the data it describes land together or not at all.
    /// Replaces the catalog, and notices when nothing actually changed.
    ///
    /// Callers above hand the whole catalog back on every commit because that is the simplest
    /// thing for them to do. Comparing it here costs a memcmp; rewriting it costs the chain,
    /// which for a database with many fragments is hundreds of kilobytes and two fsyncs' worth
    /// of latency behind them.
    pub fn set_catalog(&mut self, entries: Vec<Vec<u8>>) {
        if entries != self.catalog {
            self.dirty_chains.catalog = true;
            self.catalog = entries;
        }
    }

    pub fn catalog(&self) -> &[Vec<u8>] {
        &self.catalog
    }

    pub fn dirty_len(&self) -> usize {
        self.dirty.len()
    }

    pub fn commit(mut self) -> Result<TxnId> {
        // **Before anything below allocates.** Nothing in `commit_txn` knows `scratch` exists,
        // which is exactly how a transaction whose tree shrank used to lose its own churn into
        // `page_count`. It has to happen here rather than deeper in: lowering `next_pgno` after
        // a chain page had been taken from the tail would hand that page's number out twice.
        self.next_pgno = self.freelist.absorb_scratch(
            core::mem::take(&mut self.scratch),
            self.next_pgno,
            self.txn_id,
        );
        let txn_id = self.store.commit_txn(
            self.txn_id,
            self.base,
            core::mem::take(&mut self.dirty),
            core::mem::take(&mut self.freelist),
            core::mem::take(&mut self.roots),
            core::mem::take(&mut self.snapshots),
            core::mem::take(&mut self.catalog),
            self.dirty_chains,
            &mut self.next_pgno,
        )?;
        self.committed = true;
        Ok(txn_id)
    }
}

impl<P: PagerMut> Drop for WriteTxn<'_, P> {
    /// Rollback is doing nothing at all: no page reached the disk and the old meta still
    /// points at the old tree.
    fn drop(&mut self) {
        let _ = self.committed;
    }
}
