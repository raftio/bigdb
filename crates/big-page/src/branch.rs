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

//! Branch page: fixed 16-byte cells of `(ckey, flags, pgno)`.

use crate::error::PageError;
use crate::layout::*;
use crate::page::*;

#[derive(Clone, Copy)]
pub struct BranchPage<'a> {
    page: &'a Page,
    count: usize,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct BranchCell {
    pub key: ContainerKey,
    pub flags: u32,
    pub child: Pgno,
}

impl<'a> BranchPage<'a> {
    pub fn parse(page: &'a Page) -> Result<Self, PageError> {
        let (count, _) = validate_cell_index(page, PageType::Branch, BRANCH_CELL)?;
        Ok(Self { page, count })
    }

    pub fn len(&self) -> usize {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    pub fn cell(&self, i: usize) -> Option<BranchCell> {
        (i < self.count).then(|| {
            let o = cell_offset(self.page, i);
            let b = &self.page.0;
            BranchCell {
                key: u64::from_le_bytes(b[o..o + 8].try_into().unwrap()),
                flags: u32::from_le_bytes(b[o + 8..o + 12].try_into().unwrap()),
                child: u32::from_le_bytes(b[o + 12..o + 16].try_into().unwrap()),
            }
        })
    }

    /// The child covering `ckey` is the last cell whose key is <= ckey.
    pub fn child_for(&self, ckey: ContainerKey) -> Option<Pgno> {
        if self.count == 0 {
            return None;
        }
        let (mut lo, mut hi) = (0usize, self.count);
        while lo < hi {
            let mid = (lo + hi) / 2;
            if self.cell(mid).unwrap().key <= ckey {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        (lo > 0).then(|| self.cell(lo - 1).unwrap().child)
    }

    pub fn iter(&self) -> impl Iterator<Item = BranchCell> + '_ {
        (0..self.count).map(move |i| self.cell(i).unwrap())
    }
}

pub struct BranchBuilder {
    cells: Vec<BranchCell>,
}

impl BranchBuilder {
    pub fn new() -> Self {
        Self { cells: Vec::new() }
    }

    pub fn len(&self) -> usize {
        self.cells.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cells.is_empty()
    }

    /// Max cells per branch page, derived from the layout rather than hardcoded.
    pub const CAPACITY: usize = {
        let mut n = 0;
        while align_up(PAGE_HEADER + (n + 1) * CELL_INDEX_ENTRY) + (n + 1) * BRANCH_CELL
            <= PAGE_SIZE - PAGE_TRAILER
        {
            n += 1;
        }
        n
    };

    pub fn push(&mut self, cell: BranchCell) -> Option<()> {
        (self.cells.len() < Self::CAPACITY).then(|| self.cells.push(cell))
    }

    pub fn finish(self, pgno: Pgno) -> Page {
        let mut page = Page::zeroed();
        let count = self.cells.len();
        page.set_header(pgno, PageType::Branch, count as u16);

        let mut cursor = align_up(PAGE_HEADER + count * CELL_INDEX_ENTRY);
        for (i, c) in self.cells.iter().enumerate() {
            let b = &mut page.0;
            b[cursor..cursor + 8].copy_from_slice(&c.key.to_le_bytes());
            b[cursor + 8..cursor + 12].copy_from_slice(&c.flags.to_le_bytes());
            b[cursor + 12..cursor + 16].copy_from_slice(&c.child.to_le_bytes());
            let at = PAGE_HEADER + i * CELL_INDEX_ENTRY;
            b[at..at + 2].copy_from_slice(&(cursor as u16).to_le_bytes());
            cursor += BRANCH_CELL;
        }
        page.seal();
        page
    }
}

impl Default for BranchBuilder {
    fn default() -> Self {
        Self::new()
    }
}
