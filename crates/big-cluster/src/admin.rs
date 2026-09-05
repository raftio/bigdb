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
            if self.map().epoch >= epoch && self.settled() {
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
            Err(e) => {
                // Counted as well as said: an operator reads the sentence, a background loop
                // drops it, and the range it names silences every balancing step from here on.
                self.counters.move_cancel_failed();
                ClusterError::Refused(format!(
                    "{why} - and the move could not be called off either ({e}), so range {id} \
                     is still marked as moving. `cluster cancel {id}` clears it"
                ))
            }
        }
    }

    /// Abandons a move deliberately.
    pub fn cancel_move(&self, id: RangeId) -> Result<()> {
        self.with_map(|m| m.cancel_move(id).map_err(|e| e.to_string())).map(|_| ())
    }

    /// Whether everything this node appended has been agreed. `true` with no agreement at all,
    /// where there is nothing to be in flight.
    fn settled(&self) -> bool {
        self.controller.as_ref().is_none_or(|c| c.settled())
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
    /// election while it contributed nothing to one. With the balancer on, the agreement's
    /// leader admits it once it answers for data and holds the log to within the compaction
    /// margin; otherwise an operator does, with `admit`. Either way it is given nothing to
    /// serve until it counts.
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
        // **Settled, not merely visible.** A membership change takes effect when it is
        // appended, so the new list appears here while the entry is still in flight - and a
        // caller that stopped there would have its next proposal refused as busy.
        let deadline = Instant::now() + PROPOSAL_TIMEOUT;
        while Instant::now() < deadline {
            if self.members() == next && self.settled() {
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

    /// Hands the row-key namespace to another node.
    ///
    /// **The one change here that corrupts rather than fails.** Every other verb, done wrong,
    /// leaves a range unavailable or a report unhappy; this one, done wrong, hands two different
    /// strings the same row id - and a row id is what every bit in every fragment means. The
    /// engine refuses a contradicting assignment outright, so what it looks like afterwards is
    /// a write that will not land, for ever, on a key nobody can see is duplicated.
    ///
    /// So the order is the whole design:
    ///
    /// 1. **The keys go first**, every table's, and they are not scoped to a range - a row key
    ///    has to mean the same number in every shard, so the successor needs the whole mapping
    ///    or it will invent a second id for a string that already has one.
    /// 2. **The old leader stops.** It is told the epoch the move read and refuses to intern or
    ///    allocate until the map is past it. The window that opens here is one in which nobody
    ///    interns, and a write with a new key waits; the window it closes is one in which two
    ///    nodes do, which no write could ever wait out.
    /// 3. **The floor goes next.** A leader hands out record ids from a number held in memory
    ///    and not yet written anywhere; a successor that started from what is *on disk* would
    ///    hand out ids the old leader has already given away. This is the part that has no
    ///    second chance - the ids are gone by the time anybody notices - and it is read only
    ///    once nothing can still be raising it.
    /// 4. **Then the decision commits**, and only then does the successor answer.
    pub fn move_schema_leader(&self, to: &str) -> Result<()> {
        let target = self.node_named(to)?;
        let source = self.schema_leader();
        if source == target {
            return Err(ClusterError::Refused(format!("`{to}` already leads the schema")));
        }

        // 1. Every key of every table. The successor's schema has to exist first, or a key
        // naming a field it has never heard of has nowhere to land.
        self.match_schema(source, target)?;
        for table in self.pull_schema(source)?.0 {
            let keys = self.pull_keys(source, &table.name)?;
            self.push_keys(target, &table.name, keys)?;
        }

        // 2. The old leader stops. From here until the decision lands nobody interns, which
        // is the gap this move is allowed to have; the one it is not allowed to have is two
        // nodes interning at once, and a source still answering while its floor is read - or
        // after, to a coordinator holding the map from before the move - is exactly that. A
        // source that cannot be told stops the move: refused, not raced.
        let epoch = self.map().epoch;
        self.step_down(source, epoch)?;

        // 3. The floor: one past the highest id the old leader has handed out, whether or not
        // it has landed. Taken *after* the keys and *after* the source stopped, so it is a
        // number nothing is still adding to.
        let floors = self.allocation_floors(source)?;
        self.seed_floors(target, &floors)?;

        // 4. The decision. Ready at once: the keys and the floor went across above, by hand,
        // which is the whole of what a successor has to take over.
        self.with_map(|m| {
            m.schema_leader = target;
            m.schema_ready = true;
            Ok(())
        })
        .map(|_| ())
    }

    /// Finishes a handover the agreement decided: gives the successor every row key the
    /// survivors hold, then marks it ready.
    ///
    /// **Run by the agreement's leader, from every node it can reach.** The old leader is not
    /// among them - that is why there is a handover - so the mapping is rebuilt from the
    /// copies every owner took when it was told what a key meant. A key the old leader
    /// interned for a write that never landed anywhere is the one thing this cannot recover;
    /// the successor will hand that row id to a different string, and the deposed node, if
    /// it ever comes back, will refuse the contradiction when it is repaired. That is the
    /// residual risk of an automatic failover, and it is stated in `docs/clustering.md`
    /// rather than hidden.
    ///
    /// **A contradiction among the survivors stops it.** Two live nodes disagreeing about
    /// what a row id means is a cluster that has already diverged; marking the successor
    /// ready would let it pick one side and write on it. Nothing interns until somebody
    /// looks, and `big_cluster_schema_handover_blocked` is what makes them.
    pub fn finish_schema_handover(&self) -> Result<Option<String>> {
        let map = self.map();
        if map.schema_ready {
            return Ok(None);
        }
        let successor = map.schema_leader;
        let this = self.config.this_index();
        let outcome = self.hand_over_to(successor, this);
        self.handover_blocked.store(
            matches!(&outcome, Err(e) if !e.is_unreachable()),
            std::sync::atomic::Ordering::Relaxed,
        );
        outcome?;
        self.with_map(|m| {
            if m.schema_leader != successor {
                return Err("the namespace moved again while it was being handed over".into());
            }
            m.schema_ready = true;
            Ok(())
        })?;
        Ok(Some(self.name_of_agreed(successor)))
    }

    /// Every row key every reachable member holds, pushed to `successor`.
    fn hand_over_to(&self, successor: usize, this: usize) -> Result<()> {
        // The successor's schema has to exist first, or a key naming a field it has never
        // heard of has nowhere to land. Every node carries the same schema - DDL fans out -
        // so this node's is as good as any.
        self.match_schema(this, successor)?;
        let tables = self.pull_schema(this)?.0;
        for (i, member) in self.members().iter().enumerate() {
            if !member.reachable() || i == successor {
                continue;
            }
            for table in &tables {
                let keys = match self.pull_keys(i, &table.name) {
                    Ok(keys) => keys,
                    // A survivor that is not answering right now contributes nothing, and
                    // that is not a contradiction: the next pass will ask it again.
                    Err(e) if e.is_unreachable() => continue,
                    Err(e) => return Err(e),
                };
                if !keys.is_empty() {
                    self.push_keys(successor, &table.name, keys)?;
                }
            }
        }
        Ok(())
    }

    /// Tells a node to stop leading the schema until the map has moved past `epoch`.
    fn step_down(&self, node: usize, epoch: u64) -> Result<()> {
        if node == self.config.this_index() {
            self.stand_down_schema(epoch);
            return Ok(());
        }
        self.ask(node, path::SCHEMA_STEP_DOWN, &wire::put_u64_body(epoch), None).map(|_| ())
    }

    /// One past the highest record id a node has handed out for each table, landed or not.
    fn allocation_floors(&self, node: usize) -> Result<Vec<(String, RecordId)>> {
        if node == self.config.this_index() {
            return Ok(self.floors_here());
        }
        let bytes = self.ask(node, path::FLOORS, &[], None)?;
        self.read(node, || wire::get_floors(&bytes))
    }

    /// This node's own floors, which only the schema leader has anything in.
    pub fn floors_here(&self) -> Vec<(String, RecordId)> {
        let allocated = self.allocated.lock().unwrap_or_else(|e| e.into_inner());
        let map = self.map();
        let mut out: Vec<(String, RecordId)> = Vec::new();
        for table in self.schema() {
            // The greatest of what is on disk, what has been promised, and what the agreement
            // has been told may be promised. A leader that has never allocated still has a
            // floor - it is just the data's own high-water mark.
            let landed =
                self.api.max_record(&table.name).ok().flatten().map_or(0, |m| m.saturating_add(1));
            let promised = allocated.get(&table.name).copied().unwrap_or(0);
            let reserved = map.reserved_for(&table.name);
            out.push((table.name.clone(), landed.max(promised).max(reserved)));
        }
        out
    }

    /// Raises the agreement's ceiling on the record ids `table` may be handed, to `upto`.
    ///
    /// **Through the agreement's leader, whoever that is.** The schema leader and the
    /// agreement's leader are two roles that usually sit on two nodes, and only the second can
    /// propose - so this is one hop when they differ, amortised over a block of ids. Waits
    /// until this node has applied the result: a ceiling that has been proposed and not landed
    /// is one a successor could still start below.
    ///
    /// A cluster with no agreement has nothing to commit to and nothing to be succeeded by;
    /// its floor in memory is all there is, and all there needs to be.
    pub(super) fn reserve_ids(&self, table: &str, upto: RecordId) -> Result<()> {
        let Some(controller) = &self.controller else { return Ok(()) };
        if controller.is_leader() {
            return self.reserve_here(table, upto).map(|_| ());
        }
        let Some(leader) = controller.leader() else {
            return Err(ClusterError::Refused(
                "no node leads the agreement right now, so no record ids can be reserved; \
                 try again once an election has finished"
                    .to_string(),
            ));
        };
        let body = wire::put_floors(&[(table.to_string(), upto)]);
        self.ask(leader, path::RESERVE, &body, None)?;
        self.await_reserved(table, upto)
    }

    /// The agreement leader's half of `Cluster::reserve_ids`: commits the ceiling.
    ///
    /// Public because the peer route calls it. Answers the epoch it landed at.
    pub fn reserve_here(&self, table: &str, upto: RecordId) -> Result<u64> {
        self.with_map(|m| {
            m.reserve(table, upto);
            Ok(())
        })
    }

    /// Waits until this node's own map carries a ceiling of at least `upto` for `table`.
    fn await_reserved(&self, table: &str, upto: RecordId) -> Result<()> {
        let deadline = Instant::now() + PROPOSAL_TIMEOUT;
        while Instant::now() < deadline {
            if self.map().reserved_for(table) >= upto {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        Err(ClusterError::Refused(format!(
            "the agreement's leader took the reservation but it has not reached this node \
             after {}s; nothing was handed out",
            PROPOSAL_TIMEOUT.as_secs()
        )))
    }

    /// Starts a node's floors at least as high as these.
    fn seed_floors(&self, node: usize, floors: &[(String, RecordId)]) -> Result<()> {
        if node == self.config.this_index() {
            self.raise_floors(floors);
            return Ok(());
        }
        let body = wire::put_floors(floors);
        self.ask(node, path::FLOORS_PUT, &body, None).map(|_| ())
    }

    /// Raises this node's floors, never lowering one.
    ///
    /// Never lowering is what makes it safe to apply twice, and safe to apply to a node that
    /// has been allocating on its own: a floor is a promise about ids already handed out, and
    /// the higher of two promises is the one that keeps both.
    pub fn raise_floors(&self, floors: &[(String, RecordId)]) {
        let mut allocated = self.allocated.lock().unwrap_or_else(|e| e.into_inner());
        for (table, floor) in floors {
            let entry = allocated.entry(table.clone()).or_insert(0);
            *entry = (*entry).max(*floor);
        }
    }

    // ---------------------------------------------------------------------------------------
    // Balancing
    // ---------------------------------------------------------------------------------------

    /// What this node weighs, which is what the balancer decides from.
    ///
    /// Pages rather than records: pages are what a disk fills with, and the pager counts them
    /// already so asking costs nothing. The frontier is one past the highest record id this
    /// node holds, which is where a tail split is cut above.
    pub fn load(&self) -> crate::balance::NodeLoad {
        let pages = self.api.metrics().page_count;
        let frontier = self
            .schema()
            .iter()
            .filter_map(|t| self.api.max_record(&t.name).ok().flatten())
            .max()
            .map_or(0, |m| m.saturating_add(1));
        // This node holds its own log, whatever else is true of it.
        crate::balance::NodeLoad { pages: Some(pages), frontier, caught_up: true }
    }

    /// What every node weighs, in member order.
    ///
    /// A node that does not answer is `None` rather than zero, and the difference matters: an
    /// empty node and an unreachable one call for opposite actions.
    fn loads(&self) -> Vec<crate::balance::NodeLoad> {
        let members = self.members();
        (0..members.len())
            .map(|i| {
                if !members[i].reachable() {
                    return crate::balance::NodeLoad::default();
                }
                if i == self.config.this_index() {
                    return self.load();
                }
                // **A node that does not answer is `None`, not zero** - and it is counted,
                // because to the balancer it is a node that can neither give nor take, and a
                // transient timeout and a dead machine would otherwise be the same silence.
                match self
                    .ask(i, path::LOAD, &[], None)
                    .and_then(|b| self.read(i, || wire::get_load(&b)))
                {
                    Err(_) => {
                        self.counters.load_unanswered();
                        crate::balance::NodeLoad::default()
                    }
                    Ok((pages, frontier)) => crate::balance::NodeLoad {
                        pages: Some(pages),
                        frontier,
                        caught_up: self.controller.as_ref().is_some_and(|c| c.caught_up(i)),
                    },
                }
            })
            .collect()
    }

    /// Takes one balancing step, if the facts call for one.
    ///
    /// **One step, and the caller comes back.** `controller.rs` states the principle: *each
    /// move is a moment where a query can fail*. So this never chains two changes, and a
    /// cluster that needs three moves takes three calls - each one against facts gathered
    /// afresh, rather than against a plan made before the first move happened.
    pub fn rebalance(&self, policy: &crate::balance::Policy) -> Result<Option<Balanced>> {
        let (map, members, loads) = (self.map(), self.members(), self.loads());
        let Some(action) = crate::balance::plan(&map, &members, &loads, policy) else {
            return Ok(None);
        };
        let done = match action {
            crate::balance::Action::Admit { node } => {
                self.admit(&node).map(|_| Balanced::Admitted { node })
            }
            crate::balance::Action::SplitTail { at, to } => {
                self.split_range(at, Some(&to)).map(|_| Balanced::SplitTail { at, to })
            }
            crate::balance::Action::Move { range, to } => {
                self.move_range(range, &to).map(Balanced::Moved)
            }
        };
        // Counted here rather than by the caller, so the operator's one step and the steward's
        // many are one number - and so that a move whose source could not let go is a number
        // at all, which as a field on a report handed to a loop it was not.
        match &done {
            Ok(Balanced::Admitted { .. }) => self.counters.balance_admit(),
            Ok(Balanced::SplitTail { .. }) => self.counters.balance_split(),
            Ok(Balanced::Moved(report)) => {
                self.counters.balance_move();
                if !report.dropped {
                    self.counters.balance_drop_failed();
                }
            }
            Err(_) => self.counters.balance_error(),
        }
        done.map(Some)
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
            schema_leader: self.name_of_agreed(map.schema_leader),
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

/// What one balancing step did.
///
/// A value rather than a sentence, so that the caller who wants the sentence gets the same one
/// it always got and the caller who wants to count gets something to count. The move carries
/// its whole report: `dropped` in particular is a fact a background loop must not lose.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Balanced {
    /// A learner that had caught up was made a full member.
    Admitted { node: String },
    /// The tail was cut above everything written and the empty half given to a node with
    /// nothing. No bytes moved.
    SplitTail { at: big_engine::ShardId, to: String },
    /// A populated range was handed to another node.
    Moved(MoveReport),
}

impl Balanced {
    /// The sentence `POST /admin/cluster/rebalance` has always answered with.
    pub fn describe(&self) -> String {
        match self {
            Self::Admitted { node } => format!("admitted `{node}` as a full member"),
            Self::SplitTail { at, to } => format!("split the tail at {at} and gave it to `{to}`"),
            Self::Moved(r) => format!("moved shards {} to `{}`", r.shards, r.to),
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
