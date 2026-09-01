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

//! Without these, the failure mode of copy-on-write only shows up when the disk fills.

use crate::io::IoStats;
use big_page::TxnId;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Metrics {
    /// Which reader is holding the freelist back. `None` means no reader is alive.
    pub oldest_reader_txn_id: Option<TxnId>,
    /// File growth caused by long-running queries.
    pub pages_pending_reclaim_reader: u64,
    /// File growth caused by time travel. Kept apart from the reader count, because merged
    /// they would not tell you which knob to turn.
    pub pages_pending_reclaim_retention: u64,
    /// Freelist pages that are reclaimable right now.
    pub free_pages_reusable: u64,
    pub page_count: u64,
    pub live_readers: usize,
    pub snapshots: usize,
    pub fragments: usize,
    pub txn_id: TxnId,
    /// What a commit currently promises. Exported because an ingest that relaxed it and never
    /// tightened back is otherwise invisible: the file looks fine, right up until it does not.
    pub durability: crate::Durability,
    /// Where the last commit's pages went, by class.
    pub last_commit: CommitBreakdown,
    /// What the storage backend has done to the disk since open, when it counts it.
    ///
    /// Everything else here is a *gauge over the file* - how many pages there are, how many
    /// are pinned, which reader is holding them. None of it says how hard the disk is being
    /// worked to keep that shape, and the two come apart in exactly the case that matters: a
    /// database whose page count is flat while its write rate is enormous is one rewriting the
    /// same pages over and over, and the gauges alone show nothing at all.
    ///
    /// `None` when the backend keeps no count. See [`crate::Pager::io_stats`].
    pub io: Option<IoStats>,
}

/// Pages written by one commit, split by what they were.
///
/// Exists because "the commit wrote 41 pages" is not an answer anyone can act on, and the two
/// obvious explanations - the b-tree paths, and the fixed chains a commit rewrites whether or not
/// it needed to - call for completely different fixes. A number per class is the difference
/// between optimising the engine and optimising a guess about it.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct CommitBreakdown {
    /// B-tree pages: leaves, branches, and the bitmap pages dense containers own.
    pub data: u64,
    /// The root-record chain. Rewritten **whole** whenever any fragment's root moved, so this
    /// grows with how many fragments the database has rather than with how many it touched.
    pub roots: u64,
    /// The catalog chain, rewritten whole on any schema or zone-map change - which every write
    /// to a bit-sliced index is, because it observes the value.
    pub catalog: u64,
    /// The freelist chain, rewritten on every commit without exception.
    pub freelist: u64,
    /// The snapshot registry, rewritten only when a snapshot is taken or dropped.
    pub snapshots: u64,
}

impl CommitBreakdown {
    pub fn total(&self) -> u64 {
        self.data + self.roots + self.catalog + self.freelist + self.snapshots
    }
}
