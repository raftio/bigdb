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

//! The file and the transactions over it: an mmap read path, a `pwrite` write path, and pure
//! copy-on-write transactions.
//!
//! **No WAL, no checkpoint, nothing to replay after a crash.** A commit writes pages, fsyncs,
//! flips the meta page, and fsyncs again. There is no intermediate state, so there is no
//! recovery path to get wrong.
//!
//! What this crate owns is the part of the format that describes the file itself - meta page,
//! root records, freelist, page allocation, and the commit sequence. Everything above it sees
//! pages and transactions; it does not see files, mappings, or `unsafe`.
//!
//! See `readme.md` for the full design, including why the write side takes `&self`.

#![deny(unsafe_code)]

pub mod audit;
pub mod chainio;
#[cfg(feature = "counting-pager")]
pub mod counting;
pub mod durability;
pub mod error;
pub mod freelist;
pub mod io;
pub mod mem;
pub mod metrics;
pub mod pager;
pub mod roots;
pub mod snapshot;
pub mod txn;

#[cfg(unix)]
pub mod mmap;

pub use audit::{AuditTally, LeakReport, PageSet};
#[cfg(feature = "counting-pager")]
pub use counting::{CountingPager, PagerCounts};
pub use durability::Durability;
pub use error::{Result, StoreError};
pub use freelist::{FreeRun, Freelist, FREE_ENTRY_BYTES};
pub use io::{IoCounters, IoStats};
pub use mem::MemPager;
pub use metrics::Metrics;
pub use pager::{Pager, PagerMut};
pub use roots::RootRecords;
pub use snapshot::{Snapshot, SnapshotId, SnapshotRegistry, SNAPSHOT_ENTRY_BYTES, SNAP_PINNED};
pub use txn::{DirtyChains, ReadTxn, TxnPage, WriteTxn};

#[cfg(unix)]
pub use mmap::{MmapPager, DEFAULT_MAPSIZE};

// `PageError` is re-exported because `StoreError::Page` carries one: without it the variant
// is public but its payload is unnameable, so no crate above this one can match on it.
// `PAGE_SIZE` comes with them because this crate's own interface counts in *pages* -
// `page_count`, `capacity`, `grow` - and every caller that has to turn one of those into a
// number of bytes would otherwise reach past this crate for the constant that does it.
pub use big_page::{
    kind, meta, meta::META_PAGES, FragmentKey, MetaPage, Page, PageError, Pgno, TxnId,
    CATALOG_ENTRY_BYTES, PAGE_SIZE,
};

use big_page::{
    build_chain, chain_pages_needed as pages_needed, pick_meta, PageType, ROOT_RECORD_BYTES,
};
use chainio::{chain_pgnos, chain_pgnos_checked, load_chain};
use std::collections::BTreeMap;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, RwLock};

/// Test-only failpoint. Compiled out unless the `crash-injection` feature is on, so production
/// builds cannot be aborted by an environment variable.
#[cfg(feature = "crash-injection")]
pub(crate) fn crash_point(name: &str) {
    use std::sync::OnceLock;
    static AT: OnceLock<Option<String>> = OnceLock::new();
    if AT.get_or_init(|| std::env::var("BIG_CRASH_AT").ok()).as_deref() == Some(name) {
        // abort, not exit: nothing gets flushed, which is the whole point.
        std::process::abort();
    }
}

#[cfg(not(feature = "crash-injection"))]
pub(crate) fn crash_point(_name: &str) {}

/// Where a metadata chain ended up: rewritten into new pages, or left exactly where it was.
enum Placed {
    Fresh(Vec<Pgno>),
    Kept(Option<Pgno>),
}

impl Placed {
    /// The page the meta record points at.
    fn head(&self) -> Option<Pgno> {
        match self {
            Self::Fresh(p) => p.first().copied(),
            Self::Kept(p) => *p,
        }
    }
}

/// Allocates `n` pages, preferring a reused one over growing the file.
fn alloc_pages(
    n: usize,
    freelist: &mut Freelist,
    horizon: TxnId,
    next_pgno: &mut u64,
    cap: Option<u64>,
) -> Result<Vec<Pgno>> {
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        out.push(match freelist.alloc(horizon) {
            Some(p) => p,
            None => alloc_tail(next_pgno, cap)?,
        });
    }
    Ok(out)
}

/// Allocates the freelist's own pages, which is the one allocation that changes how many are
/// needed.
///
/// It runs to a fixed point rather than in one shot, and it terminates: `Freelist::alloc`
/// always takes from the front of a run and so never splits one, which means the entry count
/// is non-increasing. The requirement can only shrink while the page list only grows.
fn alloc_freelist_pages(
    freelist: &mut Freelist,
    horizon: TxnId,
    next_pgno: &mut u64,
    cap: Option<u64>,
) -> Result<Vec<Pgno>> {
    freelist.compact();
    let mut out: Vec<Pgno> = Vec::new();
    while out.len() < pages_needed(FREE_ENTRY_BYTES, freelist.entry_count()) {
        out.push(match freelist.alloc(horizon) {
            Some(p) => p,
            None => alloc_tail(next_pgno, cap)?,
        });
    }
    Ok(out)
}

fn alloc_tail(next_pgno: &mut u64, cap: Option<u64>) -> Result<Pgno> {
    if let Some(c) = cap {
        if *next_pgno >= c {
            return Err(StoreError::MapSizeExhausted { need: *next_pgno + 1, mapsize: c });
        }
    }
    let p = *next_pgno as Pgno;
    *next_pgno += 1;
    Ok(p)
}

fn read_meta<P: Pager>(pager: &P, slot: Pgno) -> Result<MetaPage> {
    let page = pager.read(slot)?;
    Ok(MetaPage::decode(&page)?)
}

/// Which of two meta failures to report.
///
/// A version mismatch wins over anything else: it is the only one with an action attached,
/// and the other slot failing for a different reason does not make it less true. Everything
/// else collapses to `NoValidMeta`, because "the first slot had a bad magic and the second a
/// bad checksum" tells an operator nothing the summary does not.
fn worse_of(a: StoreError, b: StoreError) -> StoreError {
    let is_version =
        |e: &StoreError| matches!(e, StoreError::Page(big_page::PageError::UnsupportedVersion(_)));
    if is_version(&a) {
        a
    } else if is_version(&b) {
        b
    } else {
        StoreError::NoValidMeta
    }
}

struct StoreState {
    meta: MetaPage,
    roots: Arc<RootRecords>,
    snapshots: Arc<SnapshotRegistry>,
    catalog: Arc<Vec<Vec<u8>>>,
    freelist: Freelist,
}

/// An audit in progress: the file's own bookkeeping, captured at one instant, plus the reader
/// that keeps it true while the caller walks the trees.
///
/// Everything reachable without leaving this crate is already marked. The caller marks the
/// trees - `roots` and then `snapshot_roots`, both of which need `big-btree` - and calls
/// [`Audit::finish`].
pub struct Audit<'db, P: Pager> {
    /// Held for the life of the audit. Dropping it early would let the horizon advance and a
    /// page named in the capture be recycled while the walk is still reading it.
    _read: ReadTxn<'db, P>,
    /// Pages something points at. The caller adds the trees.
    pub marks: PageSet,
    /// Pages the freelist holds, pending runs included.
    pub free: PageSet,
    /// Roots of the current trees, for the caller to walk.
    pub roots: Vec<Pgno>,
    /// Roots reachable only from a snapshot's old roots chain. **The class every other walk in
    /// this tree skips**, and the one an audit must not.
    pub snapshot_roots: Vec<Pgno>,
    /// Runs a writer could take right now. Anything both reachable and in one of these has
    /// been handed out twice.
    reusable: Vec<FreeRun>,
    tally: AuditTally,
    dangling: u64,
    free_total: u64,
    free_reusable: u64,
    page_count: u64,
    file_pages: u64,
    txn_id: TxnId,
}

impl<P: Pager> Audit<'_, P> {
    /// Marks a page the caller reached, counting it against a tree or a snapshot's tree.
    pub fn mark_tree_page(&mut self, pgno: Pgno, from_snapshot: bool) {
        if self.marks.insert(pgno) {
            if from_snapshot {
                self.tally.snapshot_trees += 1;
            } else {
                self.tally.trees += 1;
            }
        } else if pgno as u64 >= self.page_count {
            self.dangling += 1;
        }
    }

    /// The complement, once the caller has marked everything it can reach.
    pub fn finish(self) -> LeakReport {
        const SAMPLE: usize = 64;
        let leaked: Vec<Pgno> = self.marks.absent_from_both(&self.free).take(SAMPLE).collect();
        let mut reusable = PageSet::with_pages(self.page_count);
        for run in &self.reusable {
            for p in audit::run_pages(run, self.page_count) {
                reusable.insert(p);
            }
        }
        let doubled: Vec<Pgno> = self.marks.present_in_both(&reusable).take(SAMPLE).collect();

        LeakReport {
            txn_id: self.txn_id,
            page_count: self.page_count,
            file_pages: self.file_pages,
            reachable: self.marks.count(),
            free_total: self.free_total,
            free_reusable: self.free_reusable,
            leaked: self.marks.count_absent_from_both(&self.free),
            highest_leaked: self.marks.absent_from_both(&self.free).next_back(),
            leaked_sample: leaked,
            beyond_meta: self.file_pages.saturating_sub(self.page_count),
            dangling: self.dangling,
            double_allocated: self.marks.present_in_both(&reusable).count() as u64,
            double_allocated_sample: doubled,
            by_class: self.tally,
        }
    }
}

/// Bound only by `Pager`/`PagerMut`, so swapping the backend touches nothing in here.
pub struct Store<P: Pager> {
    pager: P,
    state: RwLock<StoreState>,
    /// Live readers, refcounted per `txn_id`. The smallest one is the freelist's floor.
    readers: Mutex<BTreeMap<TxnId, u32>>,
    write_lock: Mutex<()>,
    /// How hard a commit flushes. Atomic rather than behind the write lock: a commit reads it
    /// once at the top, and a reader of the metrics must not have to take a lock to report it.
    durability: std::sync::atomic::AtomicU8,
    /// Where the last commit's pages went. Behind its own lock rather than the state lock: it is
    /// written once per commit and read by whoever is curious, and neither should wait on the
    /// other.
    last_commit: Mutex<metrics::CommitBreakdown>,
    /// Notified after every commit, for [`Store::wait_for_txn`].
    ///
    /// **Its own mutex rather than the state lock.** A waiter has to hold something while it
    /// waits, and holding `state` would block the very commit it is waiting for. The mutex
    /// guards nothing at all — the transaction id it wakes to read lives in `state.meta` — so
    /// it is a `()`, and every waiter re-reads the real answer after each wake.
    committed: (Mutex<()>, Condvar),
}

impl<P: Pager> Store<P> {
    pub fn pager(&self) -> &P {
        &self.pager
    }

    /// The transaction the file is at right now.
    pub fn txn_id(&self) -> TxnId {
        self.state.read().unwrap().meta.txn_id
    }

    /// Waits until this store has committed `txn` or later, and says whether it did.
    ///
    /// **Bounded, always.** A caller naming a transaction this store will never reach - one
    /// from a different node, or one that was never committed here - must get an answer rather
    /// than a thread that never comes back, so the deadline is the caller's and there is no
    /// form of this without one.
    ///
    /// The condvar is only a hint that *something* committed: the loop re-reads the meta each
    /// time it wakes, so a spurious wake or a commit of some other transaction costs a read and
    /// nothing else.
    pub fn wait_for_txn(&self, txn: TxnId, deadline: std::time::Instant) -> bool {
        if self.txn_id() >= txn {
            return true;
        }
        let mut guard = self.committed.0.lock().unwrap_or_else(|p| p.into_inner());
        loop {
            if self.txn_id() >= txn {
                return true;
            }
            let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) else {
                return false;
            };
            let (next, timed_out) =
                self.committed.1.wait_timeout(guard, left).unwrap_or_else(|p| p.into_inner());
            guard = next;
            if timed_out.timed_out() {
                return self.txn_id() >= txn;
            }
        }
    }

    /// What a commit currently promises.
    pub fn durability(&self) -> Durability {
        Durability::from_u8(self.durability.load(std::sync::atomic::Ordering::Relaxed))
    }

    /// Gives the backend back, closing the store.
    ///
    /// For callers that built a store in order to hand the file to something else - a test
    /// reopening it under different conditions, a tool that is done with it. Taking `self`
    /// means no reader or writer can still be alive.
    pub fn into_pager(self) -> P {
        self.pager
    }

    pub fn meta(&self) -> MetaPage {
        self.state.read().unwrap().meta
    }

    pub fn roots(&self) -> Arc<RootRecords> {
        Arc::clone(&self.state.read().unwrap().roots)
    }

    pub fn snapshots(&self) -> Arc<SnapshotRegistry> {
        Arc::clone(&self.state.read().unwrap().snapshots)
    }

    /// Opaque catalog records, exactly as some layer above wrote them.
    pub fn catalog(&self) -> Arc<Vec<Vec<u8>>> {
        Arc::clone(&self.state.read().unwrap().catalog)
    }

    /// Whichever meta has the higher txn_id and a valid checksum wins. That is all of recovery.
    pub fn load(pager: P) -> Result<Self> {
        if pager.page_count() < META_PAGES {
            return Err(StoreError::NoValidMeta);
        }
        // Why each slot failed is kept, not flattened. Both slots failing used to be reported
        // as `NoValidMeta` whatever the cause, so a file written by a different build of big
        // was indistinguishable from a corrupted one - and the two call for opposite actions:
        // run the migration, or restore from backup.
        let a = read_meta(&pager, 0);
        let b = read_meta(&pager, 1);
        let meta = match (a, b) {
            (Ok(x), Ok(y)) => pick_meta(Ok(x), Ok(y)).map_err(|_| StoreError::NoValidMeta)?,
            (Ok(x), Err(_)) => x,
            (Err(_), Ok(y)) => y,
            (Err(ea), Err(eb)) => return Err(worse_of(ea, eb)),
        };

        let roots = RootRecords::from_entries(&load_chain(
            &pager,
            meta.root_records,
            PageType::RootRecords,
            ROOT_RECORD_BYTES,
        )?);
        let snapshots = SnapshotRegistry::from_entries(&load_chain(
            &pager,
            meta.snapshots,
            PageType::Snapshots,
            SNAPSHOT_ENTRY_BYTES,
        )?);
        let catalog = load_chain(&pager, meta.catalog, PageType::Catalog, CATALOG_ENTRY_BYTES)?;
        let freelist = Freelist::from_entries(&load_chain(
            &pager,
            meta.freelist,
            PageType::Freelist,
            FREE_ENTRY_BYTES,
        )?);

        Ok(Self {
            pager,
            state: RwLock::new(StoreState {
                meta,
                roots: Arc::new(roots),
                snapshots: Arc::new(snapshots),
                catalog: Arc::new(catalog),
                freelist,
            }),
            readers: Mutex::new(BTreeMap::new()),
            write_lock: Mutex::new(()),
            durability: std::sync::atomic::AtomicU8::new(Durability::default().as_u8()),
            last_commit: Mutex::new(metrics::CommitBreakdown::default()),
            committed: (Mutex::new(()), Condvar::new()),
        })
    }

    /// A reader reads the meta, takes the root, and is done. No lock, no waiting.
    pub fn begin_read(&self) -> ReadTxn<'_, P> {
        let st = self.state.read().unwrap();
        ReadTxn::new(self, st.meta.txn_id, Arc::clone(&st.roots), Arc::clone(&st.catalog))
    }

    /// Only registered snapshots are readable: the meta page carries no history of its own.
    ///
    /// The catalog handed back is the *current* one, not the one that was live when the
    /// snapshot was taken: a snapshot record pins root records and nothing else. For a schema
    /// that only ever grows this is harmless - the extra entries describe fragments the
    /// snapshot's roots simply do not have - but it stops being harmless the moment dropping
    /// a field can remove one, so a snapshot must not be used to read across a drop.
    pub fn begin_read_at(&self, id: SnapshotId) -> Result<ReadTxn<'_, P>> {
        let (snap, catalog) = {
            let st = self.state.read().unwrap();
            let snap = st.snapshots.get(id).ok_or(StoreError::SnapshotNotFound(id))?;
            (snap, Arc::clone(&st.catalog))
        };
        let root = (snap.root_records != 0).then_some(snap.root_records);
        let roots = RootRecords::from_entries(&load_chain(
            &self.pager,
            root,
            PageType::RootRecords,
            ROOT_RECORD_BYTES,
        )?);
        Ok(ReadTxn::new(self, snap.txn_id, Arc::new(roots), catalog))
    }

    /// Opens an audit: a reader, plus everything about the file's own bookkeeping taken at the
    /// same instant.
    ///
    /// **The capture has to happen here, under one lock.** A caller assembling the same facts
    /// from `meta()`, `snapshots()` and the freelist would take three separate read guards, and
    /// `commit_txn` publishes all of them under one write guard - so a mark set built from a
    /// straddled state would be wrong in a way no test reliably catches.
    ///
    /// This marks everything it can reach without leaving this crate: the meta pages, the four
    /// chains, and every chain a snapshot still names. What it cannot do is walk a b-tree -
    /// `big-btree` depends on this crate, not the other way round - so it hands back the roots
    /// and leaves that to the caller. See `Db::audit_pages`.
    ///
    /// The read transaction is held for the whole audit, which pins the reclaim horizon just as
    /// a backup does: `truncate_tail` will refuse for the duration, and any page named in this
    /// capture is safe from being recycled underneath the walk.
    pub fn begin_audit(&self) -> Result<Audit<'_, P>> {
        // One guard. The reader is registered before it is dropped, so nothing committed
        // between the capture and the registration can move the horizon past us.
        let (read, meta, freelist, snapshots, roots, horizon) = {
            let st = self.state.read().unwrap();
            let reader_h = self.oldest_reader().unwrap_or(st.meta.txn_id);
            let snap_h = st.snapshots.oldest_txn_id().unwrap_or(st.meta.txn_id);
            let read =
                ReadTxn::new(self, st.meta.txn_id, Arc::clone(&st.roots), Arc::clone(&st.catalog));
            (
                read,
                st.meta,
                st.freelist.clone(),
                Arc::clone(&st.snapshots),
                Arc::clone(&st.roots),
                reader_h.min(snap_h).min(st.meta.txn_id),
            )
        };

        let pages = meta.page_count;
        let mut marks = PageSet::with_pages(pages);
        let mut free = PageSet::with_pages(pages);
        let mut tally = AuditTally::default();
        let mut dangling = 0u64;

        // 1. Both meta slots. They are double-buffered - a commit writes only `txn_id % 2` -
        // so the one this file is not currently governed by still holds the previous commit
        // and is what `Store::load` falls back to.
        for slot in 0..META_PAGES {
            if marks.insert(slot as Pgno) {
                tally.meta += 1;
            }
        }

        // 2. The four chains' own pages.
        for (head, ty, stride) in Self::chain_shapes(&meta) {
            for p in chain_pgnos_checked(&self.pager, head, ty, stride)? {
                if marks.insert(p) {
                    tally.chains += 1;
                } else {
                    dangling += 1;
                }
            }
        }

        // 3. Every snapshot's *old* roots chain, and the roots on it. This is the class no
        // other walk in the tree visits: those trees are unreachable from the current roots
        // and entirely live. `0` is the absent marker, the same one `begin_read_at` uses.
        let mut snapshot_roots = Vec::new();
        for snap in snapshots.all() {
            let head = (snap.root_records != 0).then_some(snap.root_records);
            if head.is_none() {
                continue;
            }
            for p in
                chain_pgnos_checked(&self.pager, head, PageType::RootRecords, ROOT_RECORD_BYTES)?
            {
                if marks.insert(p) {
                    tally.snapshot_chains += 1;
                } else {
                    dangling += 1;
                }
            }
            let old = RootRecords::from_entries(&load_chain(
                &self.pager,
                head,
                PageType::RootRecords,
                ROOT_RECORD_BYTES,
            )?);
            snapshot_roots.extend(old.iter().map(|(_, p)| *p));
        }

        // 4. Everything the freelist holds, pending runs included: pending is not reusable,
        // but it is certainly not unaccounted for.
        let mut free_total = 0u64;
        let mut reusable = Vec::new();
        for run in freelist.runs() {
            for p in audit::run_pages(run, pages) {
                if free.insert(p) {
                    free_total += 1;
                }
            }
            if run.freed_at <= horizon {
                reusable.push(*run);
            }
        }

        Ok(Audit {
            _read: read,
            marks,
            free,
            roots: roots.iter().map(|(_, p)| *p).collect(),
            snapshot_roots,
            reusable,
            tally,
            dangling,
            free_total,
            free_reusable: freelist.reusable_pages(horizon),
            page_count: pages,
            file_pages: self.pager.page_count(),
            txn_id: meta.txn_id,
        })
    }

    /// The four chains, as `(head, type, stride)`. One list, so a caller cannot walk three of
    /// them and forget the fourth.
    fn chain_shapes(meta: &MetaPage) -> [(Option<Pgno>, PageType, usize); 4] {
        [
            (meta.root_records, PageType::RootRecords, ROOT_RECORD_BYTES),
            (meta.freelist, PageType::Freelist, FREE_ENTRY_BYTES),
            (meta.snapshots, PageType::Snapshots, SNAPSHOT_ENTRY_BYTES),
            (meta.catalog, PageType::Catalog, CATALOG_ENTRY_BYTES),
        ]
    }

    pub(crate) fn register_reader(&self, txn_id: TxnId) {
        *self.readers.lock().unwrap().entry(txn_id).or_insert(0) += 1;
    }

    pub(crate) fn release_reader(&self, txn_id: TxnId) {
        let mut r = self.readers.lock().unwrap();
        if let Some(n) = r.get_mut(&txn_id) {
            *n -= 1;
            if *n == 0 {
                r.remove(&txn_id);
            }
        }
    }

    fn oldest_reader(&self) -> Option<TxnId> {
        self.readers.lock().unwrap().keys().next().copied()
    }

    pub fn metrics(&self) -> Metrics {
        let st = self.state.read().unwrap();
        let oldest_reader = self.oldest_reader();
        let reader_h = oldest_reader.unwrap_or(st.meta.txn_id);
        let snap_h = st.snapshots.oldest_txn_id().unwrap_or(st.meta.txn_id);
        let h = reader_h.min(snap_h);

        // What snapshots block is known exactly; the rest is the readers' doing. Two separate knobs.
        let by_retention = st.freelist.pending_pages(snap_h);
        Metrics {
            oldest_reader_txn_id: oldest_reader,
            pages_pending_reclaim_reader: st.freelist.pending_pages(h) - by_retention,
            pages_pending_reclaim_retention: by_retention,
            free_pages_reusable: st.freelist.reusable_pages(h),
            page_count: self.pager.page_count(),
            live_readers: self.readers.lock().unwrap().values().map(|v| *v as usize).sum(),
            snapshots: st.snapshots.len(),
            fragments: st.roots.len(),
            txn_id: st.meta.txn_id,
            durability: self.durability(),
            last_commit: *self.last_commit.lock().unwrap(),
            io: self.pager.io_stats(),
        }
    }
}

impl<P: PagerMut> Store<P> {
    /// Fresh file: two meta pages, slot 0 written. The three chains are empty and cost nothing.
    pub fn init(pager: P) -> Result<Self> {
        pager.grow(META_PAGES)?;
        let meta = MetaPage { page_count: META_PAGES, ..Default::default() };
        pager.write(meta.slot() as Pgno, &meta.encode())?;
        pager.sync()?;
        Self::load(pager)
    }

    /// Opens a database, creating one only where there was nothing at all.
    ///
    /// **The empty case and the too-short case are not the same case.** An empty path is a
    /// database nobody has created yet, and creating it is what every caller wants. A file with
    /// bytes in it is somebody else's, and this used to initialise it - anything under two pages
    /// failed the `>= META_PAGES` test and was written over, so `big verify` pointed at the
    /// wrong path returned zero and left a fresh empty database where that file had been. A
    /// larger file was already refused, by the magic in its meta page; this refuses the rest,
    /// which is the range where there was no magic to check.
    pub fn open_or_init(pager: P) -> Result<Self> {
        match pager.page_count() {
            0 => Self::init(pager),
            n if n >= META_PAGES => Self::load(pager),
            n => Err(StoreError::NotADatabase { bytes: n * big_page::PAGE_SIZE as u64 }),
        }
    }

    /// Changes what a commit promises, from now on.
    ///
    /// Settable at any time rather than fixed at open, because the caller this exists for is a
    /// bulk load inside a process that also serves ordinary traffic: relax, load, tighten, and
    /// the relaxation lasts exactly as long as the load. Fixing it at open would have made that
    /// a restart.
    ///
    /// **Tightening flushes first.** Everything committed under the looser setting is made
    /// durable before the new setting takes effect, so the change is a line the caller can
    /// reason about: after it returns, every commit that has already happened is covered by at
    /// least the new promise. Without that, `set_durability(Full)` would announce a guarantee it
    /// had not yet delivered on, and the loader that carefully tightened up at the end of its
    /// batch would still lose it.
    ///
    /// Relaxing does not flush. There is nothing to make less durable.
    pub fn set_durability(&self, next: Durability) -> Result<()> {
        // Under the write lock: a commit reads the setting once at its start, and changing it
        // out from under one in flight would give that commit a guarantee neither level offers.
        let _excl = match self.write_lock.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        let current = self.durability();
        if next == current {
            return Ok(());
        }
        if next.at_least(current) {
            self.pager.sync()?;
        }
        self.durability.store(next.as_u8(), std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }

    /// Releases free pages sitting at the end of the file back to the filesystem.
    ///
    /// Only trailing pages are handled, because moving a page anywhere else means rewriting
    /// whoever points at it, and that knowledge lives in the b-tree rather than here.
    ///
    /// Refuses to run while any reader is alive: shrinking the file turns the region past the
    /// new EOF back into unbacked mapping, and a live borrow into it would be a SIGBUS.
    pub fn truncate_tail(&self) -> Result<u64> {
        let _excl = match self.write_lock.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        if !self.readers.lock().unwrap().is_empty() {
            return Err(StoreError::ReadersActive);
        }

        let (mut freelist, meta, horizon) = {
            let st = self.state.read().unwrap();
            let snap_h = st.snapshots.oldest_txn_id().unwrap_or(st.meta.txn_id);
            (st.freelist.clone(), st.meta, snap_h.min(st.meta.txn_id))
        };

        let released = freelist.trim_tail(meta.page_count, horizon);
        if released == 0 {
            return Ok(0);
        }
        let new_count = meta.page_count - released;

        // The freelist has to reach the disk too, and this is the whole reason:
        //
        // Trimming happens in memory. Without writing it back, the *chain on disk* still lists
        // the runs that were just trimmed - and the meta page now says the file is shorter than
        // they are. In this process nothing notices, because `st.freelist` below is correct and
        // the next commit rewrites the chain anyway. Reopen the file before that commit and the
        // freelist comes back with pages past the end of the file, which `alloc` then hands out:
        // the next write lands on a page that does not exist.
        //
        // Rewritten **in place**, over the pages the chain already occupies, which is safe here
        // and nowhere else: this method already refuses to run while any reader is alive, so
        // nobody can be looking at the old chain. The pages are guaranteed to survive the
        // truncation because they are live - and a live page cannot be inside a free run, which
        // is all the trim removed. The trimmed list has fewer entries than the old one, so it
        // never needs more pages than it already has.
        //
        // A crash between this and the meta flip leaves the old meta pointing at a chain that
        // has forgotten some free pages: they leak until the next compaction finds them again.
        // That is the right way round - a leak, never a page handed out twice.
        let chain = chain_pgnos(&self.pager, meta.freelist, PageType::Freelist, FREE_ENTRY_BYTES)?;
        debug_assert!(
            chain.iter().all(|p| (*p as u64) < new_count),
            "a live freelist chain page cannot sit inside the free run being trimmed"
        );
        for (pgno, page) in
            build_chain(PageType::Freelist, FREE_ENTRY_BYTES, &freelist.encode(), &chain)
        {
            self.pager.write(pgno, &page)?;
        }

        // Meta first, then shrink. A crash in between leaves a file longer than the meta says,
        // which is harmless; the reverse would leave the meta pointing past EOF.
        let mut next = meta;
        next.txn_id = meta.txn_id + 1;
        next.page_count = new_count;
        self.pager.write(next.slot() as Pgno, &next.encode())?;
        // Unconditional, whatever the durability setting says. "Harmless" above is only true if
        // the shorter meta is actually on disk when the file shrinks; the other order leaves a
        // meta naming pages past EOF, which is a file that does not open. Nobody asked for a
        // faster truncate, and this is not the place to invent the request.
        self.pager.sync()?;
        self.pager.truncate(new_count)?;

        let mut st = self.state.write().unwrap();
        st.meta = next;
        st.freelist = freelist;
        Ok(released)
    }

    /// One writer at a time. Readers carry on unaffected while it runs.
    pub fn begin_write(&self) -> WriteTxn<'_, P> {
        let excl: MutexGuard<'_, ()> = match self.write_lock.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let st = self.state.read().unwrap();
        let reader_h = self.oldest_reader().unwrap_or(st.meta.txn_id);
        let snap_h = st.snapshots.oldest_txn_id().unwrap_or(st.meta.txn_id);
        WriteTxn::new(
            self,
            excl,
            st.meta,
            reader_h.min(snap_h).min(st.meta.txn_id),
            st.freelist.clone(),
            (*st.roots).clone(),
            (*st.snapshots).clone(),
            (*st.catalog).clone(),
        )
    }

    /// ftruncate, pwrite, fsync, meta, fsync. Atomicity lives entirely in the meta write.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn commit_txn(
        &self,
        txn_id: TxnId,
        base: MetaPage,
        pages: BTreeMap<Pgno, Page>,
        mut freelist: Freelist,
        roots: RootRecords,
        snapshots: SnapshotRegistry,
        catalog: Vec<Vec<u8>>,
        dirty: DirtyChains,
        next_pgno: &mut u64,
    ) -> Result<TxnId> {
        let cap = self.pager.capacity();
        // The oldest transaction anything can still be reading. Pages freed at or after it
        // must stay where they are, however dead they look from here.
        let horizon = {
            let reader_h = self.oldest_reader().unwrap_or(base.txn_id);
            let snap_h = snapshots.oldest_txn_id().unwrap_or(base.txn_id);
            reader_h.min(snap_h).min(base.txn_id)
        };
        let alloc = |n: usize, freelist: &mut Freelist, next_pgno: &mut u64| {
            alloc_pages(n, freelist, horizon, next_pgno, cap)
        };

        // 1 and 2. A chain this transaction did not change keeps the pages it already has:
        //    they hold exactly the bytes that would be written back into new ones. Only a
        //    changed chain is recycled and reallocated.
        let mut root_entries = Vec::new();
        let root_pgnos = if dirty.roots {
            self.recycle_chain(
                base.root_records,
                PageType::RootRecords,
                ROOT_RECORD_BYTES,
                &mut freelist,
                txn_id,
            )?;
            root_entries = roots.encode();
            Placed::Fresh(alloc(
                pages_needed(ROOT_RECORD_BYTES, root_entries.len()),
                &mut freelist,
                next_pgno,
            )?)
        } else {
            Placed::Kept(base.root_records)
        };

        let mut snap_entries = Vec::new();
        let snap_pgnos = if dirty.snapshots {
            self.recycle_chain(
                base.snapshots,
                PageType::Snapshots,
                SNAPSHOT_ENTRY_BYTES,
                &mut freelist,
                txn_id,
            )?;
            snap_entries = snapshots.encode();
            Placed::Fresh(alloc(
                pages_needed(SNAPSHOT_ENTRY_BYTES, snap_entries.len()),
                &mut freelist,
                next_pgno,
            )?)
        } else {
            Placed::Kept(base.snapshots)
        };

        let cat_pgnos = if dirty.catalog {
            self.recycle_chain(
                base.catalog,
                PageType::Catalog,
                CATALOG_ENTRY_BYTES,
                &mut freelist,
                txn_id,
            )?;
            Placed::Fresh(alloc(
                pages_needed(CATALOG_ENTRY_BYTES, catalog.len()),
                &mut freelist,
                next_pgno,
            )?)
        } else {
            Placed::Kept(base.catalog)
        };

        // 3. The freelist last, because allocating for it mutates the very thing being
        //    serialised.
        self.recycle_chain(
            base.freelist,
            PageType::Freelist,
            FREE_ENTRY_BYTES,
            &mut freelist,
            txn_id,
        )?;
        let free_pgnos = alloc_freelist_pages(&mut freelist, horizon, next_pgno, cap)?;
        // The freelist is about to be written down as the file's record of what is spare, so
        // nothing in it may name a page beyond where the file ends. Guards the old invariant as
        // much as `absorb_scratch`, which is the newest way to reach it.
        debug_assert!(
            freelist.runs().iter().all(|r| r.first as u64 + r.len as u64 <= *next_pgno),
            "a free run runs past the end of the file: {:?} against {next_pgno}",
            freelist.runs()
        );
        let free_entries = freelist.encode();

        // 4. Collect every page that has to reach the disk. A kept chain contributes none.
        //
        // Counted by class on the way, because "this commit wrote 41 pages" is not something
        // anyone can act on: the b-tree paths and the fixed chains call for opposite fixes, and
        // which of them dominates is a question about the workload rather than about the engine.
        let mut tally = metrics::CommitBreakdown::default();
        let mut out: Vec<(Pgno, Page)> = pages.into_iter().collect();
        tally.data = out.len() as u64;
        if let Placed::Fresh(p) = &root_pgnos {
            out.extend(build_chain(
                PageType::RootRecords,
                ROOT_RECORD_BYTES,
                &root_entries,
                p.as_slice(),
            ));
            tally.roots = p.len() as u64;
        }
        if let Placed::Fresh(p) = &snap_pgnos {
            out.extend(build_chain(
                PageType::Snapshots,
                SNAPSHOT_ENTRY_BYTES,
                &snap_entries,
                p.as_slice(),
            ));
            tally.snapshots = p.len() as u64;
        }
        if let Placed::Fresh(p) = &cat_pgnos {
            out.extend(build_chain(PageType::Catalog, CATALOG_ENTRY_BYTES, &catalog, p.as_slice()));
            tally.catalog = p.len() as u64;
        }
        out.extend(build_chain(PageType::Freelist, FREE_ENTRY_BYTES, &free_entries, &free_pgnos));
        tally.freelist = free_pgnos.len() as u64;

        let next_meta = MetaPage {
            txn_id,
            page_count: *next_pgno,
            root_records: root_pgnos.head(),
            freelist: free_pgnos.first().copied(),
            snapshots: snap_pgnos.head(),
            catalog: cat_pgnos.head(),
            flags: base.flags,
        };

        // 5. Everything above only decided what to write. This is the part a crash can
        //    interrupt, and the only part whose order matters.
        self.write_and_flip(&out, *next_pgno, &next_meta)?;

        let mut st = self.state.write().unwrap();
        st.meta = next_meta;
        st.roots = Arc::new(roots);
        st.snapshots = Arc::new(snapshots);
        if dirty.catalog {
            st.catalog = Arc::new(catalog);
        }
        st.freelist = freelist;
        drop(st);
        *self.last_commit.lock().unwrap() = tally;
        // After the state is published, so anybody woken here reads the new meta rather than
        // the one this commit replaced.
        self.committed.1.notify_all();
        Ok(txn_id)
    }

    /// Returns every page of an existing chain to the freelist.
    ///
    /// Stamped with this transaction, not released outright: a reader still on the old meta
    /// page is walking these pages right now, and the horizon is what keeps them alive until
    /// it is gone.
    fn recycle_chain(
        &self,
        base: Option<Pgno>,
        kind: PageType,
        span: usize,
        freelist: &mut Freelist,
        txn_id: TxnId,
    ) -> Result<()> {
        for p in chain_pgnos(&self.pager, base, kind, span)? {
            freelist.push(p, txn_id);
        }
        Ok(())
    }

    /// ftruncate, pwrite, fsync, meta, fsync. Atomicity lives entirely in the meta write.
    ///
    /// Until the meta page lands, every byte written above is unreachable garbage and a crash
    /// costs nothing. After it lands, all of it is live. There is no state in between, which
    /// is the whole reason there is no WAL.
    fn write_and_flip(
        &self,
        out: &[(Pgno, Page)],
        next_pgno: u64,
        next_meta: &MetaPage,
    ) -> Result<()> {
        // Read once, at the top. A setting that changed halfway through a commit would give
        // that commit two different guarantees, and the weaker half is the one that counts.
        let durability = self.durability();

        self.pager.grow(next_pgno)?;
        for (pgno, page) in out {
            self.pager.write(*pgno, page)?;
        }
        self.flush(durability)?;
        crash_point("after_data_sync");

        self.pager.write(next_meta.slot() as Pgno, &next_meta.encode())?;
        crash_point("after_meta_write");
        self.flush(durability)?;
        Ok(())
    }

    /// One of a commit's two flushes.
    ///
    /// Both calls in `write_and_flip` go through here, and no level distinguishes between them.
    /// That is deliberate and it is the whole safety argument: a level that skipped the first
    /// flush but kept the second would let the meta page reach the disk while the pages it
    /// names had not, and a crash there is not lost data but an unopenable file. Relaxing
    /// durability may cost recent commits. It may never cost the file.
    fn flush(&self, durability: Durability) -> Result<()> {
        match durability {
            Durability::Full => self.pager.sync(),
            Durability::Barrier => self.pager.sync_data(),
            Durability::None => Ok(()),
        }
    }
}
