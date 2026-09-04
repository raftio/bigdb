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

//! Deciding, from what the nodes weigh, whether to move anything.
//!
//! **A pure function, and that is the whole design.** It takes the map, the membership and one
//! number per node, and answers with at most one thing to do. It opens no sockets, reads no
//! clock, and holds nothing between calls - so two leaders elected in sequence reach the same
//! decision from the same facts, and a range does not move twice because the node deciding
//! changed. That is the same discipline `Controller::promotion` follows, and for the same
//! reason.
//!
//! **One action at a time, and never eagerly.** `controller.rs` states the principle this rests
//! on: *each move is a moment where a query can fail*. So a plan is one step, the caller applies
//! it and comes back, and nothing is proposed until the imbalance has lasted long enough to be a
//! shape rather than a spike.

use crate::raft::{Member, RangeId, RangeMap};
use big_engine::{ShardId, ShardRange};

/// What one node weighs.
///
/// Pages rather than records, because pages are what a disk fills with and what an operator is
/// looking at when they decide to add a machine. It is also nearly free to read: the pager
/// counts them already.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct NodeLoad {
    /// `None` for a node that did not answer. **Not zero** - an empty node and an unreachable
    /// one call for opposite actions, and rolling them together would have the balancer send
    /// work to a machine that is not there.
    pub pages: Option<u64>,
    /// One past the highest record id the node holds, over every table. What a tail split is
    /// cut above.
    pub frontier: u64,
    /// Whether the node holds the agreement's log to within a margin of its end.
    ///
    /// What decides whether a learner may become a member. A node still replaying what it
    /// missed would raise the bar for every election while being able to help decide none -
    /// the exact reason a learner is not a voter to begin with. Only the leader knows this,
    /// which is fine: only the leader proposes.
    pub caught_up: bool,
}

/// When the balancer is allowed to act, and how hard.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Policy {
    /// Off unless an operator turns it on. A cluster that reshapes itself without being asked
    /// is a cluster whose shape an operator cannot predict.
    pub enabled: bool,
    /// How far the fullest node may exceed the emptiest, as a percentage, before it is worth
    /// moving anything. 150 means half again as full.
    pub spread_percent: u64,
    /// Below this, a difference is noise rather than an imbalance - two nodes a few pages apart
    /// are two nodes that agree.
    pub floor_pages: u64,
}

impl Default for Policy {
    fn default() -> Self {
        Self { enabled: false, spread_percent: 150, floor_pages: 1_024 }
    }
}

/// The one thing to do next.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Action {
    /// Make a learner that has caught up a full member, so that it votes and may hold a range.
    ///
    /// **A change to who the cluster is, not to who holds what** - and the only one the
    /// balancer makes. It has to come first: until the node is a member, nothing below can see
    /// it as a node with nothing to give it the tail.
    Admit { node: String },
    /// Cut the tail above everything written and give the empty half to a node with nothing.
    ///
    /// **The scale-out that moves no bytes**, and the reason it is preferred: it costs one
    /// entry in the agreement and nothing on the wire.
    SplitTail { at: ShardId, to: String },
    /// Hand a populated range to a node that has room.
    Move { range: RangeId, to: String },
}

/// What to do next, or nothing.
///
/// The order is the policy, and the first that applies wins:
///
/// 1. **A learner that is answering and has caught up** is admitted. It joined to take work,
///    and nothing below can give it any until it counts.
/// 2. **A node that is draining** gives up a range. An operator asked for this, so it outranks
///    anything the balancer noticed by itself.
/// 3. **A node with nothing** is given the tail, cut above everything written. No bytes move.
/// 4. **A spread that is too wide** moves one range from the fullest node to the emptiest.
///
/// `None` whenever anything is already in flight: a second decision made about a map that has
/// not settled is a decision about a state that is already gone.
pub fn plan(
    map: &RangeMap,
    members: &[Member],
    load: &[NodeLoad],
    policy: &Policy,
) -> Option<Action> {
    if !policy.enabled {
        return None;
    }
    // One at a time. A move in flight is a range whose owner is about to change, and planning
    // against it would be planning against a map that is already out of date.
    if map.ranges.iter().any(|r| r.moving.is_some()) {
        return None;
    }

    let name = |n: usize| members.get(n).map(|m: &Member| m.name.clone());

    // 1. Somebody joined, and is ready to count. Answering `/internal/load` is what proves it
    // is up for data and not only for the log; caught up is what proves counting it will not
    // stall the next election.
    for (i, m) in members.iter().enumerate() {
        if !matches!(m.state, crate::raft::MemberState::Learner) {
            continue;
        }
        if load.get(i).is_some_and(|l| l.pages.is_some() && l.caught_up) {
            return Some(Action::Admit { node: name(i)? });
        }
    }
    // A node that did not answer is not a candidate in either direction: it may be full, it may
    // be empty, and sending work to a machine that is not there is the worse guess.
    let live = |n: usize| {
        members.get(n).is_some_and(|m| m.takes_ranges())
            && load.get(n).is_some_and(|l| l.pages.is_some())
    };
    let pages = |n: usize| load.get(n).and_then(|l| l.pages).unwrap_or(0);

    // 2. Somebody asked for this.
    for (i, m) in members.iter().enumerate() {
        if !matches!(m.state, crate::raft::MemberState::Draining) {
            continue;
        }
        let Some(&range) = map.held_by(i).first() else { continue };
        let to = (0..members.len())
            .filter(|n| live(*n) && !map.ranges[range].group.contains(n))
            .min_by_key(|n| pages(*n))?;
        return Some(Action::Move { range: map.ranges[range].id, to: name(to)? });
    }

    // 3. A node that holds nothing. Give it the part of the space nothing has been written to
    // yet, which is the only thing that can change hands without a copy.
    if let Some(empty) = (0..members.len()).find(|n| live(*n) && map.held_by(*n).is_empty()) {
        // Above every record any node holds, so the half handed over is empty by construction.
        let frontier = load.iter().filter_map(|l| l.pages.map(|_| l.frontier)).max().unwrap_or(0);
        let at = big_engine::shard_of(frontier) + 1;
        // Only if that lands inside the open tail; a cut anywhere else would divide a range
        // that already holds records.
        let tail = map.ranges.last()?;
        if tail.shards.end.is_none() && at > tail.shards.start {
            return Some(Action::SplitTail { at, to: name(empty)? });
        }
    }

    // 4. A spread that has grown too wide.
    let fullest = (0..members.len()).filter(|n| live(*n)).max_by_key(|n| pages(*n))?;
    let emptiest = (0..members.len()).filter(|n| live(*n)).min_by_key(|n| pages(*n))?;
    if fullest == emptiest {
        return None;
    }
    let (high, low) = (pages(fullest), pages(emptiest));
    if high < policy.floor_pages || high.saturating_mul(100) < low.max(1) * policy.spread_percent {
        return None;
    }
    // The smallest range the fullest node serves, so the cluster is levelled in the cheapest
    // step that helps rather than in one enormous one.
    let range = map
        .served_by(fullest)
        .into_iter()
        .filter(|r| !map.ranges[*r].group.contains(&emptiest))
        .min_by_key(|r| span_of(map.ranges[*r].shards))?;
    Some(Action::Move { range: map.ranges[range].id, to: name(emptiest)? })
}

/// How wide a range is, with an open end counting as the rest of the space.
fn span_of(r: ShardRange) -> u64 {
    r.end.unwrap_or(u64::MAX).saturating_sub(r.start)
}
