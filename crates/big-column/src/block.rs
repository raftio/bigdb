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

//! One block: its header, its nulls, and how it splits into cells.
//!
//! # The two shapes
//!
//! A **scalar** block holds at most one value per record - every field kind but the keyed ones.
//! It is always exactly one cell, because [`crate::BLOCK_RECORDS`] values at the full width is
//! one page and [`crate::codec`] never encodes larger than that.
//!
//! A **list** block holds any number per record, which is what a set field is. It cannot be
//! bounded the same way, so it is one cell of *counts* followed by as many cells of values as it
//! takes. Reading one record's list is: decode the counts, sum the ones before it, and read that
//! window out of the value cells.
//!
//! # Where a null lives
//!
//! In a bitmap of [`crate::BLOCK_RECORDS`] bits at a fixed place in every header, and never in
//! the values. That is what lets the codecs work on a dense run of real values with no sentinel
//! and no widening - a column of `u64` has no spare value to mean absent, and picking one would
//! make some legitimate number unstorable.
//!
//! The bitmap costs [`NULL_BYTES`] whatever the block holds, which is the one place this format
//! spends space it might not need. It is bounded, it is the same for every block, and record ids
//! are dense inside a shard by construction - so a block that exists is usually a block that is
//! mostly full.

use crate::codec::{self, Codec, Encoded};
use crate::error::{ColumnError, Result};
use crate::{BLOCK_RECORDS, INLINE_MAX, MAX_PARTS, PAGE_BYTES};

/// Bytes of null bitmap: one bit per record in the block.
pub const NULL_BYTES: usize = (BLOCK_RECORDS as usize).div_ceil(8);

/// Header of a part that carries a null bitmap - the scalar block, and a list block's counts.
pub const HEADER_BYTES: usize = 16 + NULL_BYTES;
/// Header of a list block's value parts, which have no nulls of their own.
pub const CONT_HEADER_BYTES: usize = 16;

/// Values in one part of a list block's value run.
///
/// The same 1024 a block holds records, and for the same reason: at the full width that is one
/// page exactly, so a value part never needs a second one either.
pub const VALUES_PER_PART: usize = BLOCK_RECORDS as usize;

/// What a part claims to be. Part of the on-disk format.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Shape {
    /// At most one value per record, with a null bitmap.
    Scalar = 0,
    /// The counts of a list block: how many values each record holds.
    Counts = 1,
    /// A run of a list block's values, with no nulls of its own.
    Values = 2,
}

impl Shape {
    pub fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            0 => Self::Scalar,
            1 => Self::Counts,
            2 => Self::Values,
            _ => return None,
        })
    }
}

/// What one record holds in one column.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub enum Cell {
    /// The record holds nothing here. Not zero, and not an empty list: the same absence a `min`
    /// over nothing answers with.
    #[default]
    Null,
    /// A scalar column's value, already in the form storage keeps it - a signed field's bias is
    /// applied above this layer, exactly as it is for a bit-sliced index.
    Value(u64),
    /// A keyed column's row ids, ascending and deduplicated.
    List(Vec<u64>),
}

impl Cell {
    pub fn is_null(&self) -> bool {
        match self {
            Self::Null => true,
            Self::Value(_) => false,
            Self::List(v) => v.is_empty(),
        }
    }

    /// The single value, for a scalar column. `None` for absent and for a list.
    pub fn value(&self) -> Option<u64> {
        match self {
            Self::Value(v) => Some(*v),
            _ => None,
        }
    }

    /// The row ids, for a keyed column. Empty for absent and for a scalar.
    pub fn list(&self) -> &[u64] {
        match self {
            Self::List(v) => v,
            _ => &[],
        }
    }
}

/// One block, decoded: exactly [`BLOCK_RECORDS`] slots in record order.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Block {
    slots: Vec<Cell>,
}

impl Default for Block {
    fn default() -> Self {
        Self::new()
    }
}

impl Block {
    pub fn new() -> Self {
        Self { slots: vec![Cell::Null; BLOCK_RECORDS as usize] }
    }

    pub fn get(&self, slot: usize) -> &Cell {
        self.slots.get(slot).unwrap_or(&Cell::Null)
    }

    /// Replaces one slot. A write is a replace and never a merge, because the caller has
    /// already decided what the record holds now.
    ///
    /// An empty list is stored as [`Cell::Null`]. They are the same absence - the encoder emits
    /// nothing for either - and leaving both spellings in the type would mean a block had two
    /// representations of one value, so a decoded block could never be compared against the one
    /// that was written.
    pub fn set(&mut self, slot: usize, cell: Cell) {
        if slot < self.slots.len() {
            self.slots[slot] = if cell.is_null() { Cell::Null } else { cell };
        }
    }

    pub fn slots(&self) -> &[Cell] {
        &self.slots
    }

    /// How many slots hold anything. This is what a cell's `cardinality` records, which is why
    /// `count_values` over a segment costs a descent and no payload at all.
    pub fn present(&self) -> u32 {
        self.slots.iter().filter(|c| !c.is_null()).count() as u32
    }

    pub fn is_empty(&self) -> bool {
        self.slots.iter().all(Cell::is_null)
    }

    /// Whether any slot holds more than one value, which is what decides the shape.
    fn is_list(&self) -> bool {
        self.slots.iter().any(|c| matches!(c, Cell::List(_)))
    }

    /// Encodes into the parts that become cells, in ascending part order.
    ///
    /// Empty when the block holds nothing: an absent block and a block of nulls are the same
    /// thing, and storing the second would be storing a header to say so.
    pub fn encode(&self) -> Result<Vec<Part>> {
        if self.is_empty() {
            return Ok(Vec::new());
        }
        if self.is_list() {
            self.encode_list()
        } else {
            Ok(vec![self.encode_scalar()])
        }
    }

    fn encode_scalar(&self) -> Part {
        let mut nulls = [0u8; NULL_BYTES];
        let mut values = Vec::with_capacity(self.slots.len());
        for (i, cell) in self.slots.iter().enumerate() {
            if let Some(v) = cell.value() {
                nulls[i / 8] |= 1 << (i % 8);
                values.push(v);
            }
        }
        Part::with_nulls(Shape::Scalar, &nulls, &codec::encode(&values), values.len())
    }

    fn encode_list(&self) -> Result<Vec<Part>> {
        let mut nulls = [0u8; NULL_BYTES];
        let mut counts = Vec::new();
        let mut flat = Vec::new();
        for (i, cell) in self.slots.iter().enumerate() {
            let list = cell.list();
            if list.is_empty() {
                continue;
            }
            nulls[i / 8] |= 1 << (i % 8);
            counts.push(list.len() as u64);
            flat.extend_from_slice(list);
        }

        let mut parts =
            vec![Part::with_nulls(Shape::Counts, &nulls, &codec::encode(&counts), counts.len())];
        for chunk in flat.chunks(VALUES_PER_PART) {
            parts.push(Part::plain(Shape::Values, &codec::encode(chunk), chunk.len()));
        }
        if parts.len() as u64 > MAX_PARTS {
            return Err(ColumnError::TooManyParts { block: 0 });
        }
        Ok(parts)
    }

    /// Rebuilds a block from the parts of one block, in ascending part order.
    pub fn decode(parts: &[PartRef<'_>]) -> Result<Self> {
        let mut block = Self::new();
        let Some(first) = parts.first() else { return Ok(block) };
        let head = Header::parse(first.header)?;

        match head.shape {
            Shape::Scalar => {
                let mut values = Vec::new();
                codec::decode(&head.encoded(first.body), head.n, &mut values)?;
                for (next, slot) in head.present_slots().enumerate() {
                    let Some(v) = values.get(next) else {
                        return Err(ColumnError::Truncated { need: next + 1, have: values.len() });
                    };
                    block.slots[slot] = Cell::Value(*v);
                }
            }
            Shape::Counts => {
                let mut counts = Vec::new();
                codec::decode(&head.encoded(first.body), head.n, &mut counts)?;

                // Every value part, concatenated. A list block is read whole because its parts
                // carry no record boundary of their own - the counts are the boundary, and they
                // are all in the first part.
                let mut flat = Vec::new();
                for part in &parts[1..] {
                    let h = Header::parse(part.header)?;
                    if h.shape != Shape::Values {
                        return Err(ColumnError::UnknownShape(h.shape as u8));
                    }
                    let mut chunk = Vec::new();
                    codec::decode(&h.encoded(part.body), h.n, &mut chunk)?;
                    flat.extend_from_slice(&chunk);
                }

                // Checked, because `count` is a number off the disk and nothing above has
                // bounded it. An unchecked `at + count` overflows on a corrupt block - and in a
                // release build it wraps to a small `end`, slips past the length test, and
                // panics on a backwards slice. A decode of stored bytes must refuse, never
                // panic; that is the same contract `big-page`'s parser holds.
                let mut at = 0usize;
                for (slot, count) in head.present_slots().zip(counts.iter()) {
                    let end = usize::try_from(*count)
                        .ok()
                        .and_then(|n| at.checked_add(n))
                        .filter(|end| *end <= flat.len())
                        .ok_or(ColumnError::Truncated { need: flat.len() + 1, have: flat.len() })?;
                    block.slots[slot] = Cell::List(flat[at..end].to_vec());
                    at = end;
                }
            }
            // A block never begins with a value part. One that does is a segment whose first
            // cell has been lost, and every record in it would read back short.
            Shape::Values => return Err(ColumnError::UnknownShape(Shape::Values as u8)),
        }
        Ok(block)
    }
}

/// One encoded part, ready to become a leaf cell.
///
/// `inline` is what goes in the cell and `page` is what goes on a page of its own, exactly as a
/// dense container splits. Which of the two a part uses is decided by size and nothing else.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Part {
    /// The bytes that live in the leaf cell: always the header, plus the payload when it fits.
    pub inline: Vec<u8>,
    /// The payload, when it did not fit. Padded to a whole page, because a dense page is raw
    /// bytes and its checksum covers all of them.
    pub page: Option<Vec<u8>>,
    /// Slots holding a value, for the cell's cached cardinality.
    pub present: u32,
}

impl Part {
    fn with_nulls(shape: Shape, nulls: &[u8; NULL_BYTES], e: &Encoded, n: usize) -> Self {
        let mut header = Self::header(shape, e, n);
        header.extend_from_slice(nulls);
        Self::split(header, &e.bytes, n as u32)
    }

    fn plain(shape: Shape, e: &Encoded, n: usize) -> Self {
        Self::split(Self::header(shape, e, n), &e.bytes, n as u32)
    }

    fn header(shape: Shape, e: &Encoded, n: usize) -> Vec<u8> {
        let mut h = Vec::with_capacity(HEADER_BYTES);
        h.push(shape as u8);
        h.push(e.codec as u8);
        h.extend_from_slice(&(n as u16).to_le_bytes());
        h.push(e.width);
        h.extend_from_slice(&[0u8; 3]);
        h.extend_from_slice(&e.base.to_le_bytes());
        h
    }

    /// Decides whether the payload rides in the cell or gets a page.
    fn split(header: Vec<u8>, payload: &[u8], present: u32) -> Self {
        if header.len() + payload.len() <= INLINE_MAX {
            let mut inline = header;
            inline.extend_from_slice(payload);
            return Self { inline, page: None, present };
        }
        let mut page = vec![0u8; PAGE_BYTES];
        page[..payload.len()].copy_from_slice(payload);
        Self { inline: header, page: Some(page), present }
    }
}

/// One part as it is read back: the cell's bytes, and the page's when it has one.
#[derive(Clone, Copy, Debug)]
pub struct PartRef<'a> {
    pub header: &'a [u8],
    pub body: &'a [u8],
}

/// A part's header, parsed.
struct Header<'a> {
    shape: Shape,
    codec: Codec,
    n: usize,
    width: u8,
    base: u64,
    /// Present when the shape carries one; empty for a value part.
    nulls: &'a [u8],
    /// The payload bytes that followed the header inside the cell, which is where it is when
    /// the part was small enough to stay inline.
    inline_payload: &'a [u8],
}

impl<'a> Header<'a> {
    fn parse(bytes: &'a [u8]) -> Result<Self> {
        if bytes.len() < CONT_HEADER_BYTES {
            return Err(ColumnError::Truncated { need: CONT_HEADER_BYTES, have: bytes.len() });
        }
        let shape = Shape::from_u8(bytes[0]).ok_or(ColumnError::UnknownShape(bytes[0]))?;
        let codec = Codec::from_u8(bytes[1]).ok_or(ColumnError::UnknownCodec(bytes[1]))?;
        let n = u16::from_le_bytes(bytes[2..4].try_into().expect("two bytes")) as usize;
        let width = bytes[4];
        if width > 64 {
            return Err(ColumnError::BadWidth(width));
        }
        let base = u64::from_le_bytes(bytes[8..16].try_into().expect("eight bytes"));

        let (nulls, rest) = match shape {
            Shape::Values => (&bytes[0..0], &bytes[CONT_HEADER_BYTES..]),
            Shape::Scalar | Shape::Counts => {
                if bytes.len() < HEADER_BYTES {
                    return Err(ColumnError::Truncated { need: HEADER_BYTES, have: bytes.len() });
                }
                (&bytes[CONT_HEADER_BYTES..HEADER_BYTES], &bytes[HEADER_BYTES..])
            }
        };
        Ok(Self { shape, codec, n, width, base, nulls, inline_payload: rest })
    }

    /// The payload, wherever it turned out to be. A part that spilled has an empty tail in the
    /// cell and its bytes on the page; one that did not has them in the cell and no page.
    fn encoded(&self, page: &'a [u8]) -> Encoded {
        let bytes = if self.inline_payload.is_empty() { page } else { self.inline_payload };
        Encoded { codec: self.codec, base: self.base, width: self.width, bytes: bytes.to_vec() }
    }

    /// The slots this part's null bitmap marks present, ascending.
    fn present_slots(&self) -> impl Iterator<Item = usize> + '_ {
        (0..BLOCK_RECORDS as usize)
            .filter(move |i| self.nulls.get(i / 8).is_some_and(|b| b >> (i % 8) & 1 == 1))
    }
}
