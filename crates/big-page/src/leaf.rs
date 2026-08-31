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

//! Leaf page: cell index + cell 24 byte header, payload align 8.

use crate::error::PageError;
use crate::layout::*;
use crate::page::*;
use big_container::{Container, ContainerRef, ContainerType, Interval, BITMAP_WORDS};

/// Field offsets inside the cell header. The one at 20 holds the bitmap page number,
/// which is why a BitmapPtr cell needs no payload at all.
mod off {
    pub const KEY: usize = 0;
    pub const TYPE: usize = 8;
    pub const ELEM_N: usize = 10;
    pub const CARDINALITY: usize = 12;
    pub const BITMAP_CRC: usize = 16;
    pub const BITMAP_PGNO: usize = 20;
}

#[derive(Clone, Copy)]
pub struct LeafPage<'a> {
    page: &'a Page,
    count: usize,
}

impl<'a> LeafPage<'a> {
    /// The cell index is fully validated here, which makes `key_at`/`search` below total.
    pub fn parse(page: &'a Page) -> Result<Self, PageError> {
        let (count, _) = validate_cell_index(page, PageType::Leaf, LEAF_CELL_HEADER)?;
        Ok(Self { page, count })
    }

    pub fn len(&self) -> usize {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    pub fn key_at(&self, i: usize) -> Option<ContainerKey> {
        (i < self.count).then(|| {
            let o = cell_offset(self.page, i) + off::KEY;
            u64::from_le_bytes(self.page.0[o..o + 8].try_into().unwrap())
        })
    }

    /// `Ok(i)` on a hit, `Err(i)` is the insertion point: same convention as `slice::binary_search`.
    pub fn search(&self, ckey: ContainerKey) -> Result<usize, usize> {
        let (mut lo, mut hi) = (0usize, self.count);
        while lo < hi {
            let mid = (lo + hi) / 2;
            match self.key_at(mid).unwrap().cmp(&ckey) {
                core::cmp::Ordering::Less => lo = mid + 1,
                core::cmp::Ordering::Greater => hi = mid,
                core::cmp::Ordering::Equal => return Ok(mid),
            }
        }
        Err(lo)
    }

    pub fn cell(&self, i: usize) -> Result<LeafCell<'a>, PageError> {
        if i >= self.count {
            return Err(PageError::CellOffsetOutOfRange { index: i, offset: 0 });
        }
        let base = cell_offset(self.page, i);
        let b = &self.page.0;
        let rd16 = |o: usize| u16::from_le_bytes(b[base + o..base + o + 2].try_into().unwrap());
        let rd32 = |o: usize| u32::from_le_bytes(b[base + o..base + o + 4].try_into().unwrap());

        let raw_type = rd16(off::TYPE);
        let ty =
            ContainerType::from_u16(raw_type).ok_or(PageError::UnknownContainerType(raw_type))?;
        let elem_n = rd16(off::ELEM_N) as usize;

        let payload_len = match ty {
            ContainerType::Array => elem_n * core::mem::size_of::<u16>(),
            ContainerType::Run => elem_n * core::mem::size_of::<Interval>(),
            ContainerType::BitmapPtr => 0,
            ContainerType::BitmapDelta => elem_n * big_container::DELTA_ENTRY_BYTES,
            // `elem_n` is a *byte* length for the values types, not a count of elements.
            // A block's encoding is chosen per block and its units are its own, so there is
            // no fixed width to multiply by - and the field is already the right size, since
            // an inline payload can never exceed half a page.
            ContainerType::ValuesInline | ContainerType::ValuesPtr => elem_n,
        };
        let start = base + LEAF_CELL_HEADER;
        let end = start + payload_len;
        if end > PAGE_SIZE - PAGE_TRAILER {
            return Err(PageError::PayloadOutOfRange { index: i });
        }

        Ok(LeafCell {
            key: u64::from_le_bytes(b[base..base + 8].try_into().unwrap()),
            ty,
            elem_n,
            cardinality: rd32(off::CARDINALITY),
            bitmap_checksum: rd32(off::BITMAP_CRC),
            bitmap_pgno: rd32(off::BITMAP_PGNO),
            payload: &b[start..end],
        })
    }

    pub fn iter(&self) -> impl Iterator<Item = Result<LeafCell<'a>, PageError>> + '_ {
        (0..self.count).map(move |i| self.cell(i))
    }

    /// Sums the cached cardinalities: O(cells), never touches a payload.
    pub fn total_cardinality(&self) -> Result<u64, PageError> {
        let mut sum = 0u64;
        for i in 0..self.count {
            sum += self.cell(i)?.cardinality as u64;
        }
        Ok(sum)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct LeafCell<'a> {
    pub key: ContainerKey,
    pub ty: ContainerType,
    pub elem_n: usize,
    pub cardinality: u32,
    pub bitmap_checksum: u32,
    pub bitmap_pgno: Pgno,
    pub payload: &'a [u8],
}

impl<'a> LeafCell<'a> {
    /// Checks the dense page against the checksum this cell carries, then borrows it. This is
    /// the one parent-to-child integrity link in the tree; a branch cell has no equivalent.
    ///
    /// Only for a cell with no delta. A `BitmapDelta` cell's container is not on the page - the
    /// page is its *base* - so borrowing it would hand back a container missing every change
    /// since it was last written whole. Use [`resolve`](LeafCell::resolve).
    pub fn bitmap<'p>(&self, page: &'p Page) -> Result<ContainerRef<'p>, PageError> {
        if self.ty != ContainerType::BitmapPtr {
            return Err(PageError::UnknownContainerType(self.ty as u16));
        }
        self.verify_base(page)?;
        bitmap_container(page)
    }

    /// The bits this cell's delta changes, decoded.
    ///
    /// Empty for every other type, so a caller that does not care which it has still gets the
    /// right answer.
    pub fn delta(&self) -> Result<Vec<big_container::DeltaEntry>, PageError> {
        if self.ty != ContainerType::BitmapDelta {
            return Ok(Vec::new());
        }
        big_container::delta::decode(self.payload).ok_or(PageError::PayloadOutOfRange { index: 0 })
    }

    /// The container this cell stands for, whatever shape it is stored in.
    ///
    /// `page` is the dense page for a `BitmapPtr` or `BitmapDelta` cell and is ignored otherwise.
    /// Owned rather than borrowed because a delta has to be applied somewhere, and the honest
    /// place is here: a caller that got a borrow for one type and a copy for another would have
    /// no way to reason about what a read costs.
    pub fn resolve(&self, page: Option<&Page>) -> Result<Container, PageError> {
        if let (Some(page), true) = (page, self.has_base()) {
            self.verify_base(page)?;
        }
        self.resolve_checked(page)
    }

    /// `resolve`, with the parent-to-child check already done by the caller. See
    /// [`contains_checked`](LeafCell::contains_checked) for why that split exists and what a
    /// caller owes in exchange.
    pub fn resolve_checked(&self, page: Option<&Page>) -> Result<Container, PageError> {
        match self.ty {
            ContainerType::Array | ContainerType::Run => {
                Ok(self.container()?.expect("inline types always have a container").to_owned())
            }
            ContainerType::BitmapPtr => {
                let page = page.ok_or(PageError::UnknownContainerType(self.ty as u16))?;
                Ok(bitmap_container(page)?.to_owned())
            }
            ContainerType::BitmapDelta => {
                let page = page.ok_or(PageError::UnknownContainerType(self.ty as u16))?;
                Ok(Container::Bitmap(big_container::delta::apply(
                    base_words(page)?,
                    &self.delta()?,
                )))
            }
            // A values block is not a set and has no container form. Refused by tag rather
            // than decoded, because its payload is a run of numbers and reading it as offsets
            // would answer with a plausible bitmap that is nonsense.
            ContainerType::ValuesInline | ContainerType::ValuesPtr => {
                Err(PageError::NotAContainer(self.ty as u16))
            }
        }
    }

    /// Borrows a `BitmapPtr` cell's page without the parent-to-child check. See
    /// [`contains_checked`](LeafCell::contains_checked).
    pub fn bitmap_checked<'p>(&self, page: &'p Page) -> Result<ContainerRef<'p>, PageError> {
        if self.ty != ContainerType::BitmapPtr {
            return Err(PageError::UnknownContainerType(self.ty as u16));
        }
        bitmap_container(page)
    }

    /// The mismatch error this cell would raise for `page`, for a caller whose cached
    /// verification said no and now needs to say why.
    pub fn checksum_error(&self, page: &Page) -> PageError {
        PageError::ChecksumMismatch {
            stored: self.bitmap_checksum,
            computed: bitmap_page_checksum(page),
        }
    }

    /// Whether `offset` is in this container, without materialising it.
    ///
    /// The point-read path, and the reason a delta does not make point reads slower: at most
    /// [`MAX_DELTA`](big_container::MAX_DELTA) sorted entries are searched before the base word
    /// is touched, and nothing is copied either way.
    pub fn contains(&self, page: Option<&Page>, offset: u16) -> Result<bool, PageError> {
        if let (Some(page), true) = (page, self.has_base()) {
            self.verify_base(page)?;
        }
        self.contains_checked(page, offset)
    }

    /// Whether this cell's container lives on a page of its own, so the parent-to-child
    /// checksum applies to it.
    pub fn has_base(&self) -> bool {
        self.ty.owns_page()
    }

    /// `contains`, with the parent-to-child check already done by the caller.
    ///
    /// Split out because that check is a CRC over the whole 8 KiB page while a point read
    /// touches one bit, and a bit-sliced read asks the question once per plane - so the check,
    /// not the lookup, was the entire cost of reading one integer. A caller that can remember
    /// which pages it has already verified should not pay it per probe, and remembering is
    /// sound: copy-on-write never rewrites a page in place, so a page that verified once
    /// verifies forever, until its number is recycled and written again.
    ///
    /// **Callers must verify.** [`bitmap_page_checksum`] against [`LeafCell::bitmap_checksum`]
    /// is the check; skipping it drops the only integrity link between a leaf and the dense
    /// page it points at.
    pub fn contains_checked(&self, page: Option<&Page>, offset: u16) -> Result<bool, PageError> {
        match self.ty {
            ContainerType::Array | ContainerType::Run => Ok(self
                .container()?
                .expect("inline types always have a container")
                .contains(offset)),
            ContainerType::BitmapPtr => {
                let page = page.ok_or(PageError::UnknownContainerType(self.ty as u16))?;
                Ok(bitmap_container(page)?.contains(offset))
            }
            ContainerType::BitmapDelta => {
                let page = page.ok_or(PageError::UnknownContainerType(self.ty as u16))?;
                Ok(big_container::delta::contains(base_words(page)?, &self.delta()?, offset))
            }
            ContainerType::ValuesInline | ContainerType::ValuesPtr => {
                Err(PageError::NotAContainer(self.ty as u16))
            }
        }
    }

    /// The parent-to-child integrity check, shared by both dense forms.
    ///
    /// A delta cell's checksum covers the **base page as written**, not the container the cell
    /// stands for - so it stays valid across every write that only touches the delta, which is
    /// the entire point of the delta.
    fn verify_base(&self, page: &Page) -> Result<(), PageError> {
        let computed = bitmap_page_checksum(page);
        if computed != self.bitmap_checksum {
            return Err(PageError::ChecksumMismatch { stored: self.bitmap_checksum, computed });
        }
        Ok(())
    }

    /// `None` when the container is dense: it lives on its own page, see `bitmap_container`.
    pub fn container(&self) -> Result<Option<ContainerRef<'a>>, PageError> {
        Ok(Some(match self.ty {
            ContainerType::Array => ContainerRef::Array(
                bytemuck::try_cast_slice(self.payload).map_err(|_| PageError::Misaligned)?,
            ),
            ContainerType::Run => ContainerRef::Run(
                bytemuck::try_cast_slice(self.payload).map_err(|_| PageError::Misaligned)?,
            ),
            ContainerType::BitmapPtr | ContainerType::BitmapDelta => return Ok(None),
            ContainerType::ValuesInline | ContainerType::ValuesPtr => {
                return Err(PageError::NotAContainer(self.ty as u16))
            }
        }))
    }
}

/// The raw words of a dense page, unchecked.
pub fn base_words(page: &Page) -> Result<&[u64; BITMAP_WORDS], PageError> {
    let words: &[u64] = bytemuck::try_cast_slice(&page.0).map_err(|_| PageError::Misaligned)?;
    words.try_into().map_err(|_| PageError::Misaligned)
}

/// A dense container borrows a whole page: all 8192 bytes are payload, so a bitmap page has
/// neither a header nor a trailer. Nothing on the page identifies it as one, and there is no
/// room for a checksum of its own -- both live in the parent cell instead.
pub fn bitmap_container(page: &Page) -> Result<ContainerRef<'_>, PageError> {
    let words: &[u64] = bytemuck::try_cast_slice(&page.0).map_err(|_| PageError::Misaligned)?;
    let arr: &[u64; BITMAP_WORDS] = words.try_into().map_err(|_| PageError::Misaligned)?;
    Ok(ContainerRef::Bitmap(arr))
}

/// crc32 over the whole page. Distinct from `Page::compute_checksum`, which skips the trailer
/// a bitmap page does not have.
pub fn bitmap_page_checksum(page: &Page) -> u32 {
    crc32fast::hash(&page.0)
}

pub fn build_bitmap_page(words: &[u64; BITMAP_WORDS]) -> Page {
    let mut page = Page::zeroed();
    page.0.copy_from_slice(bytemuck::cast_slice(words));
    page
}

/// Builds a leaf page. Keys must be pushed in ascending order.
pub struct LeafBuilder {
    cells: Vec<StagedCell>,
    payload_bytes: usize,
}

struct StagedCell {
    key: ContainerKey,
    ty: ContainerType,
    elem_n: u16,
    cardinality: u32,
    bitmap_checksum: u32,
    bitmap_pgno: Pgno,
    payload: Vec<u8>,
}

impl LeafBuilder {
    pub fn new() -> Self {
        Self { cells: Vec::new(), payload_bytes: 0 }
    }

    pub fn len(&self) -> usize {
        self.cells.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cells.is_empty()
    }

    /// The layout can only be fixed at `finish`, because the cell index grows with each cell.
    /// The last cell needs no trailing padding, so it enters the formula unrounded.
    fn fits(&self, extra_payload: usize) -> bool {
        let index = align_up(PAGE_HEADER + (self.cells.len() + 1) * CELL_INDEX_ENTRY);
        let body = self.payload_bytes + LEAF_CELL_HEADER + extra_payload;
        index + body <= PAGE_SIZE - PAGE_TRAILER
    }

    /// `None` means out of room; the caller has to split.
    /// Many parameters because this is the layout-level API; the usual door is `push_container`.
    #[allow(clippy::too_many_arguments)]
    pub fn push(
        &mut self,
        key: ContainerKey,
        ty: ContainerType,
        elem_n: u16,
        cardinality: u32,
        bitmap_checksum: u32,
        bitmap_pgno: Pgno,
        payload: &[u8],
    ) -> Option<()> {
        if !self.fits(payload.len()) {
            return None;
        }
        self.payload_bytes += align_up(LEAF_CELL_HEADER + payload.len());
        self.cells.push(StagedCell {
            key,
            ty,
            elem_n,
            cardinality,
            bitmap_checksum,
            bitmap_pgno,
            payload: payload.to_vec(),
        });
        Some(())
    }

    /// `None` when out of room, or when the container is dense: dense goes via `push_bitmap_ptr`.
    pub fn push_container(&mut self, key: ContainerKey, c: ContainerRef<'_>) -> Option<()> {
        let card = c.cardinality();
        match c {
            ContainerRef::Array(a) => self.push(
                key,
                ContainerType::Array,
                a.len() as u16,
                card,
                0,
                0,
                bytemuck::cast_slice(a),
            ),
            ContainerRef::Run(r) => self.push(
                key,
                ContainerType::Run,
                r.len() as u16,
                card,
                0,
                0,
                bytemuck::cast_slice(r),
            ),
            ContainerRef::Bitmap(_) => None,
        }
    }

    pub fn push_bitmap_ptr(
        &mut self,
        key: ContainerKey,
        cardinality: u32,
        pgno: Pgno,
        checksum: u32,
    ) -> Option<()> {
        self.push(key, ContainerType::BitmapPtr, 0, cardinality, checksum, pgno, &[])
    }

    /// Derives cardinality and checksum from the dense page itself, so the two cannot drift.
    pub fn push_dense(&mut self, key: ContainerKey, pgno: Pgno, page: &Page) -> Option<()> {
        let card = bitmap_container(page).ok()?.cardinality();
        self.push_bitmap_ptr(key, card, pgno, bitmap_page_checksum(page))
    }

    pub fn finish(self, pgno: Pgno) -> Page {
        let mut page = Page::zeroed();
        let count = self.cells.len();
        page.set_header(pgno, PageType::Leaf, count as u16);

        let mut cursor = align_up(PAGE_HEADER + count * CELL_INDEX_ENTRY);
        for (i, c) in self.cells.iter().enumerate() {
            let b = &mut page.0;
            b[cursor..cursor + 8].copy_from_slice(&c.key.to_le_bytes());
            let f = cursor + off::TYPE;
            b[f..f + 2].copy_from_slice(&(c.ty as u16).to_le_bytes());
            let f = cursor + off::ELEM_N;
            b[f..f + 2].copy_from_slice(&c.elem_n.to_le_bytes());
            let f = cursor + off::CARDINALITY;
            b[f..f + 4].copy_from_slice(&c.cardinality.to_le_bytes());
            let f = cursor + off::BITMAP_CRC;
            b[f..f + 4].copy_from_slice(&c.bitmap_checksum.to_le_bytes());
            let f = cursor + off::BITMAP_PGNO;
            b[f..f + 4].copy_from_slice(&c.bitmap_pgno.to_le_bytes());
            let payload_at = cursor + LEAF_CELL_HEADER;
            b[payload_at..payload_at + c.payload.len()].copy_from_slice(&c.payload);

            let at = PAGE_HEADER + i * CELL_INDEX_ENTRY;
            b[at..at + 2].copy_from_slice(&(cursor as u16).to_le_bytes());
            cursor = align_up(payload_at + c.payload.len());
        }
        page.seal();
        page
    }
}

impl Default for LeafBuilder {
    fn default() -> Self {
        Self::new()
    }
}
