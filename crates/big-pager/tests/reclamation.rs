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

//! A live reader holds the reclaim horizon down.
//!
//! This is the invariant the whole no-WAL design rests on, and it had no test. Copy-on-write
//! means a writer frees the page a reader is looking at as a matter of routine; what keeps that
//! from being a use-after-free is that a freed page is only handed out again once no live reader
//! could still reach it.
//!
//! **Every test here asserts on bytes, not on page numbers or counters.** The first draft
//! asserted that the freed page number was not handed back by `alloc`, and that the pending
//! counters moved - and when the horizon was deliberately broken to ignore live readers, six of
//! seven tests still passed. They passed for reasons that had nothing to do with the invariant:
//! `metrics` computes its own horizon and so stays right while the commit path is wrong, and a
//! commit consumes freed pages for its own root and catalog chains before the caller's `alloc`
//! ever sees the freelist, so the doomed page was taken by the machinery rather than withheld.
//!
//! What actually catches the bug is reading the page back through the old reader and comparing
//! it to what was there before. That is also the only thing a caller would notice, which is
//! probably not a coincidence.

use big_page::{FragmentKey, LeafBuilder, LeafPage};
use big_pager::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier};

fn key(field: u32, shard: u64) -> FragmentKey {
    FragmentKey::new(1, field, 0, shard)
}

/// One page holding a leaf, pointed at by a fragment root. Returns the page number.
fn put_fragment<P: PagerMut>(store: &Store<P>, k: FragmentKey) -> u32 {
    let mut w = store.begin_write();
    let p = w.alloc().unwrap();
    w.write(p, LeafBuilder::new().finish(p)).unwrap();
    w.set_root(k, p);
    w.commit().unwrap();
    p
}

/// Drops the fragment, which frees every page it held.
fn drop_fragment<P: PagerMut>(store: &Store<P>, k: FragmentKey) {
    let mut w = store.begin_write();
    let root = w.remove_root(&k).expect("fragment should exist");
    w.free(root);
    w.commit().unwrap();
}

/// Enough allocation and freeing to drive the freelist hard. Any page a reader still needs that
/// has wrongly become reusable will have been handed out and overwritten by the end of this.
fn churn<P: PagerMut>(store: &Store<P>, rounds: usize) {
    for round in 0..rounds {
        let k = key(100 + (round % 4) as u32, 0);
        put_fragment(store, k);
        let mut w = store.begin_write();
        let extra: Vec<u32> = (0..4).map(|_| w.alloc().unwrap()).collect();
        for &p in &extra {
            // A distinguishable payload: a leaf written over a page a reader still holds is
            // still a valid leaf, so "it parses" would not catch the overwrite. The bytes have
            // to differ from what the reader put there.
            w.write(p, LeafBuilder::new().finish(p)).unwrap();
        }
        w.commit().unwrap();
        drop_fragment(store, k);
    }
}

fn page_bytes<P: Pager>(txn: &ReadTxn<'_, P>, pgno: u32) -> Vec<u8> {
    txn.read(pgno).unwrap().as_bytes().to_vec()
}

#[test]
fn a_live_reader_keeps_every_page_it_could_reach() {
    let store = Store::init(MemPager::new()).unwrap();
    let roots: Vec<u32> = (0..8).map(|f| put_fragment(&store, key(f, 0))).collect();

    // The reader starts before the frees, so every one of those pages is reachable from the
    // state it holds.
    let reader = store.begin_read();
    let before: Vec<Vec<u8>> = roots.iter().map(|&p| page_bytes(&reader, p)).collect();

    for f in 0..8 {
        drop_fragment(&store, key(f, 0));
    }
    churn(&store, 24);

    for (i, &p) in roots.iter().enumerate() {
        assert_eq!(
            page_bytes(&reader, p),
            before[i],
            "page {p} was freed under a live reader and reused underneath it"
        );
    }
}

#[test]
fn the_reader_still_sees_the_old_bytes() {
    let store = Store::init(MemPager::new()).unwrap();
    let doomed = put_fragment(&store, key(7, 0));

    let reader = store.begin_read();
    let before = page_bytes(&reader, doomed);

    drop_fragment(&store, key(7, 0));
    churn(&store, 16);

    assert_eq!(
        page_bytes(&reader, doomed),
        before,
        "a live reader's page was rewritten underneath it"
    );
}

#[test]
fn reclaim_resumes_when_the_reader_goes() {
    let store = Store::init(MemPager::new()).unwrap();
    put_fragment(&store, key(7, 0));

    let reader = store.begin_read();
    drop_fragment(&store, key(7, 0));

    let blocked = store.metrics().pages_pending_reclaim_reader;
    assert!(blocked > 0, "precondition: the reader is holding something back");

    drop(reader);
    // The horizon is recomputed at the next commit, so it takes one to move.
    churn(&store, 1);

    let m = store.metrics();
    assert_eq!(m.live_readers, 0);
    assert_eq!(
        m.pages_pending_reclaim_reader, 0,
        "nothing is reading any more, so nothing should still be pending on a reader"
    );
}

#[test]
fn a_snapshot_keeps_its_pages_the_same_way() {
    let store = Store::init(MemPager::new()).unwrap();
    let doomed = put_fragment(&store, key(7, 0));

    let mut w = store.begin_write();
    let snap = w.create_snapshot(u64::MAX, "held", true);
    w.commit().unwrap();

    let before = page_bytes(&store.begin_read_at(snap.id).unwrap(), doomed);

    drop_fragment(&store, key(7, 0));
    churn(&store, 16);

    // Read through the snapshot, with no live reader anywhere: the only thing keeping this page
    // alive is the snapshot's own txn id in the horizon.
    let held = store.begin_read_at(snap.id).unwrap();
    assert_eq!(
        page_bytes(&held, doomed),
        before,
        "a snapshot's page was reused while the snapshot still existed"
    );
    drop(held);

    // The two knobs are separate on purpose: what a snapshot blocks is known exactly, and what
    // is left over is the readers' doing. A test that summed them would not notice them merging.
    let m = store.metrics();
    assert!(m.pages_pending_reclaim_retention > 0, "the snapshot should be holding pages back");
    assert_eq!(
        m.pages_pending_reclaim_reader, 0,
        "no reader is alive, so nothing should be attributed to one"
    );

    let mut w = store.begin_write();
    w.drop_snapshot(snap.id);
    w.commit().unwrap();
    churn(&store, 1);
    assert_eq!(store.metrics().pages_pending_reclaim_retention, 0);
}

#[test]
fn truncate_tail_refuses_while_a_reader_is_alive() {
    let store = Store::init(MemPager::new()).unwrap();
    put_fragment(&store, key(7, 0));
    drop_fragment(&store, key(7, 0));

    let reader = store.begin_read();
    // Not "returns zero" - refuses. Shrinking the file turns the region past the new EOF back
    // into unbacked mapping, and a live borrow into it would be a SIGBUS rather than an error,
    // so this has to fail loudly instead of quietly doing nothing.
    assert!(
        matches!(store.truncate_tail(), Err(StoreError::ReadersActive)),
        "truncate_tail must refuse, not silently no-op, while a reader holds a borrow"
    );

    drop(reader);
    assert!(store.truncate_tail().is_ok());
}

/// **The lowest reusable page is the one handed out**, which is what lets a file that has
/// churned ever get smaller.
///
/// Ordering the freelist by generation first was correct and hopeless for the tail: writes went
/// on landing at the end of the file while holes near the front stayed holes, so nothing was
/// ever flush against EOF for `truncate_tail` to release. The reclaim rule is untouched - a run
/// newer than the horizon is still skipped - and only the choice among reusable pages changed.
/// Against the freelist itself rather than through a commit, and for the reason this file's
/// header gives about every other assertion here: a commit takes pages for its own root,
/// catalog and freelist chains before the caller's `alloc` ever sees the list, so a page number
/// observed through one says as much about the machinery as about the rule. The rule is a
/// property of this data structure, and this is where it is exact.
#[test]
fn the_lowest_reusable_page_is_allocated_first() {
    let mut f = Freelist::default();
    // Freed newest-page-first, so the *oldest* generation holds the *highest* page - which is
    // what the old ordering, by generation, would have handed out first.
    f.push(30, 1);
    f.push(20, 2);
    f.push(10, 3);
    f.compact();

    assert_eq!(f.alloc(9), Some(10), "the lowest hole, not the oldest one");
    assert_eq!(f.alloc(9), Some(20));
    assert_eq!(f.alloc(9), Some(30));
    assert_eq!(f.alloc(9), None);
}

/// And the horizon still decides *whether* a page may be reused, not just which one. A run
/// newer than the horizon is skipped however low it sits.
#[test]
fn a_page_too_new_to_reuse_is_skipped_however_low_it_is() {
    let mut f = Freelist::default();
    f.push(10, 7); // low, but freed by a transaction a reader can still see
    f.push(30, 1);
    f.compact();

    assert_eq!(f.alloc(3), Some(30), "the low page is not reusable yet");
    assert_eq!(f.alloc(3), None);
    assert_eq!(f.alloc(7), Some(10), "and is once the horizon passes it");
}

/// And the point of that ordering: a file that churns comes back down instead of only ever
/// growing. Written and dropped repeatedly, then trimmed - the tail is free by then, because
/// every write went into a hole near the front rather than onto the end.
#[test]
fn a_file_that_churns_can_be_given_back_to_the_filesystem() {
    let store = Store::init(MemPager::new()).unwrap();
    for i in 0..16 {
        put_fragment(&store, key(i, 0));
    }
    let high_water = store.metrics().page_count;

    for i in 0..16 {
        drop_fragment(&store, key(i, 0));
    }
    // A few more commits, which is what a live database is doing while this happens: each one
    // takes its pages from the freelist, lowest first, and leaves the tail alone.
    for i in 0..4 {
        put_fragment(&store, key(100 + i, 0));
    }

    let released = store.truncate_tail().unwrap();
    assert!(released > 0, "the tail was free and none of it was given back");
    assert!(
        store.metrics().page_count < high_water,
        "the file did not shrink: {} vs {high_water}",
        store.metrics().page_count
    );
}

// -------------------------------------------------------------------------------------------
// Pages a transaction allocated and then gave up on
//
// A page freed above the tail floor goes to the transaction's scratch list rather than the
// freelist, so `alloc` can hand it straight back. What is left there at commit used to be lost:
// counted in `page_count`, present in the file, and referenced by nothing. Against the freelist
// directly, for the reason at the top of this file.
// -------------------------------------------------------------------------------------------

/// A page flush against the end is given back by not growing the file that far, which is
/// better than recording it as free: there is nothing to record and nothing to reuse.
#[test]
fn scratch_against_the_tail_is_given_back_rather_than_freed() {
    let mut f = Freelist::default();
    assert_eq!(f.absorb_scratch(vec![10, 11], 12, 5), 10);
    assert!(f.is_empty(), "the file simply never grew that far");
}

/// A page with something live above it cannot be given back that way, so it is recorded free.
#[test]
fn scratch_below_the_tail_goes_to_the_freelist() {
    let mut f = Freelist::default();
    assert_eq!(f.absorb_scratch(vec![10], 12, 5), 12, "page 11 is live, so 10 cannot be dropped");
    assert_eq!(f.runs(), [FreeRun { freed_at: 5, first: 10, len: 1 }]);
}

/// The giveback stops at the first gap - everything below the gap is recorded instead.
#[test]
fn the_giveback_stops_at_the_first_gap() {
    let mut f = Freelist::default();
    assert_eq!(f.absorb_scratch(vec![100, 102], 103, 7), 102);
    assert_eq!(f.runs(), [FreeRun { freed_at: 7, first: 100, len: 1 }]);
}

/// **Stamped with the committing transaction, so it is pending for the whole of that commit.**
/// A lower stamp would make the page allocatable while the freelist's own pages are still being
/// chosen, which is the loop whose termination argument assumes the entry count only falls.
#[test]
fn an_absorbed_page_is_not_reusable_inside_its_own_commit() {
    let mut f = Freelist::default();
    f.absorb_scratch(vec![10], 12, 5);
    assert_eq!(f.alloc(4), None, "the horizon is below this transaction");
    assert_eq!(f.alloc(5), Some(10), "and it is reusable once the horizon reaches it");
}

#[test]
fn absorbing_nothing_changes_nothing() {
    let mut f = Freelist::default();
    assert_eq!(f.absorb_scratch(Vec::new(), 12, 5), 12);
    assert!(f.is_empty());
}

#[test]
fn readers_are_refcounted_not_flagged() {
    let store = Store::init(MemPager::new()).unwrap();
    put_fragment(&store, key(7, 0));

    // Two readers at the same txn id collapse to one key in the registry. If the count were a
    // flag, dropping either would declare the other gone.
    let a = store.begin_read();
    let b = store.begin_read();
    assert_eq!(store.metrics().live_readers, 2);

    drop(a);
    assert_eq!(store.metrics().live_readers, 1);
    assert!(
        matches!(store.truncate_tail(), Err(StoreError::ReadersActive)),
        "the second reader at the same txn id was forgotten when the first was dropped"
    );

    drop(b);
    assert_eq!(store.metrics().live_readers, 0);
}

#[test]
fn a_reader_held_across_concurrent_writers_never_sees_a_recycled_page() {
    let store = Arc::new(Store::init(MemPager::new()).unwrap());
    let roots: Vec<u32> = (0..16).map(|f| put_fragment(&store, key(f, 0))).collect();

    // The deterministic tests above pick one interleaving: the one the author thought of. Here
    // each reader holds its transaction open across whatever the scheduler does, and keeps
    // re-reading the same pages. Opening a fresh transaction per loop would defeat the point -
    // it would always see current state, and so would pass even with the horizon disabled.
    let stop = Arc::new(AtomicBool::new(false));
    let start = Arc::new(Barrier::new(5));

    std::thread::scope(|scope| {
        for _ in 0..4 {
            let store = Arc::clone(&store);
            let stop = Arc::clone(&stop);
            let start = Arc::clone(&start);
            let roots = roots.clone();
            scope.spawn(move || {
                let reader = store.begin_read();
                let before: Vec<Vec<u8>> = roots.iter().map(|&p| page_bytes(&reader, p)).collect();
                start.wait();
                while !stop.load(Ordering::Relaxed) {
                    for (i, &p) in roots.iter().enumerate() {
                        assert_eq!(
                            page_bytes(&reader, p),
                            before[i],
                            "a live reader was handed a page that had been recycled"
                        );
                        assert!(LeafPage::parse(&reader.read(p).unwrap()).is_ok());
                    }
                }
            });
        }

        start.wait();
        for f in 0..16 {
            drop_fragment(&store, key(f, 0));
        }
        churn(&store, 40);
        stop.store(true, Ordering::Relaxed);
    });
}
