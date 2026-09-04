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

//! Which node answers for which part of the space.
//!
//! One question - *who serves this range* - asked from four directions: by the fan-out picking
//! a node, by a probe asking whether this one still counts, by a listing skipping a range that
//! cannot hold what it is looking for, and by a report naming a node. The config answers it
//! until the agreement has an answer, and the agreement's wins after that.

use super::*;

impl<P: PagerMut + Sync> Cluster<P> {
    /// Who serves a range right now.
    ///
    /// The config's answer until the agreement has one, and the agreement's after that. The
    /// two are the same until a machine dies, which is the point: failing over changes who
    /// answers and changes nothing else.
    pub(super) fn serving(&self, range: usize) -> usize {
        match &self.controller {
            Some(c) => c.ownership().primary.get(range).copied().unwrap_or(range),
            None => range,
        }
    }

    /// Every node holding a range, the one currently serving it first.
    ///
    /// The order is what decides which copy has a batch when only one of them does, so it
    /// follows the agreement rather than the file: after a promotion the new primary is
    /// written first, because it is the one reads now go to.
    pub(super) fn copies_of_range(&self, range: usize) -> Vec<usize> {
        let serving = self.serving(range);
        let mut out = vec![serving];
        out.extend(self.config.group(range).iter().copied().filter(|n| *n != serving));
        out
    }

    /// The copies the agreement believes are behind, by name.
    ///
    /// Empty on the path everybody hopes for. A name here is a copy that a write could not
    /// reach, which is a copy the agreement will not promote until a repair has been run.
    pub fn behind(&self) -> Vec<String> {
        let Some(c) = &self.controller else { return Vec::new() };
        c.ownership().stale.iter().map(|n| self.config.nodes()[*n].name.clone()).collect()
    }

    /// Whether this node may answer for the range it holds.
    ///
    /// A node that has lost touch with the agreement has to assume it may already have been
    /// replaced. Answering anyway is the two-primaries failure the whole module exists to
    /// prevent, and it is the one failure a client could never see.
    pub fn may_serve(&self) -> bool {
        self.controller.as_ref().is_none_or(|c| c.may_serve())
    }

    /// The refusal a node out of touch with the agreement owes every request for its own data.
    ///
    /// Reads *and* writes. A read from a stale primary is a stale answer; a write to one is a
    /// copy of the range diverging from the copy that is being read. Neither is visible to the
    /// client that receives it, which is why both are refused rather than one.
    pub fn guard(&self) -> Result<()> {
        if self.may_serve() {
            return Ok(());
        }
        let node = self.config.this();
        Err(ClusterError::NotServing { node: node.name.clone(), shards: node.shards.to_string() })
    }

    /// The shards a range covers.
    ///
    /// A range's index is its configured primary's index - primaries come first, in range
    /// order - so the shards of range `r` are the shards node `r` was given in the file. That
    /// stays true when ownership moves, because a promotion moves who answers and not what
    /// the range is.
    pub(super) fn shards_of_range(&self, range: usize) -> big_engine::ShardRange {
        self.config.nodes()[range].shards
    }

    /// What one slot of a fan-out asks its owner to answer for.
    ///
    /// **A list rather than a range**, because a node will hold several before long and the
    /// wire should not have to change again when it does.
    pub(super) fn scope_of_range(&self, range: usize) -> Option<Vec<big_engine::ShardRange>> {
        Some(vec![self.shards_of_range(range)])
    }

    /// Whether a range can hold a record id after the cursor.
    pub(super) fn may_hold_after(&self, range: usize, after: Option<RecordId>) -> bool {
        self.shards_of_range(range).may_hold_after(after)
    }

    /// The node each range is read from.
    ///
    /// **One, never a fallback.** A copy that is not the one serving the range may have missed
    /// writes - that is precisely the state `stale` records - and an answer assembled from a
    /// copy that is behind is the answer that cannot be told from a correct one. When the
    /// serving copy is unreachable the query fails and the agreement moves the range; reading
    /// somewhere else in the meantime would be answering from data this node has no way to
    /// vouch for.
    pub(super) fn candidates(&self, ranges: impl Iterator<Item = usize>) -> Vec<Vec<usize>> {
        ranges.map(|r| vec![self.serving(r)]).collect()
    }

    /// `name (shards)`, which is what every report about a node says.
    pub(super) fn describe(&self, i: usize) -> String {
        let node = &self.config.nodes()[i];
        format!("{} ({})", node.name, node.shards)
    }
}
