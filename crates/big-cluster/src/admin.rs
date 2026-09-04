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

//! Changing the shape of the cluster on purpose.
//!
//! **Every one of these is a proposal, and the leader is the only node that makes one.** A
//! command sent to any other node is forwarded rather than answered, so an operator does not
//! have to know which node leads - and two operators racing cannot both win, because the
//! agreement serialises them.
//!
//! What is *not* here is a decision about when to run any of it. That belongs to whoever is
//! asking: an operator at a terminal, an autoscaler reading a metric, or a controller reacting
//! to a pod. All four call the same three verbs.

use super::*;
use crate::raft::{RangeId, RangeMap};

impl<P: PagerMut + Sync> Cluster<P> {
    /// Cuts the range holding `at` in two, and optionally hands the upper half to another node.
    ///
    /// **This is the scale-out that moves no bytes**, and the only one. Both halves stay
    /// exactly where they were unless `to` is given; when it is, the upper half changes owner
    /// without a copy - which is safe only because the check below proves it is empty.
    ///
    /// Handing over a half that holds records would be silent data loss: the map would say one
    /// node answers for them while the records sat on another. So the current owner is asked,
    /// and a half with anything in it is refused with the record that stopped it.
    pub fn split_range(&self, at: big_engine::ShardId, to: Option<&str>) -> Result<RangeId> {
        let mut next = self.map();
        let id = next.split(at).map_err(|e| ClusterError::Refused(e.to_string()))?;

        if let Some(name) = to {
            let target = self.node_named(name)?;
            let upper = next
                .position(id)
                .map(|i| next.ranges[i].shards)
                .expect("the range that was just created");
            self.refuse_unless_empty(upper)?;
            next.assign(id, vec![target]).map_err(|e| ClusterError::Refused(e.to_string()))?;
        }

        self.propose(next)?;
        Ok(id)
    }

    /// Joins a range to the one after it. Both must already be held by the same nodes.
    pub fn merge_range(&self, id: RangeId) -> Result<()> {
        let mut next = self.map();
        next.merge(id).map_err(|e| ClusterError::Refused(e.to_string()))?;
        self.propose(next)
    }

    /// **Refuses unless the shards hold nothing at all.**
    ///
    /// Asked of the node that owns them right now, table by table, because "is this range
    /// empty" is a fact about data and the map has no way to know it. A range with one record
    /// in it is a range that cannot change hands without a copy.
    ///
    /// The window between this answer and the proposal landing is closed by the epoch a routed
    /// write carries: a batch that slipped in meanwhile is written under the old map, and the
    /// new owner refuses it rather than accepting a record it does not have.
    fn refuse_unless_empty(&self, shards: big_engine::ShardRange) -> Result<()> {
        let scope = Some(vec![shards]);
        for table in self.schema() {
            let range = self.range_of(shards.start);
            let owner = self.serving(range);
            let highest = self.highest_record(owner, &table.name, scope.clone())?;
            if let Some(record) = highest {
                return Err(ClusterError::Refused(format!(
                    "shards {shards} still hold record {record} in `{}`; a range with records \
                     in it cannot change hands without a copy. Move it instead",
                    table.name
                )));
            }
        }
        Ok(())
    }

    /// The highest record one node holds for a table within a scope, or `None` for nothing.
    fn highest_record(
        &self,
        node: usize,
        table: &str,
        shards: Option<Vec<big_engine::ShardRange>>,
    ) -> Result<Option<RecordId>> {
        if node == self.config.this_index() {
            return self.api.max_record_in(table, shards).map_err(ClusterError::Local);
        }
        let body = wire::TableRequest { table: table.to_string(), shards }.encode();
        let bytes = self.ask(node, path::NEXT_RECORD, &body, None)?;
        let next = self.read(node, || wire::get_u64_body(&bytes))?;
        // `next_record` answers with one past the highest, and zero for a table with nothing.
        Ok(next.checked_sub(1))
    }

    /// Puts a change to the agreement, wherever the leader happens to be.
    fn propose(&self, next: RangeMap) -> Result<()> {
        let Some(controller) = &self.controller else {
            return Err(ClusterError::Refused(
                "this cluster runs no agreement, so its map is whatever its file says. Give a \
                 range a copy and the map becomes something that can be changed"
                    .to_string(),
            ));
        };
        controller.propose_map(next).map(|_| ()).map_err(|e| ClusterError::Refused(e.to_string()))
    }

    /// A node's index by name, refusing a name the cluster does not have.
    ///
    /// **From the agreement, not the file.** A node that joined at runtime is not in the file
    /// this process read, and looking it up there would make it unaddressable by the very
    /// commands that manage it.
    fn node_named(&self, name: &str) -> Result<usize> {
        self.members()
            .iter()
            .position(|n| n.name == name && n.state != raft::MemberState::Gone)
            .ok_or_else(|| ClusterError::Refused(format!("there is no node called `{name}`")))
    }

    // ---------------------------------------------------------------------------------------
    // Who is in the cluster
    // ---------------------------------------------------------------------------------------

    /// Adds a node, as a learner holding nothing.
    ///
    /// **A learner, not a voter.** A node that has just arrived holds no range and has not
    /// caught up on the log; counting it towards a majority would raise the bar for every
    /// election while it contributed nothing to one. The balancer promotes it once it has
    /// something to serve - or an operator does, with `admit`.
    pub fn add_node(&self, name: &str, addr: &str) -> Result<()> {
        let mut next = self.members();
        if let Some(existing) = next.iter_mut().find(|m| m.name == name) {
            // A node coming back after being removed takes its slot again rather than a new
            // one, so no `NodeId` ever means two machines.
            if existing.state != raft::MemberState::Gone {
                return Err(ClusterError::Refused(format!("`{name}` is already in this cluster")));
            }
            existing.addr = addr.to_string();
            existing.state = raft::MemberState::Learner;
        } else {
            next.push(raft::Member {
                name: name.to_string(),
                addr: addr.to_string(),
                state: raft::MemberState::Learner,
            });
        }
        self.propose_members(next)
    }

    /// Makes a learner a full member, which is what lets it hold a range and vote.
    pub fn admit(&self, name: &str) -> Result<()> {
        self.set_state(name, raft::MemberState::Voter)
    }

    /// Starts taking a node out of the cluster.
    ///
    /// **It keeps answering.** A draining node still votes and still coordinates - which is
    /// what keeps the one address a client is holding from going dark halfway through - but the
    /// balancer moves its ranges away and gives it no new ones. `remove` is the step after,
    /// once it holds nothing.
    pub fn drain_node(&self, name: &str) -> Result<()> {
        self.set_state(name, raft::MemberState::Draining)
    }

    /// Takes a node out for good. Refused while it still holds a range.
    ///
    /// The refusal is the point: removing a node that still serves something is removing the
    /// only copy of it, and the map would go on naming a node nobody talks to.
    pub fn remove_node(&self, name: &str) -> Result<()> {
        let node = self.node_named(name)?;
        let map = self.map();
        let held = map.held_by(node);
        if !held.is_empty() {
            let shards: Vec<String> =
                held.iter().map(|r| map.ranges[*r].shards.to_string()).collect();
            return Err(ClusterError::Refused(format!(
                "`{name}` still holds shards {}; drain it first, or those records leave with it",
                shards.join(", ")
            )));
        }
        self.set_state(name, raft::MemberState::Gone)
    }

    fn set_state(&self, name: &str, state: raft::MemberState) -> Result<()> {
        let mut next = self.members();
        let node = self.node_named(name)?;
        let Some(member) = next.get_mut(node) else {
            return Err(ClusterError::Refused(format!("there is no node called `{name}`")));
        };
        if member.state == state {
            return Ok(());
        }
        member.state = state;
        self.propose_members(next)
    }

    /// Who the agreement believes is in the cluster.
    pub fn members(&self) -> Vec<raft::Member> {
        match &self.controller {
            Some(c) => c.members(),
            None => self.config.seed_members(),
        }
    }

    fn propose_members(&self, next: Vec<raft::Member>) -> Result<()> {
        let Some(controller) = &self.controller else {
            return Err(ClusterError::Refused(
                "this cluster runs no agreement, so its membership is whatever its file says"
                    .to_string(),
            ));
        };
        controller.propose_members(next).map_err(|e| ClusterError::Refused(e.to_string()))
    }

    /// What the cluster looks like right now: every range, who holds it, and what is moving.
    ///
    /// **The one thing an autoscaler or an operator outside bigdb reads.** It is a report, not
    /// a handle: nothing here is held while it is rendered.
    pub fn topology(&self) -> Topology {
        let map = self.map();
        Topology {
            epoch: map.epoch,
            leader: self.controller.as_ref().and_then(|c| c.leader()).and_then(|n| self.name_of(n)),
            schema_leader: self.name_of(map.schema_leader).unwrap_or_default(),
            members: self
                .members()
                .iter()
                .filter(|m| m.state != raft::MemberState::Gone)
                .map(|m| MemberReport {
                    name: m.name.clone(),
                    addr: m.addr.clone(),
                    state: match m.state {
                        raft::MemberState::Learner => "learner",
                        raft::MemberState::Voter => "voter",
                        raft::MemberState::Draining => "draining",
                        raft::MemberState::Gone => "gone",
                    },
                })
                .collect(),
            ranges: map
                .ranges
                .iter()
                .map(|r| RangeReport {
                    id: r.id,
                    shards: r.shards.to_string(),
                    primary: self.name_of(r.primary).unwrap_or_default(),
                    holders: r.group.iter().filter_map(|n| self.name_of(*n)).collect(),
                    moving_to: r.moving.as_ref().and_then(|m| self.name_of(m.target)),
                    moving_state: r.moving.as_ref().map(|m| match m.state {
                        raft::MoveState::Seeding => "seeding",
                        raft::MoveState::Cutover => "cutover",
                    }),
                })
                .collect(),
            behind: self.behind(),
        }
    }
}

/// The cluster's shape, as a report.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Topology {
    pub epoch: u64,
    /// Who leads the agreement, when this node knows.
    pub leader: Option<String>,
    pub schema_leader: String,
    pub members: Vec<MemberReport>,
    pub ranges: Vec<RangeReport>,
    /// Copies the agreement will not promote until a repair has been run.
    pub behind: Vec<String>,
}

/// One node, as the agreement sees it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct MemberReport {
    pub name: String,
    pub addr: String,
    pub state: &'static str,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct RangeReport {
    pub id: RangeId,
    pub shards: String,
    pub primary: String,
    pub holders: Vec<String>,
    pub moving_to: Option<String>,
    pub moving_state: Option<&'static str>,
}
