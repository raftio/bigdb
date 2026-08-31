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

//! Reading a segment: one block, or every block in order.

use crate::columnar::block::{Block, PartRef};
use crate::columnar::error::{ColumnError, Result};
use crate::columnar::{block_keys, block_of, block_of_key, slot_of, Cell};
use big_page::{ContainerKey, Pgno};
use big_pager::Pager;
use core::ops::ControlFlow;

/// A handle on one field's segment inside one shard.
#[derive(Clone, Copy)]
pub struct ColumnRead<'p, P: Pager> {
    pager: &'p P,
    root: Pgno,
}

/// One part's bytes, taken out of the tree so the page borrow can be dropped.
///
/// Owned rather than borrowed on purpose: a spilled part's payload is on a *different* page from
/// the cell that names it, and holding the leaf borrowed while reading that page would mean the
/// scan could not move on. The copy is one block's worth - at most a page - and it buys a
/// two-phase read that needs no lifetime gymnastics.
struct RawPart {
    key: ContainerKey,
    inline: Vec<u8>,
    page: Option<(Pgno, u32)>,
}

impl<'p, P: Pager> ColumnRead<'p, P> {
    pub fn new(pager: &'p P, root: Pgno) -> Self {
        Self { pager, root }
    }

    /// The block covering a record's local offset within its shard.
    pub fn block_for(&self, local: u64) -> Result<Block> {
        self.block(block_of(local))
    }

    /// One record's cell. The point read, and the reason a projection is cheap: it costs one
    /// descent and one block decode, whatever the field's width.
    pub fn cell(&self, local: u64) -> Result<Cell> {
        Ok(self.block_for(local)?.get(slot_of(local)).clone())
    }

    /// One whole block, decoded. An absent block reads back as a block of nulls, which is what
    /// it means.
    pub fn block(&self, block: u64) -> Result<Block> {
        let span = block_keys(block);
        let raw = self.collect(*span.start(), *span.end())?;
        if raw.is_empty() {
            return Ok(Block::new());
        }
        self.decode(&raw)
    }

    /// Every block this segment holds, ascending. The basis for a scan.
    pub fn blocks(&self) -> Result<Vec<u64>> {
        let mut out: Vec<u64> = Vec::new();
        big_btree::scan(self.pager, self.root, 0, ContainerKey::MAX, |c| {
            let block = block_of_key(c.key);
            if out.last() != Some(&block) {
                out.push(block);
            }
            ControlFlow::Continue(())
        })?;
        Ok(out)
    }

    /// Visits every block in ascending order.
    ///
    /// One pass over the tree rather than a descent per block, which is what makes a scan cost
    /// the segment's size instead of its size times its depth.
    pub fn for_each_block(
        &self,
        mut f: impl FnMut(u64, Block) -> Result<ControlFlow<()>>,
    ) -> Result<()> {
        let mut pending: Vec<RawPart> = Vec::new();
        let mut current: Option<u64> = None;
        let mut stop = false;

        // Collected whole, then decoded. `scan` holds a page borrowed for the length of its
        // callback, and decoding a spilled part has to read a different page.
        let mut all = Vec::new();
        big_btree::scan(self.pager, self.root, 0, ContainerKey::MAX, |c| {
            all.push(RawPart {
                key: c.key,
                inline: c.payload.to_vec(),
                page: c.ty.owns_page().then_some((c.bitmap_pgno, c.bitmap_checksum)),
            });
            ControlFlow::Continue(())
        })?;

        for part in all {
            let block = block_of_key(part.key);
            if current != Some(block) {
                if let Some(done) = current.take() {
                    if f(done, self.decode(&pending)?)?.is_break() {
                        stop = true;
                        break;
                    }
                }
                pending.clear();
                current = Some(block);
            }
            pending.push(part);
        }
        if !stop {
            if let Some(done) = current {
                // The last block's answer has nowhere to break out of, so the flow it returns
                // is deliberately dropped rather than checked.
                let _ = f(done, self.decode(&pending)?)?;
            }
        }
        Ok(())
    }

    /// How many records in this segment hold a value.
    ///
    /// Read off the cached cardinality in each leaf cell, so it costs a walk of the leaves and
    /// touches no payload at all - the same trick `count_all` plays on the exists row. Only the
    /// first part of a block carries the count; the value parts of a list block would count
    /// their values rather than their records.
    pub fn count(&self) -> Result<u64> {
        let mut total = 0u64;
        big_btree::scan(self.pager, self.root, 0, ContainerKey::MAX, |c| {
            if crate::columnar::part_of_key(c.key) == 0 {
                total += c.cardinality as u64;
            }
            ControlFlow::Continue(())
        })?;
        Ok(total)
    }

    fn collect(&self, lo: ContainerKey, hi: ContainerKey) -> Result<Vec<RawPart>> {
        let mut out = Vec::new();
        big_btree::scan(self.pager, self.root, lo, hi, |c| {
            out.push(RawPart {
                key: c.key,
                inline: c.payload.to_vec(),
                page: c.ty.owns_page().then_some((c.bitmap_pgno, c.bitmap_checksum)),
            });
            ControlFlow::Continue(())
        })?;
        Ok(out)
    }

    /// Resolves each part's payload - from the cell or from its page - and decodes the block.
    ///
    /// The page is verified against the checksum the cell carries. That link is the only
    /// integrity check between a leaf and a page it points at, and skipping it here would make
    /// a rotted values page decode into numbers that look entirely reasonable.
    fn decode(&self, parts: &[RawPart]) -> Result<Block> {
        if parts.is_empty() {
            return Ok(Block::new());
        }
        let mut bodies: Vec<Vec<u8>> = Vec::with_capacity(parts.len());
        for part in parts {
            match part.page {
                None => bodies.push(Vec::new()),
                Some((pgno, expected)) => {
                    let page = self.pager.read(pgno).map_err(big_btree::BTreeError::from)?;
                    let computed = big_page::bitmap_page_checksum(&page);
                    if computed != expected {
                        return Err(ColumnError::Page(big_page::PageError::ChecksumMismatch {
                            stored: expected,
                            computed,
                        }));
                    }
                    bodies.push(page.0.to_vec());
                }
            }
        }
        let refs: Vec<PartRef<'_>> = parts
            .iter()
            .zip(&bodies)
            .map(|(p, body)| PartRef { header: &p.inline, body })
            .collect();
        Block::decode(&refs)
    }
}
