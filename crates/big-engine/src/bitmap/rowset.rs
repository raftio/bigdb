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

//! A materialised row: a set of record ids inside one shard.
//!
//! Keyed by *slot* -- the container's index within the row -- not by absolute container key.
//! Two rows never share an absolute key, so keying by that would make every cross-row set
//! operation trivially empty.

use crate::base::coords::*;
use big_container::{apply, Container, ContainerRef, SetOp};

/// Sparse by construction: a row of a wide field is mostly empty containers.
#[derive(Clone, Default, Debug)]
pub struct RowSet {
    slots: Vec<(u64, Container)>,
}

impl RowSet {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, slot: u64, c: Container) {
        if c.is_empty() {
            return;
        }
        match self.slots.binary_search_by_key(&slot, |(k, _)| *k) {
            Ok(i) => self.slots[i].1 = c,
            Err(i) => self.slots.insert(i, (slot, c)),
        }
    }

    /// Whether this row holds `record`, which belongs to `shard`.
    ///
    /// A binary search over the slots and a lookup inside one container. Written for the write
    /// path rather than the read path: a flush asks it once per record it is about to write, to
    /// find out whether that record has an old value that needs clearing.
    pub fn contains(&self, shard: ShardId, record: RecordId) -> bool {
        let local = local_of(record);
        let slot = local / CONTAINER_WIDTH;
        let offset = (local % CONTAINER_WIDTH) as u16;
        debug_assert_eq!(shard_of(record), shard, "a record from another shard");
        self.get(slot).is_some_and(|c| c.contains(offset))
    }

    pub fn get(&self, slot: u64) -> Option<&Container> {
        self.slots.binary_search_by_key(&slot, |(k, _)| *k).ok().map(|i| &self.slots[i].1)
    }

    pub fn iter(&self) -> impl Iterator<Item = (u64, ContainerRef<'_>)> {
        self.slots.iter().map(|(k, c)| (*k, c.as_ref()))
    }

    /// Roughly how much memory this row set holds, for callers that have to cap it.
    ///
    /// A cardinality is not a size: a thousand records can be one run of eight bytes or a
    /// thousand scattered ones of two thousand, and it is the bytes that end the process.
    pub fn byte_size(&self) -> usize {
        self.slots.iter().map(|(_, c)| c.as_ref().byte_size()).sum()
    }

    pub fn cardinality(&self) -> u64 {
        self.slots.iter().map(|(_, c)| c.cardinality() as u64).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    pub fn len(&self) -> usize {
        self.slots.len()
    }

    /// Record ids in this row, given the shard the fragment belongs to.
    pub fn records(&self, shard: ShardId) -> impl Iterator<Item = RecordId> + '_ {
        self.slots.iter().flat_map(move |(slot, c)| {
            c.as_ref().iter().map(move |o| record_of_slot(shard, *slot, o))
        })
    }

    /// Record ids in this row that are at or after `from`, given the shard.
    ///
    /// The point of taking a floor rather than filtering afterwards: a caller paging through a
    /// table asks for the next page starting where the last one ended, and a shard holds up to
    /// a million records. Filtering would walk all of them to skip them, making each page cost
    /// the whole shard and the whole scan quadratic. Slots below the floor are skipped by
    /// binary search, so only the one container the floor falls inside is filtered - at most
    /// `CONTAINER_WIDTH` comparisons for the entire page, not per page.
    ///
    /// `from` below this shard's range is the same as no floor at all, which is what makes the
    /// caller's per-shard arithmetic a `max` rather than a branch.
    pub fn records_from(
        &self,
        shard: ShardId,
        from: RecordId,
    ) -> impl Iterator<Item = RecordId> + '_ {
        let first_slot = local_of(from) >> CONTAINER_EXPONENT;
        let start = self.slots.partition_point(|(slot, _)| *slot < first_slot);
        self.slots[start..]
            .iter()
            .flat_map(move |(slot, c)| {
                c.as_ref().iter().map(move |o| record_of_slot(shard, *slot, o))
            })
            .filter(move |r| *r >= from)
    }

    /// Container-wise set operation. Rows line up by key, so this is a merge, not a scan.
    pub fn combine(&self, op: SetOp, other: &RowSet) -> RowSet {
        let mut out = RowSet::new();
        let (mut i, mut j) = (0usize, 0usize);
        while i < self.slots.len() || j < other.slots.len() {
            let ka = self.slots.get(i).map(|(k, _)| *k);
            let kb = other.slots.get(j).map(|(k, _)| *k);
            let take_left = match (ka, kb) {
                (Some(a), Some(b)) if a == b => {
                    let r = apply(op, self.slots[i].1.as_ref(), other.slots[j].1.as_ref());
                    out.insert(a, r.into_owned());
                    i += 1;
                    j += 1;
                    continue;
                }
                (Some(a), Some(b)) => a < b,
                (Some(_), None) => true,
                (None, Some(_)) => false,
                (None, None) => break,
            };
            if take_left {
                if op.keeps_left_only() {
                    out.insert(self.slots[i].0, self.slots[i].1.clone());
                }
                i += 1;
            } else {
                if op.keeps_right_only() {
                    out.insert(other.slots[j].0, other.slots[j].1.clone());
                }
                j += 1;
            }
        }
        out
    }

    pub fn and(&self, other: &RowSet) -> RowSet {
        self.combine(SetOp::And, other)
    }

    pub fn or(&self, other: &RowSet) -> RowSet {
        self.combine(SetOp::Or, other)
    }

    pub fn andnot(&self, other: &RowSet) -> RowSet {
        self.combine(SetOp::AndNot, other)
    }

    pub fn xor(&self, other: &RowSet) -> RowSet {
        self.combine(SetOp::Xor, other)
    }
}
