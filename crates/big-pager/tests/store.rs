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

use big_page::{FragmentKey, LeafBuilder, Page};
use big_pager::*;

fn key(field: u32, shard: u64) -> FragmentKey {
    FragmentKey::new(1, field, 0, shard)
}

/// Allocate one page, write a leaf into it and point a fragment root at it.
fn put_fragment<P: PagerMut>(store: &Store<P>, k: FragmentKey) -> u32 {
    let mut w = store.begin_write();
    let p = w.alloc().unwrap();
    w.write(p, LeafBuilder::new().finish(p)).unwrap();
    w.set_root(k, p);
    w.commit().unwrap();
    p
}

#[test]
fn init_then_load_sees_the_same_state() {
    let store = Store::init(MemPager::new()).unwrap();
    assert_eq!(store.meta().txn_id, 0);
    put_fragment(&store, key(7, 0));
    assert_eq!(store.meta().txn_id, 1);
    assert_eq!(store.roots().len(), 1);

    let reloaded = Store::load(store.pager().clone()).unwrap();
    assert_eq!(reloaded.meta().txn_id, 1);
    assert_eq!(reloaded.roots().len(), 1);
    assert!(reloaded.roots().get(&key(7, 0)).is_some());
}

#[test]
fn meta_slots_alternate() {
    let store = Store::init(MemPager::new()).unwrap();
    for expected in [1u64, 0, 1, 0] {
        store.begin_write().commit().unwrap();
        assert_eq!(store.meta().slot(), expected);
    }
}

#[test]
fn root_records_survive_many_fragments() {
    let store = Store::init(MemPager::new()).unwrap();
    let mut w = store.begin_write();
    for shard in 0..500u64 {
        let p = w.alloc().unwrap();
        w.write(p, LeafBuilder::new().finish(p)).unwrap();
        w.set_root(key(1, shard), p);
    }
    w.commit().unwrap();

    let reloaded = Store::load(store.pager().clone()).unwrap();
    assert_eq!(reloaded.roots().len(), 500, "root records must span several chained pages");
    for shard in 0..500u64 {
        assert!(reloaded.roots().get(&key(1, shard)).is_some());
    }
}

/// A modified page gets a NEW pgno; the old one stays intact.
#[test]
fn cow_leaves_the_old_page_untouched() {
    let store = Store::init(MemPager::new()).unwrap();
    let old = put_fragment(&store, key(1, 0));

    let mut w = store.begin_write();
    let new = w.cow(old).unwrap();
    assert_ne!(new, old);
    w.write(new, LeafBuilder::new().finish(new)).unwrap();
    w.set_root(key(1, 0), new);
    w.commit().unwrap();

    assert_eq!(store.pager().read(old).unwrap().pgno(), old, "old page still readable");
    assert_eq!(store.roots().get(&key(1, 0)), Some(new));
}

/// Dropping without commit writes nothing: meta still points at the old tree.
#[test]
fn rollback_is_a_no_op() {
    let store = Store::init(MemPager::new()).unwrap();
    put_fragment(&store, key(1, 0));
    let before = store.meta();

    let mut w = store.begin_write();
    let p = w.alloc().unwrap();
    w.write(p, LeafBuilder::new().finish(p)).unwrap();
    w.set_root(key(2, 0), p);
    drop(w);

    assert_eq!(store.meta(), before);
    assert!(store.roots().get(&key(2, 0)).is_none());
}

#[test]
fn freed_pages_come_back_once_no_reader_can_see_them() {
    let store = Store::init(MemPager::new()).unwrap();
    let a = put_fragment(&store, key(1, 0));

    let mut w = store.begin_write();
    let b = w.cow(a).unwrap();
    w.set_root(key(1, 0), b);
    w.commit().unwrap();

    assert!(store.metrics().free_pages_reusable > 0, "page a must be reclaimable now");

    let mut w = store.begin_write();
    let reused = w.alloc().unwrap();
    assert_eq!(reused, a, "allocator must prefer the freelist over growing the file");
}

/// A long-running reader holds an old txn_id, so the freelist cannot be reclaimed.
#[test]
fn a_live_reader_blocks_reclaim_and_is_reported_as_such() {
    let store = Store::init(MemPager::new()).unwrap();
    let a = put_fragment(&store, key(1, 0));

    let reader = store.begin_read();
    let mut w = store.begin_write();
    let b = w.cow(a).unwrap();
    w.set_root(key(1, 0), b);
    w.commit().unwrap();

    let m = store.metrics();
    assert_eq!(m.oldest_reader_txn_id, Some(reader.txn_id()));
    assert!(m.pages_pending_reclaim_reader > 0, "must be blamed on the reader");
    assert_eq!(m.pages_pending_reclaim_retention, 0, "no snapshot is involved");

    drop(reader);
    assert_eq!(store.metrics().oldest_reader_txn_id, None);
}

/// Time travel blocks reclaim too, but through a different knob, so it is reported separately.
#[test]
fn a_snapshot_blocks_reclaim_under_the_retention_reason() {
    let store = Store::init(MemPager::new()).unwrap();
    let a = put_fragment(&store, key(1, 0));

    let mut w = store.begin_write();
    w.create_snapshot(u64::MAX, "before-risky-job", true);
    w.commit().unwrap();

    let mut w = store.begin_write();
    let b = w.cow(a).unwrap();
    w.set_root(key(1, 0), b);
    w.commit().unwrap();

    let m = store.metrics();
    assert_eq!(m.oldest_reader_txn_id, None, "no live reader");
    assert!(m.pages_pending_reclaim_retention > 0, "must be blamed on retention");
    assert_eq!(m.pages_pending_reclaim_reader, 0);
}

#[test]
fn begin_read_at_sees_the_registered_snapshot_not_the_present() {
    let store = Store::init(MemPager::new()).unwrap();
    put_fragment(&store, key(1, 0));

    let mut w = store.begin_write();
    let snap = w.create_snapshot(u64::MAX, "pinned", true);
    w.commit().unwrap();

    put_fragment(&store, key(2, 0));
    assert_eq!(store.roots().len(), 2, "present has both fragments");

    let past = store.begin_read_at(snap.id).unwrap();
    assert_eq!(past.roots().len(), 1, "snapshot only has the first one");
    assert!(past.root(&key(2, 0)).is_none());

    assert!(matches!(store.begin_read_at(999), Err(StoreError::SnapshotNotFound(999))));
}

#[test]
fn expired_snapshots_stop_holding_the_freelist() {
    let store = Store::init(MemPager::new()).unwrap();
    let a = put_fragment(&store, key(1, 0));

    let mut w = store.begin_write();
    w.create_snapshot(100, "short-lived", false);
    w.commit().unwrap();

    let mut w = store.begin_write();
    let b = w.cow(a).unwrap();
    w.set_root(key(1, 0), b);
    w.commit().unwrap();
    assert!(store.metrics().pages_pending_reclaim_retention > 0);

    let mut w = store.begin_write();
    assert_eq!(w.expire_snapshots(101).len(), 1);
    w.commit().unwrap();
    assert_eq!(store.metrics().pages_pending_reclaim_retention, 0);
}

/// If the new meta is broken the old one still wins, and there is nothing to replay.
#[test]
fn corrupt_newer_meta_falls_back_to_older() {
    let pager = MemPager::new();
    let store = Store::init(pager).unwrap();
    store.begin_write().commit().unwrap();
    store.begin_write().commit().unwrap();
    assert_eq!(store.meta().txn_id, 2);
    assert_eq!(store.meta().slot(), 0);

    let pager = store.pager().clone();
    let mut bad = pager.read(0).unwrap().clone();
    bad.as_bytes_mut()[20] ^= 0xFF;
    pager.write(0, &bad).unwrap();

    assert_eq!(Store::load(pager).unwrap().meta().txn_id, 1, "must fall back to the intact meta");
}

#[test]
fn both_meta_corrupt_is_an_error_not_a_panic() {
    let store = Store::init(MemPager::new()).unwrap();
    store.begin_write().commit().unwrap();
    let pager = store.pager().clone();
    for p in [0u32, 1] {
        let mut bad = pager.read(p).unwrap().clone();
        bad.as_bytes_mut()[20] ^= 0xFF;
        pager.write(p, &bad).unwrap();
    }
    assert!(matches!(Store::load(pager), Err(StoreError::NoValidMeta)));
}

#[test]
fn writing_a_page_the_txn_never_allocated_is_rejected() {
    let store = Store::init(MemPager::new()).unwrap();
    let mut w = store.begin_write();
    assert!(matches!(w.write(9999, Page::zeroed()), Err(StoreError::UnallocatedPage(9999))));
}

#[test]
fn reading_past_eof_is_an_error() {
    let pager = MemPager::with_pages(2);
    assert!(matches!(pager.read(2), Err(StoreError::OutOfBounds { pgno: 2, .. })));
}

/// Store is generic over PagerMut, so the same routine runs on every backend.
fn exercise<P: PagerMut>(pager: P) -> usize {
    let store = Store::init(pager).unwrap();
    put_fragment(&store, key(1, 0));
    put_fragment(&store, key(2, 0));
    store.roots().len()
}

#[test]
fn backend_is_swappable() {
    assert_eq!(exercise(MemPager::new()), 2);

    #[cfg(unix)]
    {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(exercise(MmapPager::open(dir.path().join("t.big"), 1 << 20).unwrap()), 2);
    }
}

/// Freelist pages always come from the file tail, so a quiet loop of commits must still
/// reach a steady state rather than growing the file forever.
#[test]
fn repeated_commits_do_not_grow_the_file_without_bound() {
    let store = Store::init(MemPager::new()).unwrap();
    put_fragment(&store, key(1, 0));
    for _ in 0..50 {
        store.begin_write().commit().unwrap();
    }
    let settled = store.pager().page_count();
    for _ in 0..200 {
        store.begin_write().commit().unwrap();
    }
    assert_eq!(store.pager().page_count(), settled, "page count must plateau");
}

/// The freelist has to reach the disk before the file shrinks.
///
/// It did not, and nothing noticed for as long as the store stayed open: `truncate_tail` trimmed
/// its in-memory freelist and wrote only the meta page, so the chain on disk still listed the
/// runs that had just been trimmed. The next commit rewrote the chain and the discrepancy
/// vanished. Reopen the file *before* that commit and the freelist came back holding pages past
/// the end of the file, which `alloc` then handed out - and the next write went to a page that
/// did not exist.
///
/// Found by a benchmark that reopened a store to take a cold read, which is the only reason the
/// window was ever entered.
#[test]
fn a_truncated_store_reopens_without_free_pages_past_the_end() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.big");

    let released = {
        let store = Store::init(MmapPager::open(&path, 1 << 22).unwrap()).unwrap();
        let mut pages = Vec::new();
        let mut w = store.begin_write();
        for i in 0..60u32 {
            let p = w.alloc().unwrap();
            w.write(p, LeafBuilder::new().finish(p)).unwrap();
            w.set_root(key(i, 0), p);
            pages.push(p);
        }
        w.commit().unwrap();

        let mut w = store.begin_write();
        for (i, p) in pages.iter().enumerate() {
            w.remove_root(&key(i as u32, 0));
            w.free(*p);
        }
        w.commit().unwrap();
        store.begin_write().commit().unwrap();

        let released = store.truncate_tail().unwrap();
        assert!(released > 0, "the test has to reach the code it is about");
        released
    };

    // Reopened with no commit in between, which is the window the bug lived in.
    let store = Store::load(MmapPager::open(&path, 1 << 22).unwrap()).unwrap();
    let page_count = store.pager().page_count();

    // Every page this store hands out has to be inside the file it just reopened.
    let mut w = store.begin_write();
    for _ in 0..(released as usize + 8) {
        let p = w.alloc().unwrap();
        assert!(
            (p as u64) < w.page_count(),
            "alloc handed out page {p} against a file of {page_count} pages"
        );
        w.write(p, LeafBuilder::new().finish(p))
            .unwrap_or_else(|e| panic!("writing freshly allocated page {p} failed: {e}"));
    }
    w.commit().unwrap();
}

#[test]
fn truncate_tail_gives_pages_back_to_the_filesystem() {
    let store = Store::init(MemPager::new()).unwrap();
    let mut w = store.begin_write();
    let mut pages = Vec::new();
    for _ in 0..200 {
        let p = w.alloc().unwrap();
        w.write(p, LeafBuilder::new().finish(p)).unwrap();
        pages.push(p);
    }
    for (i, p) in pages.iter().enumerate() {
        w.set_root(key(i as u32, 0), *p);
    }
    w.commit().unwrap();
    let grown = store.pager().page_count();

    let mut w = store.begin_write();
    for (i, p) in pages.iter().enumerate() {
        w.remove_root(&key(i as u32, 0));
        w.free(*p);
    }
    w.commit().unwrap();
    store.begin_write().commit().unwrap();

    let before = store.pager().page_count();
    let released = store.truncate_tail().unwrap();
    assert!(released > 0, "trailing free pages must come back");
    assert_eq!(store.pager().page_count(), before - released);
    assert!(store.pager().page_count() < grown / 2, "the file must actually shrink");

    let reloaded = Store::load(store.pager().clone()).unwrap();
    assert!(reloaded.roots().is_empty());
}

#[test]
fn truncate_tail_is_a_no_op_when_the_tail_is_in_use() {
    let store = Store::init(MemPager::new()).unwrap();
    put_fragment(&store, key(1, 0));
    assert_eq!(store.truncate_tail().unwrap(), 0);
}

/// Rewrites both meta slots to declare `version`, keeping the checksum valid.
///
/// Sealing matters: `MetaPage::decode` verifies the checksum before it looks at the version,
/// so a file patched without resealing would test the checksum path instead.
fn stamp_version(pager: &MemPager, version: u32) {
    for slot in 0..2 {
        let mut page = (*pager.read(slot).unwrap()).clone();
        page.as_bytes_mut()[4..8].copy_from_slice(&version.to_le_bytes());
        page.seal();
        pager.write(slot, &page).unwrap();
    }
}

#[test]
fn a_file_from_another_format_version_says_so() {
    // Both meta pages fail to decode here, which used to collapse into `NoValidMeta` - the
    // same answer a genuinely damaged file gives. An operator has to be able to tell "restore
    // from backup" apart from "this file needs the migration tool", and only the engine knows
    // which it is.
    let pager = MemPager::new();
    let store = Store::init(pager).unwrap();
    store.begin_write().commit().unwrap();

    let pager = store.into_pager();
    stamp_version(&pager, big_page::meta::VERSION + 1);

    match Store::load(pager) {
        Err(StoreError::Page(big_page::PageError::UnsupportedVersion(found))) => {
            assert_eq!(found, big_page::meta::VERSION + 1);
        }
        Err(other) => panic!("expected a version mismatch, got {other:?}"),
        Ok(_) => panic!("a file from another format version must not open"),
    }
}

#[test]
fn the_version_error_names_both_versions() {
    let err = big_page::PageError::UnsupportedVersion(99).to_string();
    assert!(err.contains("99"), "the file's version must be in the message: {err}");
    assert!(
        err.contains(&big_page::meta::VERSION.to_string()),
        "so must the one this build writes: {err}"
    );
}

#[test]
fn one_damaged_meta_slot_is_still_recoverable() {
    // The reason there are two. Losing one must stay a non-event, and must not be reported as
    // a version problem just because the reporting got more specific.
    let pager = MemPager::new();
    let store = Store::init(pager).unwrap();
    store.begin_write().commit().unwrap();
    let good = store.meta().txn_id;

    let pager = store.into_pager();
    let mut wrecked = Page::zeroed();
    wrecked.as_bytes_mut()[0..4].copy_from_slice(&0xDEAD_BEEFu32.to_le_bytes());
    wrecked.seal();
    // Slot `txn_id % 2` holds the winner, so wreck the other one.
    pager.write(((good + 1) % 2) as u32, &wrecked).unwrap();

    let store = Store::load(pager).unwrap();
    assert_eq!(store.meta().txn_id, good);
}
