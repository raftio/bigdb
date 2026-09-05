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
    /// How many ranges the space is divided into right now.
    ///
    /// **From the map, not the file.** The file only ever seeded it, and after a split or a
    /// merge the two do not agree.
    pub(super) fn range_count(&self) -> usize {
        self.ranges.read().expect("no panic holds this lock").len()
    }

    /// Which range a shard falls in.
    pub(super) fn range_of(&self, shard: big_engine::ShardId) -> usize {
        self.ranges.read().expect("no panic holds this lock").range_of(shard)
    }

    /// Who serves a range right now.
    ///
    /// The config's answer until the agreement has one, and the agreement's after that - the
    /// map is seeded from the file and replaced by every committed decision. The two are the
    /// same until a machine dies or a range moves.
    pub(super) fn serving(&self, range: usize) -> usize {
        let map = self.ranges.read().expect("no panic holds this lock");
        map.ranges.get(range).map_or(range, |r| r.primary)
    }

    /// Every node holding a range, the one currently serving it first.
    ///
    /// The order is what decides which copy has a batch when only one of them does, so it
    /// follows the agreement rather than the file: after a promotion the new primary is
    /// written first, because it is the one reads now go to.
    pub(super) fn copies_of_range(&self, range: usize) -> Vec<usize> {
        let map = self.ranges.read().expect("no panic holds this lock");
        let Some(r) = map.ranges.get(range) else { return vec![range] };
        let mut out = vec![r.primary];
        out.extend(r.group.iter().copied().filter(|n| *n != r.primary));
        out
    }

    /// The copies the agreement believes are behind, by name.
    ///
    /// Empty on the path everybody hopes for. A name here is a copy that a write could not
    /// reach, which is a copy the agreement will not promote until a repair has been run.
    pub fn behind(&self) -> Vec<String> {
        self.map().stale.iter().filter_map(|n| self.name_of(*n)).collect()
    }

    /// The node that owns the row-key namespace.
    ///
    /// **From the agreement, not the file.** It was a name in `cluster.toml` for the life of
    /// the process, which meant draining the node holding it was draining a role nothing could
    /// move. It is a field in the map now, and moving it is a decision like any other.
    pub(super) fn schema_leader(&self) -> usize {
        self.ranges.read().expect("no panic holds this lock").schema_leader
    }

    /// Whether the map names this node as the one that assigns row ids.
    ///
    /// The map's answer and nothing more. Whether this node may *act* on it is
    /// [`Cluster::guard_schema`], which also asks whether it has stood down and whether it has
    /// heard from the agreement recently enough to know it still leads.
    pub(super) fn leads_schema(&self) -> bool {
        self.schema_leader() == self.config.this_index()
    }

    /// Who leads the schema and at which epoch the map said so, read together so that a
    /// request built from them carries one consistent assumption.
    pub(super) fn schema_lead(&self) -> (usize, u64) {
        let map = self.ranges.read().expect("no panic holds this lock");
        (map.schema_leader, map.epoch)
    }

    /// The refusal this node owes a request to intern or allocate when it may not.
    ///
    /// Three ways to owe one. **The map names somebody else**: a coordinator holding an older
    /// map sent this here, and the answer says where to send it instead. **This node was told
    /// to stand down** by a move that has not committed yet, so by the map it still leads and
    /// by the move it no longer does. **This node has lost touch with the agreement**, so it
    /// cannot know that it has not been replaced - the same reason a range stops being served,
    /// and here there is no unfenced case: the schema leader can always be replaced.
    ///
    /// Two nodes interning at once hand one string two row ids, which nothing downstream can
    /// see. Every refusal here is worth that.
    pub fn guard_schema(&self) -> Result<()> {
        let (leader, epoch) = self.schema_lead();
        let this = self.config.this_index();
        let node = self.config.this().name.clone();
        if leader != this {
            return Err(ClusterError::NotSchemaLeader {
                node,
                leader: self.describe(leader),
                epoch,
            });
        }
        if self.stood_down_at.load(std::sync::atomic::Ordering::Relaxed) > epoch {
            return Err(ClusterError::NotSchemaLeader {
                node,
                leader: "a node this one is handing the namespace to".to_string(),
                epoch,
            });
        }
        if !self.ranges.read().expect("no panic holds this lock").schema_ready {
            return Err(ClusterError::SchemaHandover { node });
        }
        if !self.controller.as_ref().is_none_or(|c| c.may_lead_schema()) {
            return Err(ClusterError::SchemaLeaseLost { node });
        }
        Ok(())
    }

    /// Refuses a request to the schema leader whose assumptions this node does not share.
    ///
    /// The one assumption a sender can state is who it thinks leads. A sender that names
    /// another node reached this one by a map it has not yet applied - or by one this node has
    /// not - and is told who leads *here*, so that it can ask the right node rather than have
    /// this one answer a question that is no longer its to answer.
    pub fn check_schema_lead(&self, led: Option<&wire::Led>) -> Result<()> {
        if let Some(led) = led {
            if led.leader != self.config.this_index() {
                let (leader, epoch) = self.schema_lead();
                return Err(ClusterError::NotSchemaLeader {
                    node: self.config.this().name.clone(),
                    leader: self.describe(leader),
                    epoch,
                });
            }
        }
        self.guard_schema()
    }

    /// Stops this node interning until the map has moved past `epoch`.
    ///
    /// What a move of the namespace asks of its source before it reads the floor: after this
    /// the floor is final rather than racing, and the window between here and the decision
    /// landing is one in which nobody interns - which is allowed - rather than one in which
    /// two nodes do.
    pub fn stand_down_schema(&self, epoch: u64) {
        self.stood_down_at.fetch_max(epoch.saturating_add(1), std::sync::atomic::Ordering::Relaxed);
    }

    /// A node's name, or `None` for an index the cluster file never had.
    ///
    /// An option rather than an index, because a node can now join at runtime: a `NodeId` from
    /// the map may name a machine the file this process read has never heard of.
    pub(super) fn name_of(&self, node: usize) -> Option<String> {
        if let Some(n) = self.config.nodes().get(node) {
            return Some(n.name.clone());
        }
        // A node that joined while this one was running is in the agreement and not in the
        // file, so the agreement is what can name it.
        let c = self.controller.as_ref()?;
        c.members().get(node).map(|m| m.name.clone())
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

    /// Refuses a routed request whose assumptions this node no longer shares.
    ///
    /// **The check that makes a map change safe while writes are in flight.** A coordinator
    /// holding a map one decision old routes a batch to whoever owned those records before a
    /// split or a move; without this the batch lands here, is reported as written, and is never
    /// read again - because reads go to whoever owns them now.
    ///
    /// Two ways to disagree, and only the second is a refusal. A *newer* epoch on the request
    /// than this node has is a coordinator that is ahead: it read the map from the leader, this
    /// node has not applied it yet, and the records are still this node's until it does. What
    /// is refused is a request for shards this node does not hold at all.
    ///
    /// `None` for a request that is not routed, which is every path where this node could only
    /// ever have been the destination.
    pub fn check_route(&self, routed: Option<&wire::Routed>) -> Result<()> {
        let Some(routed) = routed else { return Ok(()) };
        let map = self.ranges.read().expect("no panic holds this lock");
        let this = self.config.this_index();
        let mine: Vec<big_engine::ShardRange> =
            map.ranges.iter().filter(|r| r.group.contains(&this)).map(|r| r.shards).collect();

        // Held rather than served: a replica takes the write too, and is not the primary.
        let held = |want: &big_engine::ShardRange| {
            mine.iter().any(|m| m.start <= want.start && covers_end(m, want))
        };
        if routed.shards.iter().all(held) {
            // **The one window a move denies anything.** During cutover the source still holds
            // these shards - so the check above passes - and must nonetheless stop taking
            // writes, or the difference being copied would never stop growing and the copy
            // would be made against something still moving.
            for want in &routed.shards {
                if let Some(moving) = map.moving_at(want.start) {
                    if moving.state == raft::MoveState::Cutover {
                        return Err(ClusterError::RangeMoving {
                            node: self.config.this().name.clone(),
                            shards: want.to_string(),
                        });
                    }
                }
            }
            return Ok(());
        }
        Err(ClusterError::StaleRoute {
            node: self.config.this().name.clone(),
            wanted: routed.shards.iter().map(|s| s.to_string()).collect::<Vec<_>>().join(", "),
            epoch: routed.epoch,
            mine: map.epoch,
        })
    }

    /// The shards a range covers.
    ///
    /// A range's index is its configured primary's index - primaries come first, in range
    /// order - so the shards of range `r` are the shards node `r` was given in the file. That
    /// stays true when ownership moves, because a promotion moves who answers and not what
    /// the range is.
    pub(super) fn shards_of_range(&self, range: usize) -> big_engine::ShardRange {
        let map = self.ranges.read().expect("no panic holds this lock");
        map.ranges.get(range).map_or(big_engine::ShardRange::ALL, |r| r.shards)
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

    /// Whether `held` reaches at least as far up as `want` does.
    ///
    /// A free function because it is arithmetic about two ranges and nothing about a cluster.
    /// An open end covers everything, including another open end.
    /// `name (shards)`, which is what every report about a node says.
    pub(super) fn describe(&self, i: usize) -> String {
        match self.config.nodes().get(i) {
            Some(node) => format!("{} ({})", node.name, node.shards),
            // A node that joined at runtime is not in the file this process read. Naming it
            // from the agreement is the honest answer; a panic here would be a report about
            // the cluster taking the cluster down.
            None => self.name_of_agreed(i),
        }
    }

    /// A node's name from the agreement, for a report about a node the file never had.
    pub(super) fn name_of_agreed(&self, i: usize) -> String {
        match &self.controller {
            Some(c) => c.members().get(i).map_or_else(|| format!("node {i}"), |m| m.name.clone()),
            None => format!("node {i}"),
        }
    }
}

fn covers_end(held: &big_engine::ShardRange, want: &big_engine::ShardRange) -> bool {
    match (held.end, want.end) {
        (None, _) => true,
        (Some(_), None) => false,
        (Some(h), Some(w)) => h >= w,
    }
}
