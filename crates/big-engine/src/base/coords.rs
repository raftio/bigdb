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

/// A half-open range of shard ids, with an open end for "the rest of the space".
///
/// **Here rather than in the cluster layer**, though the cluster layer is what assigns them.
/// A range is also what a read may be *scoped to*: once a node can hold more than one of them,
/// "answer for these shards and no others" is a question the storage layer has to be able to
/// answer, and it cannot depend on a crate that sits above it to say what a range is.
///
/// The open end is not a convenience. Ownership has to be *total* - every record id a client
/// can choose has to belong to somebody - and the space is `0..=u64::MAX`, which no half-open
/// range with a written end can reach. `"64.."` is how the last node says it takes what is
/// left, and a file whose ranges stop short is refused.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct ShardRange {
    /// First shard owned.
    pub start: ShardId,
    /// One past the last shard owned; `None` runs to the end of the space.
    pub end: Option<ShardId>,
}

impl ShardRange {
    /// The whole space, which is what a node with no peers holds.
    pub const ALL: Self = Self { start: 0, end: None };

    pub fn contains(&self, shard: ShardId) -> bool {
        shard >= self.start && self.end.is_none_or(|e| shard < e)
    }

    /// Whether a record id falls in this range, which is the same question one shift earlier.
    pub fn holds(&self, record: RecordId) -> bool {
        self.contains(shard_of(record))
    }

    /// The lowest record id this range can hold. What a paging cursor is clamped to.
    pub fn first_record(&self) -> RecordId {
        self.start.saturating_mul(SHARD_WIDTH)
    }

    /// One past the highest record id this range can hold, saturating at the end of the space.
    ///
    /// Saturating rather than wrapping is what makes an absurd end - a range built from a
    /// record id near `u64::MAX` - stop pruning rather than prune everything.
    pub fn end_record(&self) -> Option<RecordId> {
        self.end.map(|e| e.saturating_mul(SHARD_WIDTH))
    }

    /// Whether this range could hold anything at or after `after`.
    ///
    /// What a paging fan-out asks before spending a request on a node: a range entirely below
    /// the cursor has nothing left to say.
    pub fn may_hold_after(&self, after: Option<RecordId>) -> bool {
        let Some(after) = after else { return true };
        self.end_record().is_none_or(|end| end > after.saturating_add(1))
    }
}

impl core::fmt::Display for ShardRange {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self.end {
            Some(e) => write!(f, "{}..{e}", self.start),
            None => write!(f, "{}..", self.start),
        }
    }
}
