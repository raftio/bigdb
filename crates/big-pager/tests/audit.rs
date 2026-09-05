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

//! Accounting for every page: reachable, free, or neither.
//!
//! **The test that matters most here is the positive control.** A leak counter that returns
//! zero is indistinguishable from one that never looks, so `an_orphaned_page_is_reported` -
//! which deliberately strands a page and demands the counter find it - is what makes every
//! other zero in this file mean something.
//!
//! Everything below drives the pager directly rather than a `Db`, because the trees are the
//! caller's half of the walk. The `Db` half is `crates/big-db/tests/leaks.rs`.

use big_page::{FragmentKey, LeafBuilder};
use big_pager::*;

fn key(field: u32, shard: u64) -> FragmentKey {
    FragmentKey::new(1, field, 0, shard)
}

/// One leaf page, named by a root. Returns its page number.
fn put_fragment<P: PagerMut>(store: &Store<P>, k: FragmentKey) -> u32 {
    let mut w = store.begin_write();
    let p = w.alloc().unwrap();
    w.write(p, LeafBuilder::new().finish(p)).unwrap();
    w.set_root(k, p);
    w.commit().unwrap();
    p
}

/// Runs an audit, marking every root's single leaf page - which is the whole of a tree in
/// these fixtures. The real walk is `Db::audit_pages`.
fn audit<P: Pager>(store: &Store<P>) -> LeakReport {
    let mut a = store.begin_audit().expect("the file is readable");
    for root in core::mem::take(&mut a.roots) {
        a.mark_tree_page(root, false);
    }
    for root in core::mem::take(&mut a.snapshot_roots) {
        a.mark_tree_page(root, true);
    }
    a.finish()
}

// -------------------------------------------------------------------------------------------
// The bitset
// -------------------------------------------------------------------------------------------

#[test]
fn marking_the_same_page_twice_counts_once() {
    let mut s = PageSet::with_pages(100);
    assert!(s.insert(7));
    assert!(s.insert(7));
    assert_eq!(s.count(), 1);
    assert!(s.contains(7));
    assert!(!s.contains(8));
}

/// A page past the end of the file is not a member that happens to be absent - it is outside
/// the question, and the caller is told so rather than left to assume it landed.
#[test]
fn a_page_past_the_end_is_refused_rather_than_dropped() {
    let mut s = PageSet::with_pages(64);
    assert!(!s.insert(64));
    assert!(!s.insert(1_000));
    assert_eq!(s.count(), 0);
}

/// **The off-by-one that will actually happen.** A file whose page count is not a multiple of
/// 64 has bits in its last word describing pages that do not exist, and a complement that
/// counted them would report leaks which are not pages at all.
#[test]
fn the_complement_ignores_the_bits_past_the_end_of_the_file() {
    let pages = 70; // not a multiple of 64
    let mut marks = PageSet::with_pages(pages);
    let free = PageSet::with_pages(pages);
    for p in 0..pages {
        marks.insert(p as u32);
    }
    assert_eq!(marks.count(), pages);
    assert_eq!(marks.count_absent_from_both(&free), 0, "six phantom pages in the last word");

    marks = PageSet::with_pages(pages);
    assert_eq!(
        marks.count_absent_from_both(&free),
        pages,
        "and all of them when nothing is marked"
    );
}

#[test]
fn the_highest_unaccounted_page_is_the_one_that_pins_the_tail() {
    let pages = 200;
    let free = PageSet::with_pages(pages);
    let mut marks = PageSet::with_pages(pages);
    // Everything marked but two holes. The higher one is what a truncation walking down from
    // the end of the file meets first, and so the one that matters.
    for p in 0..pages {
        if p != 40 && p != 150 {
            marks.insert(p as u32);
        }
    }
    assert_eq!(marks.count_absent_from_both(&free), 2);
    assert_eq!(marks.absent_from_both(&free).next_back(), Some(150));
    assert_eq!(marks.absent_from_both(&free).next(), Some(40));
}

// -------------------------------------------------------------------------------------------
// The audit
// -------------------------------------------------------------------------------------------

/// A file that has only ever been written to accounts for every page it has.
#[test]
fn a_fresh_store_accounts_for_every_page() {
    let store = Store::init(MemPager::new()).unwrap();
    let r = audit(&store);
    assert!(r.is_clean(), "{r:?}");
    assert_eq!(r.leaked, 0);
    assert_eq!(r.by_class.meta, 2, "both meta slots, always");
    assert_eq!(r.reachable + r.free_total, r.page_count, "{r:?}");
}

#[test]
fn a_store_with_fragments_accounts_for_every_page() {
    let store = Store::init(MemPager::new()).unwrap();
    for i in 0..16 {
        put_fragment(&store, key(1, i));
    }
    let r = audit(&store);
    assert_eq!(r.leaked, 0, "{r:?}");
    assert_eq!(r.by_class.trees, 16);
    assert_eq!(r.reachable + r.free_total, r.page_count, "{r:?}");
}

/// **The positive control.** A page is allocated, written, and then named by nothing: not a
/// root, not a chain, and never freed. Without this test every zero above could be a counter
/// that simply never looks.
#[test]
fn an_orphaned_page_is_reported() {
    let store = Store::init(MemPager::new()).unwrap();
    put_fragment(&store, key(1, 0));

    let stranded = {
        let mut w = store.begin_write();
        let p = w.alloc().unwrap();
        w.write(p, LeafBuilder::new().finish(p)).unwrap();
        // No `set_root`, no `free`. The page exists and nothing will ever refer to it again.
        w.commit().unwrap();
        p
    };

    let r = audit(&store);
    assert_eq!(r.leaked, 1, "{r:?}");
    assert_eq!(r.highest_leaked, Some(stranded));
    assert_eq!(r.leaked_sample, vec![stranded]);
    assert!(!r.is_clean());
    assert_eq!(r.reachable + r.free_total + r.leaked, r.page_count, "{r:?}");
}

/// Every page a snapshot pins is reachable, even though the current roots have forgotten it.
///
/// **This is the class no other walk in the tree visits** - `copy_to` skips it deliberately
/// and `scrub` skips it by omission. Without it the audit would report every snapshot-pinned
/// page as leaked, and nobody would find out until a file had a snapshot in it.
#[test]
fn a_tree_only_a_snapshot_still_names_is_reachable() {
    let store = Store::init(MemPager::new()).unwrap();
    for i in 0..8 {
        put_fragment(&store, key(1, i));
    }

    let mut w = store.begin_write();
    w.create_snapshot(u64::MAX, "before", true);
    w.commit().unwrap();

    // Every fragment goes. Their pages are pending-free and reachable only from the snapshot's
    // old roots chain.
    for i in 0..8 {
        let mut w = store.begin_write();
        let root = w.remove_root(&key(1, i)).expect("still there");
        w.free(root);
        w.commit().unwrap();
    }

    let r = audit(&store);
    assert_eq!(r.leaked, 0, "a snapshot's trees were counted as leaked: {r:?}");
    assert_eq!(r.by_class.snapshot_trees, 8, "{r:?}");
    assert!(r.by_class.snapshot_chains > 0, "the old roots chain itself: {r:?}");
    assert_eq!(r.double_allocated, 0, "pinned pages are pending-free, never reusable: {r:?}");
}

/// An audit reads. It must leave the file at exactly the transaction it found it at.
#[test]
fn an_audit_never_changes_the_file() {
    let store = Store::init(MemPager::new()).unwrap();
    for i in 0..8 {
        put_fragment(&store, key(1, i));
    }
    let before = store.metrics();
    let r = audit(&store);
    let after = store.metrics();

    assert_eq!(before.txn_id, after.txn_id);
    assert_eq!(before.page_count, after.page_count);
    assert_eq!(before.free_pages_reusable, after.free_pages_reusable);
    assert_eq!(r.txn_id, before.txn_id);
}

/// The reader an audit holds is released when it finishes, or the next truncation would refuse
/// for ever.
#[test]
fn an_audit_releases_its_reader() {
    let store = Store::init(MemPager::new()).unwrap();
    put_fragment(&store, key(1, 0));
    let _ = audit(&store);
    assert_eq!(store.metrics().live_readers, 0);
    assert!(store.truncate_tail().is_ok(), "the audit's reader outlived the audit");
}
