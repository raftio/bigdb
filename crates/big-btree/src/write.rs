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

//! Copy-on-write mutation. Every page touched gets a new number; the old one is freed for a
//! later transaction to reuse.

use crate::error::{BTreeError, Result};
use crate::item::LeafItem;
use big_container::{optimize, Caps, Container, ContainerRef, DeltaEntry, BITMAP_WORDS, MAX_DELTA};
use big_page::{
    bitmap_page_checksum, build_bitmap_page, BranchBuilder, BranchCell, BranchPage, ContainerKey,
    LeafBuilder, LeafPage, PageType, Pgno, ARRAY_MAX_ELEMS, RUN_MAX_INTERVALS,
};
use big_pager::{PagerMut, WriteTxn};
use std::collections::VecDeque;

/// Physical ceilings a container has to respect to stay inline in a leaf cell.
pub const CAPS: Caps = Caps { array_max: ARRAY_MAX_ELEMS, run_max: RUN_MAX_INTERVALS };

/// The ceiling actually used, which is about **write cost** rather than about fit.
///
/// `optimize` minimises stored size, and for a container just under the physical cap it is right
/// to: four kilobytes of runs beat eight kilobytes of bitmap. But size is not what a
/// copy-on-write engine pays. An inline container is rewritten *in full* on every change, so a
/// four-kilobyte run cell costs four kilobytes per changed bit and crowds every other cell out
/// of its leaf; a dense one costs a hundred and fifty bytes of delta and folds to a whole page
/// once per [`MAX_DELTA`] changes.
///
/// Measured, not assumed: a fragment holding twenty thousand records kept five of its
/// twenty-one containers as runs averaging 4.4 KB, and those five were most of what a
/// single-record commit wrote.
///
/// Half a page is the line. Below it, inline is both smaller and cheap enough to rewrite; above
/// it, a container is bounded at 2x the space and becomes eligible for a delta - which is the
/// right trade for an engine whose compacted footprint is already the smallest of its peers.
const DENSE_ABOVE_BYTES: usize = 4096;
pub const WRITE_CAPS: Caps = Caps {
    array_max: {
        let by_bytes = DENSE_ABOVE_BYTES / 2;
        if by_bytes < ARRAY_MAX_ELEMS {
            by_bytes
        } else {
            ARRAY_MAX_ELEMS
        }
    },
    run_max: {
        let by_bytes = DENSE_ABOVE_BYTES / 4;
        if by_bytes < RUN_MAX_INTERVALS {
            by_bytes
        } else {
            RUN_MAX_INTERVALS
        }
    },
};

/// Turns a container into a leaf item, promoting it onto its own page when it will not fit.
///
/// This is where the container layer and the b-tree meet: the decision to go dense is not
/// "the cell is full, split the leaf" but "the cell can never fit, give it a page".
pub fn make_item<P: PagerMut>(
    txn: &mut WriteTxn<'_, P>,
    key: ContainerKey,
    c: ContainerRef<'_>,
) -> Result<LeafItem> {
    make_item_over(txn, key, c, None)
}

/// The same, told what was already at this key.
///
/// `base` is the page and checksum of the dense container currently stored there, when there is
/// one. With it, a container that is still dense can be written as a **delta against that page**
/// rather than as a new page of its own - which is the difference between one 8 KiB write per
/// changed bit and one per `MAX_DELTA` of them.
///
/// The delta is always computed against the base **as written**, never against the container the
/// old cell stood for. That is what keeps it a single subtraction with no history: whatever the
/// old delta was, the new one is `new XOR base` and nothing has to be merged or replayed.
pub fn make_item_over<P: PagerMut>(
    txn: &mut WriteTxn<'_, P>,
    key: ContainerKey,
    c: ContainerRef<'_>,
    base: Option<(Pgno, u32)>,
) -> Result<LeafItem> {
    let best = optimize(c, WRITE_CAPS).into_owned();
    let cardinality = best.cardinality();

    // Only a container that is still dense can be a delta. One that shrank back to an array or a
    // run is written inline and its old page is released by `splice`, which is the same thing
    // that happened before deltas existed.
    if let (Container::Bitmap(words), Some((pgno, checksum))) = (&best, base) {
        if let Some(item) = try_delta(txn, key, words, cardinality, pgno, checksum)? {
            return Ok(item);
        }
    }

    match best {
        Container::Array(a) => Ok(LeafItem {
            key,
            ty: big_page::ContainerType::Array,
            elem_n: a.len() as u16,
            cardinality,
            bitmap_checksum: 0,
            bitmap_pgno: 0,
            payload: bytemuck::cast_slice(&a).to_vec(),
        }),
        Container::Run(r) => Ok(LeafItem {
            key,
            ty: big_page::ContainerType::Run,
            elem_n: r.len() as u16,
            cardinality,
            bitmap_checksum: 0,
            bitmap_pgno: 0,
            payload: bytemuck::cast_slice(&r).to_vec(),
        }),
        Container::Bitmap(w) => {
            let page = build_bitmap_page(&w);
            let pgno = txn.alloc()?;
            let checksum = bitmap_page_checksum(&page);
            txn.write(pgno, page)?;
            Ok(LeafItem::dense(key, pgno, cardinality, checksum))
        }
    }
}

/// A delta item against `pgno`, or `None` when too much changed to be worth one.
///
/// Returning `None` rather than a partial delta is the whole safety property: the caller then
/// writes a fresh page, and the only two states a cell can be in are "this page, exactly" and
/// "this page plus these few bits".
fn try_delta<P: PagerMut>(
    txn: &mut WriteTxn<'_, P>,
    key: ContainerKey,
    words: &[u64; BITMAP_WORDS],
    cardinality: u32,
    pgno: Pgno,
    checksum: u32,
) -> Result<Option<LeafItem>> {
    let page = txn.read(pgno)?;
    // The base has to be the page the old cell promised. A mismatch means the tree is damaged,
    // and writing a delta against a page that is not what it claims to be would turn a detectable
    // corruption into a silently wrong container.
    let computed = bitmap_page_checksum(&page);
    if computed != checksum {
        return Err(big_page::PageError::ChecksumMismatch { stored: checksum, computed }.into());
    }

    let base = big_page::base_words(&page)?;
    let mut entries = Vec::new();
    for (w, (new, old)) in words.iter().zip(base.iter()).enumerate() {
        let mut diff = new ^ old;
        while diff != 0 {
            let bit = diff.trailing_zeros();
            let offset = (w * 64 + bit as usize) as u16;
            entries.push(DeltaEntry { offset, set: new >> bit & 1 == 1 });
            // Past the cap the answer is already "no", and continuing would walk the rest of a
            // page to build a list that gets thrown away.
            if entries.len() > MAX_DELTA {
                return Ok(None);
            }
            diff &= diff - 1;
        }
    }
    Ok(Some(LeafItem::delta(key, pgno, checksum, cardinality, &entries)))
}

/// Greedily fills leaf pages, splitting by bytes rather than by cell count.
///
/// Cells are variable size and one of them can be nearly a whole page, so "half the cells each"
/// is not a usable split rule. A single item always fits, which is what keeps this making
/// progress.
fn pack_leaves<P: PagerMut>(
    txn: &mut WriteTxn<'_, P>,
    items: &[LeafItem],
) -> Result<Vec<BranchCell>> {
    if items.is_empty() {
        let pgno = txn.alloc()?;
        txn.write(pgno, LeafBuilder::new().finish(pgno))?;
        return Ok(vec![BranchCell { key: 0, flags: 0, child: pgno }]);
    }

    let mut out = Vec::new();
    let mut i = 0usize;
    while i < items.len() {
        let mut b = LeafBuilder::new();
        let first = items[i].key;
        while i < items.len() && items[i].push_into(&mut b).is_some() {
            i += 1;
        }
        if b.is_empty() {
            return Err(BTreeError::PayloadTooLarge { bytes: items[i].payload.len() });
        }
        let pgno = txn.alloc()?;
        txn.write(pgno, b.finish(pgno))?;
        out.push(BranchCell { key: first, flags: 0, child: pgno });
    }
    Ok(out)
}

fn pack_branches<P: PagerMut>(
    txn: &mut WriteTxn<'_, P>,
    cells: &[BranchCell],
) -> Result<Vec<BranchCell>> {
    let mut out = Vec::new();
    for chunk in cells.chunks(BranchBuilder::CAPACITY) {
        let mut b = BranchBuilder::new();
        for c in chunk {
            b.push(*c).expect("chunked to CAPACITY");
        }
        let pgno = txn.alloc()?;
        txn.write(pgno, b.finish(pgno))?;
        out.push(BranchCell { key: chunk[0].key, flags: 0, child: pgno });
    }
    Ok(out)
}

/// Builds a whole tree bottom-up from sorted items. This is the main write path: containers
/// arrive already serialised, so there is nothing to read back first.
pub fn build<P: PagerMut>(txn: &mut WriteTxn<'_, P>, items: &[LeafItem]) -> Result<Pgno> {
    let mut level = pack_leaves(txn, items)?;
    while level.len() > 1 {
        level = pack_branches(txn, &level)?;
    }
    Ok(level[0].child)
}

/// One step of the descent: which branch we came through, and at which child index.
struct Step {
    pgno: Pgno,
    index: usize,
}

fn descend<P: PagerMut>(
    txn: &WriteTxn<'_, P>,
    root: Pgno,
    ckey: ContainerKey,
) -> Result<(Pgno, Vec<Step>)> {
    let mut path = Vec::new();
    let mut cur = root;
    for _ in 0..crate::read::MAX_DEPTH {
        let (kind, next) = {
            let page = txn.read(cur)?;
            match page.page_type()? {
                PageType::Leaf => (PageType::Leaf, None),
                PageType::Branch => {
                    let br = BranchPage::parse(&page)?;
                    if br.is_empty() {
                        return Err(BTreeError::EmptyBranch { pgno: cur });
                    }
                    // The last separator that is still <= ckey; below the first one, take child 0.
                    let mut idx = 0usize;
                    for i in 0..br.len() {
                        if br.cell(i).unwrap().key <= ckey {
                            idx = i;
                        } else {
                            break;
                        }
                    }
                    (PageType::Branch, Some((idx, br.cell(idx).unwrap().child)))
                }
                other => return Err(BTreeError::UnexpectedPage { pgno: cur, found: other }),
            }
        };
        match (kind, next) {
            (PageType::Leaf, _) => return Ok((cur, path)),
            (_, Some((index, child))) => {
                path.push(Step { pgno: cur, index });
                cur = child;
            }
            _ => unreachable!(),
        }
    }
    Err(BTreeError::TooDeep { root })
}

/// Rewrites the path from the leaf up to the root, splicing `entries` in place of the child the
/// descent came through. A parent that overflows splits, and the split propagates upward.
fn rewrite_path<P: PagerMut>(
    txn: &mut WriteTxn<'_, P>,
    path: Vec<Step>,
    entries: Vec<BranchCell>,
) -> Result<Pgno> {
    rewrite_path_span(txn, path, entries, 1)
}

/// The same, where `entries` stand in for `span` of the parent's children rather than one.
///
/// `span` is 2 exactly when a leaf was merged with its right sibling: the parent then loses a
/// cell, which is how a tree that only ever split learns to shrink again.
fn rewrite_path_span<P: PagerMut>(
    txn: &mut WriteTxn<'_, P>,
    path: Vec<Step>,
    mut entries: Vec<BranchCell>,
    span: usize,
) -> Result<Pgno> {
    let mut span = span;
    for step in path.into_iter().rev() {
        let mut cells: Vec<BranchCell> = {
            let page = txn.read(step.pgno)?;
            BranchPage::parse(&page)?.iter().collect()
        };
        txn.free(step.pgno);

        // Only the leaf's own parent can be replacing more than one child; every level above it
        // is replacing the single subtree the descent came through.
        let end = (step.index + span).min(cells.len());
        cells.splice(step.index..end, entries.iter().copied());
        span = 1;
        entries = if cells.is_empty() { Vec::new() } else { pack_branches(txn, &cells)? };
    }

    if entries.is_empty() {
        // The tree emptied out completely; a fragment always has at least a root page.
        return build(txn, &[]);
    }
    while entries.len() > 1 {
        entries = pack_branches(txn, &entries)?;
    }
    Ok(entries[0].child)
}

/// Bytes a leaf item occupies on a page, index entry included.
fn item_bytes(it: &LeafItem) -> usize {
    big_page::align_up(big_page::LEAF_CELL_HEADER + it.payload.len()) + big_page::CELL_INDEX_ENTRY
}

/// Space a leaf page has for cells.
fn leaf_capacity() -> usize {
    big_page::PAGE_SIZE - big_page::PAGE_TRAILER - big_page::align_up(big_page::PAGE_HEADER)
}

/// Folds the right-hand sibling into this leaf when the two now fit in one page.
///
/// **A copy-on-write b-tree that only ever splits gets permanently worse.** Nothing here used to
/// merge, so a leaf that once held one four-kilobyte cell kept its own page forever - even after
/// that cell shrank to a hundred bytes. A fragment whose containers passed through a large phase
/// on the way to becoming dense therefore ended up with one cell per leaf, and every commit paid
/// a leaf rewrite and a root-to-leaf path *per container*. Measured on a twenty-bit field built
/// one record at a time: twenty-one containers, twenty-one leaves, forty-two pages per commit -
/// against two pages for the identical tree built in one commit.
///
/// Only the right sibling, and only when everything fits in a single page. Both restrictions are
/// about keeping this cheap and obviously correct: the sibling's keys are all greater than this
/// leaf's, so merging is an append rather than a merge sort, and refusing unless one page holds
/// the result means the parent loses exactly one cell and no further rebalancing follows.
///
/// Returns the items to pack and how many of the parent's children they now stand for.
fn merge_right<P: PagerMut>(
    txn: &mut WriteTxn<'_, P>,
    path: &[Step],
    items: Vec<LeafItem>,
) -> Result<(Vec<LeafItem>, usize)> {
    let used: usize = items.iter().map(item_bytes).sum();
    // Cheap rejection before any page is read. A leaf that is more than half full cannot take a
    // sibling worth having, and this is the common case once a tree has settled.
    if used * 2 > leaf_capacity() {
        return Ok((items, 1));
    }
    let Some(step) = path.last() else {
        // The leaf is the root: there is no parent and so no sibling.
        return Ok((items, 1));
    };

    let sibling = {
        let page = txn.read(step.pgno)?;
        BranchPage::parse(&page)?.cell(step.index + 1).map(|c| c.child)
    };
    let Some(sibling) = sibling else {
        return Ok((items, 1));
    };

    // Peek before taking: `take_leaf` frees the page, and a sibling that turns out not to fit
    // would have to be written straight back.
    let sibling_items: Vec<LeafItem> = {
        let page = txn.read(sibling)?;
        let leaf = LeafPage::parse(&page)?;
        (0..leaf.len())
            .map(|i| leaf.cell(i).map(|c| LeafItem::from_cell(&c)))
            .collect::<core::result::Result<_, _>>()?
    };
    let sibling_used: usize = sibling_items.iter().map(item_bytes).sum();
    if used + sibling_used > leaf_capacity() {
        return Ok((items, 1));
    }

    txn.free(sibling);
    let mut merged = items;
    debug_assert!(
        merged.last().zip(sibling_items.first()).is_none_or(|(a, b)| a.key < b.key),
        "the right sibling's keys must all be greater, or appending would unsort the leaf"
    );
    merged.extend(sibling_items);
    Ok((merged, 2))
}

/// Reads back a leaf's items and frees the pages it owned.
fn take_leaf<P: PagerMut>(txn: &mut WriteTxn<'_, P>, pgno: Pgno) -> Result<Vec<LeafItem>> {
    let items: Vec<LeafItem> = {
        let page = txn.read(pgno)?;
        let leaf = LeafPage::parse(&page)?;
        (0..leaf.len())
            .map(|i| leaf.cell(i).map(|c| LeafItem::from_cell(&c)))
            .collect::<core::result::Result<_, _>>()?
    };
    txn.free(pgno);
    Ok(items)
}

/// Places one item into a leaf's sorted item list, replacing any item with the same key.
fn splice<P: PagerMut>(txn: &mut WriteTxn<'_, P>, items: &mut Vec<LeafItem>, item: LeafItem) {
    match items.binary_search_by_key(&item.key, |i| i.key) {
        Ok(i) => {
            // Replacing a dense container releases the page it used to own - unless the
            // replacement is a delta *against that same page*, which is the whole mechanism.
            // Comparing the page numbers rather than the types keeps this correct however the
            // two forms are combined.
            if let Some(old) = items[i].base_pgno() {
                if item.base_pgno() != Some(old) {
                    txn.free(old);
                }
            }
            items[i] = item;
        }
        Err(i) => items.insert(i, item),
    }
}

/// The first key that belongs to a later leaf, or `None` when this is the last one.
///
/// Read off the descent path rather than the leaf: a leaf does not know where it ends, but the
/// separator sitting next to it in its nearest ancestor does.
fn leaf_upper_bound<P: PagerMut>(
    txn: &WriteTxn<'_, P>,
    path: &[Step],
) -> Result<Option<ContainerKey>> {
    for step in path.iter().rev() {
        let page = txn.read(step.pgno)?;
        let br = BranchPage::parse(&page)?;
        if let Some(next) = br.cell(step.index + 1) {
            return Ok(Some(next.key));
        }
    }
    Ok(None)
}

/// Inserts or replaces one container. Returns the new root.
pub fn put<P: PagerMut>(
    txn: &mut WriteTxn<'_, P>,
    root: Option<Pgno>,
    item: LeafItem,
) -> Result<Pgno> {
    let Some(root) = root else {
        return build(txn, core::slice::from_ref(&item));
    };
    let (leaf_pgno, path) = descend(txn, root, item.key)?;
    let mut items = take_leaf(txn, leaf_pgno)?;
    splice(txn, &mut items, item);
    let entries = pack_leaves(txn, &items)?;
    rewrite_path(txn, path, entries)
}

/// Inserts or replaces many containers, rewriting each root-to-leaf path once per *leaf*
/// rather than once per item.
///
/// `put` in a loop is quadratic in disguise: every call rewrites the whole path, so twenty
/// containers landing in one leaf rewrite that leaf twenty times and leave nineteen copies
/// behind as garbage. A BSI write touches one container per bit plane, so that loop was
/// costing a plain integer write twenty-one full path rewrites.
///
/// `items` must be sorted by key and free of duplicates; the caller already has them grouped
/// that way, and re-sorting here would hide a caller that does not.
pub fn put_many<P: PagerMut>(
    txn: &mut WriteTxn<'_, P>,
    root: Option<Pgno>,
    items: Vec<LeafItem>,
) -> Result<Pgno> {
    debug_assert!(
        items.windows(2).all(|w| w[0].key < w[1].key),
        "put_many needs keys sorted and unique"
    );
    let Some(mut root) = root else {
        // No tree yet, so there is no path to rewrite: build one bottom-up instead.
        return build(txn, &items);
    };
    if items.is_empty() {
        return Ok(root);
    }

    let mut pending: VecDeque<LeafItem> = items.into();
    while let Some(first) = pending.front().map(|i| i.key) {
        // Descend fresh every round. The previous round may have split a leaf and reshaped the
        // levels above it, so a bound computed earlier cannot be trusted for a later key.
        let (leaf_pgno, path) = descend(txn, root, first)?;
        let bound = leaf_upper_bound(txn, &path)?;
        let mut leaf = take_leaf(txn, leaf_pgno)?;

        while let Some(next) = pending.front() {
            if bound.is_some_and(|b| next.key >= b) {
                break;
            }
            let item = pending.pop_front().expect("front was just observed");
            splice(txn, &mut leaf, item);
        }

        let entries = pack_leaves(txn, &leaf)?;
        root = rewrite_path(txn, path, entries)?;
    }
    Ok(root)
}

/// Removes one container. Returns the new root; a tree always keeps at least a root page.
pub fn remove<P: PagerMut>(
    txn: &mut WriteTxn<'_, P>,
    root: Pgno,
    key: ContainerKey,
) -> Result<Pgno> {
    let (leaf_pgno, path) = descend(txn, root, key)?;
    let mut items = take_leaf(txn, leaf_pgno)?;

    // A miss still falls through: the leaf was already freed, so it has to be written back.
    if let Ok(i) = items.binary_search_by_key(&key, |i| i.key) {
        if items[i].is_dense() {
            txn.free(items[i].bitmap_pgno);
        }
        items.remove(i);
    }

    // An empty leaf disappears rather than lingering, unless it is the only one left.
    let entries =
        if items.is_empty() && !path.is_empty() { Vec::new() } else { pack_leaves(txn, &items)? };
    rewrite_path(txn, path, entries)
}

/// Inserts or replaces many containers, deciding each one's representation against what is
/// already at its key.
///
/// **This is where the delta becomes possible.** Building the items first and inserting them
/// afterwards - which is what this used to do - means every dense container is turned into a
/// freshly allocated 8 KiB page before anything knows a page is already sitting at that key. The
/// leaf has to be in hand *before* the representation is chosen, so the order is: descend, take
/// the leaf, then decide per container.
///
/// Containers must arrive in key order.
pub fn put_containers<'c, P: PagerMut>(
    txn: &mut WriteTxn<'_, P>,
    root: Option<Pgno>,
    containers: impl IntoIterator<Item = (ContainerKey, ContainerRef<'c>)>,
) -> Result<Pgno> {
    let pending: Vec<(ContainerKey, ContainerRef<'c>)> = containers.into_iter().collect();
    debug_assert!(
        pending.windows(2).all(|w| w[0].0 < w[1].0),
        "put_containers needs keys sorted and unique"
    );

    // No tree yet, so there is nothing to delta against and no path to rewrite: build one
    // bottom-up, exactly as before.
    let Some(mut root) = root else {
        let mut items = Vec::with_capacity(pending.len());
        for (key, c) in pending {
            items.push(make_item(txn, key, c)?);
        }
        return build(txn, &items);
    };
    if pending.is_empty() {
        return Ok(root);
    }

    let mut pending: VecDeque<(ContainerKey, ContainerRef<'c>)> = pending.into();
    while let Some(first) = pending.front().map(|(k, _)| *k) {
        // Descend fresh every round. The previous round may have split a leaf and reshaped the
        // levels above it, so a bound computed earlier cannot be trusted for a later key.
        let (leaf_pgno, path) = descend(txn, root, first)?;
        let bound = leaf_upper_bound(txn, &path)?;
        let mut leaf = take_leaf(txn, leaf_pgno)?;

        while let Some((key, _)) = pending.front() {
            if bound.is_some_and(|b| *key >= b) {
                break;
            }
            let (key, c) = pending.pop_front().expect("front was just observed");
            // Only the page number and its checksum, not the item: copying those two is free,
            // and holding a borrow into `leaf` would collide with splicing into it below.
            let base = leaf
                .binary_search_by_key(&key, |i| i.key)
                .ok()
                .and_then(|i| leaf[i].base_pgno().map(|p| (p, leaf[i].bitmap_checksum)));
            let item = make_item_over(txn, key, c, base)?;
            splice(txn, &mut leaf, item);
        }

        let (leaf, span) = merge_right(txn, &path, leaf)?;
        let entries = pack_leaves(txn, &leaf)?;
        root = rewrite_path_span(txn, path, entries, span)?;
    }
    Ok(root)
}

/// One container, through the same path so that it gets the same treatment.
pub fn put_container<P: PagerMut>(
    txn: &mut WriteTxn<'_, P>,
    root: Option<Pgno>,
    key: ContainerKey,
    c: ContainerRef<'_>,
) -> Result<Pgno> {
    put_containers(txn, root, [(key, c)])
}
