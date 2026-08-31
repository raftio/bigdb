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

//! What a commit costs, counted rather than timed.
//!
//! A benchmark answers "how fast on this machine today". These answer "how much work, on every
//! machine, forever", which is the half that can be asserted. If a change makes a commit write
//! more pages, this file fails; the benchmarks would only wobble.

use big_page::LeafBuilder;
use big_pager::*;
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

fn key(shard: u64) -> FragmentKey {
    FragmentKey::new(1, 1, 0, shard)
}

fn catalog_entries(n: usize) -> Vec<Vec<u8>> {
    (0..n)
        .map(|i| {
            let mut b = vec![0u8; CATALOG_ENTRY_BYTES];
            b[0] = 1;
            b[4..12].copy_from_slice(&(i as u64).to_le_bytes());
            b
        })
        .collect()
}

/// A store with `n` fragments and one catalog entry each, mirroring what `big-db` keeps.
fn fixture(n: u64) -> Store<CountingPager<MemPager>> {
    let store = Store::open_or_init(CountingPager::new(MemPager::new())).unwrap();
    if n > 0 {
        let mut w = store.begin_write();
        for shard in 0..n {
            let p = w.alloc().unwrap();
            w.write(p, LeafBuilder::new().finish(p)).unwrap();
            w.set_root(key(shard), p);
        }
        w.set_catalog(catalog_entries(n as usize));
        w.commit().unwrap();
    }
    store
}

/// Pages written by a commit that rewrites exactly one fragment root.
fn pages_per_single_fragment_commit(n: u64) -> u64 {
    let store = fixture(n);
    store.pager().reset();

    let mut w = store.begin_write();
    if n == 0 {
        w.commit().unwrap();
    } else {
        let old = w.root(&key(0)).unwrap();
        let new = w.cow(old).unwrap();
        w.write(new, LeafBuilder::new().finish(new)).unwrap();
        w.set_root(key(0), new);
        w.set_catalog(catalog_entries(n as usize));
        w.commit().unwrap();
    }
    store.pager().counts().writes
}

/// The commit sequence rewrites the root-record, catalog and freelist chains in full, so the
/// cost of changing one fragment is set by how many fragments exist rather than by the change.
///
/// The bounds below are deliberately loose: they are here to catch a regression in the growth
/// rate, not to pin an exact page count that refactoring may legitimately shift.
#[test]
fn commit_cost_grows_with_the_database_not_the_change() {
    let mut table = Vec::new();
    for n in [0u64, 100, 1_000, 4_000] {
        let pages = pages_per_single_fragment_commit(n);
        println!("{n:>6} fragments -> {pages:>6} pages written ({} KiB)", pages * 8);
        table.push((n, pages));
    }

    let at = |n: u64| table.iter().find(|(k, _)| *k == n).unwrap().1;

    // An empty database still pays for three chains plus the meta page.
    assert!(at(0) <= 8, "empty commit floor regressed: {} pages", at(0));

    // Still grows, because the root-record chain is rewritten whenever any fragment root
    // moves - which is every data commit. The catalog no longer contributes: it is skipped
    // when unchanged, which is what took this from 79 pages to 15 at four thousand fragments.
    //
    // What remains is a root record per fragment at 24 bytes. Removing that needs the roots
    // to become a tree rather than a list, and this test is where that would show up.
    assert!(
        at(4_000) > at(0),
        "cost no longer scales with fragment count - if that is intentional, tighten this test"
    );
    let ratio = at(4_000) as f64 / at(1_000).max(1) as f64;
    assert!(
        ratio <= 5.0,
        "4x the fragments changed cost by {ratio:.1}x; growth should be at most linear"
    );

    // A single-fragment commit against 4k fragments must not quietly get worse than this.
    // 4k root records at 24 bytes is 94 KiB, so twelve pages plus the data page and the
    // freelist. Tightened from 128 once the catalog stopped being rewritten.
    assert!(
        at(4_000) <= 32,
        "single-fragment commit at 4k fragments now writes {} pages",
        at(4_000)
    );
}

/// Reads are borrows, not copies: the same page read twice yields the same address, and no
/// page read allocates. `begin_read` does allocate - it registers the reader in a map - which
/// is why the transaction is opened outside the counted region.
#[test]
fn read_path_allocates_nothing() {
    let store = fixture(64);
    let t = store.begin_read();

    let a = t.read(1).unwrap();
    let b = t.read(1).unwrap();
    assert!(std::ptr::eq(&*a, &*b), "two reads of one page must borrow the same bytes");
    drop((a, b));

    count_allocations(true);
    let before = allocs();
    let mut sum = 0u64;
    for pgno in 1..64 {
        sum += t.read(pgno).unwrap().pgno() as u64;
    }
    let allocs = allocs() - before;
    count_allocations(false);

    assert!(sum > 0);
    assert_eq!(allocs, 0, "read path allocated {allocs} times across 63 page reads");
}

// Per **thread**, not global. A global counter armed by one test counts every allocation every
// other test makes, and cargo runs them in parallel - so this test used to pass or fail depending
// on what its neighbours were doing, which is worse than not having it. The allocator hook runs
// on the allocating thread, so a thread-local attributes correctly with no coordination at all.
//
// `const` initialisers, because a lazily initialised thread-local allocates on first touch - from
// inside the allocator.
thread_local! {
    static ALLOCS: Cell<u64> = const { Cell::new(0) };
    static COUNTING: Cell<bool> = const { Cell::new(false) };
}

fn allocs() -> u64 {
    ALLOCS.with(|a| a.get())
}

fn count_allocations(on: bool) {
    COUNTING.with(|c| c.set(on));
}

/// Counts allocations while armed, and delegates everything else to the system allocator.
///
/// SAFETY: every method forwards its arguments unchanged to `System`, which is a valid
/// `GlobalAlloc`. The counter is the only added state and it never touches the allocation.
struct Counting;

impl Counting {
    /// `try_with`, because a thread tearing down has already destroyed its thread-locals and
    /// still deallocates on the way out; touching a destroyed one would panic inside the
    /// allocator.
    fn tick() {
        let armed = COUNTING.try_with(|c| c.get()).unwrap_or(false);
        if armed {
            let _ = ALLOCS.try_with(|a| a.set(a.get() + 1));
        }
    }
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        Self::tick();
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        Self::tick();
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

/// A chain nobody changed must cost nothing.
///
/// The catalog is the expensive one - 128 bytes per fragment against a root record's 24 - and
/// most commits do not alter a byte of it. Rewriting it anyway was the bulk of what a commit
/// wrote once a database had any number of fragments.
#[test]
fn an_unchanged_chain_is_not_rewritten() {
    let store = fixture(1_000);
    let catalog = catalog_entries(1_000);

    // Same catalog, one fragment root moved: only the roots and the freelist may be rewritten.
    store.pager().reset();
    let mut w = store.begin_write();
    let old = w.root(&key(0)).unwrap();
    let new = w.cow(old).unwrap();
    w.write(new, LeafBuilder::new().finish(new)).unwrap();
    w.set_root(key(0), new);
    w.set_catalog(catalog.clone());
    w.commit().unwrap();
    let unchanged = store.pager().counts().writes;

    // Now change one catalog entry and watch the whole chain come back.
    store.pager().reset();
    let mut changed = catalog;
    changed[0][40] = 7;
    let mut w = store.begin_write();
    w.set_catalog(changed);
    w.commit().unwrap();
    let rewritten = store.pager().counts().writes;

    println!("catalog untouched: {unchanged} pages, catalog changed: {rewritten} pages");
    assert!(
        unchanged < rewritten,
        "a commit that left the catalog alone wrote {unchanged} pages, one that changed it \
         wrote {rewritten}: the untouched chain is still being rewritten"
    );

    // 1000 entries at 128 bytes is sixteen pages, and none of them should be in the first
    // commit. Loose enough to survive a layout change, tight enough to catch a regression.
    assert!(unchanged <= 8, "a commit touching one fragment wrote {unchanged} pages");
}
