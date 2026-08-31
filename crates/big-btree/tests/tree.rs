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

//! The tree is exercised entirely against an in-memory pager: no file, no mmap, no fsync.
//! Splitting and rebalancing are the easiest things here to get wrong, and this runs at unit
//! test speed.

use big_btree::*;
use big_container::{Container, ContainerRef};
use big_page::{ContainerType, FragmentKey, Pgno};
use big_pager::{CountingPager, MemPager, Pager, PagerMut, Store, WriteTxn};
use proptest::prelude::*;
use std::collections::BTreeMap;
use std::ops::ControlFlow;

const KEY: FragmentKey = FragmentKey { table: 1, field: 1, view: 0, shard: 0 };

fn store() -> Store<MemPager> {
    Store::init(MemPager::new()).unwrap()
}

fn payload(vals: &[u16]) -> Container {
    Container::from_values(vals.iter().copied())
}

/// Applies a batch of container writes in one transaction and records the new root.
fn write_all(s: &Store<MemPager>, root: Option<Pgno>, items: &[(u64, Vec<u16>)]) -> Pgno {
    let mut w = s.begin_write();
    let mut r = root;
    for (k, vals) in items {
        let c = payload(vals);
        r = Some(put_container(&mut w, r, *k, c.as_ref()).unwrap());
    }
    let root = r.unwrap();
    w.set_root(KEY, root);
    w.commit().unwrap();
    root
}

fn read_back(s: &Store<MemPager>, root: Pgno) -> BTreeMap<u64, Vec<u16>> {
    let mut out = BTreeMap::new();
    scan(s.pager(), root, 0, u64::MAX, |c| {
        let vals: Vec<u16> = match c.container().unwrap() {
            Some(cr) => cr.iter().collect(),
            None => Vec::new(),
        };
        out.insert(c.key, vals);
        ControlFlow::Continue(())
    })
    .unwrap();
    out
}

/// Reads back including dense containers, which live on their own page.
fn read_back_full(s: &Store<MemPager>, root: Pgno) -> BTreeMap<u64, Vec<u16>> {
    let mut keys = Vec::new();
    scan(s.pager(), root, 0, u64::MAX, |c| {
        keys.push((c.key, c.ty, c.bitmap_pgno));
        ControlFlow::Continue(())
    })
    .unwrap();

    let mut out = BTreeMap::new();
    for (k, ty, dense_pgno) in keys {
        let found = find(s.pager(), root, k).unwrap().unwrap();
        let cell = found.cell().unwrap();
        let vals: Vec<u16> = if ty == ContainerType::BitmapPtr {
            let dense = s.pager().read(dense_pgno).unwrap();
            cell.bitmap(&dense).unwrap().iter().collect()
        } else {
            cell.container().unwrap().unwrap().iter().collect()
        };
        out.insert(k, vals);
    }
    out
}

#[test]
fn build_then_collect_round_trips() {
    let s = store();
    let items: Vec<(u64, Vec<u16>)> = (0..50u64).map(|k| (k, vec![1, 2, (k as u16) + 3])).collect();
    let root = write_all(&s, None, &items);

    let got = read_back(&s, root);
    assert_eq!(got.len(), 50);
    for (k, vals) in &items {
        assert_eq!(got.get(k).unwrap(), vals);
    }
}

/// One cell can be half a page, so a leaf may hold exactly one of them. Splitting by cell count
/// instead of by bytes would produce pages that cannot be written, and merging by cell count
/// would produce pages that cannot be written either.
///
/// Sized at the inline ceiling rather than at the physical one: a container larger than
/// `WRITE_CAPS` allows is stored dense, precisely so that it is *not* rewritten in full on every
/// change, so a four-thousand-element array is no longer a near-page-sized cell.
#[test]
fn near_page_sized_cells_force_one_cell_per_leaf() {
    let s = store();
    let big: Vec<u16> = (0..2048u16).map(|i| i * 3).collect();
    let items: Vec<(u64, Vec<u16>)> = (0..12u64).map(|k| (k, big.clone())).collect();
    let root = write_all(&s, None, &items);

    let got = read_back(&s, root);
    assert_eq!(got.len(), 12);
    for k in 0..12u64 {
        assert_eq!(got.get(&k).unwrap().len(), 2048);
    }
    assert!(depth(&s, root) > 1, "12 near-full leaves must need a branch level");
}

fn depth(s: &Store<MemPager>, root: Pgno) -> usize {
    let mut d = 1;
    let mut cur = root;
    while let Ok(big_page::PageType::Branch) = s.pager().read(cur).unwrap().page_type() {
        let page = s.pager().read(cur).unwrap();
        cur = big_page::BranchPage::parse(&page).unwrap().cell(0).unwrap().child;
        d += 1;
    }
    d
}

/// A container that cannot fit inline gets its own page, verified through the parent checksum.
#[test]
fn oversized_containers_are_promoted_to_a_dense_page() {
    let s = store();
    let shredded: Vec<u16> = (0..50000u16).step_by(2).collect();
    let root = write_all(&s, None, &[(7, shredded.clone())]);

    let found = find(s.pager(), root, 7).unwrap().unwrap();
    let cell = found.cell().unwrap();
    assert_eq!(cell.ty, ContainerType::BitmapPtr, "must not stay inline");
    assert_ne!(cell.bitmap_pgno, 0);

    let dense = s.pager().read(cell.bitmap_pgno).unwrap();
    let got: Vec<u16> = cell.bitmap(&dense).unwrap().iter().collect();
    assert_eq!(got, shredded);
}

/// The old root must stay readable after a write: that is the whole point of copy-on-write.
#[test]
fn the_previous_root_survives_a_write() {
    let s = store();
    let v1 = write_all(&s, None, &(0..40u64).map(|k| (k, vec![k as u16])).collect::<Vec<_>>());
    let before = read_back(&s, v1);

    let v2 = write_all(&s, Some(v1), &[(5, vec![99, 100, 101])]);
    assert_ne!(v1, v2);

    assert_eq!(read_back(&s, v1), before, "old tree must be untouched");
    assert_eq!(read_back(&s, v2).get(&5).unwrap(), &vec![99, 100, 101]);
}

#[test]
fn scan_returns_exactly_the_requested_range() {
    let s = store();
    let root = write_all(&s, None, &(0..200u64).map(|k| (k * 3, vec![1u16])).collect::<Vec<_>>());

    let mut seen = Vec::new();
    scan(s.pager(), root, 30, 90, |c| {
        seen.push(c.key);
        ControlFlow::Continue(())
    })
    .unwrap();
    assert_eq!(seen, (10..=30).map(|i| i * 3).collect::<Vec<u64>>());
}

/// A row is 16 consecutive container keys; that has to be one walk, not 16 descents.
#[test]
fn a_row_is_one_contiguous_scan() {
    let s = store();
    let root = write_all(&s, None, &(0..320u64).map(|k| (k, vec![k as u16])).collect::<Vec<_>>());

    for row in 0..20u64 {
        let mut seen = Vec::new();
        scan(s.pager(), root, row * 16, row * 16 + 15, |c| {
            seen.push(c.key);
            ControlFlow::Continue(())
        })
        .unwrap();
        assert_eq!(seen.len(), 16, "row {row}");
        assert_eq!(seen[0], row * 16);
    }
}

#[test]
fn count_sums_cached_cardinalities() {
    let s = store();
    let items: Vec<(u64, Vec<u16>)> =
        (0..30u64).map(|k| (k, (0..(k as u16 + 1)).collect())).collect();
    let root = write_all(&s, None, &items);
    let expected: u64 = items.iter().map(|(_, v)| v.len() as u64).sum();
    assert_eq!(count(s.pager(), root).unwrap(), expected);
}

#[test]
fn removing_everything_leaves_an_empty_but_valid_tree() {
    let s = store();
    let mut root =
        write_all(&s, None, &(0..80u64).map(|k| (k, vec![k as u16])).collect::<Vec<_>>());

    let mut w = s.begin_write();
    for k in 0..80u64 {
        root = remove(&mut w, root, k).unwrap();
    }
    w.set_root(KEY, root);
    w.commit().unwrap();

    assert_eq!(read_back(&s, root).len(), 0);
    assert_eq!(count(s.pager(), root).unwrap(), 0);
    assert_eq!(depth(&s, root), 1, "an emptied tree collapses back to a single leaf");
}

#[test]
fn free_tree_returns_every_page_including_dense_ones() {
    let s = store();
    let shredded: Vec<u16> = (0..50000u16).step_by(2).collect();
    let root = write_all(&s, None, &[(1, shredded), (2, vec![1, 2, 3])]);
    let before = s.metrics().page_count;

    let mut w = s.begin_write();
    free_tree(&mut w, root).unwrap();
    w.remove_root(&KEY);
    w.commit().unwrap();

    assert!(s.metrics().free_pages_reusable + s.metrics().pages_pending_reclaim_reader > 0);

    // Never smaller: a freed page is recorded, not surrendered, and only `truncate_tail`
    // gives one back. It can be slightly larger, because recording this many freed pages
    // needs a longer freelist chain and those pages cannot come from the very freelist being
    // written - they are stamped with the transaction doing the writing.
    assert!(
        s.metrics().page_count >= before,
        "freeing must not shrink the file by itself: {} < {before}",
        s.metrics().page_count
    );
}

fn ops() -> impl Strategy<Value = Vec<(u64, Option<Vec<u16>>)>> {
    proptest::collection::vec(
        (0u64..400, proptest::option::of(proptest::collection::vec(any::<u16>(), 0..40))),
        0..60,
    )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(120))]

    /// Puts and removes in one transaction must track a plain map exactly.
    #[test]
    fn tree_tracks_a_btreemap(ops in ops()) {
        let s = store();
        let mut model: BTreeMap<u64, Vec<u16>> = BTreeMap::new();
        let mut root: Option<Pgno> = None;

        let mut w = s.begin_write();
        for (k, v) in &ops {
            match v {
                Some(vals) => {
                    let c = payload(vals);
                    root = Some(put_container(&mut w, root, *k, c.as_ref()).unwrap());
                    model.insert(*k, c.as_ref().iter().collect());
                }
                None => {
                    if let Some(r) = root {
                        root = Some(remove(&mut w, r, *k).unwrap());
                        model.remove(k);
                    }
                }
            }
        }
        let Some(root) = root else { return Ok(()); };
        w.set_root(KEY, root);
        w.commit().unwrap();

        prop_assert_eq!(read_back_full(&s, root), model);
    }

    /// The same sequence, but one transaction per step, so every write goes through a commit.
    #[test]
    fn tree_survives_one_commit_per_operation(ops in ops()) {
        let s = store();
        let mut model: BTreeMap<u64, Vec<u16>> = BTreeMap::new();
        let mut root: Option<Pgno> = None;

        for (k, v) in &ops {
            let mut w = s.begin_write();
            match v {
                Some(vals) => {
                    let c = payload(vals);
                    root = Some(put_container(&mut w, root, *k, c.as_ref()).unwrap());
                    model.insert(*k, c.as_ref().iter().collect());
                }
                None => {
                    if let Some(r) = root {
                        root = Some(remove(&mut w, r, *k).unwrap());
                        model.remove(k);
                    }
                }
            }
            if let Some(r) = root {
                w.set_root(KEY, r);
            }
            w.commit().unwrap();
        }
        let Some(root) = root else { return Ok(()); };

        prop_assert_eq!(read_back_full(&s, root), model.clone());

        let reloaded = Store::load(s.pager().clone()).unwrap();
        prop_assert_eq!(reloaded.roots().get(&KEY), Some(root));
        prop_assert_eq!(read_back_full(&reloaded, root), model);
    }

    /// Every key must be reachable through a descent, not just through a full scan.
    #[test]
    fn find_agrees_with_scan(keys in proptest::collection::btree_set(0u64..2000, 0..120)) {
        let s = store();
        let items: Vec<(u64, Vec<u16>)> = keys.iter().map(|k| (*k, vec![*k as u16])).collect();
        if items.is_empty() { return Ok(()); }
        let root = write_all(&s, None, &items);

        for k in &keys {
            let found = find(s.pager(), root, *k).unwrap();
            prop_assert!(found.is_some(), "key {} not reachable", k);
            prop_assert_eq!(found.unwrap().cell().unwrap().key, *k);
        }
        for miss in [2001u64, 5000, u64::MAX] {
            prop_assert!(find(s.pager(), root, miss).unwrap().is_none());
        }
    }
}

fn _assert_generic_over_pager<P: PagerMut>(txn: &mut WriteTxn<'_, P>) {
    let _ = build(txn, &[]);
}

fn _assert_container_ref_used(c: ContainerRef<'_>) -> u32 {
    c.cardinality()
}

/// Applies a batch through `put_many` in one transaction.
fn write_batched(s: &Store<MemPager>, root: Option<Pgno>, items: &[(u64, Vec<u16>)]) -> Pgno {
    let mut w = s.begin_write();
    let containers: Vec<Container> = items.iter().map(|(_, v)| payload(v)).collect();
    let pairs: Vec<(u64, ContainerRef<'_>)> =
        items.iter().zip(&containers).map(|((k, _), c)| (*k, c.as_ref())).collect();
    let root = put_containers(&mut w, root, pairs).unwrap();
    w.set_root(KEY, root);
    w.commit().unwrap();
    root
}

/// Pages actually pushed through the pager while applying a batch one way or the other.
fn writes_for(batch: &[(u64, Vec<u16>)], batched: bool) -> u64 {
    let s = Store::init(CountingPager::new(MemPager::new())).unwrap();
    let mut w = s.begin_write();
    let mut root = None;
    if batched {
        let containers: Vec<Container> = batch.iter().map(|(_, v)| payload(v)).collect();
        let pairs: Vec<(u64, ContainerRef<'_>)> =
            batch.iter().zip(&containers).map(|((k, _), c)| (*k, c.as_ref())).collect();
        root = Some(put_containers(&mut w, None, pairs).unwrap());
    } else {
        for (k, vals) in batch {
            let c = payload(vals);
            root = Some(put_container(&mut w, root, *k, c.as_ref()).unwrap());
        }
    }
    w.set_root(KEY, root.unwrap());
    w.commit().unwrap();
    s.pager().counts().writes
}

/// Sorted, deduplicated batches: what `put_many` documents as its precondition, and what the
/// grouping in `big-fragment` already produces.
fn sorted_batch() -> impl Strategy<Value = Vec<(u64, Vec<u16>)>> {
    proptest::collection::btree_map(
        0u64..4_000,
        proptest::collection::vec(any::<u16>(), 0..40),
        1..80,
    )
    .prop_map(|m| m.into_iter().collect())
}

proptest! {
    /// The batch path must be indistinguishable from the loop it replaces. Anything else is a
    /// silent divergence between two ways of writing the same fact.
    #[test]
    fn put_many_matches_put_in_a_loop(batch in sorted_batch()) {
        let a = store();
        let one_at_a_time = write_all(&a, None, &batch);

        let b = store();
        let batched = write_batched(&b, None, &batch);

        prop_assert_eq!(read_back_full(&a, one_at_a_time), read_back_full(&b, batched));
    }

    /// Same, but onto a tree that already has content, which is where the descent bound and
    /// the leaf splitting actually get exercised.
    #[test]
    fn put_many_matches_put_over_an_existing_tree(
        first in sorted_batch(),
        second in sorted_batch(),
    ) {
        let a = store();
        let mut root_a = write_all(&a, None, &first);
        root_a = write_all(&a, Some(root_a), &second);

        let b = store();
        let mut root_b = write_batched(&b, None, &first);
        root_b = write_batched(&b, Some(root_b), &second);

        prop_assert_eq!(read_back_full(&a, root_a), read_back_full(&b, root_b));
    }

    /// The batch path must never make a transaction write more pages than the loop it
    /// replaces.
    ///
    /// Only `<=`, and that is the honest claim. A transaction holds its dirty pages in a map
    /// keyed by page number and recycles the ones its own rewrites abandon, so a page rewritten
    /// twenty times still reaches the disk once. What `put_many` saves is the twenty descents
    /// and twenty page rebuilds - work, not bytes - and that shows up on a clock rather than in
    /// a counter. `bench/` measures it.
    #[test]
    fn put_many_never_writes_more_pages(batch in sorted_batch()) {
        prop_assume!(batch.len() >= 8);

        let one_at_a_time = writes_for(&batch, false);
        let batched = writes_for(&batch, true);

        prop_assert!(
            batched <= one_at_a_time,
            "batched wrote {batched} pages, one-at-a-time wrote {one_at_a_time}"
        );
    }
}

// ---------------------------------------------------------------------------
// The shared walk: `free_tree` and `copy_tree` must agree on what a tree is made of.
// ---------------------------------------------------------------------------

/// Every page a tree occupies, in the order the walk yields them.
fn walked(s: &Store<MemPager>, root: Pgno) -> Vec<(Pgno, big_page::PageType)> {
    let mut out = Vec::new();
    visit_tree(s.pager(), root, &mut |pgno, ty| {
        out.push((pgno, ty));
        Ok(())
    })
    .unwrap();
    out
}

/// A tree holding all three container shapes at once: arrays wide enough that a handful fill
/// a leaf and the tree needs a branch, one run, and one container too large to sit inline.
///
/// The stepped values matter - a contiguous range would be optimised into a run, and the
/// point of these cells is that they stay arrays and stay big.
fn mixed_tree(s: &Store<MemPager>) -> Pgno {
    let dense: Vec<u16> = (0..50000u16).step_by(2).collect();
    let run: Vec<u16> = (0..4000u16).collect();
    let mut items: Vec<(u64, Vec<u16>)> =
        (10..50u64).map(|k| (k, (0..3000u16).step_by(3).map(|v| v + k as u16).collect())).collect();
    items.push((1, dense));
    items.push((2, run));
    items.sort_by_key(|(k, _)| *k);
    write_all(s, None, &items)
}

#[test]
fn the_walk_reaches_branches_leaves_and_dense_pages() {
    let s = store();
    let root = mixed_tree(&s);
    let pages = walked(&s, root);

    let count = |want: big_page::PageType| pages.iter().filter(|(_, ty)| *ty == want).count();
    assert!(count(big_page::PageType::Leaf) > 1, "the fixture must span several leaves");
    assert!(count(big_page::PageType::Branch) >= 1, "several leaves need a branch above them");
    assert_eq!(count(big_page::PageType::Bitmap), 1, "exactly one container was promoted");

    // The root is the last thing yielded: children before parents, so a copy can remap a
    // child pointer before it writes the page holding it.
    assert_eq!(pages.last().unwrap().0, root);

    let mut seen: Vec<Pgno> = pages.iter().map(|(p, _)| *p).collect();
    let n = seen.len();
    seen.sort_unstable();
    seen.dedup();
    assert_eq!(seen.len(), n, "no page may be visited twice");
}

#[test]
fn copy_tree_reproduces_every_container() {
    let src = store();
    let root = mixed_tree(&src);
    let before = read_back_full(&src, root);

    let dst = store();
    let mut w = dst.begin_write();
    let new_root = copy_tree(src.pager(), &mut w, root).unwrap();
    w.set_root(KEY, new_root);
    w.commit().unwrap();

    assert_eq!(read_back_full(&dst, new_root), before);
    assert_eq!(count(dst.pager(), new_root).unwrap(), count(src.pager(), root).unwrap());
}

#[test]
fn copy_tree_writes_exactly_the_pages_the_walk_finds() {
    // The anti-drift check. `free_tree` and `copy_tree` are two consumers of one walk; if the
    // copy ever stops handling a page class, the two counts diverge here rather than in a
    // backup that silently lost a container.
    let src = store();
    let root = mixed_tree(&src);
    let live = walked(&src, root).len() as u64;

    let dst = store();
    let before = dst.metrics().page_count;
    let mut w = dst.begin_write();
    let new_root = copy_tree(src.pager(), &mut w, root).unwrap();
    w.set_root(KEY, new_root);
    w.commit().unwrap();

    assert_eq!(walked(&dst, new_root).len() as u64, live);
    // The copy's own pages, plus whatever the commit spent on metadata chains.
    assert!(
        dst.metrics().page_count - before >= live,
        "the copy must have grown the file by at least the tree it wrote"
    );
    assert_eq!(dst.metrics().free_pages_reusable, 0, "a fresh copy has nothing to reclaim");
}

#[test]
fn copy_tree_leaves_the_source_untouched() {
    let src = store();
    let root = mixed_tree(&src);
    let before = read_back_full(&src, root);
    let pages_before = src.metrics().page_count;

    let dst = store();
    let mut w = dst.begin_write();
    copy_tree(src.pager(), &mut w, root).unwrap();
    w.commit().unwrap();

    assert_eq!(read_back_full(&src, root), before);
    assert_eq!(src.metrics().page_count, pages_before);
}
