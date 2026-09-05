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

//! The agreement, run in one process against a clock this file controls.
//!
//! Every test here is a cluster: several `Raft`s, a queue of messages between them, and a
//! timestamp that moves when this file says so. Nothing sleeps, nothing binds a port, and a
//! partition is a predicate. That is the point - the bugs in a consensus protocol are all in
//! the rules, and a rule that can only be observed by starting five processes and waiting is a
//! rule nobody tests.
//!
//! What is being checked is the property that makes the protocol worth having: **two nodes
//! never lead the same term, and a committed decision is never taken back.** Everything else
//! is a way of stressing that.

use big_cluster::controller::{Controller, Leases};
use big_cluster::raft::{
    Decision, Member, MemberState, Message, NodeId, Raft, Range, RangeMap, Role, State, Store,
    Timing,
};
use big_engine::ShardRange;
use std::collections::BTreeSet;

/// A cluster in one process, with a clock and a network this test drives.
struct Sim {
    nodes: Vec<Raft>,
    queue: Vec<(NodeId, Message)>,
    now: u64,
    /// Everything each node has applied, in order. What "committed" means from outside.
    applied: Vec<Vec<Decision>>,
    /// Nodes that are down: they neither send nor receive.
    down: BTreeSet<NodeId>,
    /// A partition, as the pairs that cannot talk. Symmetric by construction below.
    cut: BTreeSet<(NodeId, NodeId)>,
}

impl Sim {
    fn new(n: usize) -> Self {
        Self::with_timing(n, Timing { election_min: 1_000, election_spread: 1_000, heartbeat: 200 })
    }

    /// The same, on the clocks a real deployment runs.
    ///
    /// Free, because the clock here is a number this file increments: the numbers a datacentre
    /// waits seconds for are exercised in microseconds. What it cannot exercise is the *wall*
    /// clock - that is `a_range_fails_over_on_the_clocks_it_ships_with` over real sockets.
    fn with_timing(n: usize, timing: Timing) -> Self {
        let members = voters(n);
        Self {
            nodes: (0..n).map(|i| Raft::new(i, members.clone(), timing, 0)).collect(),
            queue: Vec::new(),
            now: 0,
            applied: vec![Vec::new(); n],
            down: BTreeSet::new(),
            cut: BTreeSet::new(),
        }
    }

    fn reachable(&self, from: NodeId, to: NodeId) -> bool {
        !self.down.contains(&from)
            && !self.down.contains(&to)
            && !self.cut.contains(&(from, to))
            && !self.cut.contains(&(to, from))
    }

    fn take(&mut self, node: NodeId, out: big_cluster::raft::Output) {
        for d in out.applied {
            self.applied[node].push(d);
        }
        for (to, m) in out.send {
            if self.reachable(node, to) {
                self.queue.push((to, m));
            }
        }
    }

    /// Moves the clock on and delivers everything that is in flight.
    ///
    /// Messages are delivered in the order they were sent, which is the least interesting
    /// order a network could choose and the one that makes a failure reproducible. The rules
    /// under test do not depend on ordering; a test that shuffled would be testing the shuffle.
    fn run(&mut self, ms: u64) {
        for _ in 0..ms / 10 {
            self.now += 10;
            for i in 0..self.nodes.len() {
                if self.down.contains(&i) {
                    continue;
                }
                let out = self.nodes[i].tick(self.now);
                self.take(i, out);
            }
            for _ in 0..64 {
                let Some((to, m)) = (!self.queue.is_empty()).then(|| self.queue.remove(0)) else {
                    break;
                };
                if self.down.contains(&to) {
                    continue;
                }
                let out = self.nodes[to].deliver(m, self.now);
                self.take(to, out);
            }
        }
    }

    fn leaders(&self) -> Vec<NodeId> {
        self.nodes
            .iter()
            .enumerate()
            .filter(|(i, n)| n.is_leader() && !self.down.contains(i))
            .map(|(i, _)| i)
            .collect()
    }

    /// The one leader, or a failure saying how many there were.
    fn leader(&self) -> NodeId {
        let leaders = self.leaders();
        assert_eq!(leaders.len(), 1, "expected one leader, found {leaders:?}");
        leaders[0]
    }

    fn propose(&mut self, node: NodeId, decision: Decision) -> bool {
        match self.nodes[node].propose(decision) {
            Some(out) => {
                self.take(node, out);
                true
            }
            None => false,
        }
    }
}

/// `n` nodes, all of them voting, which is what a cluster file describes.
fn voters(n: usize) -> Vec<Member> {
    (0..n)
        .map(|i| Member {
            name: format!("n{i}"),
            addr: format!("10.0.0.{i}:7654"),
            state: MemberState::Voter,
        })
        .collect()
}

/// A map in which node `primary[i]` serves range `i`, each over an arbitrary slice of the
/// space. What the ranges cover does not matter to the agreement - only that a decision is a
/// value that survives a round trip and that two of them compare unequal.
fn owning(primary: &[NodeId]) -> Decision {
    Decision::Ranges(map_of(primary, &[]))
}

fn map_of(primary: &[NodeId], stale: &[NodeId]) -> RangeMap {
    let ranges = primary
        .iter()
        .enumerate()
        .map(|(i, p)| Range {
            id: i as u64,
            shards: ShardRange {
                start: i as u64 * 64,
                end: (i + 1 < primary.len()).then(|| (i as u64 + 1) * 64),
            },
            group: vec![*p],
            primary: *p,
            moving: None,
        })
        .collect();
    RangeMap {
        epoch: 1,
        ranges,
        stale: stale.to_vec(),
        schema_leader: 0,
        reserved: Vec::new(),
        schema_ready: true,
    }
}

/// A node alone in its config file leads immediately: a majority of one is one.
///
/// Not a special case so much as the general one, and it is what lets `big serve` without peers use
/// the same code path as everything else.
#[test]
fn one_node_leads_itself() {
    let mut sim = Sim::new(1);
    sim.run(3_000);
    assert_eq!(sim.leader(), 0);
    assert!(sim.propose(0, owning(&[0])));
    sim.run(100);
    assert_eq!(sim.applied[0].last(), Some(&owning(&[0])));
}

/// **One leader per term, and exactly one.** The randomised timeout is what ends a split vote,
/// and it is derived from the node and the term rather than from a random number generator so
/// that this test says the same thing every time it runs.
#[test]
fn exactly_one_node_wins() {
    for size in [3usize, 5] {
        let mut sim = Sim::new(size);
        sim.run(10_000);
        assert_eq!(sim.leaders().len(), 1, "{size} nodes elected {:?}", sim.leaders());

        // And every node agrees who it is, which is the half that matters to a caller: a
        // cluster with one leader nobody can name is a cluster with no leader.
        let leader = sim.leader();
        for (i, node) in sim.nodes.iter().enumerate() {
            assert_eq!(node.leader(), Some(leader), "node {i} follows {:?}", node.leader());
        }
    }
}

/// A decision reaches every node, not only a majority. The majority is what makes it
/// committed; the rest catch up because the leader keeps sending.
#[test]
fn a_decision_reaches_every_node() {
    let mut sim = Sim::new(3);
    sim.run(5_000);
    let leader = sim.leader();
    assert!(sim.propose(leader, owning(&[1, 2, 0])));
    sim.run(2_000);

    for i in 0..3 {
        assert_eq!(
            sim.applied[i].last(),
            Some(&owning(&[1, 2, 0])),
            "node {i} applied {:?}",
            sim.applied[i]
        );
    }
}

/// Only a leader may propose. A follower that tried would be deciding on its own, which is
/// the entire failure this protocol exists to prevent.
#[test]
fn a_follower_cannot_decide() {
    let mut sim = Sim::new(3);
    sim.run(5_000);
    let leader = sim.leader();
    for i in 0..3 {
        if i != leader {
            assert!(!sim.propose(i, owning(&[0, 0, 0])), "node {i} decided while following");
        }
    }
}

/// A leader that stops answering is replaced, and the replacement holds everything the old one
/// had committed. This is the failover the whole module exists for.
#[test]
fn a_dead_leader_is_replaced_without_losing_a_decision() {
    let mut sim = Sim::new(3);
    sim.run(5_000);
    let old = sim.leader();
    assert!(sim.propose(old, owning(&[0, 1, 2])));
    sim.run(2_000);

    sim.down.insert(old);
    sim.run(10_000);

    let new = sim.leader();
    assert_ne!(new, old);
    assert!(sim.nodes[new].term() > 0);
    // The decision the dead leader committed is still there, on the node that replaced it.
    assert!(
        sim.applied[new].contains(&owning(&[0, 1, 2])),
        "node {new} lost a committed decision: {:?}",
        sim.applied[new]
    );
}

/// **A minority cannot decide anything.** A leader cut off from the majority keeps thinking it
/// is one, and that is fine: it cannot commit, so nothing it accepted becomes a decision.
#[test]
fn a_partitioned_leader_commits_nothing() {
    let mut sim = Sim::new(5);
    sim.run(6_000);
    let old = sim.leader();

    // Cut the leader off from everybody.
    for i in 0..5 {
        if i != old {
            sim.cut.insert((old, i));
        }
    }
    let before = sim.nodes[old].commit_index();
    assert!(sim.propose(old, owning(&[9, 9, 9, 9, 9])));
    sim.run(10_000);

    assert_eq!(sim.nodes[old].commit_index(), before, "a minority committed something");
    for i in 0..5 {
        assert!(
            !sim.applied[i].contains(&owning(&[9, 9, 9, 9, 9])),
            "node {i} applied a decision a minority made"
        );
    }

    // The majority elected somebody, and it is not the node that was cut off.
    let majority_leader = (0..5).find(|i| *i != old && sim.nodes[*i].is_leader());
    assert!(majority_leader.is_some(), "the majority never replaced the leader it lost");
}

/// The old leader comes back, discovers a higher term, and gives up whatever it was holding.
///
/// The entry it appended alone is overwritten rather than merged: it was never committed, so
/// nothing was promised about it, and the log that a majority agreed on wins.
#[test]
fn a_returning_leader_gives_up_what_it_decided_alone() {
    let mut sim = Sim::new(3);
    sim.run(5_000);
    let old = sim.leader();

    for i in 0..3 {
        if i != old {
            sim.cut.insert((old, i));
        }
    }
    assert!(sim.propose(old, owning(&[7, 7, 7])));
    sim.run(6_000);

    let new = (0..3).find(|i| *i != old && sim.nodes[*i].is_leader()).expect("a new leader");
    assert!(sim.propose(new, owning(&[1, 1, 1])));
    sim.run(2_000);

    // Heal.
    sim.cut.clear();
    sim.run(6_000);

    assert!(!sim.nodes[old].is_leader(), "the old leader did not stand down");
    assert!(
        !sim.applied[old].contains(&owning(&[7, 7, 7])),
        "a decision made alone was applied after all"
    );
    // Every log agrees, entry for entry. This is the invariant, not a consequence of one.
    let reference: Vec<_> = sim.nodes[new].log().to_vec();
    for i in 0..3 {
        let mine = sim.nodes[i].log();
        let shared = mine.len().min(reference.len());
        assert_eq!(&mine[..shared], &reference[..shared], "node {i} disagrees about its log");
    }
}

/// A vote is granted at most once per term.
///
/// Checked directly rather than through an election, because the case that matters is two
/// candidates in the same term and a simulation is free not to produce one.
#[test]
fn a_node_votes_once_per_term() {
    let members = voters(3);
    let mut node = Raft::new(0, members, Timing::default(), 0);

    let ask = |candidate: NodeId| Message::RequestVote {
        term: 5,
        candidate,
        last_index: 0,
        last_term: 0,
    };
    let granted = |out: &big_cluster::raft::Output| {
        matches!(out.send.first(), Some((_, Message::VoteReply { granted: true, .. })))
    };

    assert!(granted(&node.deliver(ask(1), 0)), "the first ask was refused");
    assert!(!granted(&node.deliver(ask(2), 0)), "a second candidate got the same term's vote");
    // Asking again is not asking twice: a retransmission has to be answered the same way, or a
    // dropped reply costs an election.
    assert!(granted(&node.deliver(ask(1), 0)), "a retry was refused");
}

/// **A vote is refused to a log that is behind.** This is what keeps a node that missed the
/// last decisions from being elected and then imposing its own log on everybody.
#[test]
fn a_stale_log_cannot_win_a_vote() {
    let mut sim = Sim::new(3);
    sim.run(5_000);
    let leader = sim.leader();
    let behind = (0..3).find(|i| *i != leader).expect("a follower");

    // Cut one node off, then decide something the other two agree on.
    for i in 0..3 {
        if i != behind {
            sim.cut.insert((behind, i));
        }
    }
    assert!(sim.propose(leader, owning(&[2, 2, 2])));
    sim.run(4_000);
    assert!(sim.nodes[leader].commit_index() >= 2, "the majority did not commit");

    // The isolated node has been standing for election the whole time and has a high term. It
    // still must not win, because its log is missing what the others committed.
    sim.cut.clear();
    sim.run(10_000);

    let winner = sim.leader();
    assert_ne!(winner, behind, "a node missing a committed decision was elected");
    assert!(
        sim.applied[behind].contains(&owning(&[2, 2, 2])),
        "the node that was behind never caught up"
    );
}

/// A follower whose log disagrees has the disagreement removed, not merged.
#[test]
fn a_conflicting_log_is_truncated_and_repaired() {
    let members = voters(3);
    let mut node = Raft::new(1, members, Timing::default(), 0);

    // Term 1 appends two entries.
    node.deliver(
        Message::Append {
            term: 1,
            leader: 0,
            prev_index: 0,
            prev_term: 0,
            entries: vec![
                big_cluster::raft::Entry { term: 1, decision: Decision::Noop },
                big_cluster::raft::Entry { term: 1, decision: owning(&[5, 5, 5]) },
            ],
            commit: 0,
        },
        0,
    );
    assert_eq!(node.log().len(), 3);

    // A leader of term 2 says the second entry was never agreed. It goes.
    node.deliver(
        Message::Append {
            term: 2,
            leader: 2,
            prev_index: 1,
            prev_term: 1,
            entries: vec![big_cluster::raft::Entry { term: 2, decision: owning(&[6, 6, 6]) }],
            commit: 2,
        },
        0,
    );
    assert_eq!(node.log().len(), 3);
    assert_eq!(node.log()[2].decision, owning(&[6, 6, 6]));
    assert_eq!(node.term(), 2);
}

/// An append from a stale term is refused outright, and the stale leader is told the real one.
#[test]
fn an_append_from_an_old_term_is_refused() {
    let members = voters(3);
    let mut node = Raft::new(1, members, Timing::default(), 0);
    node.deliver(
        Message::Append {
            term: 4,
            leader: 0,
            prev_index: 0,
            prev_term: 0,
            entries: Vec::new(),
            commit: 0,
        },
        0,
    );
    assert_eq!(node.term(), 4);

    let out = node.deliver(
        Message::Append {
            term: 2,
            leader: 2,
            prev_index: 0,
            prev_term: 0,
            entries: vec![big_cluster::raft::Entry { term: 2, decision: owning(&[1, 1, 1]) }],
            commit: 1,
        },
        0,
    );
    assert!(matches!(
        out.send.first(),
        Some((2, Message::AppendReply { success: false, term: 4, .. }))
    ));
    assert_eq!(node.log().len(), 1, "a stale leader appended something");
}

/// The election is deterministic, so a test that runs it twice gets the same answer. Without
/// that, a failure here would be a failure that reproduces one run in ten.
#[test]
fn the_same_cluster_elects_the_same_node_twice() {
    let first = {
        let mut sim = Sim::new(5);
        sim.run(8_000);
        sim.leader()
    };
    let second = {
        let mut sim = Sim::new(5);
        sim.run(8_000);
        sim.leader()
    };
    assert_eq!(first, second);
}

/// A node that was elected knows it, a node that was not does not, and nobody is a candidate
/// once the dust has settled.
#[test]
fn the_cluster_settles() {
    let mut sim = Sim::new(5);
    sim.run(8_000);
    for (i, node) in sim.nodes.iter().enumerate() {
        assert!(
            matches!(node.role(), Role::Leader | Role::Follower),
            "node {i} is still standing for election"
        );
    }
}

// -------------------------------------------------------------------------------------------
// What a restart may not lose
// -------------------------------------------------------------------------------------------

/// **A vote that is forgotten is a vote that can be cast twice**, which is two leaders in one
/// term. The three fields below are the ones a restart may not lose, and this is the round trip
/// that says so.
#[test]
fn a_restart_keeps_the_term_the_vote_and_the_log() {
    let dir = tempfile::tempdir().unwrap();
    let store = big_cluster::raft::FileStore::new(dir.path().join("state.raft"));
    assert!(store.load().unwrap().is_none(), "an absent file is not a failure");

    let log = vec![
        big_cluster::raft::Entry { term: 0, decision: Decision::Noop },
        big_cluster::raft::Entry { term: 3, decision: Decision::Noop },
        big_cluster::raft::Entry { term: 3, decision: owning(&[2, 0, 1]) },
        big_cluster::raft::Entry { term: 4, decision: Decision::Ranges(map_of(&[1, 1], &[0, 2])) },
        // A configuration change is a decision like any other and has to survive the same trip.
        big_cluster::raft::Entry {
            term: 4,
            decision: Decision::Members(vec![
                Member {
                    name: "a".to_string(),
                    addr: "10.0.0.1:7654".to_string(),
                    state: MemberState::Voter,
                },
                Member {
                    name: "d".to_string(),
                    addr: "10.0.0.4:7654".to_string(),
                    state: MemberState::Learner,
                },
            ]),
        },
    ];
    let state = State {
        term: 4,
        voted_for: Some(2),
        commit: 3,
        base: 0,
        log: log.clone(),
        seed: voters(3),
    };
    store.save(&state).unwrap();

    let back = store.load().unwrap().expect("just written");
    assert_eq!(back, state, "the seed included: a snapshot replaces it, so it has to be kept");

    // **The commit point is part of the state.** A node that forgot it would replay its whole
    // log into the state machine on the way up, applying a map no majority ever agreed to.
    assert_eq!(back.commit, 3);

    // A node with no vote is a different state from a node that voted for node zero, and the
    // two must not encode alike.
    store
        .save(&State { term: 9, voted_for: None, commit: 1, base: 0, log, seed: Vec::new() })
        .unwrap();
    assert_eq!(store.load().unwrap().unwrap().voted_for, None);
}

/// A node comes back holding what it held, and does not vote for a log that is behind its own.
#[test]
fn a_restarted_node_still_refuses_a_stale_candidate() {
    let dir = tempfile::tempdir().unwrap();
    let store = big_cluster::raft::FileStore::new(dir.path().join("state.raft"));
    let log = vec![
        big_cluster::raft::Entry { term: 0, decision: Decision::Noop },
        big_cluster::raft::Entry { term: 7, decision: owning(&[1, 0]) },
    ];
    store
        .save(&State { term: 7, voted_for: Some(1), commit: 1, base: 0, log, seed: Vec::new() })
        .unwrap();

    let state = store.load().unwrap().unwrap();
    let mut node = Raft::new(0, voters(3), Timing::default(), 0);
    node.restore(state);
    assert_eq!(node.term(), 7);

    // A candidate at a higher term with an empty log: newer term, older log. Refused, because
    // the log is what decides, not the term.
    let out = node
        .deliver(Message::RequestVote { term: 8, candidate: 2, last_index: 0, last_term: 0 }, 0);
    assert!(matches!(out.send.first(), Some((2, Message::VoteReply { granted: false, .. }))));
    // The term still moves: a higher term is not an opinion.
    assert_eq!(node.term(), 8);
}

/// Bytes that are not a state file are refused rather than half-read.
#[test]
fn a_damaged_state_file_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.raft");
    let store = big_cluster::raft::FileStore::new(&path);
    store
        .save(&State {
            term: 2,
            voted_for: Some(0),
            commit: 0,
            base: 0,
            log: vec![big_cluster::raft::Entry { term: 0, decision: Decision::Noop }],
            seed: Vec::new(),
        })
        .unwrap();

    let good = std::fs::read(&path).unwrap();
    for cut in 0..good.len() {
        std::fs::write(&path, &good[..cut]).unwrap();
        assert!(store.load().is_err(), "{cut} bytes of {} loaded", good.len());
    }
    std::fs::write(&path, b"not a raft file at all").unwrap();
    assert!(store.load().is_err());
}

// -------------------------------------------------------------------------------------------
// The numbers a deployment actually runs on
// -------------------------------------------------------------------------------------------

/// **The shipped clocks elect a leader and commit a decision**, on a virtual clock so that
/// checking them costs microseconds rather than the seconds they describe.
///
/// The relationships that make those numbers safe - a heartbeat several times inside the
/// shortest election, a spread that breaks split votes - are asserted at compile time next to
/// the constants. This is the other half: that the numbers work, not just that they are
/// ordered correctly.
#[test]
fn the_shipped_clocks_elect_and_commit() {
    for size in [3usize, 5] {
        let mut sim = Sim::with_timing(size, Timing::default());
        // Twice the longest possible timeout, which is what an election is allowed to take
        // when the first one splits.
        sim.run(2 * (Timing::default().election_min + Timing::default().election_spread));
        assert_eq!(sim.leaders().len(), 1, "{size} nodes elected {:?}", sim.leaders());

        let leader = sim.leader();
        assert!(sim.propose(leader, owning([1; 5][..size].to_vec().as_slice())));
        sim.run(4 * Timing::default().heartbeat);
        for i in 0..size {
            assert!(
                !sim.applied[i].is_empty(),
                "node {i} applied nothing under the shipped clocks"
            );
        }
    }
}

/// A leader on the shipped clocks is not deposed while it is doing its job.
///
/// The failure this rules out is a heartbeat interval too close to the election timeout: the
/// cluster would re-elect every second or two, and every election is a window where a range is
/// not served. Twenty seconds of virtual time, one leader throughout.
#[test]
fn the_shipped_clocks_do_not_re_elect_a_healthy_leader() {
    let mut sim = Sim::with_timing(5, Timing::default());
    sim.run(2 * (Timing::default().election_min + Timing::default().election_spread));
    let leader = sim.leader();
    let term = sim.nodes[leader].term();

    sim.run(20_000);
    assert_eq!(sim.leader(), leader, "the leader changed with nothing wrong");
    assert_eq!(
        sim.nodes[leader].term(),
        term,
        "the term moved with nothing wrong: an election was held for no reason"
    );
}

// -------------------------------------------------------------------------------------------
// Membership, changed while the cluster runs
//
// **The part of Raft most often got wrong**, and the reason it stayed out of this file for so
// long. Three rules, and every one of them has a way of failing quietly rather than loudly.
// -------------------------------------------------------------------------------------------

fn learner(name: &str) -> Member {
    Member {
        name: name.to_string(),
        addr: "10.0.0.9:7654".to_string(),
        state: MemberState::Learner,
    }
}

/// **A configuration change takes effect when it is appended, not when it commits.**
///
/// The majority that commits an entry has to be the majority the entry describes. A leader that
/// waited for the commit would be counting a quorum the change is in the middle of abolishing.
#[test]
fn a_membership_change_counts_from_the_moment_it_is_appended() {
    let mut node = Raft::new(0, voters(1), Timing::default(), 0);
    node.tick(2_000);
    assert!(node.is_leader(), "alone, so a majority of one");
    assert_eq!(node.voters(), &[0]);

    // A second voter: the majority is now two, immediately - before anything has committed.
    let mut next = voters(1);
    next.push(Member {
        name: "n1".to_string(),
        addr: "10.0.0.1:7654".to_string(),
        state: MemberState::Voter,
    });
    node.propose(Decision::Members(next)).expect("the leader proposes");
    assert_eq!(node.voters(), &[0, 1], "the new member counts at once");
    assert_eq!(node.members(), 2);
}

/// **A learner replicates and does not vote.** Counting one towards a majority would raise the
/// bar for every election while it contributed nothing to one - and a node still filling up is
/// exactly the node least able to help.
#[test]
fn a_learner_is_replicated_to_and_does_not_count_towards_a_majority() {
    let mut node = Raft::new(0, voters(1), Timing::default(), 0);
    node.tick(2_000);

    let mut next = voters(1);
    next.push(learner("joining"));
    node.propose(Decision::Members(next)).expect("the leader proposes");

    assert_eq!(node.members(), 2, "it is replicated to");
    assert_eq!(node.voters(), &[0], "and it does not vote");
    // Still a majority of one, so this node can still commit on its own.
    assert!(node.is_leader());
}

/// **A membership that was appended and then truncated away goes back.**
///
/// This is the one that fails silently. A leader with a better log can take away an entry this
/// node already applied; a membership mutated in place would have no way back, and the node
/// would go on counting a quorum that never existed. Deriving it from the log makes the undo
/// free - which is the whole reason it is derived.
#[test]
fn a_membership_taken_away_by_a_better_log_is_taken_back() {
    let mut node = Raft::new(1, voters(3), Timing::default(), 0);

    // A leader at term 1 appends a change this node applies on the spot.
    let mut grown = voters(3);
    grown.push(learner("joining"));
    let out = node.deliver(
        Message::Append {
            term: 1,
            leader: 0,
            prev_index: 0,
            prev_term: 0,
            entries: vec![big_cluster::raft::Entry { term: 1, decision: Decision::Members(grown) }],
            commit: 0,
        },
        0,
    );
    assert!(matches!(out.send.first(), Some((0, Message::AppendReply { success: true, .. }))));
    assert_eq!(node.members(), 4, "applied on append, before any commit");

    // A different leader at a higher term overwrites that index with something else.
    node.deliver(
        Message::Append {
            term: 2,
            leader: 2,
            prev_index: 0,
            prev_term: 0,
            entries: vec![big_cluster::raft::Entry { term: 2, decision: Decision::Noop }],
            commit: 0,
        },
        0,
    );
    assert_eq!(node.members(), 3, "the change went away with the entry that carried it");
    assert_eq!(node.voters(), &[0, 1, 2]);
}

/// A node that comes back reads its cluster out of its own log, not out of the file it was
/// first started with - which, once a node has joined or left, describes a cluster that is gone.
#[test]
fn a_restarted_node_comes_back_with_the_cluster_as_the_log_left_it() {
    let mut grown = voters(3);
    grown.push(learner("joining"));
    let log = vec![
        big_cluster::raft::Entry { term: 0, decision: Decision::Noop },
        big_cluster::raft::Entry { term: 5, decision: Decision::Members(grown) },
    ];

    // Started from a file that knows three nodes, restored from a log that knows four.
    let mut node = Raft::new(0, voters(3), Timing::default(), 0);
    assert_eq!(node.members(), 3);
    node.restore(State { term: 5, voted_for: Some(0), commit: 1, base: 0, log, seed: Vec::new() });

    assert_eq!(node.members(), 4, "the log is what says who is in the cluster");
    assert_eq!(node.voters(), &[0, 1, 2], "and the fourth is still catching up");
}

/// A node this cluster has removed can still be running and still campaigning. Granting it a
/// vote would let a machine nobody has agreed to lead a cluster it has left.
#[test]
fn a_candidate_that_is_not_a_member_here_gets_no_vote() {
    let mut node = Raft::new(0, voters(2), Timing::default(), 0);
    let out = node
        .deliver(Message::RequestVote { term: 9, candidate: 5, last_index: 0, last_term: 0 }, 0);
    assert!(
        matches!(out.send.first(), Some((5, Message::VoteReply { granted: false, .. }))),
        "{:?}",
        out.send
    );
}

/// And the other half: a node that does not vote does not campaign either. One that did would
/// depose a working leader once per election timeout for as long as it was running.
#[test]
fn a_node_that_does_not_vote_never_stands_for_election() {
    let mut members = voters(2);
    members[0].state = MemberState::Learner;
    let mut node = Raft::new(0, members, Timing::default(), 0);

    node.tick(100_000);
    assert!(!node.is_leader());
    assert_eq!(node.term(), 0, "it never even raised the term");
}

// -------------------------------------------------------------------------------------------
// A log that does not grow for ever
//
// The log used to be short by construction - one entry per election, one per machine that dies.
// Once a balancer proposes, that is no longer true, and the whole log is rewritten every time
// anything is persisted. So it is compacted; and compacting means a follower can fall behind
// what the leader still holds, which is what the snapshot is for.
// -------------------------------------------------------------------------------------------

/// **Nothing a follower still needs is dropped.** Compaction is best effort on purpose: a node
/// that is down holds the base where it is, which costs disk and keeps recovery cheap.
#[test]
fn a_leader_keeps_every_entry_a_follower_has_not_stored() {
    let mut sim = Sim::new(3);
    sim.run(3_000);
    let leader = sim.leader();

    for i in 0..8 {
        sim.nodes[leader].propose(owning(&[i % 3, (i + 1) % 3]));
        sim.run(200);
    }
    // Forced, because the shipped margin deliberately keeps far more than this test writes.
    sim.nodes[leader].compact(0);
    assert!(sim.nodes[leader].base() > 0, "a log every node has stored is one worth compacting");

    // One node goes away. From here the base cannot move past what it last stored, however
    // much the other two decide.
    let absent = (leader + 1) % 3;
    sim.down.insert(absent);
    let held = sim.nodes[leader].base();
    for i in 0..8 {
        sim.nodes[leader].propose(owning(&[i % 3, (i + 2) % 3]));
        sim.run(200);
    }
    sim.nodes[leader].compact(0);
    assert_eq!(
        sim.nodes[leader].base(),
        held,
        "the base did not move past a node that is not storing anything"
    );
}

/// **A follower the leader has compacted past is handed the answer itself.**
///
/// There is no prefix left to match against, so nothing can be merged: the state is taken as
/// given, and the follower is caught up in one message rather than never.
#[test]
fn a_follower_that_falls_behind_the_base_is_caught_up_by_a_snapshot() {
    let mut sim = Sim::new(3);
    sim.run(3_000);
    let leader = sim.leader();
    let absent = (leader + 1) % 3;

    // It misses everything, and the two that are left keep deciding - a majority of three is
    // two, so the log advances without it.
    sim.down.insert(absent);
    for i in 0..40 {
        sim.nodes[leader].propose(owning(&[i % 3, (i + 1) % 3]));
        sim.run(200);
    }

    // Its `matched` is stale rather than absent, so the leader is still holding the base for
    // it. Forcing the compaction is what puts it behind - which is the state this exists for.
    sim.nodes[leader].compact(0);
    let base = sim.nodes[leader].base();

    sim.down.remove(&absent);
    sim.run(4_000);

    assert!(
        sim.nodes[absent].base() >= base || sim.nodes[absent].last_index() >= base,
        "it came back to a log it could not extend and was handed the state instead"
    );
    // And it agrees about the one thing the agreement decides.
    let theirs = sim.applied[absent].last().cloned();
    let mine = sim.applied[leader].last().cloned();
    assert!(theirs.is_some(), "it applied something");
    assert_eq!(theirs, mine, "and it is what the leader applied");
}

/// A restart after a compaction comes back to the compacted log, not to an empty one.
#[test]
fn a_compacted_log_survives_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let store = big_cluster::raft::FileStore::new(dir.path().join("state.raft"));
    // With a reservation in it: the ceiling on record ids is the one thing in the map that a
    // successor cannot rebuild from anywhere else, so it had better survive a restart.
    let mut kept = map_of(&[1, 0], &[]);
    kept.reserve("tx", 1 << 16);
    kept.schema_ready = false;
    let log = vec![
        // The sentinel is the state as of the base, not an empty entry.
        big_cluster::raft::Entry { term: 3, decision: Decision::Ranges(kept) },
        big_cluster::raft::Entry { term: 4, decision: Decision::Noop },
    ];
    let state = State { term: 4, voted_for: Some(1), commit: 41, base: 40, log, seed: voters(3) };
    store.save(&state).unwrap();
    assert_eq!(store.load().unwrap().unwrap(), state, "the base is part of what is written down");

    let mut node = Raft::new(0, voters(3), Timing::default(), 0);
    node.restore(store.load().unwrap().unwrap());
    assert_eq!(node.base(), 40);
    assert_eq!(node.last_index(), 41, "indices are log positions, not vector positions");
    assert_eq!(node.commit_index(), 41);
}

/// **A restart lands on the seed the snapshot handed over**, not on the file's.
///
/// A follower that fell behind the base is given the state whole, members included, and takes
/// those members as its seed. Until the seed was written down, a restart forgot that and went
/// back to the file - which, once anybody had joined or left, named a cluster that was gone.
#[test]
fn a_restored_seed_is_the_membership_when_the_log_says_nothing() {
    let mut grown = voters(3);
    grown.push(learner("joined"));
    let log = vec![big_cluster::raft::Entry { term: 3, decision: Decision::Noop }];

    let mut node = Raft::new(0, voters(3), Timing::default(), 0);
    node.restore(State { term: 3, voted_for: None, commit: 40, base: 40, log, seed: grown });
    assert_eq!(node.members(), 4, "the seed that was written down is the one that counts");
    assert_eq!(node.voters(), &[0, 1, 2], "and the fourth is a learner, as the seed said");
}

/// A state file from before the seed was recorded still loads, and reads as "the file's".
#[test]
fn a_state_file_from_before_the_seed_was_recorded_still_loads() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.raft");
    // `BIGRAFT2`, by hand: magic, term, vote, commit, base, then a log of one `Noop`.
    let mut bytes = b"BIGRAFT2".to_vec();
    for v in [7u64, u64::MAX, 0, 0, 1, 0] {
        bytes.extend_from_slice(&v.to_le_bytes());
    }
    bytes.push(0);
    std::fs::write(&path, &bytes).unwrap();

    let state = big_cluster::raft::FileStore::new(&path)
        .load()
        .unwrap()
        .expect("a file from the previous format is still a file");
    assert_eq!(state.term, 7);
    assert_eq!(state.voted_for, None);
    assert_eq!(state.log.len(), 1);
    assert!(state.seed.is_empty(), "nothing was written, so nothing is claimed");
}

/// **Compaction keeps the newest map and the newest membership**, whatever else it drops.
///
/// The follower's snapshot path folds the state into `log[0]`; the leader's own compaction did
/// not, so a leader that compacted past its last decision and then restarted came back with the
/// file's map and the file's members - a cluster that, once anything had moved or joined, no
/// longer existed.
#[test]
fn a_compacted_log_still_holds_the_map_and_the_membership() {
    let mut sim = Sim::new(4);
    sim.run(3_000);
    let leader = sim.leader();
    // The fourth node steps back to learning: still replicated to, no longer voting.
    let mut shrunk = voters(4);
    shrunk[3].state = MemberState::Learner;

    assert!(sim.propose(leader, owning(&[1, 2, 0])));
    sim.run(500);
    assert!(sim.propose(leader, Decision::Members(shrunk)));
    sim.run(500);
    for _ in 0..(Raft::KEEP_ENTRIES + 8) {
        assert!(sim.propose(leader, Decision::Noop));
        sim.run(100);
    }
    sim.nodes[leader].compact(0);
    assert!(sim.nodes[leader].base() > 0, "there was something to compact");

    let log = sim.nodes[leader].log();
    assert!(
        log.iter().any(|e| matches!(e.decision, Decision::Ranges(_))),
        "the newest map is still in the log"
    );
    assert!(
        log.iter().any(|e| matches!(e.decision, Decision::Members(_))),
        "and so is the newest membership"
    );

    // And a restart from that log lands on both, whatever the file says.
    let mut back = Raft::new(leader, voters(4), Timing::default(), 0);
    back.restore(sim.nodes[leader].state());
    assert_eq!(back.members(), 4);
    assert_eq!(back.voters(), &[0, 1, 2], "the membership survived the compaction and the restart");
}

/// **A compacted leader still fails a range over.**
///
/// The guard in front of a promotion compared the commit index - a log position - with the
/// length of the vector holding the log's suffix. Once anything had been compacted away the
/// two could never agree again, and every promotion after the first compaction was silently
/// refused on a cluster reporting itself healthy.
#[test]
fn a_compacted_leader_still_fails_a_range_over() {
    let mut sim = Sim::new(3);
    sim.run(3_000);
    let leader = sim.leader();
    for i in 0..8 {
        assert!(sim.propose(leader, owning(&[i % 3, (i + 1) % 3])));
        sim.run(200);
    }
    sim.nodes[leader].compact(0);
    assert!(sim.nodes[leader].base() > 0, "the log was compacted");

    // One copy of a replicated range goes quiet for longer than the lease allows.
    let dead = (leader + 1) % 3;
    sim.down.insert(dead);
    let leases = Leases::default();
    sim.run(leases.promote_after.as_millis() as u64 * 3);
    assert_eq!(sim.leader(), leader, "two of three are still a majority");

    let mut current = map_of(&[dead], &[]);
    current.ranges[0].group = vec![dead, leader];
    let next = Controller::promotion_for(&leases, &sim.nodes[leader], &current, sim.now)
        .expect("a range whose primary went quiet is given to the copy that is answering");
    assert_eq!(next.ranges[0].primary, leader);
    assert!(next.is_stale(dead), "and the one that went quiet is marked behind");
}

// -------------------------------------------------------------------------------------------
// The row-key namespace, moved by the agreement
//
// The most expensive decision the agreement makes: it copies every row key of every table to
// the successor. So it waits far longer than a range failover, it happens only when an operator
// has said it may, and the node it takes the namespace from is marked behind whether or not it
// holds a copy of anything - it may have interned row ids nobody else ever saw.
// -------------------------------------------------------------------------------------------

/// A cluster of `n` in which the range is served by the agreement's leader, so that nothing
/// about the *ranges* moves and what is left to observe is the namespace alone.
fn quiet_schema_leader(n: usize) -> (Sim, NodeId, NodeId) {
    let mut sim = Sim::new(n);
    sim.run(3_000);
    let leader = sim.leader();
    let holder = (leader + 1) % n;
    sim.down.insert(holder);
    (sim, leader, holder)
}

fn schema_after(ms: u64) -> Leases {
    Leases { move_schema_after: Some(std::time::Duration::from_millis(ms)), ..Leases::default() }
}

/// **Silence long enough for a range is not long enough for the namespace.** Moving it copies
/// every row key of every table, so it waits until the ranges have already failed over and the
/// map has settled - and on a blip it does nothing at all.
#[test]
fn the_namespace_waits_far_longer_than_a_range_does() {
    let (mut sim, leader, holder) = quiet_schema_leader(3);
    let leases = schema_after(15_000);
    let mut current = map_of(&[leader], &[]);
    current.schema_leader = holder;

    // Past `promote_after`, nowhere near the namespace's own wait.
    sim.run(6_000);
    assert!(
        Controller::promotion_for(&leases, &sim.nodes[leader], &current, sim.now).is_none(),
        "a range would have moved by now; the namespace has not"
    );

    sim.run(12_000);
    let next = Controller::promotion_for(&leases, &sim.nodes[leader], &current, sim.now)
        .expect("silence for the whole of the longer wait moves it");
    assert_eq!(next.schema_leader, leader);
    assert!(!next.schema_ready, "the successor holds no row keys yet, and says so");
    assert!(next.is_stale(holder), "the deposed node is marked behind, holding a copy or not");
}

/// **Off unless an operator turned it on.** The namespace stays where the file put it, however
/// long its holder has been gone - which is what every deployment before this did.
#[test]
fn the_namespace_is_never_moved_unless_a_lease_says_it_may() {
    let (mut sim, leader, holder) = quiet_schema_leader(3);
    let mut current = map_of(&[leader], &[]);
    current.schema_leader = holder;
    sim.run(60_000);
    assert_eq!(
        Controller::promotion_for(&Leases::default(), &sim.nodes[leader], &current, sim.now),
        None
    );
}

/// The successor is a voter that is answering and has not missed a write - the same three
/// conditions a range's replacement has to meet, for the same reasons.
#[test]
fn a_learner_a_draining_node_and_a_copy_that_is_behind_are_all_passed_over() {
    let mut sim =
        Sim::with_timing(4, Timing { election_min: 1_000, election_spread: 1_000, heartbeat: 200 });
    sim.run(3_000);
    let leader = sim.leader();
    // Everybody but the leader is unfit in some way: one is a learner, one is behind, and the
    // fourth is the node being deposed.
    let holder = (leader + 1) % 4;
    let learner_at = (leader + 2) % 4;
    let behind_at = (leader + 3) % 4;
    let mut members = voters(4);
    members[learner_at].state = MemberState::Learner;
    assert!(sim.propose(leader, Decision::Members(members)));
    sim.run(500);

    sim.down.insert(holder);
    sim.run(20_000);

    let mut current = map_of(&[leader], &[behind_at]);
    current.schema_leader = holder;
    let next =
        Controller::promotion_for(&schema_after(15_000), &sim.nodes[leader], &current, sim.now)
            .expect("there is one node left that is fit to hold it");
    assert_eq!(next.schema_leader, leader, "not the learner, not the one marked behind");
}

/// Two leaders elected in sequence decide the same thing from the same facts, which is what
/// keeps the namespace from moving twice.
#[test]
fn the_same_facts_always_choose_the_same_successor() {
    let (mut sim, leader, holder) = quiet_schema_leader(3);
    sim.run(20_000);
    let mut current = map_of(&[leader], &[]);
    current.schema_leader = holder;
    let leases = schema_after(15_000);

    let once = Controller::promotion_for(&leases, &sim.nodes[leader], &current, sim.now);
    for _ in 0..20 {
        assert_eq!(Controller::promotion_for(&leases, &sim.nodes[leader], &current, sim.now), once);
    }
    assert!(once.is_some());
}

/// Nothing is decided while the namespace is still being handed over: the successor has not
/// finished taking it, and choosing another one would throw away the keys it already holds.
#[test]
fn a_handover_that_has_not_finished_is_not_restarted() {
    let (mut sim, leader, holder) = quiet_schema_leader(3);
    sim.run(20_000);
    // Already deposed, already waiting on its successor.
    let mut current = map_of(&[leader], &[holder]);
    current.schema_leader = leader;
    current.schema_ready = false;

    assert_eq!(
        Controller::promotion_for(&schema_after(15_000), &sim.nodes[leader], &current, sim.now),
        None,
        "the node it names is this one, and it is answering; there is nothing to decide"
    );
}
