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

//! What a tree is made of, and how to move one.
//!
//! Two operations that look unrelated are the same walk: freeing a tree and copying one both
//! have to reach every branch, every leaf, and every container promoted onto a page of its
//! own. Freeing hands each page to the freelist; copying writes it somewhere else. Keeping
//! them in one file is deliberate - a page class that one of them learns about and the other
//! does not is a silently lost container in a backup.

use crate::error::{BTreeError, Result};
use crate::item::LeafItem;
use big_page::{BranchBuilder, BranchCell, BranchPage, LeafBuilder, LeafPage, PageType, Pgno};
use big_pager::{Pager, PagerMut, WriteTxn};

/// Visits every page of a tree, children before the parent that points at them.
///
/// The order is not incidental. A copy has to know a child's new page number before it can
/// write the branch cell naming it, and a free has to finish with a page before the freelist
/// can hand it out again.
///
/// `PageType::Bitmap` is yielded for a dense container's page, immediately before the leaf
/// that points at it. Those pages carry no header and no checksum of their own - the leaf
/// cell holds both - which is why they can be copied verbatim to any page number.
pub fn visit_tree<P: Pager>(
    pager: &P,
    root: Pgno,
    visit: &mut impl FnMut(Pgno, PageType) -> Result<()>,
) -> Result<()> {
    visit_rec(pager, root, 0, visit)
}

fn visit_rec<P: Pager>(
    pager: &P,
    pgno: Pgno,
    depth: usize,
    visit: &mut impl FnMut(Pgno, PageType) -> Result<()>,
) -> Result<()> {
    if depth >= crate::read::MAX_DEPTH {
        return Err(BTreeError::TooDeep { root: pgno });
    }

    // Read into owned values and let the borrow go: the recursion below reads other pages, and
    // a caller may well want the pager mutably once this returns.
    let (kind, children, dense) = shape(pager, pgno)?;

    for c in children {
        visit_rec(pager, c, depth + 1, visit)?;
    }
    for d in dense {
        visit(d, PageType::Bitmap)?;
    }
    visit(pgno, kind)
}

/// The three facts a walk needs from one page: what it is, and what it points at.
fn shape<P: Pager>(pager: &P, pgno: Pgno) -> Result<(PageType, Vec<Pgno>, Vec<Pgno>)> {
    let page = pager.read(pgno)?;
    match page.page_type()? {
        PageType::Branch => {
            let children = BranchPage::parse(&page)?.iter().map(|c| c.child).collect();
            Ok((PageType::Branch, children, Vec::new()))
        }
        PageType::Leaf => {
            let leaf = LeafPage::parse(&page)?;
            let mut dense = Vec::new();
            for i in 0..leaf.len() {
                let c = leaf.cell(i)?;
                // Both dense forms own a page. A delta cell's base is every bit as reachable
                // and every bit as owned as a plain pointer's, so a walk that missed it would
                // leak the page on a free and lose the container on a copy.
                if c.ty.owns_page() {
                    dense.push(c.bitmap_pgno);
                }
            }
            Ok((PageType::Leaf, Vec::new(), dense))
        }
        other => Err(BTreeError::UnexpectedPage { pgno, found: other }),
    }
}

/// What one scrub of one tree found.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Scrubbed {
    /// Branch and leaf pages whose trailer checksum was recomputed and matched.
    pub pages: u64,
    /// Dense pages checked against the checksum the leaf cell above them carries.
    pub bitmaps: u64,
}

impl Scrubbed {
    /// Everything this scrub looked at, which is the number to compare against the file's own
    /// page count when asking whether a walk reached what it should have.
    pub fn total(&self) -> u64 {
        self.pages + self.bitmaps
    }

    /// Folds another tree's count into this one, so a whole-database scrub reports one number.
    pub fn add(&mut self, other: Self) {
        self.pages += other.pages;
        self.bitmaps += other.bitmaps;
    }
}

/// Recomputes every checksum in a tree and stops at the first that does not match.
///
/// **Why this is not what `read` does.** A branch or a leaf carries a crc32 over its own bytes,
/// and nothing on the read path checks it: verifying costs a pass over 8 KiB while answering a
/// point read touches one bit, so a check on every read would make the check the whole cost of
/// the query. Dense pages are the exception and are checked whenever they are actually read,
/// because the leaf cell above them carries their checksum and the pager memoises the result -
/// see `Pager::verify_bitmap`.
///
/// The consequence is that a branch or leaf page can rot without anything noticing until a
/// query happens to land on it, and what it answers then is not an error, it is a different
/// number. That is the hole this closes, and closing it means walking rather than reading:
/// corruption is found on a schedule the operator chose rather than on a request a client made.
///
/// Both classes are checked here, dense pages included, so that one walk answers "is every byte
/// this tree can reach still what it was".
///
/// Stops at the first mismatch rather than collecting them. A file with one bad page and a file
/// with four hundred call for the same action - restore from backup - and continuing would mean
/// walking a structure whose page numbers can no longer be trusted to point anywhere.
pub fn scrub_tree<P: Pager>(pager: &P, root: Pgno) -> Result<Scrubbed> {
    scrub_rec(pager, root, 0)
}

fn scrub_rec<P: Pager>(pager: &P, pgno: Pgno, depth: usize) -> Result<Scrubbed> {
    if depth >= crate::read::MAX_DEPTH {
        return Err(BTreeError::TooDeep { root: pgno });
    }
    let mut found = Scrubbed::default();

    // The checksum first, then the parse. A page whose bytes are wrong can parse into a cell
    // count that sends the walk somewhere arbitrary, so checking afterwards would mean the
    // walk had already followed the corruption.
    let (children, dense) = {
        let page = pager.read(pgno)?;
        page.verify_checksum()?;
        found.pages += 1;
        match page.page_type()? {
            PageType::Branch => {
                (BranchPage::parse(&page)?.iter().map(|c| c.child).collect::<Vec<_>>(), Vec::new())
            }
            PageType::Leaf => {
                let leaf = LeafPage::parse(&page)?;
                let mut dense = Vec::new();
                for i in 0..leaf.len() {
                    let c = leaf.cell(i)?;
                    if c.has_base() {
                        dense.push((c.bitmap_pgno, c.bitmap_checksum));
                    }
                }
                (Vec::new(), dense)
            }
            other => return Err(BTreeError::UnexpectedPage { pgno, found: other }),
        }
    };

    for (d, expected) in dense {
        let page = pager.read(d)?;
        let computed = big_page::bitmap_page_checksum(&page);
        if computed != expected {
            return Err(BTreeError::Page(big_page::PageError::ChecksumMismatch {
                stored: expected,
                computed,
            }));
        }
        found.bitmaps += 1;
    }
    for c in children {
        found.add(scrub_rec(pager, c, depth + 1)?);
    }
    Ok(found)
}

/// Frees every page of a tree. The pages stay readable until no transaction can see them.
///
/// The page numbers are collected before any of them is freed, because the walk borrows the
/// transaction to read and `free` needs it mutably. That costs four bytes per page of the
/// tree, which for the largest fragment anyone will build is a few megabytes.
pub fn free_tree<P: PagerMut>(txn: &mut WriteTxn<'_, P>, root: Pgno) -> Result<()> {
    let mut pages = Vec::new();
    visit_tree(txn, root, &mut |pgno, _| {
        pages.push(pgno);
        Ok(())
    })?;
    for p in pages {
        txn.free(p);
    }
    Ok(())
}

/// Copies a tree into another transaction, returning its root there.
///
/// Every page gets a new number, so the destination owes the source nothing: this is what
/// makes a backup a real file rather than a reference, and what makes the result compact -
/// the pages are allocated in walk order into a store with an empty freelist, so a tree that
/// had grown holes comes out without them.
///
/// The source is only read. Nothing here can fail in a way that leaves it changed.
pub fn copy_tree<S: Pager, D: PagerMut>(
    src: &S,
    dst: &mut WriteTxn<'_, D>,
    root: Pgno,
) -> Result<Pgno> {
    copy_rec(src, dst, root, 0)
}

fn copy_rec<S: Pager, D: PagerMut>(
    src: &S,
    dst: &mut WriteTxn<'_, D>,
    pgno: Pgno,
    depth: usize,
) -> Result<Pgno> {
    if depth >= crate::read::MAX_DEPTH {
        return Err(BTreeError::TooDeep { root: pgno });
    }
    match read_node(src, pgno)? {
        Node::Branch(cells) => {
            let mut b = BranchBuilder::new();
            for c in cells {
                let child = copy_rec(src, dst, c.child, depth + 1)?;
                // The source page held these cells, and a cell's size does not depend on
                // which page its child lives on, so the rebuilt page cannot overflow.
                b.push(BranchCell { child, ..c })
                    .ok_or(BTreeError::UnexpectedPage { pgno, found: PageType::Branch })?;
            }
            let new = dst.alloc()?;
            let page = b.finish(new);
            dst.write(new, page)?;
            Ok(new)
        }
        Node::Leaf(mut items) => {
            for it in &mut items {
                if it.is_dense() {
                    it.bitmap_pgno = copy_bitmap(src, dst, it.bitmap_pgno, it.bitmap_checksum)?;
                }
            }
            let mut b = LeafBuilder::new();
            for it in &items {
                it.push_into(&mut b)
                    .ok_or(BTreeError::UnexpectedPage { pgno, found: PageType::Leaf })?;
            }
            let new = dst.alloc()?;
            let page = b.finish(new);
            dst.write(new, page)?;
            Ok(new)
        }
    }
}

/// A dense page is raw bitmap words: no header, no trailer, nothing that names its own page
/// number. Copying it is copying its bytes, and the checksum in the leaf cell above still
/// describes it exactly.
fn copy_bitmap<S: Pager, D: PagerMut>(
    src: &S,
    dst: &mut WriteTxn<'_, D>,
    pgno: Pgno,
    expected: u32,
) -> Result<Pgno> {
    let page = (*src.read(pgno)?).clone();
    // Same argument as `read_node`, and it matters more here: a dense page's checksum is not
    // recomputed on the way out - it travels in the leaf cell above it - so copying a rotted
    // one produces a destination whose leaf agrees with its own corruption.
    let computed = big_page::bitmap_page_checksum(&page);
    if computed != expected {
        return Err(BTreeError::Page(big_page::PageError::ChecksumMismatch {
            stored: expected,
            computed,
        }));
    }
    let new = dst.alloc()?;
    dst.write(new, page)?;
    Ok(new)
}

enum Node {
    Branch(Vec<BranchCell>),
    Leaf(Vec<LeafItem>),
}

/// Reads one node into owned values, so the source borrow ends before the destination is
/// touched.
fn read_node<S: Pager>(src: &S, pgno: Pgno) -> Result<Node> {
    let page = src.read(pgno)?;
    // **The one place a copy is allowed to be slower than a read.** Nothing on the query path
    // verifies a branch or leaf checksum, because a crc over 8 KiB would be the whole cost of
    // a point read. A copy is different in both directions: it is already reading every page
    // and writing every page, so the check is lost in the noise - and without it a backup
    // *launders* corruption. The destination is written through the ordinary commit path, so a
    // rotted page would be copied and then sealed with a fresh, valid checksum computed over
    // the rotted bytes, and every later scrub of the copy would call it intact.
    page.verify_checksum()?;
    match page.page_type()? {
        PageType::Branch => Ok(Node::Branch(BranchPage::parse(&page)?.iter().collect())),
        PageType::Leaf => {
            let leaf = LeafPage::parse(&page)?;
            let mut items = Vec::with_capacity(leaf.len());
            for i in 0..leaf.len() {
                items.push(LeafItem::from_cell(&leaf.cell(i)?));
            }
            Ok(Node::Leaf(items))
        }
        other => Err(BTreeError::UnexpectedPage { pgno, found: other }),
    }
}
