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
//! **What this does not give you**, spelled out because the absences are the design:
//!
//! - **No replication.** A node's disk is the only copy of its shards.
//! - **No rebalancing.** Changing a range means stopping a node, copying a file, editing the
//!   config. One process holds one file - the pager takes an exclusive lock - so a shard does
//!   not move without a copy.
//! - **No cross-node atomicity.** A batch spanning two owners is two commits, and a failure in
//!   the middle is reported as [`ClusterError::Partial`] rather than hidden.
//! - **No cluster-wide snapshot.** Each owner serves the fan-out from its own read transaction,
//!   taken when its part of the request arrived, so a distributed read can straddle two
//!   commits. Fixing that means a cluster-wide transaction id, which means the meta page flip
//!   stops being the only atomic point, which is a different engine.
//! - **No membership layer.** A failure detector's answer would have to change a routing
//!   decision, and with one owner per shard there is nothing to route to. That day arrives
//!   with replication and not before.

#![deny(unsafe_code)]

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

use big_api::{Api, KeyAssignment, PagerMut, Plan, QueryOptions, RecordId, RowId, Value};
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
pub const WIRE_VERSION: u32 = 4;

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
    /// The agreement, when there is anything to agree about.
    ///
    /// `None` when no range has a copy: a range of one cannot fail over to anything, so
    /// running an election to decide who serves it would be machinery deciding a question
    /// with one possible answer.
    controller: Option<Arc<Controller>>,
}

impl<P: PagerMut + Sync> Cluster<P> {
    /// A database that is not clustered: one node, every shard, nobody to disagree with.
    pub fn solo(api: Api<P>) -> Self {
        Self::new(api, ClusterConfig::solo("127.0.0.1:7654"), None, Box::new(raft::Forgetful))
    }

    /// A node in a configured cluster. `token` is the bearer token presented to peers.
    ///
    /// `store` is where this node's vote is kept, and it is a parameter rather than a default
    /// because there is no safe default: a node that forgets its vote can cast a second one in
    /// the same term, which is two leaders. [`raft::Forgetful`] is the honest name for the
    /// thing to pass when nothing can be taken away from this node anyway.
    pub fn new(
        api: Api<P>,
        config: ClusterConfig,
        token: Option<String>,
        store: Box<dyn raft::Store>,
    ) -> Self {
        Self::with_timing(api, config, token, store, raft::Timing::default(), Leases::default())
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
        token: Option<String>,
        store: Box<dyn raft::Store>,
        timing: raft::Timing,
        leases: Leases,
    ) -> Self {
        let peers = Arc::new(client::HttpPeers::new(
            config.nodes().iter().map(|n| n.addr.clone()),
            config.this_index(),
            token,
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
    ) -> Self {
        let controller = config
            .is_replicated()
            .then(|| Controller::start(&config, Arc::clone(&peers), store, timing, leases));
        Self {
            config,
            api,
            peers,
            controller,
            counters: counters::Counters::new(),
            allocated: Default::default(),
        }
    }

    /// What an operator can see about this node's place in the cluster.
    ///
    /// A snapshot rather than a handle: rendering it must never hold anything a request needs.
    pub fn counters(&self) -> counters::Snapshot {
        let (term, leader, behind) = match &self.controller {
            None => (0, false, 0),
            Some(c) => (c.term(), c.is_leader(), c.ownership().stale.len()),
        };
        counters::Snapshot {
            nodes: self.config.nodes().len(),
            peers: self.config.nodes().len() - 1,
            replicated: self.config.is_replicated(),
            serving: self.may_serve(),
            term,
            leader,
            behind,
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

/// What a repair managed for one copy.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct RepairReport {
    pub node: String,
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
