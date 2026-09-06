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

//! The coordinator, driven against peers that never open a socket.
//!
//! **This is the seam [`Peers`] exists for.** Every claim here used to need real servers on
//! real ports with one of them arranged to misbehave, which made a refusing peer, an
//! unreachable peer and a peer running a different build the *expensive* cases to test - when
//! they are precisely what this code is for. A fake peer table answers in microseconds and can
//! be told to fail on demand, so the interesting cases are now the cheap ones.
//!
//! What this cannot claim is anything about sockets: pooling, keep-alive, or a deadline against
//! a real clock. Those stay in `big-http/tests/cluster.rs`, over real ports, where they belong.

use big_cluster::client::{ClientError, PeerResponse, Peers, Repeatable};
use big_cluster::raft::Store as _;
use big_cluster::{
    raft, wire, Cluster, ClusterConfig, ClusterError, ClusterFile, FactValue, OwnedFact,
};
use big_embed::{Api, FieldKind, MemPager, QueryOptions, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const TWO: &str = r#"
schema_leader = "a"

[[node]]
name   = "a"
addr   = "10.0.0.1:7654"
shards = "0..64"

[[node]]
name   = "b"
addr   = "10.0.0.2:7654"
shards = "64.."
"#;

/// How the fake answers one request.
#[derive(Clone)]
enum Reply {
    /// A count, encoded the way an owner encodes one.
    Count(u64),
    /// A refusal with a status and a stable code, the way `big serve` writes one.
    Refuse(u16, &'static str, &'static str),
    /// Nothing is listening.
    Unreachable,
    /// Bytes from a build that does not agree with this one.
    Garbage,
}

/// A peer table that answers from a script instead of a socket.
struct Fake {
    /// One reply per node index. `None` at this node's own index, which is never asked.
    replies: Vec<Option<Reply>>,
    /// Every request that was made, in the order it was made.
    seen: Mutex<Vec<(usize, String)>>,
    calls: AtomicUsize,
}

impl Fake {
    fn new(replies: Vec<Option<Reply>>) -> Arc<Self> {
        Arc::new(Self { replies, seen: Mutex::new(Vec::new()), calls: AtomicUsize::new(0) })
    }

    fn asked(&self) -> Vec<(usize, String)> {
        self.seen.lock().unwrap().clone()
    }

    fn call_count(&self) -> usize {
        self.calls.load(Ordering::Relaxed)
    }
}

impl Peers for Fake {
    fn post(
        &self,
        node: usize,
        path: &str,
        _body: &[u8],
        _budget: Option<Duration>,
        _repeatable: Repeatable,
    ) -> Result<PeerResponse, ClientError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.seen.lock().unwrap().push((node, path.to_string()));
        match self.replies[node].clone().expect("this node is never asked over the table") {
            Reply::Count(n) => {
                Ok(PeerResponse { status: 200, body: wire::encode_value(&Value::Count(n)) })
            }
            Reply::Refuse(status, code, message) => Ok(PeerResponse {
                status,
                body: format!("{{\"error\":\"{message}\",\"code\":\"{code}\"}}").into_bytes(),
            }),
            Reply::Unreachable => Err(ClientError::Unreachable(std::io::Error::other("refused"))),
            Reply::Garbage => Ok(PeerResponse { status: 200, body: vec![0xff, 0xff, 0xff] }),
        }
    }

    fn len(&self) -> usize {
        self.replies.len()
    }

    fn addr(&self, node: usize) -> Option<String> {
        self.replies.get(node)?.as_ref().map(|_| "fake".to_string())
    }
}

/// A two-node cluster in which this node is `a`, against the peer table given.
fn cluster(peers: Arc<Fake>) -> Cluster<MemPager> {
    let api = Api::in_memory().unwrap();
    api.create_table("tx").unwrap();
    api.create_field("tx", "country", FieldKind::Set, 0).unwrap();
    let config: ClusterConfig = ClusterFile::parse(TWO).unwrap().for_node(Some("a"), "").unwrap();
    Cluster::with_peers(
        api,
        config,
        peers,
        Box::new(raft::Forgetful),
        raft::Timing::default(),
        Default::default(),
    )
    .expect("Forgetful loads nothing and so cannot fail")
}

fn count(c: &Cluster<MemPager>) -> Result<Value, ClusterError> {
    c.query("tx", "Count(All())", &QueryOptions::default())
}

#[test]
fn a_query_asks_the_owner_of_every_range_it_does_not_hold() {
    let peers = Fake::new(vec![None, Some(Reply::Count(7))]);
    let c = cluster(Arc::clone(&peers));

    // This node owns 0..64 and holds nothing; the other owns 64.. and says seven.
    assert_eq!(count(&c).unwrap().as_count(), Some(7));
    assert_eq!(peers.asked(), vec![(1, "/internal/query".to_string())]);
}

#[test]
fn an_owner_that_refuses_fails_the_query_rather_than_answering_partially() {
    // The CAP choice, and the one no number can show: an answer here is an aggregate, so a
    // count missing one node's contribution looks exactly like a correct count.
    let peers =
        Fake::new(vec![None, Some(Reply::Refuse(503, "not_serving", "b lost the agreement"))]);
    let c = cluster(peers);

    let e = count(&c).unwrap_err();
    assert!(matches!(e, ClusterError::Peer { status: 503, .. }), "got {e:?}");
    assert!(e.to_string().contains("not_serving"), "the peer's own code survives: {e}");
}

#[test]
fn an_owner_that_cannot_be_reached_names_the_node_and_its_shards() {
    // "Which part of the space went quiet" is the first thing an operator needs and the one
    // thing they cannot work out from a bare 503.
    let peers = Fake::new(vec![None, Some(Reply::Unreachable)]);
    let c = cluster(peers);

    let e = count(&c).unwrap_err();
    let said = e.to_string();
    assert!(matches!(e, ClusterError::Unreachable { .. }), "got {e:?}");
    assert!(said.contains("64.."), "names the range that went quiet: {said}");
}

#[test]
fn bytes_from_a_build_that_does_not_agree_are_refused_rather_than_decoded() {
    // Decoding what it can and skipping the rest would make a newer peer indistinguishable from
    // one that simply has no data, and the two call for opposite actions.
    let peers = Fake::new(vec![None, Some(Reply::Garbage)]);
    let c = cluster(peers);

    let e = count(&c).unwrap_err();
    assert!(matches!(e, ClusterError::Wire { .. }), "got {e:?}");
    assert!(e.to_string().contains('b'), "the node that sent them is named: {e}");
}

#[test]
fn a_write_that_lands_here_and_is_refused_there_is_reported_half_applied() {
    // There is no transaction across nodes. What makes that safe is that the error says which
    // shards landed, rather than reporting a batch as everywhere when it is not.
    let peers = Fake::new(vec![None, Some(Reply::Refuse(503, "not_serving", "b is not there"))]);
    let c = cluster(peers);

    let facts = vec![
        OwnedFact {
            field: "country".to_string(),
            record: 1,
            value: FactValue::Key("GB".to_string()),
        },
        OwnedFact {
            field: "country".to_string(),
            record: 64 << 20,
            value: FactValue::Key("US".to_string()),
        },
    ];
    let e = c.import("tx", &facts).unwrap_err();

    assert!(matches!(e, ClusterError::Partial { .. }), "got {e:?}");
    let said = e.to_string();
    assert!(said.contains("half applied"), "says the batch is half applied: {said}");
}

#[test]
fn a_query_this_node_answers_alone_asks_nobody() {
    // A range this node owns is answered in-process. Every leg that goes over the table is one
    // this node could not have answered, which is what makes the fan-out's cost readable.
    let peers = Fake::new(vec![None, Some(Reply::Count(0))]);
    let c = cluster(Arc::clone(&peers));

    c.local().count("tx").unwrap();

    assert_eq!(peers.call_count(), 0, "nothing was asked before the first query");
}

#[test]
fn a_cluster_of_one_runs_the_same_coordinator_and_asks_nobody() {
    // `big serve` without --cluster is a cluster of one. A second path for the un-clustered case
    // would be the path nobody tests, so this checks it is not a second path.
    let api = Api::in_memory().unwrap();
    api.create_table("tx").unwrap();
    let c = Cluster::solo(api, "127.0.0.1:7654");

    assert_eq!(count(&c).unwrap().as_count(), Some(0));
}

/// A cluster file with a copy, which is what makes the agreement run at all.
const THREE: &str = r#"
schema_leader = "a"

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
"#;

/// **A state file that cannot be read stops the node**, rather than being ignored.
///
/// The old code matched `if let Ok(Some(..)) = store.load()`, so a damaged file was
/// indistinguishable from a node that had never voted: it started at term 0 with an empty log,
/// free to vote a second time in a term it had already voted in. `raft.rs` proves the store
/// refuses those bytes; this proves the controller acts on the refusal.
#[test]
fn a_node_whose_vote_is_unreadable_refuses_to_start() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.raft");
    std::fs::write(&path, b"not a raft file at all").unwrap();

    let api = Api::in_memory().unwrap();
    let config: ClusterConfig = ClusterFile::parse(THREE).unwrap().for_node(Some("a"), "").unwrap();
    let peers = Fake::new(vec![None, Some(Reply::Unreachable), Some(Reply::Unreachable)]);

    let e = Cluster::with_peers(
        api,
        config,
        peers,
        Box::new(raft::FileStore::new(&path)),
        raft::Timing::default(),
        Default::default(),
    )
    .map(|_| ())
    .expect_err("a damaged state file is not a node that has never voted");
    assert_eq!(e.kind(), std::io::ErrorKind::InvalidData, "{e}");
}

/// **A restart replays only what was committed, counted from the base.**
///
/// The replay compared a position in the vector holding the log's suffix against the commit
/// index, which is a log position. Once anything had been compacted away, an entry past the
/// commit point looked committed, and a map no majority ever agreed to became this node's map.
#[test]
fn a_restart_applies_nothing_past_the_commit_point_once_the_log_has_a_base() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.raft");
    let config: ClusterConfig = ClusterFile::parse(THREE).unwrap().for_node(Some("a"), "").unwrap();
    let at = |epoch: u64| {
        let mut m = config.seed_map();
        m.epoch = epoch;
        raft::Decision::Ranges(m)
    };
    let log = vec![
        // Index 40, the sentinel; 41, committed; 42, still in flight.
        raft::Entry { term: 3, decision: at(2) },
        raft::Entry { term: 4, decision: at(3) },
        raft::Entry { term: 4, decision: at(4) },
    ];
    raft::FileStore::new(&path)
        .save(&raft::State { term: 4, voted_for: Some(0), commit: 41, base: 40, log, seed: vec![] })
        .unwrap();

    let api = Api::in_memory().unwrap();
    let peers = Fake::new(vec![None, Some(Reply::Unreachable), Some(Reply::Unreachable)]);
    let c = Cluster::with_peers(
        api,
        config,
        peers,
        Box::new(raft::FileStore::new(&path)),
        raft::Timing::default(),
        Default::default(),
    )
    .unwrap();
    assert_eq!(c.map().epoch, 3, "the committed map, not the one still in flight");
    c.stop();
}

/// The other half of the same rule: **a file that is simply not there is a fresh node**, and a
/// fresh node starts. Without this the fix above would be a cluster that cannot be deployed.
#[test]
fn a_node_that_has_never_voted_starts() {
    let dir = tempfile::tempdir().unwrap();
    let api = Api::in_memory().unwrap();
    let config: ClusterConfig = ClusterFile::parse(THREE).unwrap().for_node(Some("a"), "").unwrap();
    let peers = Fake::new(vec![None, Some(Reply::Unreachable), Some(Reply::Unreachable)]);

    let c = Cluster::with_peers(
        api,
        config,
        peers,
        Box::new(raft::FileStore::new(dir.path().join("absent.raft"))),
        raft::Timing::default(),
        Default::default(),
    )
    .expect("a node with no state file has never voted, which is a node that may start");
    c.stop();
}

/// A store that refuses every save, to stand in for a full disk.
struct Unwritable;

impl raft::Store for Unwritable {
    fn load(&self) -> std::io::Result<Option<raft::State>> {
        Ok(None)
    }

    fn save(&self, _: &raft::State) -> std::io::Result<()> {
        Err(std::io::Error::new(std::io::ErrorKind::StorageFull, "no space left on device"))
    }
}

/// **A node that cannot write its vote down stops being a node.**
///
/// The rule is the one the module has always stated: a vote that reaches the network and not
/// the disk is a vote this node can cast again after a restart, which is two leaders in one
/// term. What was missing is that the loop only dropped *that turn's* messages and then went
/// on delivering, ticking and sending with in-memory state the disk had never seen - so a
/// leader elected in a term its own file never recorded could come back after a crash and
/// grant that term's vote to somebody else.
#[test]
fn a_node_that_cannot_persist_stops_participating() {
    let api = Api::in_memory().unwrap();
    let config: ClusterConfig = ClusterFile::parse(THREE).unwrap().for_node(Some("a"), "").unwrap();
    let peers = Fake::new(vec![None, Some(Reply::Unreachable), Some(Reply::Unreachable)]);

    let c = Cluster::with_peers(
        api,
        config,
        peers,
        Box::new(Unwritable),
        raft::Timing::default(),
        Default::default(),
    )
    .expect("a store that loads nothing is a node that has never voted");

    let controller = c.controller().expect("three nodes run an agreement").clone();

    // The first thing this node does unprompted is stand for election, and standing means
    // writing down the term and the vote. That is the save that cannot happen.
    let wedged = wait_for(Duration::from_secs(5), || controller.wedged());
    assert!(wedged, "a node whose state file refuses every write must stop participating");

    // And it stays stopped rather than quietly carrying on: nothing it holds in memory was
    // ever agreed to, so it must look exactly like a node that is down.
    assert!(!controller.is_leader(), "a node that never wrote its vote down cannot have won");
    assert!(!c.may_serve(), "a wedged node lets its lease lapse rather than answering on it");
    c.stop();
}

/// Polls a condition rather than sleeping for a fixed time, so the test is as fast as the
/// machine allows and still passes on a slow one.
fn wait_for(within: Duration, mut done: impl FnMut() -> bool) -> bool {
    let deadline = std::time::Instant::now() + within;
    while std::time::Instant::now() < deadline {
        if done() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    done()
}
