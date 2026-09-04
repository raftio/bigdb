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

/// How long a change to the map may take to be agreed before the caller is told it has not
/// been. Generous: an election has to be able to finish inside it.
const PROPOSAL_TIMEOUT: Duration = Duration::from_secs(10);

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

    /// Puts a change to the agreement and **waits for it to commit**.
    ///
    /// Waiting is what makes a sequence of changes - the three steps of a move - a sequence
    /// rather than a race. A proposal returns as soon as it is appended, so a caller that read
    /// the map straight afterwards would read the map it started from and build its next change
    /// on a state that is about to be replaced.
    fn propose(&self, next: RangeMap) -> Result<()> {
        let Some(controller) = &self.controller else {
            return Err(ClusterError::Refused(
                "this cluster runs no agreement, so its map is whatever its file says. Give a \
                 range a copy and the map becomes something that can be changed"
                    .to_string(),
            ));
        };
        let epoch =
            controller.propose_map(next).map_err(|e| ClusterError::Refused(e.to_string()))?;
        self.await_epoch(epoch)
    }

    /// Waits until this node has applied the map at `epoch`.
    ///
    /// A poll rather than a signal: the wait is over in about one round trip, this is an
    /// operator's request rather than the query path, and a condition variable to save a few
    /// milliseconds here would be machinery nobody could see the value of.
    fn await_epoch(&self, epoch: u64) -> Result<()> {
        let deadline = Instant::now() + PROPOSAL_TIMEOUT;
        while Instant::now() < deadline {
            if self.map().epoch >= epoch {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        Err(ClusterError::Refused(format!(
            "the change was proposed but has not been agreed after {}s; the cluster may have \
             lost its leader. Nothing was applied - `cluster topology` says where it stands",
            PROPOSAL_TIMEOUT.as_secs()
        )))
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

    /// Moves a populated range to another node, without stopping reads.
    ///
    /// **Three steps, each one committed decision.**
    ///
    /// 1. *Seeding.* The target is told to expect the range and then filled, **out of the write
    ///    path entirely** - it is not in the group, nothing reads from it, and abandoning the
    ///    move at any point up to the last step costs nothing.
    /// 2. *Cutover.* Writes to this one range are refused, retryably, while the difference that
    ///    accumulated during the seed is copied. This is the only window in which anything is
    ///    denied, and it is one range wide.
    /// 3. *Handover.* The group, the primary and the end of the move land in one entry, so there
    ///    is no committed state in which two nodes could both be asked for these records. Then
    ///    the source drops what it no longer owns.
    ///
    /// **Reads never stop.** The source serves the range right up to the instant the handover
    /// commits, which is the same atomic act as a failover.
    ///
    /// Synchronous, like `POST /repair`: it is a scan and a copy, and a caller that wants it in
    /// the background runs it in the background.
    pub fn move_range(&self, id: RangeId, to: &str) -> Result<MoveReport> {
        let target = self.node_named(to)?;
        let map = self.map();
        let Some(i) = map.position(id) else {
            return Err(ClusterError::Refused(format!("there is no range {id}")));
        };
        let (source, shards) = (map.ranges[i].primary, map.ranges[i].shards);
        if source == target {
            return Err(ClusterError::Refused(format!("range {id} is already on `{to}`")));
        }

        // 1. Seeding. Announced before a byte moves, so that a leader elected in the middle of
        // this finds a move it can finish or abandon rather than a half-filled node it has
        // never heard of.
        self.with_map(|m| m.begin_move(id, target).map_err(|e| e.to_string()))?;
        let seeded = match self.catch_up_in(source, target, Some(shards)) {
            Ok(n) => n,
            Err(e) => return Err(self.abandon(id, e)),
        };

        // 2. Cutover.
        //
        // **The barrier is only real once the source knows about it.** Committing the decision
        // makes it final; it does not make the source aware of it, and a source still on the
        // old map goes on accepting writes for this range. Every one of those that lands after
        // the pass below has read its fragments is a write that was acknowledged and then left
        // behind - which is the exact failure this whole shape exists to prevent, and the one
        // nothing downstream would ever contradict.
        //
        // So the epoch is waited for *at the source* before a byte of the final pass is read.
        // After that the range is genuinely still, and the cardinality comparison underneath
        // `catch_up_in` is a proof rather than a guess.
        let epoch = self.with_map(|m| {
            m.set_move_state(id, raft::MoveState::Cutover).map_err(|e| e.to_string())
        })?;
        if let Err(e) = self.await_applied(source, epoch) {
            return Err(self.abandon(id, e));
        }
        let caught = match self.catch_up_in(source, target, Some(shards)) {
            Ok(n) => n,
            Err(e) => return Err(self.abandon(id, e)),
        };

        // The proof that it worked, before anything is handed over. A move that copied
        // everything it could find and still disagrees is a move that must not complete.
        match self.digests_agree(source, target, shards) {
            Err(e) => return Err(self.abandon(id, e)),
            Ok(false) => {
                return Err(self.abandon(
                    id,
                    ClusterError::Refused(format!(
                        "`{to}` still disagrees with the node it copied shards {shards} from, \
                         so the range was left where it was"
                    )),
                ))
            }
            Ok(true) => {}
        }

        // 3. Handover, in one entry.
        self.with_map(|m| m.finish_move(id).map_err(|e| e.to_string()))?;

        // **After the source has applied the handover, not merely after it commits.**
        //
        // Committing makes the decision final; it does not make every node aware of it. A
        // source still on the old map believes it serves this range, so emptying it first makes
        // that node answer *zero* for records it thinks it owns - a wrong answer rather than a
        // refusal, which is the one failure this whole layer is built to avoid. Once it has
        // applied, it knows the range is not its own and refuses instead.
        //
        // A failure here leaves records nobody reads: it costs space and answers nothing
        // wrongly, so it is reported rather than undone.
        let dropped = self
            .await_applied(source, self.map().epoch)
            .and_then(|()| self.drop_shards(source, shards));
        Ok(MoveReport {
            range: id,
            shards: shards.to_string(),
            from: self.name_of(source).unwrap_or_default(),
            to: to.to_string(),
            fragments: seeded + caught,
            dropped: dropped.is_ok(),
            outcome: match dropped {
                Ok(()) => "moved".to_string(),
                Err(e) => format!("moved, but the old copy could not be dropped: {e}"),
            },
        })
    }

    /// Ends a move that cannot be finished, and reports why.
    ///
    /// Nothing was ever read from the target, so abandoning costs only the copying already
    /// done. The original error is what the caller hears; a failure to even cancel is added to
    /// it rather than replacing it.
    fn abandon(&self, id: RangeId, why: ClusterError) -> ClusterError {
        match self.with_map(|m| m.cancel_move(id).map_err(|e| e.to_string())) {
            Ok(_) => why,
            Err(e) => ClusterError::Refused(format!(
                "{why} - and the move could not be called off either ({e}), so range {id} is \
                 still marked as moving. `cluster cancel {id}` clears it"
            )),
        }
    }

    /// Abandons a move deliberately.
    pub fn cancel_move(&self, id: RangeId) -> Result<()> {
        self.with_map(|m| m.cancel_move(id).map_err(|e| e.to_string())).map(|_| ())
    }

    /// Waits until another node has applied the map at `epoch`.
    ///
    /// The map a node is answering from is its own, and it gets there by replication - about a
    /// heartbeat behind whoever proposed the change.
    fn await_applied(&self, node: usize, epoch: u64) -> Result<()> {
        if node == self.config.this_index() {
            return self.await_epoch(epoch);
        }
        let deadline = Instant::now() + PROPOSAL_TIMEOUT;
        while Instant::now() < deadline {
            let bytes = self.ask(node, path::EPOCH, &[], None)?;
            if self.read(node, || wire::get_u64_body(&bytes))? >= epoch {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        Err(ClusterError::Refused(format!(
            "`{}` has not caught up with the handover, so its copy was left in place",
            self.name_of(node).unwrap_or_default()
        )))
    }

    /// Whether two nodes hold the same facts for a set of shards.
    fn digests_agree(
        &self,
        source: usize,
        target: usize,
        shards: big_engine::ShardRange,
    ) -> Result<bool> {
        let scope = Some(vec![shards]);
        Ok(self.digest_in(source, scope.clone())? == self.digest_in(target, scope)?)
    }

    /// Reads the map, changes it, proposes the result, and answers with the epoch it landed at.
    fn with_map(
        &self,
        change: impl FnOnce(&mut RangeMap) -> core::result::Result<(), String>,
    ) -> Result<u64> {
        let mut next = self.map();
        change(&mut next).map_err(ClusterError::Refused)?;
        self.propose(next)?;
        Ok(self.map().epoch)
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

    /// Puts a change to who is in the cluster, and **waits for it to take effect**.
    ///
    /// Waiting for the same reason a map change waits: two changes in a row are a sequence, and
    /// the second reads what the first decided. A membership has no epoch to watch, so what is
    /// waited on is the list itself - which is exact, because it is replaced whole.
    fn propose_members(&self, next: Vec<raft::Member>) -> Result<()> {
        let Some(controller) = &self.controller else {
            return Err(ClusterError::Refused(
                "this cluster runs no agreement, so its membership is whatever its file says"
                    .to_string(),
            ));
        };
        controller
            .propose_members(next.clone())
            .map_err(|e| ClusterError::Refused(e.to_string()))?;
        let deadline = Instant::now() + PROPOSAL_TIMEOUT;
        while Instant::now() < deadline {
            if self.members() == next {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        Err(ClusterError::Refused(
            "the membership change was proposed but has not been agreed; the cluster may have \
             lost its leader. `cluster topology` says where it stands"
                .to_string(),
        ))
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

/// What a move managed.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct MoveReport {
    pub range: RangeId,
    pub shards: String,
    pub from: String,
    pub to: String,
    /// How many fragments crossed the wire, over both passes.
    pub fragments: usize,
    /// Whether the source let go of what it no longer owns. `false` leaves records nobody
    /// reads: it costs space and answers nothing wrongly.
    pub dropped: bool,
    pub outcome: String,
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
