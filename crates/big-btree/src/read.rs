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

//! Descent and scanning. Nothing here allocates; a hit hands back the page guard it found.

use crate::error::{BTreeError, Result};
use big_page::{BranchPage, ContainerKey, LeafCell, LeafPage, Page, PageType, Pgno};
use big_pager::Pager;
use core::ops::ControlFlow;

/// Pages come off disk, so a corrupt `child` pointer must not turn into an unbounded walk.
pub const MAX_DEPTH: usize = 32;

/// A hit, holding the page guard open for as long as the caller needs the cell.
pub struct Found<'p, P: Pager + 'p> {
    page: P::Ref<'p>,
    index: usize,
}

impl<'p, P: Pager + 'p> Found<'p, P> {
    pub fn cell(&self) -> Result<LeafCell<'_>> {
        Ok(LeafPage::parse(&self.page)?.cell(self.index)?)
    }

    pub fn page(&self) -> &Page {
        &self.page
    }

    pub fn index(&self) -> usize {
        self.index
    }
}

fn page_kind(page: &Page) -> Result<PageType> {
    Ok(page.page_type()?)
}

/// Walks down to the leaf that would hold `ckey`, returning its page number.
pub fn leaf_for<P: Pager>(pager: &P, root: Pgno, ckey: ContainerKey) -> Result<Pgno> {
    let mut cur = root;
    for _ in 0..MAX_DEPTH {
        let page = pager.read(cur)?;
        match page_kind(&page)? {
            PageType::Leaf => return Ok(cur),
            PageType::Branch => {
                let br = BranchPage::parse(&page)?;
                // Below the first separator, the leftmost child is still the right place.
                cur = br
                    .child_for(ckey)
                    .or_else(|| br.cell(0).map(|c| c.child))
                    .ok_or(BTreeError::EmptyBranch { pgno: cur })?;
            }
            other => return Err(BTreeError::UnexpectedPage { pgno: cur, found: other }),
        }
    }
    Err(BTreeError::TooDeep { root })
}

pub fn find<'p, P: Pager + 'p>(
    pager: &'p P,
    root: Pgno,
    ckey: ContainerKey,
) -> Result<Option<Found<'p, P>>> {
    let pgno = leaf_for(pager, root, ckey)?;
    let page = pager.read(pgno)?;
    let index = match LeafPage::parse(&page)?.search(ckey) {
        Ok(i) => i,
        Err(_) => return Ok(None),
    };
    Ok(Some(Found { page, index }))
}

/// Probes many keys in one pass, calling `f` once per key with its cell when the key is
/// present and `None` when it is not.
///
/// `find` in a loop descends the whole tree for every key. Keys that land in the same leaf can
/// share that descent, and a BSI point read is exactly that shape: one key per bit plane, all
/// in one fragment, at a fixed stride.
///
/// Keys must be sorted and free of duplicates.
pub fn find_many<P, F>(pager: &P, root: Pgno, keys: &[ContainerKey], mut f: F) -> Result<()>
where
    P: Pager,
    F: FnMut(ContainerKey, Option<LeafCell<'_>>) -> Result<()>,
{
    let mut i = 0usize;
    while i < keys.len() {
        let pgno = leaf_for(pager, root, keys[i])?;
        let page = pager.read(pgno)?;
        let leaf = LeafPage::parse(&page)?;
        let last = match leaf.len() {
            0 => None,
            n => Some(leaf.cell(n - 1)?.key),
        };

        // Answer the key this descent was for, then keep going while the next key still falls
        // inside this leaf. Consuming at least one key per descent is what stops a key routed
        // to a leaf whose highest cell sits below it from stalling the loop.
        loop {
            match leaf.search(keys[i]) {
                Ok(idx) => f(keys[i], Some(leaf.cell(idx)?))?,
                Err(_) => f(keys[i], None)?,
            }
            i += 1;
            if i >= keys.len() || last.is_none_or(|l| keys[i] > l) {
                break;
            }
        }
    }
    Ok(())
}

/// Visits every cell with a key in `[lo, hi]`, in order.
///
/// This is the hot path: a row is 16 consecutive container keys, so it must be one contiguous
/// walk rather than 16 independent descents.
pub fn scan<P, F>(pager: &P, root: Pgno, lo: ContainerKey, hi: ContainerKey, mut f: F) -> Result<()>
where
    P: Pager,
    F: FnMut(LeafCell<'_>) -> ControlFlow<()>,
{
    walk(pager, root, lo, hi, &mut f, 0).map(|_| ())
}

fn walk<P, F>(
    pager: &P,
    pgno: Pgno,
    lo: ContainerKey,
    hi: ContainerKey,
    f: &mut F,
    depth: usize,
) -> Result<ControlFlow<()>>
where
    P: Pager,
    F: FnMut(LeafCell<'_>) -> ControlFlow<()>,
{
    if depth >= MAX_DEPTH {
        return Err(BTreeError::TooDeep { root: pgno });
    }
    let page = pager.read(pgno)?;
    match page_kind(&page)? {
        PageType::Leaf => {
            let leaf = LeafPage::parse(&page)?;
            let start = match leaf.search(lo) {
                Ok(i) | Err(i) => i,
            };
            for i in start..leaf.len() {
                let cell = leaf.cell(i)?;
                if cell.key > hi {
                    return Ok(ControlFlow::Break(()));
                }
                if f(cell).is_break() {
                    return Ok(ControlFlow::Break(()));
                }
            }
            Ok(ControlFlow::Continue(()))
        }
        PageType::Branch => {
            let br = BranchPage::parse(&page)?;
            for i in 0..br.len() {
                let cell = br.cell(i).unwrap();
                // A child covers keys from its separator up to the next one.
                let next_key = br.cell(i + 1).map(|c| c.key);
                if next_key.is_some_and(|k| k <= lo) {
                    continue;
                }
                if cell.key > hi {
                    break;
                }
                if walk(pager, cell.child, lo, hi, f, depth + 1)?.is_break() {
                    return Ok(ControlFlow::Break(()));
                }
            }
            Ok(ControlFlow::Continue(()))
        }
        other => Err(BTreeError::UnexpectedPage { pgno, found: other }),
    }
}

/// Sums the cached cardinality of every cell. O(cells), never touches a payload.
pub fn count<P: Pager>(pager: &P, root: Pgno) -> Result<u64> {
    let mut total = 0u64;
    scan(pager, root, 0, u64::MAX, |c| {
        total += c.cardinality as u64;
        ControlFlow::Continue(())
    })?;
    Ok(total)
}

/// Collects every cell, in key order. Used by rebuilds and by tests.
pub fn collect<P: Pager>(pager: &P, root: Pgno) -> Result<Vec<crate::LeafItem>> {
    let mut out = Vec::new();
    scan(pager, root, 0, u64::MAX, |c| {
        out.push(crate::LeafItem::from_cell(&c));
        ControlFlow::Continue(())
    })?;
    Ok(out)
}
