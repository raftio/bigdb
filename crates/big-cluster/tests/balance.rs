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

//! What the balancer decides, from facts and nothing else.
//!
//! **A table of inputs and one output.** The planner opens no sockets and reads no clock, which
//! is what makes this a table rather than a cluster: every claim below is a claim about a
//! decision, checked in microseconds, and the thing being checked is the thing that ships.

use big_cluster::balance::{plan, Action, NodeLoad, Policy};
use big_cluster::raft::{Member, MemberState, Move, MoveState, Range, RangeMap};
use big_engine::ShardRange;

fn member(name: &str, state: MemberState) -> Member {
    Member { name: name.to_string(), addr: format!("10.0.0.1:{}", name.len()), state }
}

fn voters(names: &[&str]) -> Vec<Member> {
    names.iter().map(|n| member(n, MemberState::Voter)).collect()
}

fn range(id: u64, start: u64, end: Option<u64>, primary: usize) -> Range {
    Range { id, shards: ShardRange { start, end }, group: vec![primary], primary, moving: None }
}

fn map(ranges: Vec<Range>) -> RangeMap {
    RangeMap {
        epoch: 1,
        ranges,
        stale: Vec::new(),
        schema_leader: 0,
        reserved: Vec::new(),
        schema_ready: true,
    }
}

/// Every node that answered has also caught up on the log, which is the common case.
fn load(pages: &[Option<u64>], frontier: u64) -> Vec<NodeLoad> {
    pages.iter().map(|p| NodeLoad { pages: *p, frontier, caught_up: p.is_some() }).collect()
}

/// The same, with node `n` still replaying what it missed.
fn behind(mut loads: Vec<NodeLoad>, n: usize) -> Vec<NodeLoad> {
    loads[n].caught_up = false;
    loads
}

fn on() -> Policy {
    Policy { enabled: true, ..Policy::default() }
}

// -------------------------------------------------------------------------------------------
// Admitting a node that joined
// -------------------------------------------------------------------------------------------

/// **A learner that is answering and has caught up is made a member, before anything else.**
/// Until it is a voter the balancer cannot see it as a node with nothing, so admitting it has
/// to come first - even ahead of a drain an operator asked for, which only needs somewhere to
/// send the range and will have one more place the moment this lands.
#[test]
fn a_learner_that_answers_and_has_caught_up_is_admitted_before_anything_else() {
    let m = map(vec![range(0, 0, Some(64), 0), range(1, 64, None, 1)]);
    let mut members = voters(&["a", "b"]);
    members.push(member("d", MemberState::Learner));
    let l = load(&[Some(100_000), Some(100_000), Some(0)], 0);
    assert_eq!(plan(&m, &members, &l, &on()), Some(Action::Admit { node: "d".to_string() }));

    // Ahead of the drain: `b` wants to go, and it will have `d` to go to once `d` counts.
    members[1].state = MemberState::Draining;
    assert_eq!(
        plan(&m, &members, &l, &on()),
        Some(Action::Admit { node: "d".to_string() }),
        "a member is worth more than a move, and the move comes right after"
    );
}

/// A learner that did not say what it weighs is a learner that may not be up for data at all.
/// Replicating the log to it proves nothing about whether it can serve a range.
#[test]
fn a_learner_that_did_not_answer_is_not_admitted() {
    let m = map(vec![range(0, 0, Some(64), 0), range(1, 64, None, 1)]);
    let mut members = voters(&["a", "b"]);
    members.push(member("d", MemberState::Learner));
    let l = load(&[Some(100_000), Some(100_000), None], 0);
    assert_eq!(plan(&m, &members, &l, &on()), None);
}

/// A learner still replaying the log would raise the bar for every election while being able
/// to help decide none - the exact reason a learner is not a voter in the first place.
#[test]
fn a_learner_still_catching_up_is_not_admitted() {
    let m = map(vec![range(0, 0, Some(64), 0), range(1, 64, None, 1)]);
    let mut members = voters(&["a", "b"]);
    members.push(member("d", MemberState::Learner));
    let l = behind(load(&[Some(100_000), Some(100_000), Some(0)], 0), 2);
    assert_eq!(plan(&m, &members, &l, &on()), None);
}

/// Admitting is a change to the map's membership, and the one-at-a-time rule covers it too.
#[test]
fn nothing_is_admitted_while_something_is_moving() {
    let mut m = map(vec![range(0, 0, Some(64), 0), range(1, 64, None, 1)]);
    m.ranges[0].moving = Some(Move { target: 1, state: MoveState::Seeding });
    let mut members = voters(&["a", "b"]);
    members.push(member("d", MemberState::Learner));
    let l = load(&[Some(100_000), Some(100_000), Some(0)], 0);
    assert_eq!(plan(&m, &members, &l, &on()), None);
}

// -------------------------------------------------------------------------------------------
// When it does nothing
// -------------------------------------------------------------------------------------------

/// **Off unless somebody turned it on.** A cluster that reshapes itself unasked is a cluster
/// whose shape an operator cannot predict.
#[test]
fn a_balancer_nobody_enabled_does_nothing() {
    let m = map(vec![range(0, 0, Some(64), 0), range(1, 64, None, 0)]);
    let l = load(&[Some(100_000), Some(0)], 0);
    assert_eq!(plan(&m, &voters(&["a", "b"]), &l, &Policy::default()), None);
}

/// One at a time. Planning against a map with a move in it is planning against a state that is
/// already on its way to being something else.
#[test]
fn nothing_is_planned_while_something_is_already_moving() {
    let mut m = map(vec![range(0, 0, Some(64), 0), range(1, 64, None, 0)]);
    m.ranges[0].moving = Some(Move { target: 1, state: MoveState::Seeding });
    let l = load(&[Some(100_000), Some(0)], 0);
    assert_eq!(plan(&m, &voters(&["a", "b"]), &l, &on()), None);
}

/// **A node that did not answer is not a candidate in either direction.** It may be full, it
/// may be empty, and sending work to a machine that is not there is the worse guess of the two.
#[test]
fn a_node_that_did_not_answer_is_neither_filled_nor_emptied() {
    let m = map(vec![range(0, 0, Some(64), 0), range(1, 64, None, 0)]);
    // `b` holds nothing and would be the obvious place to send work - but it is silent.
    let l = load(&[Some(100_000), None], 0);
    assert_eq!(plan(&m, &voters(&["a", "b"]), &l, &on()), None);
}

/// Two nodes a few pages apart are two nodes that agree.
#[test]
fn a_difference_below_the_floor_is_noise_rather_than_an_imbalance() {
    let m = map(vec![range(0, 0, Some(64), 0), range(1, 64, None, 1)]);
    let l = load(&[Some(80), Some(10)], 0);
    assert_eq!(plan(&m, &voters(&["a", "b"]), &l, &on()), None, "well under the floor");
}

#[test]
fn a_spread_inside_the_threshold_is_left_alone() {
    let m = map(vec![range(0, 0, Some(64), 0), range(1, 64, None, 1)]);
    // 120% apart, and the policy tolerates 150%.
    let l = load(&[Some(12_000), Some(10_000)], 0);
    assert_eq!(plan(&m, &voters(&["a", "b"]), &l, &on()), None);
}

// -------------------------------------------------------------------------------------------
// What it prefers
// -------------------------------------------------------------------------------------------

/// **The cheapest step that helps, first.** A node holding nothing is given the part of the
/// space nothing has been written to, which changes hands without a byte crossing the wire.
#[test]
fn an_empty_node_is_given_the_tail_rather_than_somebody_elses_records() {
    let m = map(vec![range(0, 0, Some(64), 0), range(1, 64, None, 0)]);
    // Everything written so far is in shard 70, so the cut lands at 71 and the half above it
    // is empty by construction.
    let frontier = 70 * (1 << 20) + 5;
    let l = load(&[Some(100_000), Some(0)], frontier);
    assert_eq!(
        plan(&m, &voters(&["a", "b"]), &l, &on()),
        Some(Action::SplitTail { at: 71, to: "b".to_string() })
    );
}

/// A drain is something an operator asked for, so it outranks anything the balancer noticed by
/// itself - including an empty node that would otherwise be filled first.
#[test]
fn a_draining_node_gives_up_a_range_before_anything_else_is_considered() {
    let m = map(vec![range(0, 0, Some(64), 0), range(1, 64, None, 1)]);
    let mut members = voters(&["a", "b", "c"]);
    members[1].state = MemberState::Draining;
    // `c` holds nothing, so rule two would otherwise fire.
    let l = load(&[Some(10_000), Some(10_000), Some(0)], 0);
    assert_eq!(plan(&m, &members, &l, &on()), Some(Action::Move { range: 1, to: "c".to_string() }));
}

/// **A draining node is never a destination.** It is on its way out; giving it a range is work
/// that has to be undone.
#[test]
fn a_draining_node_is_not_given_anything() {
    let m = map(vec![range(0, 0, Some(64), 0), range(1, 64, None, 0)]);
    let mut members = voters(&["a", "b"]);
    members[1].state = MemberState::Draining;
    let l = load(&[Some(100_000), Some(0)], 0);
    assert_eq!(plan(&m, &members, &l, &on()), None, "`b` holds nothing and still gets nothing");
}

/// A learner is still catching up: it replicates, is not yet admitted, and takes no range.
#[test]
fn a_learner_is_not_given_a_range_until_it_is_admitted() {
    let m = map(vec![range(0, 0, Some(64), 0), range(1, 64, None, 0)]);
    let mut members = voters(&["a", "b"]);
    members[1].state = MemberState::Learner;
    let l = behind(load(&[Some(100_000), Some(0)], 70 * (1 << 20)), 1);
    assert_eq!(plan(&m, &members, &l, &on()), None);

    // Admitted, and now it is the obvious place for the tail.
    members[1].state = MemberState::Voter;
    assert_eq!(
        plan(&m, &members, &l, &on()),
        Some(Action::SplitTail { at: 71, to: "b".to_string() })
    );
}

/// Once every node holds something, levelling is a move - and the *smallest* range that helps,
/// so the cluster is levelled in the cheapest step rather than in one enormous one.
#[test]
fn a_wide_spread_moves_the_smallest_range_from_the_fullest_node() {
    let m = map(vec![
        // `a` serves a narrow range and a wide one; `b` serves the tail and is nearly empty.
        range(0, 0, Some(8), 0),
        range(1, 8, Some(900), 0),
        range(2, 900, None, 1),
    ]);
    let l = load(&[Some(100_000), Some(1_000)], 0);
    assert_eq!(
        plan(&m, &voters(&["a", "b"]), &l, &on()),
        Some(Action::Move { range: 0, to: "b".to_string() }),
        "the narrow range moves, not the wide one"
    );
}

/// **Deterministic.** Two leaders elected in sequence reach the same decision from the same
/// facts, which is what keeps a range from moving twice because the node deciding changed.
#[test]
fn the_same_facts_always_give_the_same_answer() {
    let m = map(vec![range(0, 0, Some(8), 0), range(1, 8, Some(900), 0), range(2, 900, None, 1)]);
    let l = load(&[Some(100_000), Some(1_000)], 0);
    let first = plan(&m, &voters(&["a", "b"]), &l, &on());
    for _ in 0..50 {
        assert_eq!(plan(&m, &voters(&["a", "b"]), &l, &on()), first);
    }
}

/// A range is never handed to a node that already holds it - that is not a move, and acting on
/// it would be a step that changes nothing while looking like progress.
#[test]
fn a_range_is_not_moved_to_a_node_that_already_holds_it() {
    let mut m = map(vec![range(0, 0, Some(64), 0), range(1, 64, None, 0)]);
    m.ranges[0].group = vec![0, 1];
    let l = load(&[Some(100_000), Some(1_000)], 0);
    // The only range `a` serves that `b` does not already hold is range 1.
    assert_eq!(
        plan(&m, &voters(&["a", "b"]), &l, &on()),
        Some(Action::Move { range: 1, to: "b".to_string() })
    );
}
