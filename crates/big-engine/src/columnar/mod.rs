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

//! The columnar engine: a field's values, in record order, encoded a block at a time.
//!
//! The other half of what a table can store. A fragment answers *which records* by holding one
//! bit per record per row; a segment answers *what a record holds* by keeping the values
//! themselves. Neither can cheaply do the other's job, which is why a table may carry both.
//!
//! # What a segment is
//!
//! One b-tree, addressed by the same [`FragmentKey`] as the field's bitmaps and differing only
//! in the view - see `big_db::db::COLUMN_VIEW`. That is deliberate and it is most of the
//! design: root records, the backup walk, `drop_table`, `drop_field` and the cluster's fragment
//! addressing all speak `FragmentKey`, and every one of them reaches a segment without being
//! taught what one is.
//!
//! # Why a block, and why this size
//!
//! A block is [`BLOCK_RECORDS`] consecutive records. The number is not arbitrary: 1024 values at
//! the full 64-bit width is 8192 bytes, which is exactly one page. So the worst case a scalar
//! block can reach is one page and never two, and the common cases - a boolean, a low-cardinality
//! key, an integer whose range is narrow - encode small enough to sit inside the leaf cell with
//! no page at all. A block that fits the cell costs nothing to reach: the descent that found it
//! already read it.
//!
//! The b-tree is the mark index. A column store normally needs a side structure mapping block
//! number to file offset; here the tree already maps a key to a cell, so what other engines
//! build, this one gets from the tree it already had.
//!
//! # Parts, and why a block is not one cell
//!
//! A scalar block is one cell. A **list** block - a set field, where one record may hold many
//! values - has no bound of that kind, so it spills into further cells at the same block under
//! an increasing `part`. The key is `(block << PART_BITS) | part`, which makes a block a
//! *contiguous span of keys* and reading one a range scan.
//!
//! That is the same shape `crate::base::coords::row_ckeys` already uses for a row, and it is chosen
//! over a chain of pages for one reason: a chain would be a second kind of page ownership, and
//! the free walk, the scrub and the copy would each have to learn it. A span of keys is
//! something the tree already understands.

use crate::base::coords::RowId;
use crate::base::engine::{Engine, Fact, Half, Sink};
use crate::base::field_kind::FieldKind;

pub mod block;
pub mod codec;
pub mod error;
pub mod read;
pub mod write;

pub use big_page::{ContainerKey, FragmentKey, Pgno};
pub use block::{Block, Cell, Part, PartRef};
pub use codec::Codec;
pub use error::{ColumnError, Result};
pub use read::ColumnRead;
pub use write::ColumnWrite;

/// Records per block.
///
/// 1024 because 1024 values at the full 64-bit width is exactly one 8 KiB page: the widest a
/// scalar block can encode is one page and never two, which is what lets a block reuse the
/// dense-page arrangement bitmaps already use instead of needing a chain.
pub const BLOCK_SHIFT: u32 = 10;
pub const BLOCK_RECORDS: u64 = 1 << BLOCK_SHIFT;

/// Bits of the container key reserved for a block's part number.
///
/// Eight, so a list block may spill into 256 cells - two megabytes of row ids for one thousand
/// records - before it runs out. Past that the write is refused rather than silently truncated;
/// see [`error::ColumnError::TooManyParts`].
pub const PART_BITS: u32 = 8;
pub const MAX_PARTS: u64 = 1 << PART_BITS;

/// The largest encoded block that is worth keeping inside the leaf cell.
///
/// Half a page, the same line `big_btree::WRITE_CAPS` draws for a container and for the same
/// reason: below it, inline is both smaller and cheap enough to rewrite; above it, a block is
/// better off on a page of its own where it costs one write rather than a leaf rebuild.
pub const INLINE_MAX: usize = 4096;

/// Bytes on the page a spilled block gets. A dense page is raw bytes with its checksum in the
/// cell above it, exactly as a bitmap page is.
pub const PAGE_BYTES: usize = big_page::PAGE_SIZE;

/// Which block a record's local offset falls in.
pub fn block_of(local: u64) -> u64 {
    local >> BLOCK_SHIFT
}

/// Which slot inside its block.
pub fn slot_of(local: u64) -> usize {
    (local & (BLOCK_RECORDS - 1)) as usize
}

/// The container key one part of one block lives at.
pub fn key_of(block: u64, part: u64) -> ContainerKey {
    (block << PART_BITS) | part
}

/// The keys a block occupies: a contiguous span, never a scattered set.
pub fn block_keys(block: u64) -> core::ops::RangeInclusive<ContainerKey> {
    key_of(block, 0)..=key_of(block, MAX_PARTS - 1)
}

/// The block a key belongs to, the inverse of [`key_of`].
pub fn block_of_key(key: ContainerKey) -> u64 {
    key >> PART_BITS
}

/// The part number a key carries.
pub fn part_of_key(key: ContainerKey) -> u64 {
    key & (MAX_PARTS - 1)
}

// A block has to be a whole number of records inside a shard, or the last block of one shard
// would share a key with the first of the next.
const _: () = assert!(BLOCK_RECORDS.is_power_of_two());
// One page holds exactly one block of full-width values. Everything the module claims about a
// scalar block never needing a second page rests on this.
const _: () = assert!(BLOCK_RECORDS * 8 == PAGE_BYTES as u64);

/// What one record's column cell is about to become.
///
/// Keyed by record rather than appended, exactly as `big_db`'s write buffer is, so writing the same record
/// twice in a transaction keeps the last decision rather than replaying both.
#[derive(Clone, Debug)]
pub enum ColEdit {
    /// The cell becomes exactly this. Every scalar kind, and a mutex - which is a keyed field
    /// that holds one value at a time, so a second write replaces the first.
    Replace(Cell),
    /// One row id joins whatever the record already holds.
    ///
    /// Split from [`ColEdit::Add`] because it is what every `set_key` produces and a `Vec` of one
    /// is an allocation per fact — two per record for a table with two set fields, tens of
    /// millions in a load, and the allocator was a quarter of it. The list shape is still needed:
    /// a record written twice in one transaction holds both values, and that is where these are
    /// promoted.
    AddOne(RowId),
    /// Several row ids join it. Only `big_db`'s fold builds these, by merging the above.
    ///
    /// There is deliberately no `Clear`: a delete does not go through the buffer at all. It
    /// nulls slots a block at a time in `big_db`'s delete path, because the records
    /// of one delete are already grouped and the buffer would only regroup them.
    Add(Vec<RowId>),
}

impl ColEdit {
    /// Merges `next` into an edit already buffered for the same record.
    ///
    /// A set field adds, so two `Add`s are both part of what the record holds. Anything else is
    /// the caller's last word and replaces — an `Add` landing on a `Replace` included, which the
    /// map-keyed buffer this replaced treated the same way.
    pub fn absorb(&mut self, next: ColEdit) {
        match (&mut *self, next) {
            (ColEdit::AddOne(have), ColEdit::AddOne(one)) => *self = ColEdit::Add(vec![*have, one]),
            (ColEdit::AddOne(have), ColEdit::Add(mut more)) => {
                more.insert(0, *have);
                *self = ColEdit::Add(more);
            }
            (ColEdit::Add(have), ColEdit::AddOne(one)) => have.push(one),
            (ColEdit::Add(have), ColEdit::Add(more)) => have.extend(more),
            (_, next) => *self = next,
        }
    }
}

/// The descriptor. See [`crate::base::engine`] for what a descriptor is and is not.
pub struct ColumnarEngine;

impl Engine for ColumnarEngine {
    fn code(&self) -> u8 {
        2
    }

    fn name(&self) -> &'static str {
        "columnar"
    }

    fn has_bitmap(&self) -> bool {
        false
    }

    fn has_columns(&self) -> bool {
        true
    }

    /// Every fact becomes one cell, and the only question is whether it replaces or adds.
    ///
    /// **A time quantum's extra views are dropped, and that is the fix rather than an omission.**
    /// They are bitmap views; a table with no bitmaps that wrote them would grow one fragment per
    /// day that no read of it can reach. The old write path wrote them unconditionally.
    fn place(&self, sink: &mut dyn Sink, fact: Fact<'_>) {
        sink.observe(Half::Columns, fact.observed());
        let edit = match fact {
            Fact::Value { value, .. } => ColEdit::Replace(Cell::Value(value)),
            Fact::Bool(b) => ColEdit::Replace(Cell::Value(b as u64)),
            // A mutex holds one value at a time, so its column is a replace and needs no shadow
            // at all - the segment already knows what the record held, which is the one place a
            // column is strictly simpler than the index beside it.
            Fact::Row { row, kind, .. } => {
                if kind == FieldKind::Mutex {
                    ColEdit::Replace(Cell::Value(row))
                } else {
                    ColEdit::AddOne(row)
                }
            }
        };
        sink.cell(edit);
    }
}
