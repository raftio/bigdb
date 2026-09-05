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

//! Every page of a real database accounted for.
//!
//! `crates/big-pager/tests/audit.rs` proves the counting; this proves the *walk* - that the
//! mark phase reaches every class of page a `Db` actually produces. A class it misses is a
//! class it reports as leaked, so each test below is one page class that would otherwise be
//! silently wrong.

use big_db::*;
use big_pager::MemPager;

fn audit(d: &Db<MemPager>) -> big_pager::LeakReport {
    d.audit_pages().expect("the file is readable")
}

/// Fragments spread one per shard: what fills a file is fragments, not records.
fn seeded(shards: u64) -> Db<MemPager> {
    let d = Db::in_memory().unwrap();
    d.create_table("tx").unwrap();
    d.create_field("tx", "amount", FieldKind::Int, 32).unwrap();
    d.create_field("tx", "country", FieldKind::Set, 0).unwrap();
    let mut w = d.write();
    for i in 0..shards {
        let record = i << 20;
        w.set_int("tx", "amount", record, i * 7).unwrap();
        w.set_key("tx", "country", record, if i.is_multiple_of(3) { "vn" } else { "jp" }).unwrap();
    }
    w.commit().unwrap();
    d
}

/// **The number that must stay zero.** Everything below is one way of making it non-zero.
#[test]
fn a_fresh_database_accounts_for_every_page() {
    let d = Db::in_memory().unwrap();
    let r = audit(&d);
    assert!(r.is_clean(), "{r:?}");
    assert_eq!(r.reachable + r.free_total, r.page_count, "{r:?}");
}

#[test]
fn a_database_with_data_accounts_for_every_page() {
    let d = seeded(200);
    let r = audit(&d);
    assert_eq!(r.leaked, 0, "{r:?}");
    assert_eq!(r.dangling, 0, "{r:?}");
    assert_eq!(r.reachable + r.free_total, r.page_count, "{r:?}");
}

/// **A columnar table's spill pages.** A leaf cell of kind `ValuesPtr` owns a page the way a
/// dense bitmap cell does; a walk that only followed bitmaps would report every one of them as
/// leaked, which is the loudest possible failure and therefore the best test.
#[test]
fn a_columnar_table_leaks_no_spill_pages() {
    let d = Db::in_memory().unwrap();
    d.create_table_with("tx", TableEngine::Columnar).unwrap();
    d.create_field("tx", "amount", FieldKind::Int, 32).unwrap();
    let mut w = d.write();
    // Enough distinct values in one shard that the column spills to a page of its own.
    for r in 0..5_000u64 {
        w.set_int("tx", "amount", r, r * 31).unwrap();
    }
    w.commit().unwrap();

    let r = audit(&d);
    assert_eq!(r.leaked, 0, "a columnar spill page was counted as leaked: {r:?}");
    assert!(r.by_class.trees > 0, "{r:?}");
}

/// **A delta cell's base page.** Writing a fragment twice leaves a cell that names the page
/// holding what it is a delta against. `visit_tree` follows it; a hand-rolled walk forgets it.
#[test]
fn a_fragment_written_twice_leaks_no_delta_base() {
    let d = seeded(64);
    let mut w = d.write();
    for i in 0..64u64 {
        w.set_int("tx", "amount", (i << 20) + 1, i * 13).unwrap();
    }
    w.commit().unwrap();

    let r = audit(&d);
    assert_eq!(r.leaked, 0, "{r:?}");
}

/// A database that has been mostly emptied still accounts for every page: what the delete
/// freed is in the freelist, and what is left is reachable.
#[test]
fn a_database_that_has_churned_leaks_nothing() {
    let d = seeded(900);
    let doomed: Vec<u64> = (0..900u64).filter(|i| !i.is_multiple_of(50)).map(|i| i << 20).collect();
    let mut w = d.write();
    w.delete("tx", &doomed).unwrap();
    w.commit().unwrap();
    let mut w = d.write();
    w.set_int("tx", "amount", 1, 1).unwrap();
    w.commit().unwrap();

    let r = audit(&d);
    assert_eq!(
        r.leaked, 0,
        "pages nothing points at and nothing recorded as free; highest is {:?}: {r:?}",
        r.highest_leaked
    );
}

/// **What `truncate_tail` stops at is always a page somebody owns.**
///
/// It walks down from the end of the file and halts at the first page that is not a reusable
/// free run, so whatever sits on top pins every free page beneath it - on this fixture, thousands
/// of them. Before `Freelist::absorb_scratch` the page on top was routinely one that *nobody*
/// owned, and there was no way to give it back short of rewriting the whole file.
///
/// This asserts the part that is now structural: reclaim may still stall, but never on a page the
/// file has lost track of. It deliberately does not assert how much comes back, because that is
/// not yet a property of the engine - see below.
///
/// **What still pins the tail, measured on this fixture and in this order.** Neither is a leak;
/// the audit is right to report zero for both.
///
/// 1. *The freelist chain of a large delete.* A transaction cannot reuse the pages it is itself
///    freeing - they carry its own id, which is above the horizon until it has committed - so the
///    commit that frees thousands of pages must put its own freelist chain at the top of the file.
///    An unchanged chain is then kept where it lies, so empty commits never move it; one real
///    write does, and the tail drops by the size of the chain.
/// 2. *A single live b-tree page.* Whatever is highest after that pins everything below it for
///    good. Moving it means rewriting whoever points at it, which is knowledge `big-btree` has and
///    `big-pager` does not - so `truncate_tail` cannot, by construction, and this is the work that
///    would make reclaim give back a churned file.
#[test]
fn reclaim_never_stalls_on_a_page_nobody_owns() {
    let d = seeded(900);
    let doomed: Vec<u64> = (0..900u64).filter(|i| !i.is_multiple_of(50)).map(|i| i << 20).collect();
    let mut w = d.write();
    w.delete("tx", &doomed).unwrap();
    w.commit().unwrap();
    let mut w = d.write();
    w.set_int("tx", "amount", 1, 1).unwrap();
    w.commit().unwrap();

    d.store().truncate_tail().expect("no readers are alive here");

    let r = audit(&d);
    assert_eq!(r.leaked, 0, "reclaim stopped at a page nothing owns: {r:?}");
    assert_eq!(r.reachable + r.free_total, r.page_count, "{r:?}");
}

/// **The check nothing else in the tree makes.** A page both reachable and reusable-free has
/// been handed out twice. Reachable-and-*pending*-free is ordinary - that is what a snapshot
/// pinning an old tree looks like - so only the reusable half is evidence.
#[test]
fn no_page_is_both_live_and_free_to_reuse() {
    let d = seeded(300);
    let mut w = d.write();
    w.delete("tx", &(0..150u64).map(|i| i << 20).collect::<Vec<_>>()).unwrap();
    w.commit().unwrap();

    let r = audit(&d);
    assert_eq!(r.double_allocated, 0, "a page was handed out twice: {r:?}");
    assert!(r.double_allocated_sample.is_empty(), "{r:?}");
}

/// The audit is exact as of the transaction it opened at, and stays right while writers commit
/// underneath it. This is the test that catches a scaffold captured across two different
/// states, and it is the only one that would.
#[test]
fn the_audit_is_stable_while_a_writer_commits() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    let d = Arc::new(seeded(120));
    let stop = Arc::new(AtomicBool::new(false));

    let writer = {
        let d = Arc::clone(&d);
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            let mut n = 0u64;
            while !stop.load(Ordering::Relaxed) {
                let mut w = d.write();
                w.set_int("tx", "amount", (n % 120) << 20, n).unwrap();
                w.commit().unwrap();
                n += 1;
            }
            n
        })
    };

    for _ in 0..12 {
        let r = audit(&d);
        assert_eq!(r.leaked, 0, "{r:?}");
        assert_eq!(r.dangling, 0, "{r:?}");
        assert_eq!(r.double_allocated, 0, "{r:?}");
    }
    stop.store(true, Ordering::Relaxed);
    let commits = writer.join().unwrap();
    assert!(commits > 0, "the writer never got a turn, so this proved nothing");
}

/// A compact copy is a fresh history: it must account for every page it has.
#[test]
fn a_compacted_copy_accounts_for_every_page() {
    let src = seeded(200);
    let mut w = src.write();
    w.delete("tx", &(0..100u64).map(|i| i << 20).collect::<Vec<_>>()).unwrap();
    w.commit().unwrap();

    let copy = src.copy_to(MemPager::new()).unwrap();
    let r = audit(&copy);
    assert_eq!(r.leaked, 0, "{r:?}");
    assert_eq!(r.free_total, 0, "a fresh copy has nothing free: {r:?}");
    assert_eq!(r.reachable, r.page_count, "{r:?}");
}
