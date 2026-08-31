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

//! The whole addressing scheme. Four operations, and one invariant that everything rests on.

use big_page::ContainerKey;

pub type RowId = u64;
pub type RecordId = u64;
pub type ShardId = u64;

/// Records per shard. This is part of the wire format: every peer exchanging data must agree.
pub const SHARD_WIDTH_EXPONENT: u32 = 20;
/// Bits per container.
pub const CONTAINER_EXPONENT: u32 = 16;

pub const SHARD_WIDTH: u64 = 1 << SHARD_WIDTH_EXPONENT;
pub const CONTAINER_WIDTH: u64 = 1 << CONTAINER_EXPONENT;

/// A consequence, not an independent constant. Writing 16 literally anywhere else means a later
/// change to `SHARD_WIDTH_EXPONENT` breaks the layout silently instead of failing to compile.
pub const CONTAINERS_PER_ROW: u64 = 1 << (SHARD_WIDTH_EXPONENT - CONTAINER_EXPONENT);

// Below this, a row would straddle a container boundary and a row scan would stop being
// contiguous. Every claim about prefix scans depends on it.
const _: () = assert!(SHARD_WIDTH_EXPONENT >= CONTAINER_EXPONENT);
const _: () = assert!(CONTAINERS_PER_ROW == 16);
const _: () = assert!(CONTAINER_WIDTH == 65536);

pub fn shard_of(record_id: RecordId) -> ShardId {
    record_id >> SHARD_WIDTH_EXPONENT
}

/// Offset of `record_id` inside its own shard.
pub fn local_of(record_id: RecordId) -> u64 {
    record_id & (SHARD_WIDTH - 1)
}

/// Bit position of a fact inside a fragment.
pub fn pos_of(row: RowId, record_id: RecordId) -> u64 {
    row * SHARD_WIDTH + local_of(record_id)
}

pub fn ckey_of(pos: u64) -> ContainerKey {
    pos >> CONTAINER_EXPONENT
}

pub fn offset_in_container(pos: u64) -> u16 {
    (pos & (CONTAINER_WIDTH - 1)) as u16
}

/// The container keys a row occupies: a contiguous span, never a scattered set.
pub fn row_ckeys(row: RowId) -> core::ops::RangeInclusive<ContainerKey> {
    let first = row * CONTAINERS_PER_ROW;
    first..=(first + CONTAINERS_PER_ROW - 1)
}

pub fn row_of_ckey(ckey: ContainerKey) -> RowId {
    ckey / CONTAINERS_PER_ROW
}

/// Inverse of `pos_of`, back to a record id within `shard`.
pub fn record_of(shard: ShardId, ckey: ContainerKey, offset: u16) -> RecordId {
    record_of_slot(shard, slot_of_ckey(ckey), offset)
}

/// Which of a row's containers this key is, counting from the start of the row.
///
/// Two different rows never share an absolute container key, so anything that compares rows
/// against each other has to line them up by slot instead.
pub fn slot_of_ckey(ckey: ContainerKey) -> u64 {
    ckey % CONTAINERS_PER_ROW
}

pub fn ckey_of_slot(row: RowId, slot: u64) -> ContainerKey {
    row * CONTAINERS_PER_ROW + slot
}

pub fn record_of_slot(shard: ShardId, slot: u64, offset: u16) -> RecordId {
    shard * SHARD_WIDTH + slot * CONTAINER_WIDTH + offset as u64
}
