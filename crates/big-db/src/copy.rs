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

//! Copying a whole database into a fresh one.
//!
//! Backup, compaction and format migration are the same operation wearing three names: walk
//! everything reachable from one consistent set of roots and write it somewhere else with new
//! page numbers. Doing it once means a backup cannot lose a page class that compaction knows
//! about, and it means the compaction path is exercised by every backup test.
//!
//! Two properties fall out of the destination being an ordinary write transaction rather than
//! a byte-level clone:
//!
//! - **The copy is compact.** Pages are allocated in walk order into a store whose freelist is
//!   empty, so the holes copy-on-write left behind in the source do not exist in the result.
//!   This is the only thing in the engine that gives space back rather than reusing it.
//! - **The copy is durable on its own terms.** It commits through the normal path, so it gets
//!   the same checksums, the same meta flip and the same two fsyncs as any other transaction.
//!   There is no separate recovery story for a backup file.

use crate::catalog::Catalog;
use crate::db::Db;
use crate::error::{DbError, Result};
use big_pager::{PagerMut, Store, TxnId, META_PAGES};

/// Copies `src` into a fresh store on `dest`, returning it as a database.
///
/// The read transaction is held for the whole walk. That is what makes the copy consistent:
/// a live reader holds the reclaim horizon down, so no page the walk is about to visit can be
/// handed out to a concurrent writer underneath it. Writers keep running; they simply cannot
/// reuse anything this reader can still see.
pub fn copy_to<P: PagerMut, D: PagerMut>(src: &Db<P>, dest: D) -> Result<Db<D>> {
    if dest.page_count() >= META_PAGES {
        return Err(DbError::BackupDestinationNotEmpty);
    }
    let store = Store::init(dest)?;
    copy_into_store(src, &store)?;

    let catalog = Catalog::from_entries(&store.catalog())?;
    Ok(Db::from_parts(store, catalog))
}

/// The walk itself, against a store that is already open.
fn copy_into_store<P: PagerMut, D: PagerMut>(src: &Db<P>, dest: &Store<D>) -> Result<TxnId> {
    let read = src.store().begin_read();
    let mut w = dest.begin_write();

    // Root records and catalog come from the same reader, so the schema always describes
    // exactly the fragments that were copied - never one more and never one fewer.
    for (key, root) in read.roots().iter() {
        let new_root = big_btree::copy_tree(src.store().pager(), &mut w, *root)?;
        w.set_root(*key, new_root);
    }
    w.set_catalog(read.catalog().as_ref().clone());

    // Snapshots are deliberately not carried over. A snapshot names a page that holds the
    // *source's* root records, and that page number means nothing here; a backup is a fresh
    // history starting at one transaction, not a copy of someone else's.
    Ok(w.commit()?)
}

/// What a compaction did, in the two numbers an operator will ask for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Compacted {
    pub pages_before: u64,
    pub pages_after: u64,
}

impl Compacted {
    pub fn pages_reclaimed(&self) -> u64 {
        self.pages_before.saturating_sub(self.pages_after)
    }
}

/// Rewrites a database file as a compact copy of itself, in place.
///
/// This is the one operation that hands space back to the filesystem. `Store::truncate_tail`
/// releases free pages that happen to sit at the end of the file; everything else the freelist
/// holds is reused but never surrendered, so a file that once held a large table keeps that
/// size for ever. A compaction is the copy path plus a rename.
///
/// Offline: the file is opened exclusively, so a second process holding it is an error rather
/// than a race. Nothing else may have the database open.
///
/// Crash safety comes from `rename` being atomic. Until it runs, the original file is
/// untouched and the worst outcome is a leftover `.compacting` file; after it runs, the new
/// file is the database. The directory is fsynced afterwards so the rename itself survives a
/// power cut - without that, the rename can be lost while the data it published is not.
#[cfg(unix)]
pub fn compact_path(path: impl AsRef<std::path::Path>) -> Result<Compacted> {
    use big_pager::MmapPager;

    let path = path.as_ref();
    let temp = path.with_extension("compacting");
    if temp.exists() {
        return Err(DbError::BackupDestinationExists(temp));
    }

    let (pages_before, pages_after) = {
        let src = Db::open(MmapPager::open_default(path)?)?;
        let before = src.store().metrics().page_count;
        let copy = src.copy_to(MmapPager::open_default(&temp)?)?;
        let after = copy.store().metrics().page_count;
        (before, after)
        // Both handles close here: the exclusive locks and the mappings have to be gone
        // before the file underneath one of them is replaced.
    };

    std::fs::rename(&temp, path)?;
    if let Some(dir) = path.parent() {
        // A rename is only as durable as the directory recording it.
        std::fs::File::open(dir)?.sync_all()?;
    }
    Ok(Compacted { pages_before, pages_after })
}
