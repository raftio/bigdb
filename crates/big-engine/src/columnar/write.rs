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

//! Writing a segment.
//!
//! A block is the unit, exactly as a container is the unit for a fragment. Writing one record
//! means reading its block, replacing one slot and writing the block back - so a write that
//! reaches storage per record would re-encode a thousand values for one of them. The buffering
//! that makes that affordable lives one layer up, in `big_db`, for the same reason the
//! fragment layer's does.

use crate::columnar::block::Block;
use crate::columnar::error::{ColumnError, Result};
use crate::columnar::{block_keys, key_of, MAX_PARTS};
use big_btree::LeafItem;
use big_page::{ContainerKey, ContainerType, Pgno};
use big_pager::{PagerMut, WriteTxn};
use core::ops::ControlFlow;

/// A handle on one field's segment inside one shard, within a transaction.
///
/// Holds no borrow of the transaction, for the reason [`crate::bitmap::FragmentWrite`] holds
/// none: one batch touches a table's fragments *and* its segments in a single commit, and a
/// handle that owned `&mut WriteTxn` would make the second one impossible to create.
#[derive(Clone, Copy, Debug)]
pub struct ColumnWrite {
    root: Option<Pgno>,
}

impl ColumnWrite {
    pub fn new(root: Option<Pgno>) -> Self {
        Self { root }
    }

    pub fn root(&self) -> Option<Pgno> {
        self.root
    }

    /// A read view over the transaction, so blocks written earlier in the same batch are
    /// visible. `None` until the segment has a root at all.
    pub fn reader<'t, P>(
        &self,
        txn: &'t WriteTxn<'_, P>,
    ) -> Option<crate::columnar::read::ColumnRead<'t, WriteTxn<'t, P>>>
    where
        P: PagerMut + 't,
    {
        self.root.map(|r| crate::columnar::read::ColumnRead::new(txn, r))
    }

    /// Replaces a block outright.
    ///
    /// A replace and not a merge: the caller has already decided what every slot in the block
    /// holds. Parts the new encoding does not need are removed, which is what stops a block that
    /// shrank from leaving stale value cells behind - and a stale value cell is not dead weight,
    /// it is extra values that the next read would splice into some record's list.
    pub fn write_block<P: PagerMut>(
        &mut self,
        txn: &mut WriteTxn<'_, P>,
        block: u64,
        value: &Block,
    ) -> Result<()> {
        let items = block_items(txn, 0, block, value)?;

        let existing = self.parts_at(txn, block)?;
        for part in existing.into_iter().skip(items.len()) {
            self.remove_key(txn, part)?;
        }
        if items.is_empty() {
            self.collapse_if_empty(txn)?;
            return Ok(());
        }

        self.root = Some(big_btree::put_many(txn, self.root, items)?);
        Ok(())
    }

    /// Reads a block, applies `edit`, and writes it back if anything changed.
    ///
    /// The read-modify-write a single-record change needs, in one place so that no caller has to
    /// remember that a block must be read before it is written. A caller changing many records in
    /// one block should still edit once rather than call this per record.
    pub fn edit_block<P: PagerMut>(
        &mut self,
        txn: &mut WriteTxn<'_, P>,
        block: u64,
        edit: impl FnOnce(&mut Block),
    ) -> Result<()> {
        let mut value = match self.reader(txn) {
            Some(r) => r.block(block)?,
            None => Block::new(),
        };
        let before = value.clone();
        edit(&mut value);
        if value == before {
            return Ok(());
        }
        self.write_block(txn, block, &value)
    }

    /// The keys one block currently occupies, ascending.
    fn parts_at<P: PagerMut>(
        &self,
        txn: &WriteTxn<'_, P>,
        block: u64,
    ) -> Result<Vec<ContainerKey>> {
        let Some(root) = self.root else { return Ok(Vec::new()) };
        let span = block_keys(block);
        let mut out = Vec::new();
        big_btree::scan(txn, root, *span.start(), *span.end(), |c| {
            out.push(c.key);
            ControlFlow::Continue(())
        })?;
        Ok(out)
    }

    fn remove_key<P: PagerMut>(
        &mut self,
        txn: &mut WriteTxn<'_, P>,
        key: ContainerKey,
    ) -> Result<()> {
        if let Some(root) = self.root {
            self.root = Some(big_btree::remove(txn, root, key)?);
        }
        Ok(())
    }

    /// Frees the tree once its last block is gone, so the segment has no root at all.
    ///
    /// The same rule `FragmentWrite::collapse_if_empty` follows, and it exists for the same
    /// reason: `remove` always leaves a root page standing, and a segment whose every block has
    /// been cleared is not a small tree, it is an absent one. Leaving the empty leaf would hold
    /// a page and a root record that no read can reach and no reclaim can take.
    fn collapse_if_empty<P: PagerMut>(&mut self, txn: &mut WriteTxn<'_, P>) -> Result<()> {
        let Some(root) = self.root else { return Ok(()) };
        let empty = {
            let page = txn.read(root).map_err(big_btree::BTreeError::from)?;
            page.page_type().map_err(big_btree::BTreeError::from)? == big_page::PageType::Leaf
                && page.cell_count() == 0
        };
        if empty {
            big_btree::free_tree(txn, root)?;
            self.root = None;
        }
        Ok(())
    }
}

/// One block, encoded into the leaf items a tree stores it as.
///
/// Free rather than a method, and taking `base`, because a block lives in two places: a segment
/// of its own, where `base` is zero, and inside a [`crate::part`], where every field of a shard
/// shares one tree and a field's keys start at its own offset. The encoding, the spill decision
/// and the page arrangement are the same in both, so they are written once.
///
/// Empty when the block holds nothing: a block of nulls encodes to no parts at all, and the
/// caller decides whether that means "remove what was there" or "write nothing".
pub fn block_items<P: PagerMut>(
    txn: &mut WriteTxn<'_, P>,
    base: ContainerKey,
    block: u64,
    value: &Block,
) -> Result<Vec<LeafItem>> {
    let parts = value.encode().map_err(|e| match e {
        // `Block` cannot know its own index, so it names zero and this fills it in.
        ColumnError::TooManyParts { .. } => ColumnError::TooManyParts { block },
        other => other,
    })?;
    if parts.len() as u64 > MAX_PARTS {
        return Err(ColumnError::TooManyParts { block });
    }

    let mut items = Vec::with_capacity(parts.len());
    for (n, part) in parts.iter().enumerate() {
        let key = base + key_of(block, n as u64);
        items.push(match &part.page {
            None => LeafItem {
                key,
                ty: ContainerType::ValuesInline,
                // `elem_n` is a byte length for the values types, not a count of elements.
                elem_n: part.inline.len() as u16,
                cardinality: part.present,
                bitmap_checksum: 0,
                bitmap_pgno: 0,
                payload: part.inline.clone(),
            },
            Some(bytes) => {
                // The same arrangement a dense container uses: raw bytes on a page of their own,
                // with the checksum in the cell above them. That is what lets the free walk, the
                // scrub and the backup copy handle a values page already.
                let page = big_page::Page(bytes.as_slice().try_into().map_err(|_| {
                    ColumnError::Truncated { need: crate::columnar::PAGE_BYTES, have: bytes.len() }
                })?);
                let checksum = big_page::bitmap_page_checksum(&page);
                let pgno = txn.alloc().map_err(big_btree::BTreeError::from)?;
                txn.write(pgno, page).map_err(big_btree::BTreeError::from)?;
                LeafItem {
                    key,
                    ty: ContainerType::ValuesPtr,
                    elem_n: part.inline.len() as u16,
                    cardinality: part.present,
                    bitmap_checksum: checksum,
                    bitmap_pgno: pgno,
                    payload: part.inline.clone(),
                }
            }
        });
    }
    Ok(items)
}
