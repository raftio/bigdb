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

//! A page holding a flat list of fixed-size entries, chained through `next`. Used by the
//! freelist, the root records and the snapshot registry -- all three share this shape.

use crate::error::PageError;
use crate::layout::*;
use crate::page::*;

/// `next` sits right after the page header; entries start at 16, so they stay 8-aligned.
pub const CHAIN_NEXT: usize = PAGE_HEADER;
pub const CHAIN_DATA: usize = 16;

/// How many entries of the given stride fit on one page.
pub const fn chain_capacity(stride: usize) -> usize {
    (PAGE_SIZE - PAGE_TRAILER - CHAIN_DATA) / stride
}

const _: () = assert!(CHAIN_DATA.is_multiple_of(CELL_ALIGN));

#[derive(Clone, Copy)]
pub struct ChainPage<'a> {
    page: &'a Page,
    count: usize,
    stride: usize,
}

impl<'a> ChainPage<'a> {
    pub fn parse(page: &'a Page, expected: PageType, stride: usize) -> Result<Self, PageError> {
        let found = (page.flags() & 0xFF) as u8;
        if found != expected as u8 {
            return Err(PageError::TypeMismatch { expected: expected as u8, found });
        }
        let count = page.cell_count() as usize;
        if CHAIN_DATA + count * stride > PAGE_SIZE - PAGE_TRAILER {
            return Err(PageError::CellCountOverflow(page.cell_count()));
        }
        Ok(Self { page, count, stride })
    }

    pub fn len(&self) -> usize {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    pub fn next(&self) -> Option<Pgno> {
        let v = u32::from_le_bytes(self.page.0[CHAIN_NEXT..CHAIN_NEXT + 4].try_into().unwrap());
        (v != 0).then_some(v)
    }

    pub fn entry(&self, i: usize) -> Option<&'a [u8]> {
        (i < self.count).then(|| {
            let at = CHAIN_DATA + i * self.stride;
            &self.page.0[at..at + self.stride]
        })
    }

    pub fn entries(&self) -> impl Iterator<Item = &'a [u8]> + '_ {
        (0..self.count).map(move |i| self.entry(i).unwrap())
    }
}

/// Pack entries into a chain of pages. `pgnos` must hold enough pages for `entries`.
pub fn build_chain(
    ty: PageType,
    stride: usize,
    entries: &[Vec<u8>],
    pgnos: &[Pgno],
) -> Vec<(Pgno, Page)> {
    if pgnos.is_empty() {
        return Vec::new();
    }
    let cap = chain_capacity(stride);
    let mut out = Vec::new();
    // Exactly one page per pgno: a caller may hand over more pages than the entries need,
    // and a page left out of the chain while `next` still points at it would be garbage.
    let mut chunks: Vec<&[Vec<u8>]> =
        if entries.is_empty() { Vec::new() } else { entries.chunks(cap).collect() };
    while chunks.len() < pgnos.len() {
        chunks.push(&[]);
    }

    for (i, chunk) in chunks.iter().enumerate() {
        let mut page = Page::zeroed();
        page.set_header(pgnos[i], ty, chunk.len() as u16);
        let next = pgnos.get(i + 1).copied().unwrap_or(0);
        page.0[CHAIN_NEXT..CHAIN_NEXT + 4].copy_from_slice(&next.to_le_bytes());
        for (j, e) in chunk.iter().enumerate() {
            let at = CHAIN_DATA + j * stride;
            page.0[at..at + stride].copy_from_slice(e);
        }
        page.seal();
        out.push((pgnos[i], page));
    }
    out
}

/// Pages needed to hold `n` entries. An empty list costs no page at all.
pub fn chain_pages_needed(stride: usize, n: usize) -> usize {
    n.div_ceil(chain_capacity(stride))
}
