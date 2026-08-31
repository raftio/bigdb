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

//! The result of a predicate, before anyone asks what to do with it.
//!
//! Every query method used to answer in its own final currency - a `Vec<RecordId>`, a count, a
//! bool - which meant two predicates could not be combined without materialising both. The set
//! algebra was already there in `RowSet`; it was being computed and thrown away at this
//! boundary. This type is the boundary keeping it.

use big_fragment::{shard_of, RecordId, RowSet, ShardId, SHARD_WIDTH};
use std::collections::BTreeMap;

/// Which records matched, kept per shard and unmaterialised.
///
/// Shards are independent, so combining two of these is a shard-wise merge and never a scan.
/// A shard absent from the map matched nothing, which is why `and` walks the smaller side.
#[derive(Clone, Default, Debug)]
pub struct Matches {
    shards: BTreeMap<ShardId, RowSet>,
}

impl Matches {
    pub fn new() -> Self {
        Self::default()
    }

    /// Empty row sets are dropped rather than stored: a shard that matched nothing is the same
    /// as a shard that was never looked at, and keeping it would make `and` walk it forever.
    pub fn insert(&mut self, shard: ShardId, rows: RowSet) {
        if !rows.is_empty() {
            self.shards.insert(shard, rows);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.shards.is_empty()
    }

    /// Shards holding at least one match, ascending.
    pub fn shards(&self) -> impl Iterator<Item = ShardId> + '_ {
        self.shards.keys().copied()
    }

    pub fn get(&self, shard: ShardId) -> Option<&RowSet> {
        self.shards.get(&shard)
    }

    /// How many records matched, without naming any of them. This is the operation the whole
    /// storage model exists to make cheap: no record id is ever constructed.
    pub fn cardinality(&self) -> u64 {
        self.shards.values().map(|r| r.cardinality()).sum()
    }

    /// Matching record ids, ascending.
    ///
    /// Ascending falls out of the layout rather than being imposed: shards are visited in
    /// order, a row set holds its slots in order, and a container yields its offsets in order.
    /// Callers therefore do not sort, and `database_tracks_a_map` fails if that ever stops
    /// being true.
    pub fn records(&self) -> impl Iterator<Item = RecordId> + '_ {
        self.shards.iter().flat_map(|(shard, rows)| rows.records(*shard))
    }

    /// Matching record ids at or after `from`, ascending.
    ///
    /// The paging primitive. A cursor over a result set is this plus a count: hand back `limit`
    /// ids and remember the last one, then ask again from one past it. No state is held between
    /// calls - the record id *is* the cursor - which is what lets a page be served by a request
    /// that shares nothing with the request before it.
    ///
    /// Shards below `from` are skipped whole, and inside the first shard so are the containers;
    /// see [`RowSet::records_from`]. Paging through a large result therefore costs the result,
    /// not the result times the number of pages.
    pub fn records_from(&self, from: RecordId) -> impl Iterator<Item = RecordId> + '_ {
        self.shards.range(shard_of(from)..).flat_map(move |(shard, rows)| {
            // A shard past the one the cursor landed in has no floor of its own. Expressing
            // that as a `max` rather than a branch keeps the two cases the same code.
            rows.records_from(*shard, from.max(shard * SHARD_WIDTH))
        })
    }

    /// Records matching both. Only shards present on both sides can contribute.
    pub fn and(&self, other: &Self) -> Self {
        let (small, large) =
            if self.shards.len() <= other.shards.len() { (self, other) } else { (other, self) };
        let mut out = Self::new();
        for (shard, rows) in &small.shards {
            if let Some(rhs) = large.shards.get(shard) {
                out.insert(*shard, rows.and(rhs));
            }
        }
        out
    }

    /// Records matching either. A shard on one side only passes through untouched.
    pub fn or(&self, other: &Self) -> Self {
        let mut out = self.clone();
        for (shard, rows) in &other.shards {
            match out.shards.remove(shard) {
                Some(lhs) => out.insert(*shard, lhs.or(rows)),
                None => out.insert(*shard, rows.clone()),
            }
        }
        out
    }

    /// Records matching this and not the other.
    pub fn andnot(&self, other: &Self) -> Self {
        let mut out = Self::new();
        for (shard, rows) in &self.shards {
            match other.shards.get(shard) {
                Some(rhs) => out.insert(*shard, rows.andnot(rhs)),
                None => out.insert(*shard, rows.clone()),
            }
        }
        out
    }
}
