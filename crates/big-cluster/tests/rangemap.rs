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

//! The map itself: what it refuses, and what it answers.
//!
//! **Disjoint and total is the whole invariant.** It used to be checked once, on a file, and a
//! file that failed it was a startup error an operator fixed by hand. The map now changes while
//! the cluster runs, so the check moved onto every proposal - and a proposal that slipped past
//! it would be a record id no node answers for, with nothing afterwards to notice.

use big_cluster::config::ClusterFile;
use big_cluster::raft::{MapError, Move, MoveState, Range, RangeMap};
use big_engine::ShardRange;
use proptest::prelude::*;

fn range(id: u64, start: u64, end: Option<u64>, primary: usize) -> Range {
    Range { id, shards: ShardRange { start, end }, group: vec![primary], primary, moving: None }
}

fn map(ranges: Vec<Range>) -> RangeMap {
    RangeMap { epoch: 0, ranges, stale: Vec::new(), schema_leader: 0 }
}

// -------------------------------------------------------------------------------------------
// What a map has to be
// -------------------------------------------------------------------------------------------

#[test]
fn a_map_covering_the_space_exactly_once_is_accepted() {
    let m = map(vec![range(0, 0, Some(64), 0), range(1, 64, Some(900), 1), range(2, 900, None, 2)]);
    assert_eq!(m.check(), Ok(()));
}

#[test]
fn a_map_that_does_not_start_at_zero_is_refused() {
    let m = map(vec![range(0, 1, None, 0)]);
    assert_eq!(m.check(), Err(MapError::NotStartingAtZero { start: 1 }));
}

#[test]
fn a_gap_is_refused_and_names_the_shards_nobody_owns() {
    let m = map(vec![range(0, 0, Some(64), 0), range(1, 70, None, 1)]);
    assert_eq!(m.check(), Err(MapError::Gap { from: 64, to: 70 }));
}

#[test]
fn an_overlap_is_refused_because_each_owner_would_answer_half_of_every_query() {
    let m = map(vec![range(0, 0, Some(64), 0), range(1, 32, None, 1)]);
    assert_eq!(m.check(), Err(MapError::Overlap { from: 32, to: 64 }));
}

/// The last range has to be open, or the top of the space belongs to nobody - and a client may
/// choose any record id at all.
#[test]
fn a_closed_last_range_is_refused() {
    let m = map(vec![range(0, 0, Some(64), 0)]);
    assert_eq!(m.check(), Err(MapError::NotTotal { from: 64 }));
}

#[test]
fn an_empty_map_answers_for_no_record_id_at_all() {
    assert_eq!(map(vec![]).check(), Err(MapError::Empty));
}

#[test]
fn a_range_nobody_holds_is_refused() {
    let mut m = map(vec![range(0, 0, None, 0)]);
    m.ranges[0].group.clear();
    assert_eq!(m.check(), Err(MapError::EmptyGroup { id: 0 }));
}

/// A primary that does not hold the range would be a read routed to a node with no data.
#[test]
fn a_primary_outside_its_own_group_is_refused() {
    let mut m = map(vec![range(0, 0, None, 0)]);
    m.ranges[0].primary = 7;
    assert_eq!(m.check(), Err(MapError::PrimaryNotInGroup { id: 0, primary: 7 }));
}

/// Ids name ranges across a move, so two ranges answering to one name is two different spans a
/// later decision could not tell apart.
#[test]
fn two_ranges_with_one_id_are_refused() {
    let m = map(vec![range(5, 0, Some(64), 0), range(5, 64, None, 1)]);
    assert_eq!(m.check(), Err(MapError::DuplicateId { id: 5 }));
}

// -------------------------------------------------------------------------------------------
// What a map answers
// -------------------------------------------------------------------------------------------

#[test]
fn a_shard_lands_in_exactly_one_range() {
    let m = map(vec![range(0, 0, Some(64), 0), range(1, 64, Some(900), 1), range(2, 900, None, 2)]);
    assert_eq!(m.range_of(0), 0);
    assert_eq!(m.range_of(63), 0);
    assert_eq!(m.range_of(64), 1);
    assert_eq!(m.range_of(899), 1);
    assert_eq!(m.range_of(900), 2);
    assert_eq!(m.range_of(u64::MAX), 2);
}

/// **The thing that could not be said before.** A range's name and a node's name used to be one
/// number, so a node could hold at most one range and a range could only move to a node that
/// already had one.
#[test]
fn one_node_can_hold_and_serve_several_ranges() {
    let m = map(vec![range(0, 0, Some(64), 0), range(1, 64, Some(900), 0), range(2, 900, None, 1)]);
    assert_eq!(m.check(), Ok(()));
    assert_eq!(m.held_by(0), vec![0, 1]);
    assert_eq!(m.served_by(0), vec![0, 1]);
    assert_eq!(
        m.shards_served_by(0),
        vec![ShardRange { start: 0, end: Some(64) }, ShardRange { start: 64, end: Some(900) }]
    );
    assert_eq!(m.held_by(1), vec![2]);
}

#[test]
fn a_new_range_takes_an_id_nothing_else_has() {
    let m = map(vec![range(0, 0, Some(64), 0), range(7, 64, None, 1)]);
    assert_eq!(m.next_id(), 8, "one past the highest, so a retired id is never handed out again");
    assert_eq!(map(vec![]).next_id(), 0);
}

#[test]
fn a_move_in_flight_is_part_of_the_map() {
    let mut m = map(vec![range(0, 0, None, 0)]);
    m.ranges[0].moving = Some(Move { target: 1, state: MoveState::Seeding });
    assert_eq!(m.check(), Ok(()), "a move does not reshape the space, so it stays valid");
    assert_eq!(m.position(0), Some(0));
    assert_eq!(m.position(9), None);
}

// -------------------------------------------------------------------------------------------
// The file only ever seeds it
// -------------------------------------------------------------------------------------------

/// **The seed has to be a map the checker accepts**, or the very first proposal built from it
/// would be refused. The file's own validation and the map's are two statements of one rule,
/// and this is what keeps them from drifting apart.
#[test]
fn the_map_seeded_from_a_cluster_file_is_valid_and_says_what_the_file_said() {
    let file = ClusterFile::parse(
        r#"
schema_leader = "b"

[[node]]
name   = "a"
addr   = "10.0.0.1:7654"
shards = "0..64"

[[node]]
name   = "b"
addr   = "10.0.0.2:7654"
shards = "64.."

[[node]]
name    = "a-spare"
addr    = "10.0.0.3:7654"
replica = "a"
"#,
    )
    .unwrap();
    let config = file.for_node(Some("a"), "").unwrap();
    let m = config.seed_map();

    assert_eq!(m.check(), Ok(()));
    assert_eq!(m.len(), 2, "two primaries, two ranges; a replica is not a range of its own");
    assert_eq!(m.ranges[0].shards, ShardRange { start: 0, end: Some(64) });
    assert_eq!(m.ranges[1].shards, ShardRange { start: 64, end: None });
    // The spare holds `a`'s range without serving it, which is what a replica is.
    assert_eq!(m.ranges[0].group, vec![0, 2]);
    assert_eq!(m.ranges[0].primary, 0);
    assert!(m.stale.is_empty(), "nothing is behind before anything has happened");
    assert_eq!(m.schema_leader, 1, "the file named `b`");

    let members = config.seed_members();
    assert_eq!(members.len(), 3);
    assert_eq!(members[0].name, "a");
    assert!(members.iter().all(|m| m.votes()), "every node in a file is a voter");
}

// -------------------------------------------------------------------------------------------
// The invariant, over arbitrary maps
// -------------------------------------------------------------------------------------------

proptest! {
    /// **Anything the checker accepts is total**: every shard belongs to exactly one range, so
    /// `range_of` is an index rather than an option and no record id is unroutable.
    #[test]
    fn any_accepted_map_routes_every_shard_to_exactly_one_range(
        cuts in prop::collection::btree_set(1u64..10_000, 0..8),
        probes in prop::collection::vec(0u64..20_000, 1..20),
    ) {
        let cuts: Vec<u64> = cuts.into_iter().collect();
        let mut ranges = Vec::new();
        let mut start = 0u64;
        for (i, cut) in cuts.iter().enumerate() {
            ranges.push(range(i as u64, start, Some(*cut), i % 3));
            start = *cut;
        }
        ranges.push(range(cuts.len() as u64, start, None, cuts.len() % 3));
        let m = map(ranges);

        prop_assert_eq!(m.check(), Ok(()));
        for shard in probes {
            let i = m.range_of(shard);
            prop_assert!(m.ranges[i].shards.contains(shard), "{} not in range {}", shard, i);
            let owners = m.ranges.iter().filter(|r| r.shards.contains(shard)).count();
            prop_assert_eq!(owners, 1, "{} has {} owners", shard, owners);
        }
    }
}
