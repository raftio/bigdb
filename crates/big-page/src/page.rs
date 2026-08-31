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

use crate::error::PageError;
use crate::layout::*;

pub type Pgno = u32;
pub type TxnId = u64;
pub type ContainerKey = u64;

/// One page of buffer. `align(8)` is mandatory: a bare `[u8; N]` has align 1 and payload
/// casts would fail.
#[derive(Clone)]
#[repr(C, align(8))]
pub struct Page(pub [u8; PAGE_SIZE]);

impl Page {
    pub fn zeroed() -> Self {
        Page([0u8; PAGE_SIZE])
    }

    pub fn as_bytes(&self) -> &[u8; PAGE_SIZE] {
        &self.0
    }

    pub fn as_bytes_mut(&mut self) -> &mut [u8; PAGE_SIZE] {
        &mut self.0
    }

    pub fn pgno(&self) -> Pgno {
        u32::from_le_bytes(self.0[0..4].try_into().unwrap())
    }

    pub fn flags(&self) -> u32 {
        u32::from_le_bytes(self.0[4..8].try_into().unwrap())
    }

    pub fn cell_count(&self) -> u16 {
        u16::from_le_bytes(self.0[8..10].try_into().unwrap())
    }

    pub fn page_type(&self) -> Result<PageType, PageError> {
        PageType::from_u8((self.flags() & 0xFF) as u8)
    }

    pub fn set_header(&mut self, pgno: Pgno, ty: PageType, cell_count: u16) {
        self.0[0..4].copy_from_slice(&pgno.to_le_bytes());
        self.0[4..8].copy_from_slice(&(ty as u32).to_le_bytes());
        self.0[8..10].copy_from_slice(&cell_count.to_le_bytes());
        self.0[10..12].copy_from_slice(&0u16.to_le_bytes());
    }

    /// crc32 over the whole page except the 4-byte trailer.
    pub fn compute_checksum(&self) -> u32 {
        crc32fast::hash(&self.0[..PAGE_SIZE - PAGE_TRAILER])
    }

    pub fn stored_checksum(&self) -> u32 {
        u32::from_le_bytes(self.0[PAGE_SIZE - PAGE_TRAILER..].try_into().unwrap())
    }

    pub fn seal(&mut self) {
        let c = self.compute_checksum();
        self.0[PAGE_SIZE - PAGE_TRAILER..].copy_from_slice(&c.to_le_bytes());
    }

    /// Kept out of `parse`: the hot path cannot always afford a crc on every single read.
    pub fn verify_checksum(&self) -> Result<(), PageError> {
        let (stored, computed) = (self.stored_checksum(), self.compute_checksum());
        if stored == computed {
            Ok(())
        } else {
            Err(PageError::ChecksumMismatch { stored, computed })
        }
    }
}

impl Default for Page {
    fn default() -> Self {
        Self::zeroed()
    }
}

impl core::fmt::Debug for Page {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Page")
            .field("pgno", &self.pgno())
            .field("type", &self.page_type())
            .field("cells", &self.cell_count())
            .finish()
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum PageType {
    Meta = 0,
    RootRecords = 1,
    Branch = 2,
    Leaf = 3,
    Bitmap = 4,
    Freelist = 5,
    Snapshots = 6,
    Catalog = 7,
}

impl PageType {
    pub fn from_u8(v: u8) -> Result<Self, PageError> {
        Ok(match v {
            0 => Self::Meta,
            1 => Self::RootRecords,
            2 => Self::Branch,
            3 => Self::Leaf,
            4 => Self::Bitmap,
            5 => Self::Freelist,
            6 => Self::Snapshots,
            7 => Self::Catalog,
            other => return Err(PageError::UnknownPageType(other)),
        })
    }
}

/// Shared validation for every page carrying a cell index; returns the data-area start.
pub(crate) fn validate_cell_index(
    page: &Page,
    expected: PageType,
    cell_span: usize,
) -> Result<(usize, usize), PageError> {
    let found = (page.flags() & 0xFF) as u8;
    if found != expected as u8 {
        return Err(PageError::TypeMismatch { expected: expected as u8, found });
    }
    let count = page.cell_count() as usize;
    let index_end = PAGE_HEADER + count * CELL_INDEX_ENTRY;
    if index_end > PAGE_SIZE - PAGE_TRAILER {
        return Err(PageError::CellCountOverflow(page.cell_count()));
    }
    let data_start = align_up(index_end);
    let limit = PAGE_SIZE - PAGE_TRAILER;

    let mut prev = 0usize;
    for i in 0..count {
        let at = PAGE_HEADER + i * CELL_INDEX_ENTRY;
        let off = u16::from_le_bytes(page.0[at..at + 2].try_into().unwrap());
        let off_usize = off as usize;
        if !off_usize.is_multiple_of(CELL_ALIGN) {
            return Err(PageError::CellMisaligned { index: i, offset: off });
        }
        if off_usize < data_start || off_usize + cell_span > limit {
            return Err(PageError::CellOffsetOutOfRange { index: i, offset: off });
        }
        if i > 0 && off_usize <= prev {
            return Err(PageError::CellOrderBroken { index: i });
        }
        prev = off_usize;
    }
    Ok((count, data_start))
}

pub(crate) fn cell_offset(page: &Page, i: usize) -> usize {
    let at = PAGE_HEADER + i * CELL_INDEX_ENTRY;
    u16::from_le_bytes(page.0[at..at + 2].try_into().unwrap()) as usize
}
