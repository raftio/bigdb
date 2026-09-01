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

//! The database handle. Resolves names to interned ids, fragments to roots, and keeps the
//! schema and the data it describes inside the same transaction.
//!
//! Split by *what a caller is doing*, because the three things a caller can do with a `Db` have
//! almost nothing in common beyond the handle they start from. What lives here is only the part
//! all three need: the handle itself, the name resolution every path opens with, and the two
//! address types that cross between them.
//!
//! - `schema` declares and drops. Ordinary transactions; there is no separate DDL path.
//! - `write` is one transaction committed by hand, and the buffering that makes it affordable.
//! - `read` is a read transaction and its ceilings, plus the fan-out every scan goes through.
//! - `query` is the verbs that fan-out answers, each of them three lines over `read`.

mod query;
mod read;
mod scan;
mod schema;
mod write;

pub use read::{DbRead, QueryLimits};
pub use write::{At, DbWrite};

use crate::catalog::*;
use crate::error::{DbError, Result};
use crate::matches::Matches;
use big_engine::bitmap::field::{
    bsi::EXISTS_ROW, BoolField, Bsi, Granularity, MutexField, RangeOp,
};
use big_engine::bitmap::write::{group_offsets, merge_grouped, Grouped};
use big_engine::bitmap::{
    Container, ContainerKey, FragmentKey, FragmentRead, FragmentWrite, RowSet,
};
pub use big_engine::columnar::Cell;
use big_engine::columnar::{ColumnRead, ColumnWrite};
use big_engine::{local_of, shard_of, RecordId, RowId, ShardId, SHARD_WIDTH};
use big_pager::{Durability, MemPager, Pager, PagerMut, ReadTxn, Store, TxnId, WriteTxn};
use std::collections::BTreeMap;
use std::sync::RwLock;

pub struct Db<P: Pager> {
    store: Store<P>,
    catalog: RwLock<Catalog>,
}

impl<P: PagerMut> Db<P> {
    pub fn open(pager: P) -> Result<Self> {
        let store = Store::open_or_init(pager)?;
        let catalog = Catalog::from_entries(&store.catalog())?;
        Ok(Self { store, catalog: RwLock::new(catalog) })
    }

    /// What a commit currently promises. See [`Durability`].
    pub fn durability(&self) -> Durability {
        self.store.durability()
    }

    /// Changes what a commit promises, from now on. Tightening flushes first; see
    /// [`Store::set_durability`].
    pub fn set_durability(&self, next: Durability) -> Result<()> {
        Ok(self.store.set_durability(next)?)
    }

    pub fn store(&self) -> &Store<P> {
        &self.store
    }

    /// Wraps a store whose catalog has already been decoded. The one door for code that built
    /// the store itself rather than opening a file - see [`crate::copy`].
    pub(crate) fn from_parts(store: Store<P>, catalog: Catalog) -> Self {
        Self { store, catalog: RwLock::new(catalog) }
    }

    /// Writes a consistent, compact copy of this database onto `dest`.
    ///
    /// Safe to call while writers are running: the walk holds a read transaction, so it sees
    /// one transaction's worth of state from start to finish. This is the supported way to
    /// take a backup - copying the file with `cp` while a writer is live is not, because a
    /// commit can land between the bytes the copy has already read and the ones it has not.
    pub fn copy_to<D: PagerMut>(&self, dest: D) -> Result<Db<D>> {
        crate::copy::copy_to(self, dest)
    }

    pub fn catalog(&self) -> std::sync::RwLockReadGuard<'_, Catalog> {
        self.catalog.read().unwrap()
    }

    /// Recomputes every checksum this database can reach, and stops at the first that is wrong.
    ///
    /// **What it is for.** A branch or leaf page carries a crc32 that nothing on the query path
    /// checks - verifying costs a pass over 8 KiB while answering a point read touches one bit,
    /// so checking on every read would make the check the entire cost of the query. The
    /// consequence is that a rotted page is not an error when a query lands on it, it is a
    /// different answer. Scrubbing is how that gets found on a schedule an operator chose
    /// rather than on a request a client made.
    ///
    /// Runs under an ordinary read transaction, so it is safe while writers commit. It reads
    /// every live page, so budget it for the size of the live data and run it when there is
    /// I/O to spare.
    ///
    /// The three fixed chains - roots, catalog, freelist - are not counted here because they
    /// are already verified whenever they are read, which opening the transaction has just
    /// done. What this adds is the trees, which are everything else.
    pub fn scrub(&self) -> Result<big_btree::Scrubbed> {
        let read = self.store.begin_read();
        let mut total = big_btree::Scrubbed::default();
        for (_, root) in read.roots().iter() {
            total.add(big_btree::scrub_tree(self.store.pager(), *root)?);
        }
        Ok(total)
    }

    /// What the row-key dictionary costs this process right now.
    ///
    /// Every row key is resident, in both directions, for the life of the process: a key has
    /// to mean the same thing in every shard, so the translation cannot be paged out and
    /// looked up per fact. That makes the dictionary the one part of this engine whose memory
    /// grows with the *cardinality* of the data rather than with its size, and the one an
    /// operator has no other way to see coming.
    pub fn key_stats(&self) -> KeyStats {
        let c = self.catalog.read().unwrap();
        KeyStats {
            count: c.keys.len(),
            resident_bytes: c.keys.resident_bytes(),
            limit: c.keys.key_limit(),
        }
    }

    /// Sets the ceiling on how many row keys this database will invent, or removes it.
    ///
    /// Refuses rather than evicts: a key that has been handed out names bits already written,
    /// so forgetting it would make those bits unreadable rather than free anything. The
    /// ceiling therefore stops the dictionary growing and never shrinks it.
    pub fn set_key_limit(&self, limit: Option<usize>) {
        self.catalog.write().unwrap().keys.set_key_limit(limit);
    }

    /// A bulk load into a table whose fragments are empty.
    ///
    /// Writes every fragment exactly once by grouping facts by fragment before committing, so
    /// no fragment is ever read back. That is the whole difference from [`ingest`]: the tree is
    /// already built bottom-up whenever it has no root, and what a streaming ingest cannot do is
    /// keep the commits *disjoint*.
    ///
    /// Holds the whole load in memory. For a stream, or for adding to a table that already has
    /// data, use [`ingest`] - this refuses rather than merging.
    ///
    /// [`ingest`]: Db::ingest
    pub fn bulk_load(&self, table: &str) -> Result<crate::bulk::BulkLoad<'_, P>> {
        crate::bulk::BulkLoad::new(self, table)
    }

    /// A buffered writer that decides for itself when to commit.
    ///
    /// `capacity` divided by the number of shards the workload touches is how many records
    /// each fragment receives per commit, which is the quantity the engine's write cost
    /// actually scales with. See [`crate::ingest`].
    pub fn ingest(&self, capacity: usize) -> crate::ingest::Ingest<'_, P> {
        crate::ingest::Ingest::new(self, capacity)
    }
}

impl Db<MemPager> {
    pub fn in_memory() -> Result<Self> {
        Self::open(MemPager::new())
    }
}

#[cfg(unix)]
impl<P: PagerMut> Db<P> {
    /// Backs the database up to a new file, which is a complete database of its own.
    ///
    /// Refuses an existing path outright. A backup that can overwrite is a backup that can
    /// destroy the previous one, and the caller who typed the wrong name is exactly the caller
    /// who needed it.
    ///
    /// Restoring is opening the file. There is no separate restore step, and nothing about the
    /// result remembers where it came from.
    pub fn backup_to(&self, path: impl AsRef<std::path::Path>) -> Result<()> {
        self.backup_to_sized(path, big_pager::DEFAULT_MAPSIZE)
    }

    /// The same, with the destination's address-space reservation named.
    ///
    /// Worth naming whenever the source's was: an operator who chose a small `mapsize` because
    /// this machine runs many databases has not stopped caring about address space for the
    /// duration of a backup, and a destination that reserved a terabyte anyway would undo the
    /// choice exactly when the most files are open at once.
    pub fn backup_to_sized(&self, path: impl AsRef<std::path::Path>, mapsize: u64) -> Result<()> {
        let path = path.as_ref();
        if path.exists() {
            return Err(DbError::BackupDestinationExists(path.to_path_buf()));
        }
        self.copy_to(big_pager::MmapPager::open(path, mapsize)?)?;
        Ok(())
    }
}

#[cfg(unix)]
impl Db<big_pager::MmapPager> {
    pub fn open_path(path: impl AsRef<std::path::Path>) -> Result<Self> {
        Self::open_path_sized(path, big_pager::DEFAULT_MAPSIZE)
    }

    /// Opens with an explicit address-space reservation rather than the default terabyte.
    ///
    /// The reservation is made once and never remapped, so it is the file's ceiling for the
    /// life of the process: past it a write fails with `MapSizeExhausted` rather than moving
    /// the mapping under a live borrow. That makes it an operator's decision rather than a
    /// tuning knob - one process serving many small databases wants a smaller number than the
    /// default, and one serving a large one wants a larger.
    pub fn open_path_sized(path: impl AsRef<std::path::Path>, mapsize: u64) -> Result<Self> {
        Self::open(big_pager::MmapPager::open(path, mapsize)?)
    }
}

/// What the row-key dictionary holds, and what it is allowed to hold.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct KeyStats {
    /// Distinct row keys across every field of every table.
    pub count: usize,
    /// Approximately what they occupy in memory. See [`big_keys::KeyStore::resident_bytes`]:
    /// it counts the keys and not the maps' own overhead, so it under-reports on purpose.
    pub resident_bytes: usize,
    /// The ceiling on inventing new ones, or `None` for none.
    pub limit: Option<usize>,
}

/// The depth a field was declared with, with zero meaning the full width.
///
/// Distinct from a fragment's depth on purpose: a fragment's grows with the data, and the sign
/// bias must not. See [`crate::signed`].
fn declared_depth(def: &FieldDef) -> u32 {
    if def.bit_depth == 0 {
        64
    } else {
        def.bit_depth
    }
}

/// Resolves a name pair to the ids the storage layer actually uses.
fn resolve(catalog: &Catalog, table: &str, field: &str) -> Result<(TableId, FieldDef)> {
    let t = catalog.table(table).ok_or_else(|| DbError::UnknownTable(table.to_string()))?;
    let f = catalog.field(t.id, field).ok_or_else(|| DbError::UnknownField {
        table: table.to_string(),
        field: field.to_string(),
    })?;
    Ok((t.id, f.clone()))
}

/// Refuses a write whose field is not the kind the setter is for.
///
/// Without it `set_key` on an integer field would intern a key and set a bit inside a
/// bit-sliced index, changing the stored number with no error anywhere.
fn expect_kind(
    def: &FieldDef,
    name: &str,
    ok: impl Fn(FieldKind) -> bool,
    expected: &'static str,
) -> Result<()> {
    if ok(def.kind) {
        Ok(())
    } else {
        Err(DbError::WrongFieldKind { field: name.to_string(), expected })
    }
}

/// One fragment, named the way every node can resolve for itself.
///
/// A [`FragmentKey`] is table, field and view *ids*, and those are each node's own numbering -
/// nothing on the wire may depend on two nodes agreeing about them. Names do the resolving, as
/// they do everywhere else here; the ids ride along only for the reserved slots that have no
/// name and the same number on every node.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct FragmentAddr {
    pub table: String,
    /// `None` for the reserved existence field, which is not a field anybody named.
    pub field: Option<String>,
    /// `None` for the standard view, and for the mutex shadow: neither is named.
    pub view: Option<String>,
    /// Only read when `field` is `None`.
    pub field_id: FieldId,
    /// Only read when `view` is `None`.
    pub view_id: ViewId,
    pub shard: ShardId,
}

/// Reserved view holding the shadow BSI of a mutex field, so the value row space stays exactly
/// what the user declared.
pub const MUTEX_SHADOW_VIEW: ViewId = u32::MAX;

/// The column segment a record's value lives in, and the slot inside it.
///
/// Two levels because a segment is addressed per shard and encoded per block: the shard picks
/// the tree, the block picks the cell, and the slot picks the value inside it.
pub fn column_site(record: RecordId) -> (u64, usize) {
    let local = local_of(record);
    (big_engine::columnar::block_of(local), big_engine::columnar::slot_of(local))
}

/// Reserved view holding a field's column segments.
///
/// A column segment is addressed by the *same* [`FragmentKey`] as its bitmaps, differing only in
/// the view. That is the whole reason it is a view id and not a second addressing scheme: root
/// records, the backup walk, `drop_table`, `drop_field` and the cluster's fragment addressing
/// all work in terms of a `FragmentKey`, and every one of them reaches column segments without
/// being taught what one is.
pub const COLUMN_VIEW: ViewId = u32::MAX - 1;

// Both reserved ids have to sit inside the band the view allocator refuses to enter. Without
// this, widening the band or moving a constant would go unnoticed until a named view was handed
// an id that a fragment lookup reads as a column segment.
const _: () = assert!(MUTEX_SHADOW_VIEW > ViewId::MAX - crate::catalog::RESERVED_VIEWS);
const _: () = assert!(COLUMN_VIEW > ViewId::MAX - crate::catalog::RESERVED_VIEWS);
const _: () = assert!(MUTEX_SHADOW_VIEW != COLUMN_VIEW);
/// Enough to address any row id a keyed field will realistically have.
pub const SHADOW_DEPTH: u32 = 32;
