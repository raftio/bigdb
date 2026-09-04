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

//! Agreement, over the one thing that has to be agreed.
//!
//! **What is replicated here is the ownership map, and nothing else.** Not the facts, not the
//! row keys, not the schema - those have owners already and did not need a protocol. What
//! needed one is the single question static configuration could not answer: *which copy of
//! this range is the one to read from, now?* That is a value of a few dozen bytes that changes
//! when a machine dies, so the log this agrees on is short, cold, and never on the path of a
//! query.
//!
//! That framing is what makes hand-writing this defensible at all. A consensus protocol over
//! the write path would have to be fast, would have to snapshot, would have to compact, and
//! would be the largest and least tested thing in the tree. A consensus protocol over one
//! small value has none of those problems and still answers the question.
//!
//! **This module has no I/O and no clock.** Every decision is a function of the state, the
//! message and a timestamp handed in, and every effect comes back as a value. That is not a
//! stylistic choice: a leader election that can only be observed by starting five processes
//! and waiting is a leader election nobody can test, and the bugs here are all in the rules -
//! a vote granted to a stale log, a commit counted across terms - which are exactly the
//! things a deterministic test can pin down.
//!
//! The rules implemented are Raft's, in the shape the paper states them:
//!
//! - A message carrying a higher term makes this node a follower at that term, always.
//! - A message carrying a lower term is refused, always.
//! - A vote is granted at most once per term, and only to a log at least as up to date as
//!   this one.
//! - An append is refused unless the entry before it matches; a match truncates whatever
//!   disagreed.
//! - **A leader commits by counting replicas of an entry from its own term.** Counting an
//!   older entry's replicas is the classic way to commit something that a later leader then
//!   overwrites, and the no-op appended on election is what makes the first commit of a term
//!   reachable at all.

use big_engine::{ShardId, ShardRange};
use std::collections::{BTreeMap, BTreeSet};

/// A node's index in the cluster file. Stable for the life of a process, which is the only
/// life this protocol has: membership changes are a restart, like every other config change.
pub type NodeId = usize;
/// A range's own name, minted once and never reused. See [`Range::id`].
pub type RangeId = u64;
pub type Term = u64;
/// One past the last index of the log. Index 0 is the sentinel that is always agreed.
pub type Index = u64;

/// One contiguous span of the shard space, and who answers for it.
///
/// **The range has an identity of its own.** It used to be its primary's index in the cluster
/// file, which made "which range" and "which node" the same number - so a range could never
/// move to a node that did not already have one, and a node could never hold two. Splitting
/// them is what the rest of this is built on.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Range {
    /// Stable for the life of the range. A split retires nothing and mints one new id, so an
    /// id never means two different spans.
    pub id: RangeId,
    /// The shards this range covers, half-open, with an open end on the last one.
    pub shards: ShardRange,
    /// Every node holding this range. The primary is one of them.
    pub group: Vec<NodeId>,
    /// The node a read of this range goes to.
    pub primary: NodeId,
    /// A move in flight, if there is one. `None` on every path that is not rebalancing.
    pub moving: Option<Move>,
}

/// A range on its way from one node to another.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Move {
    pub target: NodeId,
    pub state: MoveState,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MoveState {
    /// The target is filling up, out of the write path entirely. Reads and writes are
    /// untouched, and abandoning it costs nothing.
    Seeding,
    /// Writes to this range are refused - retryably - while the last difference is copied.
    /// The only window in a move where anything is denied, and it is one range wide.
    Cutover,
}

/// Which node answers for which part of the space, and which copies are behind.
///
/// **The one value the agreement decides about data.** It used to be a vector of primaries
/// indexed by position in the cluster file; it is now the map itself, because a map that only
/// says *who* and never *what* cannot describe a range that moved.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct RangeMap {
    /// Bumped on every committed change. What a routed request carries so that an owner can
    /// refuse a request naming a range it no longer holds.
    pub epoch: u64,
    /// Disjoint and total, in ascending order of `shards.start`. Both are invariants, checked
    /// before any change is proposed - see [`RangeMap::check`].
    pub ranges: Vec<Range>,
    /// Copies that were unreachable at some point since they were last repaired, and may
    /// therefore have missed a write.
    ///
    /// **This is what lets a write survive a dead spare.** A write that could not reach a copy
    /// is allowed to stand, and the copy is recorded here; a copy recorded here is one the
    /// agreement will not promote. So nothing ever reads from a copy that is behind, and no
    /// write is refused because a machine nobody is reading from has died.
    ///
    /// Conservative on purpose: a node that was briefly unreachable and missed nothing is
    /// still marked, because the alternative is deciding whether a write happened during a
    /// window nobody was watching. A repair clears it.
    pub stale: Vec<NodeId>,
    /// The node that owns the row-key namespace. In the map rather than the file because
    /// draining the node holding it has to be able to move it.
    pub schema_leader: NodeId,
}

/// Why a proposed map was refused.
///
/// Every variant is a way the space would stop being covered exactly once. They are checked
/// before a change is proposed rather than after it is committed, because a committed map that
/// leaves a gap is a record id nobody answers for.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum MapError {
    Empty,
    NotStartingAtZero { start: ShardId },
    Unordered { at: usize },
    Overlap { from: ShardId, to: ShardId },
    Gap { from: ShardId, to: ShardId },
    NotTotal { from: ShardId },
    EmptyGroup { id: RangeId },
    PrimaryNotInGroup { id: RangeId, primary: NodeId },
    DuplicateId { id: RangeId },
    NoSuchRange { id: RangeId },
}

impl core::fmt::Display for MapError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Empty => write!(f, "a map with no ranges answers for no record id at all"),
            Self::NotStartingAtZero { start } => {
                write!(f, "the first range starts at {start}; shards 0..{start} have no owner")
            }
            Self::Unordered { at } => write!(f, "range {at} starts before the one before it"),
            Self::Overlap { from, to } => write!(
                f,
                "shards {from}..{to} have two owners; each would answer half of every query"
            ),
            Self::Gap { from, to } => write!(f, "shards {from}..{to} have no owner"),
            Self::NotTotal { from } => write!(
                f,
                "shards {from}.. have no owner; the last range must be open so that every \
                 record id a client can choose belongs to somebody"
            ),
            Self::EmptyGroup { id } => write!(f, "range {id} is held by nobody"),
            Self::PrimaryNotInGroup { id, primary } => {
                write!(f, "range {id} is served by node {primary}, which does not hold it")
            }
            Self::DuplicateId { id } => write!(f, "two ranges are both called {id}"),
            Self::NoSuchRange { id } => write!(f, "there is no range {id}"),
        }
    }
}

impl RangeMap {
    /// Whether this map covers the shard space exactly once, and every range is held.
    ///
    /// **Checked on every proposal rather than only at startup.** A file was read once and
    /// could be refused; a map changes while the cluster runs, and a change that leaves a gap
    /// is a record id that no node answers for and nothing to notice it afterwards.
    pub fn check(&self) -> core::result::Result<(), MapError> {
        let Some(first) = self.ranges.first() else { return Err(MapError::Empty) };
        if first.shards.start != 0 {
            return Err(MapError::NotStartingAtZero { start: first.shards.start });
        }

        let mut seen = BTreeSet::new();
        for r in &self.ranges {
            if !seen.insert(r.id) {
                return Err(MapError::DuplicateId { id: r.id });
            }
            if r.group.is_empty() {
                return Err(MapError::EmptyGroup { id: r.id });
            }
            if !r.group.contains(&r.primary) {
                return Err(MapError::PrimaryNotInGroup { id: r.id, primary: r.primary });
            }
        }

        for (i, pair) in self.ranges.windows(2).enumerate() {
            let (a, b) = (&pair[0], &pair[1]);
            if b.shards.start < a.shards.start {
                return Err(MapError::Unordered { at: i + 1 });
            }
            // Only the last range may be open, or everything after it is unreachable.
            let Some(end) = a.shards.end else {
                return Err(MapError::Overlap {
                    from: b.shards.start,
                    to: b.shards.end.unwrap_or(ShardId::MAX),
                });
            };
            match b.shards.start.cmp(&end) {
                core::cmp::Ordering::Equal => {}
                core::cmp::Ordering::Less => {
                    return Err(MapError::Overlap {
                        from: b.shards.start,
                        to: end.min(b.shards.end.unwrap_or(ShardId::MAX)),
                    })
                }
                core::cmp::Ordering::Greater => {
                    return Err(MapError::Gap { from: end, to: b.shards.start })
                }
            }
        }

        let last = self.ranges.last().expect("checked non-empty above");
        match last.shards.end {
            None => Ok(()),
            Some(end) => Err(MapError::NotTotal { from: end }),
        }
    }

    /// Which range a shard falls in.
    ///
    /// Total by construction: [`RangeMap::check`] refused any map that left a shard uncovered,
    /// so this returns an index rather than an option.
    pub fn range_of(&self, shard: ShardId) -> usize {
        // The last range that begins at or below `shard` is the one containing it.
        self.ranges.partition_point(|r| r.shards.start <= shard).saturating_sub(1)
    }

    pub fn len(&self) -> usize {
        self.ranges.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ranges.is_empty()
    }

    pub fn is_stale(&self, node: NodeId) -> bool {
        self.stale.contains(&node)
    }

    /// The position of a range by id, for a change that names one.
    pub fn position(&self, id: RangeId) -> Option<usize> {
        self.ranges.iter().position(|r| r.id == id)
    }

    /// Every range this node holds, whether it serves it or only copies it.
    pub fn held_by(&self, node: NodeId) -> Vec<usize> {
        (0..self.ranges.len()).filter(|i| self.ranges[*i].group.contains(&node)).collect()
    }

    /// Every range this node is the one to read from.
    pub fn served_by(&self, node: NodeId) -> Vec<usize> {
        (0..self.ranges.len()).filter(|i| self.ranges[*i].primary == node).collect()
    }

    /// The shards this node serves, which is what scopes its own share of a fan-out.
    pub fn shards_served_by(&self, node: NodeId) -> Vec<ShardRange> {
        self.ranges.iter().filter(|r| r.primary == node).map(|r| r.shards).collect()
    }

    /// One past the highest id in use, which is where the next range's id comes from.
    pub fn next_id(&self) -> RangeId {
        self.ranges.iter().map(|r| r.id).max().map_or(0, |m| m + 1)
    }

    // ---------------------------------------------------------------------------------------
    // Changing the shape of the space
    //
    // Every one of these returns a *new* map and leaves the old one alone, and every one is
    // checked before it is proposed. A change that got the shape wrong would be a record id
    // nobody answers for, and there is nothing downstream that would notice.
    // ---------------------------------------------------------------------------------------

    /// Cuts the range containing `at` in two, at `at`.
    ///
    /// **This moves no data.** Both halves stay where they were, held by the same nodes; all
    /// that changes is that there are now two names for what was one. Handing the upper half to
    /// somebody else is [`RangeMap::assign`], and it is a separate step because it is a
    /// separate question - whether that half is empty enough to hand over without a copy is a
    /// fact about the data, which this type does not have.
    ///
    /// Returns the new range's id.
    pub fn split(&mut self, at: ShardId) -> core::result::Result<RangeId, SplitError> {
        if self.ranges.is_empty() {
            return Err(SplitError::NoSuchRange { at });
        }
        let i = self.range_of(at);
        let existing = &self.ranges[i];
        // A cut on a boundary divides nothing: the shards below it are already a range.
        if at == existing.shards.start {
            return Err(SplitError::OnABoundary { at });
        }
        let id = self.next_id();
        let upper = Range {
            id,
            shards: ShardRange { start: at, end: existing.shards.end },
            group: existing.group.clone(),
            primary: existing.primary,
            // A range being moved is not a range to cut in half underneath the move.
            moving: None,
        };
        if existing.moving.is_some() {
            return Err(SplitError::Moving { id: existing.id });
        }
        self.ranges[i].shards.end = Some(at);
        self.ranges.insert(i + 1, upper);
        Ok(id)
    }

    /// Joins a range to the one after it, keeping the lower one's id.
    ///
    /// Refused unless the two are held by exactly the same nodes and served by the same one.
    /// Merging across owners would mean deciding which of them keeps the data, which is a move
    /// and not a merge - and doing it silently would drop half the records.
    pub fn merge(&mut self, id: RangeId) -> core::result::Result<(), MergeError> {
        let Some(i) = self.position(id) else { return Err(MergeError::NoSuchRange { id }) };
        let Some(next) = self.ranges.get(i + 1) else {
            return Err(MergeError::NothingAfter { id });
        };
        let (a, b) = (&self.ranges[i], next);
        if a.moving.is_some() || b.moving.is_some() {
            return Err(MergeError::Moving { id });
        }
        let (mut mine, mut theirs) = (a.group.clone(), b.group.clone());
        mine.sort_unstable();
        theirs.sort_unstable();
        if mine != theirs || a.primary != b.primary {
            return Err(MergeError::DifferentOwners { id, other: b.id });
        }
        self.ranges[i].shards.end = self.ranges[i + 1].shards.end;
        self.ranges.remove(i + 1);
        Ok(())
    }

    /// Records that a range is on its way to another node.
    ///
    /// **The target is deliberately not added to the group.** It is filling up, out of the
    /// write path entirely, and nothing reads from it - so abandoning the move costs nothing
    /// and a half-seeded copy is never mistaken for one that can answer.
    ///
    /// It is tempting to put it in the group and let the existing write path dual-write to it,
    /// which would make the final catch-up short. It is also wrong: a fragment is replaced
    /// whole, so a seed copy landing after a live write silently discards that write, and the
    /// two would then have equal counts and different contents - which the cardinality
    /// comparison a repair depends on cannot tell apart. Correctness lives in the barrier at
    /// cutover instead.
    pub fn begin_move(
        &mut self,
        id: RangeId,
        target: NodeId,
    ) -> core::result::Result<(), MoveError> {
        let Some(i) = self.position(id) else { return Err(MoveError::NoSuchRange { id }) };
        if self.ranges[i].moving.is_some() {
            return Err(MoveError::AlreadyMoving { id });
        }
        if self.ranges[i].group.contains(&target) {
            return Err(MoveError::AlreadyThere { id, target });
        }
        self.ranges[i].moving = Some(Move { target, state: MoveState::Seeding });
        Ok(())
    }

    /// Moves an in-flight move to its next state.
    pub fn set_move_state(
        &mut self,
        id: RangeId,
        state: MoveState,
    ) -> core::result::Result<(), MoveError> {
        let Some(i) = self.position(id) else { return Err(MoveError::NoSuchRange { id }) };
        let Some(moving) = self.ranges[i].moving.as_mut() else {
            return Err(MoveError::NotMoving { id });
        };
        moving.state = state;
        Ok(())
    }

    /// Hands the range to the node it was being moved to, and ends the move.
    ///
    /// **One entry.** The group, the primary and the end of the move all land together, so
    /// there is no committed state in which two nodes could both be asked for these records.
    pub fn finish_move(&mut self, id: RangeId) -> core::result::Result<(), MoveError> {
        let Some(i) = self.position(id) else { return Err(MoveError::NoSuchRange { id }) };
        let Some(moving) = self.ranges[i].moving.clone() else {
            return Err(MoveError::NotMoving { id });
        };
        self.ranges[i].group = vec![moving.target];
        self.ranges[i].primary = moving.target;
        self.ranges[i].moving = None;
        Ok(())
    }

    /// Abandons a move. Nothing was read from the target, so nothing is lost.
    ///
    /// A range that is not moving is a refusal rather than a no-op: an operator naming the
    /// wrong range wants to hear so, and "there was nothing to do" reads exactly like "done".
    pub fn cancel_move(&mut self, id: RangeId) -> core::result::Result<(), MoveError> {
        let Some(i) = self.position(id) else { return Err(MoveError::NoSuchRange { id }) };
        if self.ranges[i].moving.is_none() {
            return Err(MoveError::NotMoving { id });
        }
        self.ranges[i].moving = None;
        Ok(())
    }

    /// The move in flight on the range holding these shards, if there is one.
    pub fn moving_at(&self, shard: ShardId) -> Option<&Move> {
        self.ranges.get(self.range_of(shard))?.moving.as_ref()
    }

    /// Hands a range to a set of nodes, the first of which serves it.
    ///
    /// **The map only records the decision.** Whether those nodes hold the data yet is not
    /// something this type can know, so a caller that assigns a populated range to a node that
    /// has not been seeded has moved the answer without moving the facts. That is what the
    /// move protocol is for; this is the primitive underneath it.
    pub fn assign(
        &mut self,
        id: RangeId,
        group: Vec<NodeId>,
    ) -> core::result::Result<(), MapError> {
        let Some(i) = self.position(id) else { return Err(MapError::NoSuchRange { id }) };
        let Some(&primary) = group.first() else { return Err(MapError::EmptyGroup { id }) };
        self.ranges[i].group = group;
        self.ranges[i].primary = primary;
        Ok(())
    }
}

/// Why a move could not be started, advanced or finished.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum MoveError {
    NoSuchRange {
        id: RangeId,
    },
    AlreadyMoving {
        id: RangeId,
    },
    NotMoving {
        id: RangeId,
    },
    /// The target already holds this range, so there is nothing to move.
    AlreadyThere {
        id: RangeId,
        target: NodeId,
    },
}

impl core::fmt::Display for MoveError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NoSuchRange { id } => write!(f, "there is no range {id}"),
            Self::AlreadyMoving { id } => {
                write!(f, "range {id} is already being moved; wait for it or cancel it")
            }
            Self::NotMoving { id } => write!(f, "range {id} is not being moved"),
            Self::AlreadyThere { id, target } => {
                write!(f, "node {target} already holds range {id}")
            }
        }
    }
}

/// Why a range could not be cut.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum SplitError {
    NoSuchRange {
        at: ShardId,
    },
    /// The cut is where a range already begins, so it divides nothing.
    OnABoundary {
        at: ShardId,
    },
    /// A range on its way to another node. Cutting it underneath the move would leave two
    /// halves and one move that names neither.
    Moving {
        id: RangeId,
    },
}

impl core::fmt::Display for SplitError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NoSuchRange { at } => write!(f, "no range holds shard {at}"),
            Self::OnABoundary { at } => {
                write!(f, "a range already begins at {at}, so cutting there divides nothing")
            }
            Self::Moving { id } => {
                write!(f, "range {id} is being moved; wait for that to finish or cancel it")
            }
        }
    }
}

/// Why two ranges could not be joined.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum MergeError {
    NoSuchRange {
        id: RangeId,
    },
    /// The last range has nothing after it to join to.
    NothingAfter {
        id: RangeId,
    },
    Moving {
        id: RangeId,
    },
    /// Two ranges on different nodes. Joining them would mean deciding which node's records
    /// survive, which is a move rather than a merge.
    DifferentOwners {
        id: RangeId,
        other: RangeId,
    },
}

impl core::fmt::Display for MergeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NoSuchRange { id } => write!(f, "there is no range {id}"),
            Self::NothingAfter { id } => {
                write!(f, "range {id} is the last one; there is nothing after it to join")
            }
            Self::Moving { id } => write!(f, "range {id} or the one after it is being moved"),
            Self::DifferentOwners { id, other } => write!(
                f,
                "ranges {id} and {other} are held by different nodes; joining them would mean \
                 deciding which one's records survive, which is a move and not a merge"
            ),
        }
    }
}

/// One node, as the agreement understands it.
///
/// **A slot is never reused.** A node that leaves keeps its index at [`MemberState::Gone`],
/// so a `NodeId` means one machine for the life of the cluster and every vector indexed by one
/// stays valid. Reusing a slot would hand a new machine the reputation of the old one -
/// including a behind-mark it never earned.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Member {
    pub name: String,
    pub addr: String,
    pub state: MemberState,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MemberState {
    /// Receiving the log but not voting, and holding no range yet. A node catching up must not
    /// count towards a majority: it would raise the bar for every election while contributing
    /// nothing to one.
    Learner,
    /// A full member: votes, counts towards a majority, may hold ranges.
    Voter,
    /// On its way out. Still votes and still coordinates - which is what keeps the one address
    /// a client holds from going dark mid-drain - but the balancer moves its ranges away and
    /// gives it no new ones.
    Draining,
    /// Left. Keeps its slot so that no `NodeId` ever means two machines.
    Gone,
}

impl Member {
    /// Whether this node counts towards a majority.
    pub fn votes(&self) -> bool {
        matches!(self.state, MemberState::Voter | MemberState::Draining)
    }

    /// Whether the agreement still sends to it at all.
    pub fn reachable(&self) -> bool {
        !matches!(self.state, MemberState::Gone)
    }

    /// Whether the balancer may place a range here.
    pub fn takes_ranges(&self) -> bool {
        matches!(self.state, MemberState::Voter)
    }
}

/// What one log entry decides.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Decision {
    /// Decides nothing. Appended by a new leader so that it has an entry of its own term to
    /// commit, which is what makes everything before it committable.
    Noop,
    /// From now on, the space is divided like this and these nodes answer for it.
    ///
    /// **Applied when it commits.** Routing a read to a node before a majority agreed it owns
    /// the range would be answering from a node nobody has acknowledged.
    Ranges(RangeMap),
    /// From now on, these nodes are the cluster.
    ///
    /// **Applied when it is appended, not when it commits** - the rule Raft states for a
    /// configuration change, because the majority that commits the entry has to be the one the
    /// entry describes. That is why it is a separate variant from [`Decision::Ranges`] rather
    /// than a field beside it: the two need opposite rules.
    Members(Vec<Member>),
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Entry {
    pub term: Term,
    pub decision: Decision,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Role {
    Follower,
    Candidate,
    Leader,
}

/// One message between nodes. Small enough to fit a request body with room to spare.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Message {
    RequestVote {
        term: Term,
        candidate: NodeId,
        last_index: Index,
        last_term: Term,
    },
    VoteReply {
        term: Term,
        from: NodeId,
        granted: bool,
    },
    Append {
        term: Term,
        leader: NodeId,
        prev_index: Index,
        prev_term: Term,
        entries: Vec<Entry>,
        commit: Index,
    },
    AppendReply {
        term: Term,
        from: NodeId,
        success: bool,
        match_index: Index,
    },
    /// The state machine as of one index, for a follower that has fallen behind what the
    /// leader still holds.
    ///
    /// **Only reachable after a compaction.** A leader keeps every entry a live follower still
    /// needs, so this is for one that was away long enough to be dropped past - and for that
    /// one there is nothing to send but the answer itself.
    Snapshot {
        term: Term,
        leader: NodeId,
        /// The index and term this state is as of. It becomes the follower's base.
        index: Index,
        last_term: Term,
        ranges: RangeMap,
        members: Vec<Member>,
    },
}

/// How long this node waits before doing anything about silence.
#[derive(Clone, Copy, Debug)]
pub struct Timing {
    /// Shortest time without hearing from a leader before standing for election.
    pub election_min: u64,
    /// The window the randomised part of the timeout falls in. Two nodes that time out
    /// together split the vote and neither wins, so the spread is what makes an election end.
    pub election_spread: u64,
    /// How often a leader sends an append, empty or not. Well under `election_min`, or the
    /// followers depose a leader that is doing its job.
    pub heartbeat: u64,
}

/// Milliseconds. Deliberately unhurried: this protocol decides who serves a range after a
/// machine has died, and paying an extra second to be sure costs less than an election held
/// because a garbage collector paused.
///
/// Named constants rather than literals in `Default` so that the relationships between them
/// can be checked below. Numbers whose *ratios* are what makes them safe should not be three
/// literals nobody compares.
pub const ELECTION_MIN_MS: u64 = 1_500;
pub const ELECTION_SPREAD_MS: u64 = 1_500;
pub const HEARTBEAT_MS: u64 = 400;

// A leader that cannot heartbeat several times inside the shortest election timeout is a
// leader its followers depose while it is doing its job - and every deposition is a window
// where a range is not served. Three is the usual margin and the one this assumes.
const _: () = assert!(HEARTBEAT_MS * 3 <= ELECTION_MIN_MS);
// Without a spread, two nodes time out together, split the vote, and do it again. The
// randomised part is the whole reason an election ends.
const _: () = assert!(ELECTION_SPREAD_MS > 0);

impl Default for Timing {
    fn default() -> Self {
        Self {
            election_min: ELECTION_MIN_MS,
            election_spread: ELECTION_SPREAD_MS,
            heartbeat: HEARTBEAT_MS,
        }
    }
}

/// Everything a step produced. The caller does the I/O; this module does the deciding.
#[derive(Default, Debug)]
pub struct Output {
    /// Messages to send, each to one node.
    pub send: Vec<(NodeId, Message)>,
    /// Whether term, vote or log changed, and therefore has to reach the disk **before** any
    /// of `send` reaches the network. A vote that is sent and then forgotten in a restart is
    /// two votes in one term, which is two leaders.
    pub persist: bool,
    /// Decisions that are now committed, in order, for the caller to apply.
    pub applied: Vec<Decision>,
}

impl Output {
    fn to(&mut self, node: NodeId, m: Message) {
        self.send.push((node, m));
    }
}

/// One node's view of the agreement.
pub struct Raft {
    id: NodeId,
    /// The cluster this node was started with, before the log said otherwise.
    ///
    /// Only ever consulted when no `Decision::Members` has been appended: the file seeds the
    /// membership exactly as it seeds the map, and a decision replaces it.
    seed: Vec<Member>,
    /// Every node the agreement replicates to, this node included. **Derived from the log**,
    /// never mutated in place - see [`Raft::refresh_members`].
    members: Vec<Member>,
    /// The subset of `members` whose agreement counts. A learner replicates and does not vote.
    voters: Vec<NodeId>,
    timing: Timing,

    // --- persistent: none of this may be lost in a restart ---
    term: Term,
    voted_for: Option<NodeId>,
    /// The suffix of the log this node still holds.
    ///
    /// `log[0]` is the entry at [`Raft::base`] - the sentinel every node agrees on without
    /// being told, until a compaction replaces it with the last entry that was dropped.
    /// Everything before it has been folded into the state machine and thrown away.
    log: Vec<Entry>,
    /// The index of `log[0]`. Zero until something has been compacted away.
    ///
    /// **Log positions are not vector positions.** Every index below is a position in the
    /// *log*, and reaching the vector means subtracting this.
    base: Index,

    // --- volatile ---
    role: Role,
    leader: Option<NodeId>,
    commit: Index,
    applied: Index,
    votes: BTreeSet<NodeId>,
    next: BTreeMap<NodeId, Index>,
    matched: BTreeMap<NodeId, Index>,

    // --- clocks, all in the caller's milliseconds ---
    election_at: u64,
    heartbeat_at: u64,
    /// When each member was last heard from. The failure detector, and it costs nothing: a
    /// protocol that already heartbeats knows who is answering.
    heard: BTreeMap<NodeId, u64>,
}

impl Raft {
    /// A node in a cluster described by a file, before the log has said anything.
    ///
    /// `members` is the seed. Every entry the log carries replaces it, which is what makes a
    /// restart come back with the cluster as it is rather than as the file remembers it.
    pub fn new(id: NodeId, members: Vec<Member>, timing: Timing, now: u64) -> Self {
        let voters = voters_of(&members);
        let mut raft = Self {
            id,
            seed: members.clone(),
            members,
            voters,
            timing,
            term: 0,
            voted_for: None,
            log: vec![Entry { term: 0, decision: Decision::Noop }],
            base: 0,
            role: Role::Follower,
            leader: None,
            commit: 0,
            applied: 0,
            votes: BTreeSet::new(),
            next: BTreeMap::new(),
            matched: BTreeMap::new(),
            election_at: 0,
            heartbeat_at: 0,
            heard: BTreeMap::new(),
        };
        raft.reset_election(now);
        raft
    }

    /// Restores what was on disk. The three fields that may not be lost, and nothing else:
    /// everything volatile is rebuilt by the first heartbeat.
    /// Restores what was on disk, and the commit point with it.
    ///
    /// **The commit point matters.** The state machine is rebuilt by replaying the log, and a
    /// node that came up believing nothing was committed would replay entries no majority ever
    /// agreed to - which, for a map of who owns what, is routing a read to a node nobody has
    /// acknowledged.
    pub fn restore(&mut self, state: State) {
        self.term = state.term;
        self.voted_for = state.voted_for;
        if !state.log.is_empty() {
            self.log = state.log;
        }
        // **Before the commit index is used**, because who is in the cluster is a property of
        // the log this node came back holding - not of the file it was first started with,
        // which may describe a cluster that no longer exists.
        self.refresh_members();
        // Clamped: a commit index past the log is a file that disagrees with itself, and
        // trusting it would index past the end.
        self.base = state.base;
        self.commit = state.commit.clamp(self.base, self.last_index());
        // Everything at or below the base is already folded into `log[0]`, so replaying starts
        // there. One short, so that the sentinel itself is handed to the caller.
        self.applied = self.base.saturating_sub(1);
    }

    /// Everything this node needs written down, as one value.
    pub fn state(&self) -> State {
        State {
            term: self.term,
            voted_for: self.voted_for,
            commit: self.commit,
            base: self.base,
            log: self.log.clone(),
        }
    }

    pub fn id(&self) -> NodeId {
        self.id
    }

    /// How many nodes the agreement replicates to, this one included.
    pub fn members(&self) -> usize {
        self.members.len()
    }

    /// The nodes whose agreement counts, which is not every node it replicates to.
    pub fn voters(&self) -> &[NodeId] {
        &self.voters
    }

    /// The cluster as the log describes it.
    pub fn membership(&self) -> &[Member] {
        &self.members
    }

    /// Whether this node's own vote counts. A learner replicates without voting, and a node
    /// that has left keeps neither.
    fn i_vote(&self) -> bool {
        self.voters.contains(&self.id)
    }

    /// Every node this one sends to: everybody but itself that has not left.
    fn peers(&self) -> Vec<NodeId> {
        (0..self.members.len()).filter(|n| *n != self.id && self.members[*n].reachable()).collect()
    }

    /// How many committed entries a leader keeps beyond what every follower has stored.
    ///
    /// The point of a margin at all: a follower a heartbeat or two behind is served from the
    /// log, which is cheap, rather than from a snapshot, which is the whole map.
    pub const KEEP_ENTRIES: u64 = 64;

    /// Throws away the prefix of the log every voter has already stored.
    ///
    /// **The log is short and cold, but it is not bounded.** One entry per election, one per
    /// machine that dies - and, once a balancer is proposing, one per decision it makes. Every
    /// entry carries a whole map, and the whole log is rewritten on every persist, so an
    /// unbounded log makes each persist slower until heartbeats are late and elections start
    /// happening for no reason.
    ///
    /// **Never past what a follower still needs.** Compaction is best effort on purpose: a
    /// follower that is down holds the base where it is, which costs some disk and keeps
    /// recovery cheap. A follower that has fallen behind anyway is sent a snapshot.
    ///
    /// `keep` entries are left beyond the safe point so that the common case - a follower a
    /// heartbeat or two behind - is still served from the log rather than from a snapshot.
    pub fn compact(&mut self, keep: u64) {
        if self.role != Role::Leader {
            return;
        }
        let safe = self
            .voters
            .iter()
            .map(|n| self.matched.get(n).copied().unwrap_or(0))
            .min()
            .unwrap_or(0)
            .min(self.applied);
        let target = safe.saturating_sub(keep);
        if target <= self.base {
            return;
        }
        let drop = (target - self.base) as usize;
        self.log.drain(..drop);
        self.base = target;
    }

    /// The state machine as this node has applied it, for a snapshot.
    ///
    /// Replayed from the log rather than held alongside it, because the log is the only thing
    /// that is written down - and a second copy is a second thing to get out of step.
    fn applied_state(&self) -> (RangeMap, Vec<Member>) {
        let mut ranges = RangeMap::default();
        let mut members = self.seed.clone();
        for (at, entry) in self.log.iter().enumerate() {
            if self.base + at as Index > self.applied {
                break;
            }
            match &entry.decision {
                Decision::Ranges(m) => ranges = m.clone(),
                Decision::Members(ms) => members = ms.clone(),
                Decision::Noop => {}
            }
        }
        (ranges, members)
    }

    /// Recomputes the membership from the log.
    ///
    /// **Derived rather than mutated, and that is what makes it correct.** A configuration
    /// change applies when it is *appended*, because the majority that commits an entry has to
    /// be the one the entry describes - but an appended entry can be truncated away by a leader
    /// with a better log, and a membership mutated in place would have no way back. Recomputing
    /// from the log makes the undo free, and makes a restart that replays its log land in the
    /// same place for the same reason.
    ///
    /// The log is short and cold - one entry per election, one per machine that dies, one per
    /// deliberate change - so walking it is cheaper than the bookkeeping that would avoid it.
    fn refresh_members(&mut self) {
        let latest = self.log.iter().rev().find_map(|e| match &e.decision {
            Decision::Members(ms) => Some(ms.clone()),
            _ => None,
        });
        self.members = latest.unwrap_or_else(|| self.seed.clone());
        self.voters = voters_of(&self.members);
    }

    pub fn term(&self) -> Term {
        self.term
    }

    pub fn role(&self) -> Role {
        self.role
    }

    pub fn is_leader(&self) -> bool {
        self.role == Role::Leader
    }

    /// Who this node believes is leading, which is not necessarily who is.
    pub fn leader(&self) -> Option<NodeId> {
        self.leader
    }

    pub fn voted_for(&self) -> Option<NodeId> {
        self.voted_for
    }

    pub fn log(&self) -> &[Entry] {
        &self.log
    }

    pub fn commit_index(&self) -> Index {
        self.commit
    }

    /// When this node last heard anything from `node`.
    pub fn last_heard(&self, node: NodeId) -> Option<u64> {
        self.heard.get(&node).copied()
    }

    pub fn last_index(&self) -> Index {
        self.base + self.log.len() as Index - 1
    }

    fn last_term(&self) -> Term {
        self.log.last().map_or(0, |e| e.term)
    }

    /// The index of the oldest entry this node still holds.
    pub fn base(&self) -> Index {
        self.base
    }

    /// The term of an entry, or `None` when it is off either end of what is held.
    ///
    /// `None` below the base is not "no such entry" - it is "compacted away", and the only
    /// caller that can reach it is a leader deciding what to send a follower that has fallen
    /// behind the base. That caller sends a snapshot instead.
    fn term_at(&self, index: Index) -> Option<Term> {
        let at = index.checked_sub(self.base)?;
        self.log.get(at as usize).map(|e| e.term)
    }

    /// The entries from `from` onward, empty when `from` is past the end.
    fn entries_from(&self, from: Index) -> Vec<Entry> {
        match from.checked_sub(self.base) {
            None => Vec::new(),
            Some(at) => self.log.get(at as usize..).map(<[Entry]>::to_vec).unwrap_or_default(),
        }
    }

    /// **Of the voters, not of everybody replicated to.** A learner catching up must not raise
    /// the bar for an election while contributing nothing to one.
    fn majority(&self) -> usize {
        self.voters.len() / 2 + 1
    }

    /// The randomised election timeout.
    ///
    /// Derived from the node and the term rather than from a random number generator: two
    /// nodes must not wait the same length of time, and a test must be able to say what will
    /// happen. Mixing the term in means a split vote does not repeat itself at the same
    /// offsets in the next term, which is the property randomness was there for.
    fn timeout(&self) -> u64 {
        let mut h = (self.id as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ self.term;
        h ^= h >> 33;
        h = h.wrapping_mul(0xff51_afd7_ed55_8ccd);
        h ^= h >> 29;
        self.timing.election_min + h % self.timing.election_spread.max(1)
    }

    fn reset_election(&mut self, now: u64) {
        self.election_at = now + self.timeout();
    }

    /// A higher term is not an opinion. Whatever this node was doing, it is a follower now.
    fn observe(&mut self, term: Term, out: &mut Output) {
        if term > self.term {
            self.term = term;
            self.voted_for = None;
            self.role = Role::Follower;
            self.leader = None;
            self.votes.clear();
            out.persist = true;
        }
    }

    /// The clock moved. Everything time-driven happens here and nowhere else.
    pub fn tick(&mut self, now: u64) -> Output {
        let mut out = Output::default();
        match self.role {
            Role::Leader => {
                if now >= self.heartbeat_at {
                    self.heartbeat_at = now + self.timing.heartbeat;
                    // **Bounded, or every persist gets slower.** The whole log is rewritten
                    // each time anything is written down and every entry carries a whole map,
                    // so a log that only ever grows eventually makes a heartbeat late - and a
                    // late heartbeat is an election nobody needed.
                    self.compact(Self::KEEP_ENTRIES);
                    for peer in self.peers() {
                        if peer != self.id {
                            self.send_append(peer, &mut out);
                        }
                    }
                }
            }
            Role::Follower | Role::Candidate => {
                if now >= self.election_at {
                    self.stand(now, &mut out);
                }
            }
        }
        out
    }

    /// Stands for election in the next term.
    ///
    /// A single-member cluster wins here and now, which is not a special case so much as the
    /// general one with a majority of one - and it is what lets a node that is alone in its
    /// config file work at all.
    ///
    /// **A node that does not vote does not stand.** A learner is still catching up and a node
    /// that has left is not in the cluster at all; either one campaigning would raise the term
    /// on every node that heard it and depose a leader that was doing its job, once per
    /// election timeout, for as long as it was running.
    fn stand(&mut self, now: u64, out: &mut Output) {
        if !self.i_vote() {
            self.reset_election(now);
            return;
        }
        self.term += 1;
        self.role = Role::Candidate;
        self.voted_for = Some(self.id);
        self.votes = BTreeSet::from([self.id]);
        self.leader = None;
        out.persist = true;
        self.reset_election(now);

        if self.votes.len() >= self.majority() {
            self.win(now, out);
            return;
        }
        for peer in self.peers() {
            {
                out.to(
                    peer,
                    Message::RequestVote {
                        term: self.term,
                        candidate: self.id,
                        last_index: self.last_index(),
                        last_term: self.last_term(),
                    },
                );
            }
        }
    }

    fn win(&mut self, now: u64, out: &mut Output) {
        self.role = Role::Leader;
        self.leader = Some(self.id);
        self.next.clear();
        self.matched.clear();
        for peer in self.peers() {
            self.next.insert(peer, self.last_index() + 1);
            self.matched.insert(peer, 0);
        }
        self.matched.insert(self.id, self.last_index());

        // The entry that makes everything before it committable. Without it a leader whose
        // term produced no decisions can never count a majority for an entry of its own term,
        // and entries from earlier terms may not be committed by counting - so a cluster that
        // elects a leader and is then idle would never finish applying what it already agreed.
        self.log.push(Entry { term: self.term, decision: Decision::Noop });
        self.matched.insert(self.id, self.last_index());
        out.persist = true;

        self.heartbeat_at = now + self.timing.heartbeat;
        for peer in self.peers() {
            if peer != self.id {
                self.send_append(peer, out);
            }
        }
        self.advance_commit(out);
    }

    /// Proposes a decision. `None` when this node is not the leader, because a proposal has to
    /// go through one.
    /// Appends one decision of this node's own, as the leader.
    ///
    /// **A configuration change takes effect here, before it commits**, which is the rule Raft
    /// states for one: the majority that commits the entry has to be the majority the entry
    /// describes, or a leader could commit a change using a quorum that the change abolishes.
    /// Every other decision waits for the commit, because routing a read to a node no majority
    /// has acknowledged is answering from a node nobody agreed on.
    pub fn propose(&mut self, decision: Decision) -> Option<Output> {
        if self.role != Role::Leader {
            return None;
        }
        let mut out = Output { persist: true, ..Default::default() };
        let carries_members = matches!(decision, Decision::Members(_));
        self.log.push(Entry { term: self.term, decision });
        // Before the appends go out, so that this node is already counting the majority the
        // change describes rather than the one it replaces.
        if carries_members {
            self.refresh_members();
        }
        self.matched.insert(self.id, self.last_index());
        for peer in self.peers() {
            self.send_append(peer, &mut out);
        }
        self.advance_commit(&mut out);
        Some(out)
    }

    fn send_append(&mut self, peer: NodeId, out: &mut Output) {
        let next = self.next.get(&peer).copied().unwrap_or(self.last_index() + 1);
        let prev_index = next.saturating_sub(1);
        // **Behind what this leader still holds.** There is no prefix left to match against, so
        // the only thing that can help is the answer itself.
        if prev_index < self.base {
            let (ranges, members) = self.applied_state();
            out.to(
                peer,
                Message::Snapshot {
                    term: self.term,
                    leader: self.id,
                    index: self.applied,
                    last_term: self.term_at(self.applied).unwrap_or(self.term),
                    ranges,
                    members,
                },
            );
            return;
        }
        let prev_term = self.term_at(prev_index).unwrap_or(0);
        let entries = self.entries_from(next);
        out.to(
            peer,
            Message::Append {
                term: self.term,
                leader: self.id,
                prev_index,
                prev_term,
                entries,
                commit: self.commit,
            },
        );
    }

    /// One message from another node.
    pub fn deliver(&mut self, m: Message, now: u64) -> Output {
        let mut out = Output::default();
        match m {
            Message::RequestVote { term, candidate, last_index, last_term } => {
                self.heard.insert(candidate, now);
                self.observe(term, &mut out);
                let granted = self.grant(term, candidate, last_index, last_term);
                if granted {
                    self.voted_for = Some(candidate);
                    out.persist = true;
                    // Only a granted vote resets the timer. Resetting it on every request would
                    // let a node that keeps standing and keeps losing hold off a healthy
                    // election indefinitely.
                    self.reset_election(now);
                }
                out.to(candidate, Message::VoteReply { term: self.term, from: self.id, granted });
            }

            // **A state machine handed over whole**, for a follower the leader has compacted
            // past. There is no prefix to match against, so nothing is merged: the log becomes
            // one sentinel at the snapshot's index and the state is taken as given.
            Message::Snapshot { term, leader, index, last_term, ranges, members } => {
                self.heard.insert(leader, now);
                self.observe(term, &mut out);
                if term < self.term || index <= self.base {
                    // Older than this node's own base is a snapshot it has already passed.
                    out.to(
                        leader,
                        Message::AppendReply {
                            term: self.term,
                            from: self.id,
                            success: false,
                            match_index: self.last_index(),
                        },
                    );
                    return out;
                }
                self.role = Role::Follower;
                self.leader = Some(leader);
                self.reset_election(now);
                self.log = vec![Entry { term: last_term, decision: Decision::Ranges(ranges) }];
                self.base = index;
                self.commit = index;
                // Applied one short of the base, so that the sentinel itself is handed to the
                // caller - that entry *is* the state, and nothing else carries it.
                self.applied = index.saturating_sub(1);
                self.seed = members;
                self.refresh_members();
                self.apply(&mut out);
                out.persist = true;
                out.to(
                    leader,
                    Message::AppendReply {
                        term: self.term,
                        from: self.id,
                        success: true,
                        match_index: self.last_index(),
                    },
                );
            }

            Message::VoteReply { term, from, granted } => {
                self.heard.insert(from, now);
                self.observe(term, &mut out);
                // A reply from an election this node has moved on from decides nothing.
                if self.role == Role::Candidate && term == self.term && granted {
                    self.votes.insert(from);
                    if self.votes.len() >= self.majority() {
                        self.win(now, &mut out);
                    }
                }
            }

            Message::Append { term, leader, prev_index, prev_term, entries, commit } => {
                self.heard.insert(leader, now);
                self.observe(term, &mut out);
                if term < self.term {
                    out.to(
                        leader,
                        Message::AppendReply {
                            term: self.term,
                            from: self.id,
                            success: false,
                            match_index: 0,
                        },
                    );
                    return out;
                }

                // A leader of this term exists, so this node is not standing for election in it.
                self.role = Role::Follower;
                self.leader = Some(leader);
                self.reset_election(now);

                if self.term_at(prev_index) != Some(prev_term) {
                    out.to(
                        leader,
                        Message::AppendReply {
                            term: self.term,
                            from: self.id,
                            success: false,
                            match_index: 0,
                        },
                    );
                    return out;
                }

                // Everything after the matching point is the leader's to decide. Entries that
                // agree are left alone rather than rewritten, so a heartbeat carrying a repeat
                // of what is already there does not truncate a log that is ahead of `commit`.
                let mut index = prev_index;
                let mut membership_moved = false;
                for entry in entries {
                    index += 1;
                    let carries_members = matches!(entry.decision, Decision::Members(_));
                    match self.term_at(index) {
                        Some(t) if t == entry.term => continue,
                        Some(_) => {
                            // **The truncation is where a membership can go backwards.** A
                            // leader with a better log can take away an entry this node already
                            // applied, and a membership mutated in place would have no way
                            // back - so it is recomputed from the log below instead.
                            let at = (index - self.base) as usize;
                            membership_moved |= self.log[at..]
                                .iter()
                                .any(|e| matches!(e.decision, Decision::Members(_)));
                            self.log.truncate(at);
                            self.log.push(entry);
                            out.persist = true;
                            membership_moved |= carries_members;
                        }
                        None => {
                            self.log.push(entry);
                            out.persist = true;
                            membership_moved |= carries_members;
                        }
                    }
                }
                if membership_moved {
                    self.refresh_members();
                }

                if commit > self.commit {
                    self.commit = commit.min(self.last_index());
                    self.apply(&mut out);
                }
                out.to(
                    leader,
                    Message::AppendReply {
                        term: self.term,
                        from: self.id,
                        success: true,
                        match_index: index,
                    },
                );
            }

            Message::AppendReply { term, from, success, match_index } => {
                self.heard.insert(from, now);
                self.observe(term, &mut out);
                if self.role != Role::Leader || term != self.term {
                    return out;
                }
                if success {
                    self.matched.insert(from, match_index);
                    self.next.insert(from, match_index + 1);
                    self.advance_commit(&mut out);
                } else {
                    // Back up one and try again. Slower than the paper's optimisation and
                    // enough: a follower is behind by the entries of one failed term, and this
                    // log holds one entry per machine that has died.
                    let next = self.next.entry(from).or_insert(1);
                    *next = (*next).saturating_sub(1).max(1);
                    self.send_append(from, &mut out);
                }
            }
        }
        out
    }

    /// The highest index a majority holds, **of this term**.
    ///
    /// The term check is the one that matters. Counting replicas of an entry from an earlier
    /// term can commit something a later leader is still entitled to overwrite, which is the
    /// bug that Raft's figure 8 exists to show.
    fn advance_commit(&mut self, out: &mut Output) {
        let mut candidate = self.commit;
        for index in (self.commit + 1)..=self.last_index() {
            if self.term_at(index) != Some(self.term) {
                continue;
            }
            // **Voters, not everybody replicated to.** A learner holding the entry is not
            // agreement about it: it does not vote, so counting it would let a leader commit
            // on the strength of nodes that could never have elected it.
            let replicas = self
                .voters
                .iter()
                .filter(|m| self.matched.get(m).is_some_and(|x| *x >= index))
                .count();
            if replicas >= self.majority() {
                candidate = index;
            }
        }
        if candidate > self.commit {
            self.commit = candidate;
            self.apply(out);
        }
    }

    fn apply(&mut self, out: &mut Output) {
        while self.applied < self.commit {
            self.applied += 1;
            if let Some(at) = self.applied.checked_sub(self.base) {
                if let Some(entry) = self.log.get(at as usize) {
                    out.applied.push(entry.decision.clone());
                }
            }
        }
    }

    /// Raft's vote rule, in one place: at most one per term, and never to a log that is behind.
    ///
    /// "Behind" is by last term first and length second. Length alone would let a node with a
    /// long log of entries nobody committed win over one with the entries that were.
    fn grant(&self, term: Term, candidate: NodeId, last_index: Index, last_term: Term) -> bool {
        if term < self.term {
            return false;
        }
        // **A candidate that is not a voter here gets nothing.** A node this cluster has
        // removed can still be running and still campaigning; granting it a vote would let a
        // machine nobody has agreed to become the leader of a cluster it has left.
        if !self.voters.contains(&candidate) {
            return false;
        }
        if self.voted_for.is_some_and(|v| v != candidate) {
            return false;
        }
        last_term > self.last_term()
            || (last_term == self.last_term() && last_index >= self.last_index())
    }
}

/// Where the three things that may not be lost are kept.
///
/// A vote that is sent and then forgotten in a restart is two votes in one term, which is two
/// leaders, which is the one failure this protocol exists to prevent. So this is not a cache
/// and it is not optional: a node that cannot persist may not vote.
///
/// The whole log is rewritten on every save, and that is affordable because of what the log
/// holds: one entry per election and one per machine that has died. It is not the write path
/// and it never will be.
/// What a restart may not lose.
///
/// Term and vote because forgetting either is two leaders in one term. The log because it is
/// the decisions themselves. And the commit index because the state machine is rebuilt by
/// replaying the log, and replaying past the commit point would apply a decision no majority
/// ever agreed to.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct State {
    pub term: Term,
    pub voted_for: Option<NodeId>,
    pub commit: Index,
    /// The index of `log[0]`. Zero until something has been compacted away.
    pub base: Index,
    pub log: Vec<Entry>,
}

pub trait Store: Send + Sync {
    fn load(&self) -> std::io::Result<Option<State>>;
    fn save(&self, state: &State) -> std::io::Result<()>;
}

/// Keeps nothing.
///
/// For a single-node cluster, where there is nobody to give a second vote to, and for tests.
/// **Not for a real member of a real group**: a node that forgets its vote can vote twice.
pub struct Forgetful;

impl Store for Forgetful {
    fn load(&self) -> std::io::Result<Option<State>> {
        Ok(None)
    }

    fn save(&self, _: &State) -> std::io::Result<()> {
        Ok(())
    }
}

/// One small file, written whole and renamed into place.
///
/// Rename rather than rewrite, and fsync before the rename: a torn state file is a node that
/// cannot start, and this is the same reasoning the pager applies to its meta page one layer
/// down - there is a moment before the rename when nothing has changed, and a moment after it
/// when everything has, and no moment in between.
pub struct FileStore {
    path: std::path::PathBuf,
}

impl FileStore {
    pub fn new(path: impl Into<std::path::PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

/// Bumped with the shape of the file. A `BIGRAFT1` file has a different header and different
/// decision tags, and reading one as this format would take a count for a term.
const MAGIC: &[u8; 8] = b"BIGRAFT2";

impl Store for FileStore {
    fn load(&self) -> std::io::Result<Option<State>> {
        let bytes = match std::fs::read(&self.path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        decode_state(&bytes).map(Some).map_err(|why| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("{}: {why}", self.path.display()),
            )
        })
    }

    fn save(&self, state: &State) -> std::io::Result<()> {
        let tmp = self.path.with_extension("tmp");
        let bytes = encode_state(state);
        {
            use std::io::Write;
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(&bytes)?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, &self.path)?;
        // The rename itself has to reach the disk, or a crash can leave the old file in place
        // having reported the new one written.
        if let Some(dir) = self.path.parent() {
            if let Ok(d) = std::fs::File::open(dir) {
                let _ = d.sync_all();
            }
        }
        Ok(())
    }
}

fn put_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_le_bytes());
}

/// A cursor over a byte slice that never panics and never over-allocates.
struct Bytes<'a> {
    b: &'a [u8],
    at: usize,
}

impl<'a> Bytes<'a> {
    fn u64(&mut self) -> core::result::Result<u64, &'static str> {
        let end = self.at.checked_add(8).ok_or("truncated")?;
        let slice = self.b.get(self.at..end).ok_or("truncated")?;
        self.at = end;
        Ok(u64::from_le_bytes(slice.try_into().expect("eight bytes")))
    }

    fn byte(&mut self) -> core::result::Result<u8, &'static str> {
        let v = *self.b.get(self.at).ok_or("truncated")?;
        self.at += 1;
        Ok(v)
    }

    fn opt_u64(&mut self) -> core::result::Result<Option<u64>, &'static str> {
        let v = self.u64()?;
        Ok((v != u64::MAX).then_some(v))
    }

    /// A count, refused when the file is too short to hold that many of anything.
    ///
    /// The allocation guard: a count is eight bytes of somebody else's file, and every item
    /// costs at least one byte, so a count larger than what is left cannot be honest.
    fn count(&mut self) -> core::result::Result<usize, &'static str> {
        let n = self.u64()? as usize;
        if n > self.b.len() - self.at.min(self.b.len()) {
            return Err("a count larger than the bytes that are left");
        }
        Ok(n)
    }

    fn list<T>(
        &mut self,
        mut f: impl FnMut(&mut Self) -> core::result::Result<T, &'static str>,
    ) -> core::result::Result<Vec<T>, &'static str> {
        let n = self.count()?;
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            out.push(f(self)?);
        }
        Ok(out)
    }

    fn str(&mut self) -> core::result::Result<String, &'static str> {
        let n = self.count()?;
        let end = self.at.checked_add(n).ok_or("truncated")?;
        let slice = self.b.get(self.at..end).ok_or("truncated")?;
        self.at = end;
        core::str::from_utf8(slice).map(str::to_string).map_err(|_| "a name that is not utf-8")
    }
}

fn put_str(out: &mut Vec<u8>, s: &str) {
    put_u64(out, s.len() as u64);
    out.extend_from_slice(s.as_bytes());
}

fn put_opt_u64(out: &mut Vec<u8>, v: Option<u64>) {
    put_u64(out, v.unwrap_or(u64::MAX));
}

fn put_range_map(out: &mut Vec<u8>, m: &RangeMap) {
    put_u64(out, m.epoch);
    put_u64(out, m.schema_leader as u64);
    put_u64(out, m.ranges.len() as u64);
    for r in &m.ranges {
        put_u64(out, r.id);
        put_u64(out, r.shards.start);
        put_opt_u64(out, r.shards.end);
        put_u64(out, r.primary as u64);
        put_u64(out, r.group.len() as u64);
        for n in &r.group {
            put_u64(out, *n as u64);
        }
        match &r.moving {
            None => out.push(0),
            Some(mv) => {
                out.push(match mv.state {
                    MoveState::Seeding => 1,
                    MoveState::Cutover => 2,
                });
                put_u64(out, mv.target as u64);
            }
        }
    }
    put_u64(out, m.stale.len() as u64);
    for n in &m.stale {
        put_u64(out, *n as u64);
    }
}

fn get_range_map(b: &mut Bytes<'_>) -> core::result::Result<RangeMap, &'static str> {
    let epoch = b.u64()?;
    let schema_leader = b.u64()? as NodeId;
    let ranges = b.list(|b| {
        let id = b.u64()?;
        let start = b.u64()?;
        let end = b.opt_u64()?;
        let primary = b.u64()? as NodeId;
        let group = b.list(|b| Ok(b.u64()? as NodeId))?;
        let moving = match b.byte()? {
            0 => None,
            1 => Some(Move { target: b.u64()? as NodeId, state: MoveState::Seeding }),
            2 => Some(Move { target: b.u64()? as NodeId, state: MoveState::Cutover }),
            _ => return Err("unknown move state"),
        };
        Ok(Range { id, shards: ShardRange { start, end }, group, primary, moving })
    })?;
    let stale = b.list(|b| Ok(b.u64()? as NodeId))?;
    Ok(RangeMap { epoch, ranges, stale, schema_leader })
}

fn put_members(out: &mut Vec<u8>, members: &[Member]) {
    put_u64(out, members.len() as u64);
    for m in members {
        put_str(out, &m.name);
        put_str(out, &m.addr);
        out.push(match m.state {
            MemberState::Learner => 0,
            MemberState::Voter => 1,
            MemberState::Draining => 2,
            MemberState::Gone => 3,
        });
    }
}

fn get_members(b: &mut Bytes<'_>) -> core::result::Result<Vec<Member>, &'static str> {
    b.list(|b| {
        let name = b.str()?;
        let addr = b.str()?;
        let state = match b.byte()? {
            0 => MemberState::Learner,
            1 => MemberState::Voter,
            2 => MemberState::Draining,
            3 => MemberState::Gone,
            _ => return Err("unknown member state"),
        };
        Ok(Member { name, addr, state })
    })
}

fn encode_state(state: &State) -> Vec<u8> {
    let mut out = MAGIC.to_vec();
    put_u64(&mut out, state.term);
    put_u64(&mut out, state.voted_for.map_or(u64::MAX, |v| v as u64));
    // **Persisted, though Raft does not require it.** Nothing is unsafe about recomputing it -
    // a leader re-commits what it must - but a node that forgot it would replay the whole log
    // into its state machine on the way up, applying entries that were never committed. For a
    // map of who owns what, that is routing a read to a node no majority ever acknowledged.
    put_u64(&mut out, state.commit);
    put_u64(&mut out, state.base);
    put_u64(&mut out, state.log.len() as u64);
    for entry in &state.log {
        put_u64(&mut out, entry.term);
        match &entry.decision {
            Decision::Noop => out.push(0),
            Decision::Ranges(m) => {
                out.push(1);
                put_range_map(&mut out, m);
            }
            Decision::Members(ms) => {
                out.push(2);
                put_members(&mut out, ms);
            }
        }
    }
    out
}

fn decode_state(bytes: &[u8]) -> core::result::Result<State, &'static str> {
    if bytes.get(..8) != Some(MAGIC) {
        return Err("not a raft state file");
    }
    let mut b = Bytes { b: bytes, at: 8 };
    let term = b.u64()?;
    let voted = b.u64()?;
    let voted_for = (voted != u64::MAX).then_some(voted as NodeId);
    let commit = b.u64()?;
    let base = b.u64()?;
    let log = b.list(|b| {
        let term = b.u64()?;
        let decision = match b.byte()? {
            0 => Decision::Noop,
            1 => Decision::Ranges(get_range_map(b)?),
            2 => Decision::Members(get_members(b)?),
            _ => return Err("unknown decision"),
        };
        Ok(Entry { term, decision })
    })?;
    if b.at != bytes.len() {
        return Err("bytes after the end");
    }
    Ok(State { term, voted_for, commit, base, log })
}

/// The members whose agreement counts, by index.
///
/// A free function because it is a property of a list rather than of a node, and both
/// [`Raft::new`] and [`Raft::refresh_members`] need it before there is a `self` to ask.
fn voters_of(members: &[Member]) -> Vec<NodeId> {
    (0..members.len()).filter(|i| members[*i].votes()).collect()
}
