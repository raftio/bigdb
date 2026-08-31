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

//! Every layout constant is derived from the page size. No literals anywhere else.

use big_container::{Interval, BITMAP_BYTES};
use core::mem::size_of;

pub const PAGE_SIZE: usize = 8192;

/// pgno u32 + flags u32 + cell_count u16 + reserved u16.
pub const PAGE_HEADER: usize = 12;
/// crc32 in the last 4 bytes, on every page except a bitmap page.
pub const PAGE_TRAILER: usize = 4;
/// One cell-index entry is a u16 offset.
pub const CELL_INDEX_ENTRY: usize = 2;
/// Every cell starts at a multiple of 8 so payloads can be cast in safe Rust.
pub const CELL_ALIGN: usize = 8;

pub const LEAF_CELL_HEADER: usize = 24;
/// ckey u64 + flags u32 + pgno u32.
pub const BRANCH_CELL: usize = 16;

/// Largest payload when that cell is the only cell on the page.
/// The space taken by the cell index must be `align_up`'d: the first cell starts at the next
/// multiple of 8, not immediately after the index.
pub const CELL_PAYLOAD_MAX: usize =
    PAGE_SIZE - PAGE_TRAILER - align_up(PAGE_HEADER + CELL_INDEX_ENTRY) - LEAF_CELL_HEADER;

/// The *physical* array ceiling, not the economic one. See `big_container::ARRAY_BREAK_EVEN`.
pub const ARRAY_MAX_ELEMS: usize = CELL_PAYLOAD_MAX / size_of::<u16>();
pub const RUN_MAX_INTERVALS: usize = CELL_PAYLOAD_MAX / size_of::<Interval>();

/// Round up to a multiple of `CELL_ALIGN`.
pub const fn align_up(n: usize) -> usize {
    (n + CELL_ALIGN - 1) & !(CELL_ALIGN - 1)
}

const _: () = assert!(PAGE_SIZE.is_multiple_of(CELL_ALIGN));
const _: () = assert!(PAGE_HEADER.is_multiple_of(4));
const _: () =
    assert!(LEAF_CELL_HEADER.is_multiple_of(CELL_ALIGN), "header lệch align thì payload lệch theo");
const _: () = assert!(BRANCH_CELL.is_multiple_of(CELL_ALIGN));
const _: () = assert!(CELL_PAYLOAD_MAX == 8148);
const _: () = assert!(ARRAY_MAX_ELEMS == 4074);
const _: () = assert!(RUN_MAX_INTERVALS == 2037);
const _: () = assert!(BITMAP_BYTES == PAGE_SIZE, "dense container phải trọn một page");
