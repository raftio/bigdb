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

//! More than one node, and the three roles one binary plays.
//!
//! Most of a distributed design is a choice about where data lives and how partial answers
//! combine, and the engine underneath answered both years before this crate existed. A record
//! id names its shard and the client picks the record id, so placement needs no coordination.
//! [`big_db::Matches`] is already a shard-wise merge, so there is no distributed set algebra to
//! design, only one to serialise. What is left is which node owns which shards, which is
//! [`config`], and how the row-key namespace stays the same on all of them, which is the schema
//! leader below.
//!
//! **A [`Cluster`] with one node is not a special case.** `big serve` without `--cluster` builds
//! one over `0..`, and every request takes the same path through this crate that it would take
//! with four peers. A separate un-clustered path would be the one nobody tests.
//!
//! **This node is never asked over a socket.** A coordinator that fanned out to its own
//! listener would spend a worker to reach a worker, and would deadlock outright once its pool
//! was full - the request holding the last worker would be waiting for a worker to answer it.
//! The local share of every fan-out is a direct call.
//!
//! **The map is a value the agreement decides, and the file only seeds it.** `cluster.toml`
//! says what the cluster was when it started; every committed decision replaces it. That is the
//! same rule ownership always followed - *the config's answer until the agreement has one* -
//! widened from *who serves a range* to *what the ranges are and who is in the cluster*. So a
//! range can be split, moved or merged, and a node can join or leave, without stopping anybody.
//!
//! Three things make that safe, and each is worth naming because each is a way it could
//! silently not be:
//!
//! - **A routed request says which shards it is for.** A node can hold more than one range, so
//!   a fan-out that asked it twice without naming one would have it answer twice over - a
//!   `Count` that is quietly double, with nothing downstream to contradict it.
//! - **A write says what it assumed.** Between a coordinator reading the map and its batch
//!   arriving, the map can change; without the check the batch lands on yesterday's owner, is
//!   reported written, and is never read again. The owner disagrees and the coordinator retries.
//! - **A move copies before it commits, and commits in one entry.** There is no committed state
//!   in which two nodes could both be asked for one record.
//!
//! **What this still does not give you**, spelled out because the absences are the design:
//!
//! - **No cross-node atomicity.** A batch spanning two owners is two commits, and a failure in
//!   the middle is reported as [`ClusterError::Partial`] rather than hidden.
//! - **No cluster-wide snapshot.** Each owner serves the fan-out from its own read transaction,
//!   taken when its part of the request arrived, so a distributed read can straddle two
//!   commits. Fixing that means a cluster-wide transaction id, which means the meta page flip
//!   stops being the only atomic point, which is a different engine.
//! - **No quorum reads or writes.** A read goes to one copy and a write goes to all of them.
//!   What a quorum would buy is bought instead by letting the write stand and marking the copy
//!   behind, which costs one entry in a log that is already there.
//! - **No repair in the background.** `POST /repair` is a thing an operator or cron runs. A
//!   repair is a scan and a copy, and a system that starts one by itself starts it at the worst
//!   possible moment.
//! - **Shards still do not move without a copy.** One process holds one file - the pager takes
//!   an exclusive lock - so a range that is not empty crosses the wire fragment by fragment.
//!   What changed is that it can do so while the cluster serves, not that it became free.

#![deny(unsafe_code)]

mod admin;
pub mod balance;
pub use admin::{Balanced, MemberReport, MoveReport, RangeReport, Topology};
mod ddl;
// The one item a sibling borrows across the split: `repair` recreates a field exactly as
// another node has it, and that is a `Ddl` rather than a repair concern.
/// Applying one schema change to one node's own database, which `big-http` does for a peer.
pub use ddl::apply_ddl;
pub(crate) use ddl::create_field;
mod fanout;
mod ownership;
mod query;
mod repair;
mod write;

pub mod client;
pub mod config;
pub mod controller;
pub mod counters;
pub mod digest;
pub mod error;
pub mod merge;
pub mod raft;
pub mod wire;

pub use client::{HttpPeers, Peer, Peers};
pub use config::{ClusterConfig, ClusterFile, ConfigError, Node, ShardRange};
pub use error::{ClusterError, Result};
pub use wire::{Assignment, Ddl, FactValue, OwnedFact};

use big_embed::{Api, KeyAssignment, PagerMut, Plan, QueryOptions, RecordId, RowId, Value};
use client::{ClientError, Repeatable};
use controller::{Controller, Leases};
use merge::Merge;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// What this build speaks between nodes.
///
/// **Bumped whenever a message changes shape**, and checked before any body is decoded. Without
/// it a node running a newer build sends bytes an older one reads as something else - a length
/// where a tag was, a container where a count was - and the failure is not a refusal, it is an
/// answer that is quietly wrong. That is the one class of failure this whole layer is built to
/// avoid, so it gets a number rather than a convention.
///
/// It is not the crate version. Most releases change nothing here, and a version that moves for
/// reasons the wire does not care about would make a rolling upgrade look impossible.
/// `2` since a table carries its storage engine: `Ddl::CreateTable` gained a byte and the
/// schema exchange gained one per table. Both are places where a `1` node reading a `2` body
/// would take the engine byte for the next field's length - exactly the quiet mistake above.
///
/// `3` since a table is in a database. **No message changed shape** - a table still travels as
/// one string - but that string is now `database.table` whenever the database is not the
/// default one, and a `2` node reading a `3` body would create a table whose name is literally
/// `sales.orders` in its own single namespace. That is the same class of failure as a
/// misaligned byte and worse to find: both nodes would report success, and the tables would be
/// in different places. The schema exchange carries the database for the same reason.
///
/// `4` since a view is a schema object: `Ddl` gained two tags and the schema exchange gained a
/// section. A `3` node handed a `CreateView` answers `BadTag` - a clean refusal rather than a
/// misread, because a tag it has never seen is the one thing this encoding checks first. The
/// bump is still made: the exchange's new section is a shape change, and a mixed-version
/// cluster where half the nodes silently lack a view is a cluster answering two different
/// questions depending on which node a client reaches.
/// `5` since a group says which kind of thing it is one of, and a grouping says how many columns
/// it is over. A grouping over a calendar bucket identifies its groups by the moment they start
/// rather than by a dictionary row, so a group's encoding gained a tag byte in front of the
/// number; and the pair grouping became a tuple grouping of any arity, retiring two tags and
/// adding three. A `4` node reading a `5` message would take that first tag as the top of a row
/// id - a misread rather than a refusal, which is exactly what a version bump exists to prevent.
///
/// `6` since a routed request says **which shards it is for**. `QueryRequest`, `RecordsRequest`
/// and `TableRequest` each gained a scope at the end, and `/internal/digest` grew a body that
/// used to be empty. A node may hold more than one range now, and a `5` node - which answers
/// every request from everything on its disk - asked twice by a `6` coordinator would return
/// its data twice, so `Count` would silently double. That is precisely the quiet mistake a
/// version exists to turn into a refusal.
///
/// `7` since a request to the schema leader says **who it thinks leads**. `InternRequest` and
/// `AllocateRequest` each gained an assumption at the end, and `/internal/schema/step-down`
/// was added. A `6` leader asked by a `7` coordinator would refuse the trailing bytes as
/// unreadable rather than intern for a coordinator it may no longer lead - the right failure,
/// and the version makes it a handshake failure instead of a per-request one.
///
/// The retired numbers are **not** reused. A stale peer that somehow got past the handshake
/// would then misparse rather than fail, which is what `finished` exists to prevent.
pub const WIRE_VERSION: u32 = 7;

/// The header carrying [`WIRE_VERSION`].
pub const WIRE_HEADER: &str = "x-big-wire";

/// The header carrying [`ClusterConfig::fingerprint`].
pub const CLUSTER_HEADER: &str = "x-big-cluster";

/// The routes one node uses to reach another.
///
/// Under `/internal/` so that the public surface stays the twelve routes it is: these carry a
/// binary body, answer with one, and mean nothing to a client.
pub mod path {
    pub const QUERY: &str = "/internal/query";
    pub const IMPORT: &str = "/internal/import";
    pub const DELETE: &str = "/internal/delete";
    pub const RECORDS: &str = "/internal/records";
    pub const INTERN: &str = "/internal/intern";
    pub const ALLOCATE: &str = "/internal/allocate";
    pub const NEXT_RECORD: &str = "/internal/next-record";
    pub const DDL: &str = "/internal/ddl";
    pub const DIGEST: &str = "/internal/digest";
    pub const RAFT: &str = "/internal/raft";
    pub const FRAGMENTS: &str = "/internal/fragments";
    pub const FRAGMENT: &str = "/internal/fragment";
    pub const FRAGMENT_PUT: &str = "/internal/fragment/put";
    pub const KEYS: &str = "/internal/keys";
    pub const KEYS_PUT: &str = "/internal/keys/put";
    pub const REPAIRED: &str = "/internal/repaired";
    pub const SCHEMA: &str = "/internal/schema";
    /// Which version of the map a node has applied. Asked before its data is taken away.
    pub const EPOCH: &str = "/internal/epoch";
    /// What a node weighs, for the balancer.
    pub const LOAD: &str = "/internal/load";
    /// One past the highest record id a node has handed out, per table. Read and written when
    /// the row-key namespace changes hands.
    pub const FLOORS: &str = "/internal/floors";
    pub const FLOORS_PUT: &str = "/internal/floors/put";
    /// Stop interning: the namespace is being handed over. Asked of the source before its
    /// floor is read, so that the floor is final rather than racing.
    pub const SCHEMA_STEP_DOWN: &str = "/internal/schema/step-down";
    /// Raise the agreement's ceiling on record ids for a table. Asked of the agreement's
    /// leader by a schema leader that is not it.
    pub const RESERVE: &str = "/internal/reserve";
}

/// One node, playing whichever of the three roles a given request needs.
///
/// **Coordinator** - whichever node received the request. It plans, fans out and merges, and
/// holds no state between requests, which is why there is no coordinator tier.
/// **Shard owner** - it holds the file containing its own range, which is what a node was
/// before this crate existed. **Schema leader** - exactly one node, named in the config, which
/// owns the row-key mappings.
pub struct Cluster<P: PagerMut> {
    config: ClusterConfig,
    /// What has happened between this node and the others, for `/metrics`.
    counters: counters::Counters,
    api: Api<P>,
    /// How the other nodes are reached. Shared with the agreement, so consensus traffic and
    /// query traffic use the same connections and the same tokens.
    ///
    /// A trait object rather than the peer table: everything this crate decides about a peer
    /// that is slow, refusing, or running a different build used to need a real server on a
    /// real port to exercise. See [`client::Peers`].
    peers: Arc<dyn Peers>,
    /// The record ids this node has handed out and not yet seen land, per table.
    ///
    /// **Only the schema leader ever reads or writes this**, which is what makes one number in
    /// memory enough. An allocation takes the greater of the highest id anywhere and this
    /// floor: the first term keeps it above ids written explicitly or through the import route,
    /// and the second keeps two allocations that have not yet been committed from meeting.
    ///
    /// Not persisted, and does not need to be. A leader that restarts re-derives the first term
    /// from the data itself, and the floor only ever has to outlive the writes it is ahead of.
    allocated: std::sync::Mutex<std::collections::BTreeMap<String, RecordId>>,
    /// One past the map epoch at which this node was last told to stop leading the schema.
    /// Zero for never.
    ///
    /// A move of the namespace happens in steps, and between "stop" and the decision landing
    /// this node still leads by the map and must not act like it - a source that went on
    /// interning while its floor was being read would be the second interner the whole move
    /// exists to prevent. Compared against the map's epoch, so leadership that comes back at
    /// a later epoch is leadership again.
    stood_down_at: std::sync::atomic::AtomicU64,
    /// Whether the last attempt to hand the namespace to an elected successor found two
    /// survivors disagreeing about what a row id means. Nobody interns while this is set;
    /// `/metrics` is how somebody finds out.
    handover_blocked: std::sync::atomic::AtomicBool,
    /// The agreement, when there is anything to agree about.
    ///
    /// `None` when no range has a copy: a range of one cannot fail over to anything, so
    /// running an election to decide who serves it would be machinery deciding a question
    /// with one possible answer.
    controller: Option<Arc<Controller>>,
    /// **Which node answers for which part of the space.** The one place routing reads.
    ///
    /// Seeded from the cluster file and replaced by every committed `Decision::Ranges`, which
    /// is the same rule ownership always followed - "the config's answer until the agreement
    /// has one" - widened from *who serves a range* to *what the ranges are*.
    ///
    /// Shared with the controller rather than owned by it, because a cluster with no copies
    /// runs no agreement at all and still has to route. One value, not two that can drift.
    ranges: Arc<std::sync::RwLock<raft::RangeMap>>,
}

impl<P: PagerMut + Sync> Cluster<P> {
    /// A database that is not clustered: one node, every shard, nobody to disagree with.
    ///
    /// Infallible where [`Cluster::new`] is not, and provably: a cluster of one is not
    /// replicated, so no agreement is started and no state file is read.
    pub fn solo(api: Api<P>) -> Self {
        Self::new(api, ClusterConfig::solo("127.0.0.1:7654"), None, Box::new(raft::Forgetful))
            .expect("a cluster of one starts no agreement, so nothing can fail to load")
    }

    /// A node in a configured cluster. `tls` is what this node presents to its peers.
    ///
    /// **A client certificate, not a shared secret.** The cluster used to hand every node the
    /// same `admin` bearer token, so one leaked credential was every node - a gap
    /// `docs/clustering.md` recorded and could not close, because a token has no way to say
    /// which node is holding it. A certificate does: the peer CA signs one per node, the name in
    /// it is the name in the cluster file, and revoking one node revokes one node.
    ///
    /// `store` is where this node's vote is kept, and it is a parameter rather than a default
    /// because there is no safe default: a node that forgets its vote can cast a second one in
    /// the same term, which is two leaders. [`raft::Forgetful`] is the honest name for the
    /// thing to pass when nothing can be taken away from this node anyway.
    pub fn new(
        api: Api<P>,
        config: ClusterConfig,
        tls: Option<Arc<big_tls::ClientTls>>,
        store: Box<dyn raft::Store>,
    ) -> std::io::Result<Self> {
        Self::with_timing(api, config, tls, store, raft::Timing::default(), Leases::default())
    }

    /// The same, with the clocks the agreement runs on.
    ///
    /// The defaults are unhurried on purpose - this decides who serves a range after a machine
    /// has died, and paying an extra second to be sure costs less than an election held
    /// because a garbage collector paused. A test cannot afford to wait that long for
    /// something it is trying to observe, which is the only reason this is public.
    pub fn with_timing(
        api: Api<P>,
        config: ClusterConfig,
        tls: Option<Arc<big_tls::ClientTls>>,
        store: Box<dyn raft::Store>,
        timing: raft::Timing,
        leases: Leases,
    ) -> std::io::Result<Self> {
        let peers = Arc::new(client::HttpPeers::new(
            config.nodes().iter().map(|n| n.name.clone()),
            config.nodes().iter().map(|n| n.addr.clone()),
            config.this_index(),
            tls,
            config.fingerprint(),
        ));
        Self::with_peers(api, config, peers, store, timing, leases)
    }

    /// The same again, against a peer table the caller supplies.
    ///
    /// The seam a test uses. Everything above eventually arrives here, so a fake peer table
    /// exercises the same coordinator the binary runs - which is the point: a second path for
    /// the tested case would be the path nobody ships.
    pub fn with_peers(
        api: Api<P>,
        config: ClusterConfig,
        peers: Arc<dyn Peers>,
        store: Box<dyn raft::Store>,
        timing: raft::Timing,
        leases: Leases,
    ) -> std::io::Result<Self> {
        let ranges = Arc::new(std::sync::RwLock::new(config.seed_map()));
        let controller = match config.is_replicated() {
            false => None,
            true => Some(Controller::start(
                &config,
                Arc::clone(&peers),
                store,
                timing,
                leases,
                Arc::clone(&ranges),
            )?),
        };
        Ok(Self {
            config,
            api,
            peers,
            controller,
            ranges,
            counters: counters::Counters::new(),
            allocated: Default::default(),
            stood_down_at: Default::default(),
            handover_blocked: Default::default(),
        })
    }

    /// What an operator can see about this node's place in the cluster.
    ///
    /// A snapshot rather than a handle: rendering it must never hold anything a request needs.
    pub fn counters(&self) -> counters::Snapshot {
        let (term, leader) = match &self.controller {
            None => (0, false),
            Some(c) => (c.term(), c.is_leader()),
        };
        let map = self.map();
        let behind = map.stale.len();
        let moving = map.ranges.iter().filter(|r| r.moving.is_some()).count();
        counters::Snapshot {
            nodes: self.config.nodes().len(),
            peers: self.config.nodes().len() - 1,
            replicated: self.config.is_replicated(),
            serving: self.may_serve(),
            term,
            leader,
            behind,
            moving,
            schema_ready: map.schema_ready,
            handover_blocked: self.handover_blocked.load(std::sync::atomic::Ordering::Relaxed),
            counts: self.counters.read(),
        }
    }

    /// Stops the agreement's threads. A daemon runs until it is killed; anything that has to
    /// stand down cleanly calls this.
    pub fn stop(&self) {
        if let Some(c) = &self.controller {
            c.stop();
        }
    }

    /// Hands the listener's peer roster to the agreement, so that it follows the membership.
    ///
    /// Called by whoever built the listener, because this node has to exist before it can be
    /// listened for. A cluster with no agreement keeps whatever roster it was built with,
    /// which is right: nothing can change its membership either.
    pub fn follow_roster(&self, tls: big_tls::TlsConfig) {
        if let Some(c) = &self.controller {
            c.follow_roster(tls);
        }
    }

    /// The agreement, for the routes that carry it and for anything asking who leads.
    pub fn controller(&self) -> Option<&Arc<Controller>> {
        self.controller.as_ref()
    }

    /// The database this node holds. Everything that is not about other nodes goes here:
    /// metrics, durability, and the schema snapshot a coordinator plans against.
    pub fn local(&self) -> &Api<P> {
        &self.api
    }

    pub fn config(&self) -> &ClusterConfig {
        &self.config
    }

    /// Which node answers for which part of the space, right now.
    ///
    /// A clone rather than a guard: this is read on the path of every request, so the lock is
    /// held for a pointer's worth of time and never across any I/O.
    pub fn map(&self) -> raft::RangeMap {
        self.ranges.read().expect("no panic holds this lock").clone()
    }
}

/// What a write actually managed to do.
///
/// A count and a list, because those are two different facts and rolling them together is how a
/// caller comes to believe a batch is everywhere when it is not. `missed` is empty on the path
/// everybody hopes for; a name in it is a copy that could not be reached, which is a copy the
/// agreement has marked behind and `POST /repair` is there to catch up.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct WriteOutcome {
    /// Facts written, or records removed.
    pub count: u64,
    /// Copies that did not take it, and why. Each of these is a divergence that `POST /repair`
    /// is there to close.
    pub missed: Vec<String>,
}

/// What a repair managed for one copy of one range.
///
/// **One per copy *and* range.** A behind copy can hold several ranges, and each is caught up
/// from whoever serves that one - which need not be the same node twice. Naming only the copy
/// would make two entries look identical while describing different work.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct RepairReport {
    pub node: String,
    /// The range that was repaired, as `0..64`.
    pub shards: String,
    /// How many fragments had to move. Zero means the copy was already correct and only the
    /// mark was stale, which is the common case after a brief blip.
    pub fragments: usize,
    /// What happened, in a sentence.
    pub outcome: String,
}

/// What one copy of a range said when asked what it holds.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CopyDigest {
    pub node: String,
    /// `None` when this copy could not be asked, which is not the same as disagreeing.
    pub digest: Option<u64>,
    /// Why it could not be asked.
    pub why: Option<String>,
}

/// Whether every copy of one range holds the same facts.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct RangeVerdict {
    pub shards: String,
    pub primary: String,
    /// The primary first, then its replicas.
    pub copies: Vec<CopyDigest>,
    /// Every copy answered, and every answer was the same. A copy that did not answer makes
    /// this false: unreachable is not agreement.
    pub agree: bool,
}

/// A node that cannot write down its vote may not vote.
///
/// On stderr and nowhere else, because this crate has no logger of its own and acquiring one
/// for a single line would be a dependency carried for a failure that should never happen.
/// `big-http` puts a structured line next to it when it notices the node has gone quiet.
pub(crate) fn log_persist_failure(e: &std::io::Error) {
    eprintln!(
        "big: could not write the agreement state ({e}); this node is not participating in \
         elections until it can, which looks to its peers exactly like a node that is down"
    );
}
