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

use big_cluster::raft::{Decision, Message, NodeId, Ownership, Raft, Role, Store, Timing};
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
        let members: Vec<NodeId> = (0..n).collect();
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

fn owning(primary: &[NodeId]) -> Decision {
    Decision::Own(Ownership { primary: primary.to_vec(), stale: Vec::new() })
}

/// A node alone in its config file leads immediately: a majority of one is one.
///
/// Not a special case so much as the general one, and it is what lets `bigd` without peers use
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
    let members = vec![0, 1, 2];
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
    let members = vec![0, 1, 2];
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
    let members = vec![0, 1, 2];
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
        big_cluster::raft::Entry {
            term: 4,
            decision: Decision::Own(Ownership { primary: vec![1, 1], stale: vec![0, 2] }),
        },
    ];
    store.save(4, Some(2), &log).unwrap();

    let (term, voted, back) = store.load().unwrap().expect("just written");
    assert_eq!(term, 4);
    assert_eq!(voted, Some(2));
    assert_eq!(back, log);

    // A node with no vote is a different state from a node that voted for node zero, and the
    // two must not encode alike.
    store.save(9, None, &log).unwrap();
    assert_eq!(store.load().unwrap().unwrap().1, None);
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
    store.save(7, Some(1), &log).unwrap();

    let (term, voted, log) = store.load().unwrap().unwrap();
    let mut node = Raft::new(0, vec![0, 1, 2], Timing::default(), 0);
    node.restore(term, voted, log);
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
        .save(2, Some(0), &[big_cluster::raft::Entry { term: 0, decision: Decision::Noop }])
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
