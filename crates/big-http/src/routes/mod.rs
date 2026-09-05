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

//! Thirteen routes, and the role each one needs.
//!
//! ```text
//! GET    /health                   liveness; never authenticated
//! GET    /ready                    readiness; never authenticated
//! GET    /metrics                  Prometheus text                       read
//! GET    /schema                                                         read
//! GET    /verify                   do the copies of every range agree     read
//! POST   /repair                   catch up every copy that is behind     admin
//! POST   /table/{t}/query          body: one PQL call                    read
//! POST   /sql                      one SELECT, or CREATE TABLE            read *
//! GET    /watch?sql=&interval=      a SELECT, re-answered when it changes  read *
//! POST   /table/{t}/import         body: one fact per line               write
//! POST   /table/{t}/delete         body: one record id per line          write
//! POST   /table/{t}?engine=bitmap|bitmap+columnar|columnar             admin
//! POST   /table/{t}/field/{f}?kind=...&bit_depth=N&scale=N               admin
//! DELETE /table/{t}                drop the table                        admin
//! POST   /database/{d}             create the database                   admin
//! DELETE /database/{d}?cascade=true                                      admin
//! DELETE /table/{t}/field/{f}      drop the field                        admin
//! POST   /admin/backup?name=<name> an online copy of this node's file    admin
//! ```
//!
//! **`/admin/backup` is the only route that is about the *file* rather than the data in it**,
//! which is why it needs a directory on the command line before it will do anything. It is
//! also the only one that answers for this node alone while looking like a client route: a
//! cluster is backed up one node at a time, and the copies are not one snapshot. See
//! `backup`.
//!
//! **`/sql` is the one addition, and it does not hang off a table.** Every other query route is
//! asked *of* an index and takes the table from the path; a `SELECT` names its own in `FROM`,
//! and putting it in both places would create a pair that can disagree. What it answers is a
//! result set - columns and rows - rather than the shape the other query route returns, because
//! that is what a SQL client is written to read.
//!
//! **The eleven kept their shapes.** `query`, `import`, `delete`, `records` and the four
//! schema routes gained a fan-out behind them, and a client cannot tell: a coordinator with no
//! peers is what a single node now is, so the request that worked yesterday takes the same
//! path as the one that reaches four machines.
//!
//! **`/watch` is the only route that answers with a stream**, and the only one that holds a
//! worker for as long as a client cares to stay. It is off unless `--watch-max` says otherwise,
//! and it is deliberately absent from `bigproxy`'s allowlist: that proxy is buffered
//! request/response with a per-route budget, and a connection that lives for hours has no
//! budget. A client subscribes to a node directly. See `watch`.
//!
//! \* `CREATE TABLE` over `/sql` needs `admin`. The role check runs before any body is decoded,
//! so a route's role is a *floor*: the statement raises it once it has been classified, which is
//! what stops a read-only token creating tables. See `query::sql`.
//!
//! Six more routes exist for the fan-out itself and are not part of that surface:
//!
//! ```text
//! POST   /internal/query           a plan, not query text                read
//! POST   /internal/records         a page of ids from this node's shards read
//! POST   /internal/import          facts, with their row ids already fixed  write
//! POST   /internal/delete          record ids this node owns             write
//! POST   /internal/intern          what do these keys mean (leader only) write
//! POST   /internal/allocate        a run of record ids (leader only)       write
//! POST   /internal/next-record     how far this node's ids reach           read
//! POST   /internal/ddl             one schema change, already ruled legal   admin
//! POST   /internal/digest          what do you hold, as one number        read
//! POST   /internal/raft            one message of the agreement           admin
//! POST   /internal/fragments       what do you hold for this table        read
//! POST   /internal/fragment        send me this one                       read
//! POST   /internal/fragment/put    take this one, whole                   admin
//! POST   /internal/keys            every row key of this table            read
//! POST   /internal/keys/put        take these row keys                    admin
//! POST   /internal/repaired        this copy has caught up                admin
//! POST   /internal/schema          the schema, as you hold it              read
//! ```
//!
//! They take a binary body and answer with one, they are reached only by another node, and
//! they carry the same roles their public counterparts do - a peer presents a token like
//! anything else, because a port that trusts whoever reaches it is a port that trusts
//! everybody.
//!
//! **Why `/health` and `/ready` are never authenticated.** A load balancer does not carry a
//! credential, and one that has to would be configured with a token in every environment that
//! has one - which is a token in more places than the data it guards. Neither route reveals
//! anything: liveness is a constant, and readiness is whether the engine answers at all.
//!
//! **Why `/metrics` needs only `read`.** Page counts, reader counts and I/O rates are far less
//! than the data itself - the storage counters say how many pages moved, never which - and a
//! scraper is not an administrator. Give a scraper its own read token; if a
//! deployment needs a credential that can scrape and nothing else, a fourth role is the change
//! to make, not an exception here.
//!
//! Deleting records is a `POST` to `/delete` rather than a `DELETE` on the table, because the
//! ids arrive in the body: a `DELETE` carrying a body is legal but widely mishandled by
//! proxies, and a URL long enough to name a batch is not. The two `DELETE` routes take no
//! body and drop schema, which is the case the verb actually fits.
//!
//! The query route takes the query as the whole body rather than as a parameter: a query can
//! contain any character, and percent-encoding it into a URL only to decode it here would add
//! a place to get escaping wrong.
//!
//! Routing is here; the work is one module along. The split is by *who is asking* rather than
//! by verb, because that is what decides the answer's shape: `ops` answers a probe and must
//! never touch data, `admin` and `query` answer a client through the cluster, and `peer`
//! answers another node against this node's database alone. A handler in the wrong one of those
//! is a bug that routing cannot catch, so they do not share a file.

mod admin;
mod backup;
mod ops;
mod peer;
mod query;
pub(crate) mod watch;

// Glob rather than a list, because the list would be the route table written a fourth time -
// after the doc comment above, `Target`, and `dispatch` - and the one that drifts is the one
// nothing checks. A handler that is not routed is an unused-function warning, which is the
// check that list was pretending to be.
use admin::*;
use backup::*;
use ops::*;
use peer::*;
use query::*;

use crate::auth::{Auth, Credential, Identity, Outcome, Principal};
use crate::metrics::ServerMetrics;
use crate::{json, Request, Response};
use big_cluster::wire::{self, OwnedFact};
use big_cluster::{Cluster, ClusterError};
use big_db::catalog::FieldKind;
use big_embed::{Ack, Api, Fact, FieldInfo, QueryOptions};
use big_pager::PagerMut;
use big_rbac::{Demand, ObjectRef, Privilege};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

/// Everything a handler needs that is not the request.
pub struct Ctx<'a, P: PagerMut> {
    /// The database, and the other nodes holding the rest of it.
    ///
    /// A [`Cluster`] rather than an [`Api`] even when there is one node, because a second path
    /// through here for the un-clustered case would be the path nobody tests.
    pub cluster: &'a Cluster<P>,
    /// Who may do what.
    pub auth: &'a Auth,
    /// What the *connection* proved, before a single header was read.
    ///
    /// `Identity::None` on a plaintext connection and on a server-only TLS one, which between
    /// them are every client connection. Only a peer presenting a client certificate this node's
    /// CA signed arrives as anything else.
    pub identity: &'a Identity,
    /// The server's own counters, because `/metrics` is a route like any other and rendering
    /// them is what it does. Borrowed rather than reached for through a global: a second
    /// server in one process - which every test that binds two ports is - must not share them.
    pub metrics: &'a ServerMetrics,
    /// Wall-clock budget for a query. `None` lets it run to completion.
    pub query_timeout: Option<Duration>,
    /// Set by the connection's watchdog when the client hangs up. Only queries carry one.
    pub cancel: Option<Arc<AtomicBool>>,
    /// Where `POST /admin/backup` may write, or `None` when the daemon was started without
    /// somewhere to put one. A directory rather than a path per request, because a request
    /// that chose its own path could write anywhere this process can.
    pub backup_dir: Option<&'a str>,
    /// How many `GET /watch` subscriptions this server will hold at once. `0` is the default
    /// and turns the route off: every subscriber holds a worker for the life of its connection,
    /// so this is the one route that can take the pool away from everything else.
    pub watch_max: usize,
    /// How many it is holding. Shared with the server for the reason `backup_running` is: it
    /// has to outlive the request that took a slot.
    pub watching: &'a std::sync::atomic::AtomicUsize,
    /// Whether a backup is already walking this node's file. Shared with the server rather
    /// than owned here, because a `Ctx` lives for one request and the flag has to outlive it.
    pub backup_running: &'a AtomicBool,
    /// When the cluster may reshape itself, and how hard. Off by default: a cluster that
    /// changes its own shape unasked is a cluster whose shape an operator cannot predict.
    pub balance: big_cluster::balance::Policy,
}

/// What a request has to be to reach a route.
///
/// Three kinds rather than an `Option<Role>`, because a peer is not a very privileged person -
/// it is a different kind of caller. See `Target::guard` for why that distinction is the one
/// that shrinks the blast radius of a leaked node key.
///
/// **Borrows the route's captured names**, because a privilege here is about an object and the
/// object is in the path: `DELETE /table/sales.orders` needs `DROP` on `sales.orders` and not on
/// anything else. The static table this used to be could only name a role.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Guard<'a> {
    /// Never authenticated: `/health` and `/ready`, which have to answer while the database is
    /// unhealthy - exactly when a credential check might not.
    Open,
    /// A credential that resolves, and nothing more.
    ///
    /// For the two routes where the privilege is not the route's to know: `POST /sql`, whose
    /// statement says what it needs, and `GET /schema`, which answers with names.
    Authenticated,
    /// A person holding one privilege on one object.
    Needs(Privilege, ObjectRef<'a>),
    /// Another node of this cluster, proven by its client certificate during the handshake.
    Node,
}

impl<P: PagerMut + Sync> Ctx<'_, P> {
    /// This node's own database. What the routes that are about *this process* use - metrics,
    /// readiness - and what the `/internal/` handlers use, because a request that arrived
    /// there has already been routed.
    fn api(&self) -> &Api<P> {
        self.cluster.local()
    }
}

/// One route, already matched, with the names it captured.
///
/// A resolved enum rather than a match arm per handler, because the role a route needs and the
/// work it does are two different questions and both have to be answered before either runs.
/// Folding them together is how a route ends up doing its work and then checking whether it
/// was allowed to.
enum Target<'a> {
    Health,
    Ready,
    Metrics,
    Schema,
    Verify,
    Query(&'a str),
    Sql,
    Watch,
    Records(&'a str),
    Import(&'a str),
    DeleteRecords(&'a str),
    CreateTable(&'a str),
    CreateField(&'a str, &'a str),
    DropTable(&'a str),
    DropField(&'a str, &'a str),
    CreateDatabase(&'a str),
    DropDatabase(&'a str),
    PeerQuery,
    PeerRecords,
    PeerImport,
    PeerDelete,
    PeerIntern,
    PeerAllocate,
    PeerSchemaStepDown,
    PeerReserve,
    PeerNextRecord,
    PeerDdl,
    PeerDigest,
    PeerRaft,
    PeerFragments,
    PeerFragment,
    PeerFragmentPut,
    PeerKeys,
    PeerKeysPut,
    PeerRepaired,
    PeerSchema,
    /// Which version of the map this node has applied.
    PeerEpoch,
    /// What this node weighs, for the balancer.
    PeerLoad,
    /// The record ids this node has handed out, read and written when the row-key namespace
    /// changes hands.
    PeerFloors,
    PeerFloorsPut,
    Repair,
    Backup,
    /// What the cluster looks like right now, for an operator or an autoscaler.
    ClusterTopology,
    /// Cut a range in two, and optionally hand the upper half to another node.
    ClusterSplit,
    /// Join a range to the one after it.
    ClusterMerge,
    /// Add a node, promote it, start taking one out, or take it out for good.
    ClusterAddNode,
    ClusterAdmit,
    ClusterDrain,
    ClusterRemove,
    /// Hand a populated range to another node without stopping reads.
    ClusterMove,
    /// Abandon a move that is in flight.
    ClusterCancel,
    /// Take one balancing step, if the facts call for one.
    ClusterRebalance,
    /// Hand the row-key namespace to another node.
    ClusterSchemaLeader,
}

impl<'a> Target<'a> {
    /// Whether this route is one node talking to another.
    ///
    /// The two stamps below are checked for exactly these, because they are the only requests
    /// where the sender is supposed to be running the same build and reading the same file. A
    /// client has no business sending either and is not asked for them.
    fn is_internal(&self) -> bool {
        matches!(
            self,
            Self::PeerQuery
                | Self::PeerEpoch
                | Self::PeerLoad
                | Self::PeerFloors
                | Self::PeerFloorsPut
                | Self::PeerRecords
                | Self::PeerDigest
                | Self::PeerImport
                | Self::PeerDelete
                | Self::PeerIntern
                | Self::PeerAllocate
                | Self::PeerSchemaStepDown
                | Self::PeerReserve
                | Self::PeerNextRecord
                | Self::PeerDdl
                | Self::PeerRaft
                | Self::PeerSchema
                | Self::PeerFragments
                | Self::PeerFragment
                | Self::PeerFragmentPut
                | Self::PeerKeys
                | Self::PeerKeysPut
                | Self::PeerRepaired
        )
    }

    /// What a request has to *be* to reach this route.
    ///
    /// **Not a role for the peer routes, and that is the change worth reading twice.** A role is
    /// a statement about a person's authority over data; a node certificate is a statement about
    /// which process is speaking. Mapping the certificate onto `Role::Admin` would make a leaked
    /// node key an admin credential on the *public* routes - `DELETE /table/x` would work with
    /// it. Going the other way, a username and password, however privileged, must not reach
    /// `/internal/*`: those routes take binary bodies a coordinator produced and assume the
    /// sender passed `mismatched()`. Under tokens an `admin` credential could post to them;
    /// this closes that, and it closes it for free.
    ///
    /// The read/write/admin distinctions that used to separate the peer routes from each other
    /// are gone with it. They only ever meant something while a peer presented the same *kind*
    /// of credential a person did.
    fn guard(&self) -> Guard<'a> {
        // A table segment may be `sales.orders` or a bare `orders`; the bare form means the
        // request's database, which `refuse` fills in because only it can see `?database=`.
        let table = |t: &'a str| match t.split_once('.') {
            Some((database, table)) => ObjectRef::Table { database, table },
            None => ObjectRef::Table { database: "", table: t },
        };
        // What holds a table is what a `CREATE` is granted over: there is no table yet for the
        // privilege to hang on, so it hangs on the database that will hold it.
        let holder = |t: &'a str| match t.split_once('.') {
            Some((database, _)) => ObjectRef::Database(database),
            None => ObjectRef::Database(""),
        };
        match self {
            Self::Health | Self::Ready => Guard::Open,
            // The statement says what it needs - see `Sql::demands` - and it cannot be known
            // before the body is read. This is the floor, and the floor is "somebody".
            Self::Sql => Guard::Authenticated,
            // The floor is the same as `/sql`'s and for the same reason: the statement says
            // what it needs, and it is re-asked on every push rather than only at subscribe.
            Self::Watch => Guard::Authenticated,
            // A listing of names, which is what every JDBC driver opens with. Filtering it down
            // to what the reader may query is a feature this surface does not have yet; when it
            // does, it belongs beside `Sql::demands` rather than here.
            Self::Schema => Guard::Authenticated,
            // About the process rather than about data, so it is one server-wide privilege
            // rather than a role that also happened to read tables.
            Self::Metrics | Self::Verify | Self::Repair | Self::Backup => {
                Guard::Needs(Privilege::Operate, ObjectRef::Server)
            }
            // Reshaping the cluster is the same privilege as repairing it: about the process
            // and its peers, not about anybody's rows.
            Self::ClusterTopology
            | Self::ClusterSplit
            | Self::ClusterMerge
            | Self::ClusterAddNode
            | Self::ClusterAdmit
            | Self::ClusterDrain
            | Self::ClusterRemove
            | Self::ClusterMove
            | Self::ClusterCancel
            | Self::ClusterRebalance
            | Self::ClusterSchemaLeader => Guard::Needs(Privilege::Operate, ObjectRef::Server),
            Self::Query(t) | Self::Records(t) => Guard::Needs(Privilege::Select, table(t)),
            Self::Import(t) => Guard::Needs(Privilege::Insert, table(t)),
            // Deleting records is not inserting them: a credential that may add facts is not
            // obviously one that may remove them, and the REST surface is where the two come
            // apart, because SQL has no `DELETE`.
            Self::DeleteRecords(t) => Guard::Needs(Privilege::Delete, table(t)),
            Self::CreateTable(t) => Guard::Needs(Privilege::Create, holder(t)),
            Self::CreateField(t, _) => Guard::Needs(Privilege::Alter, table(t)),
            Self::DropTable(t) => Guard::Needs(Privilege::Drop, table(t)),
            Self::DropField(t, _) => Guard::Needs(Privilege::Alter, table(t)),
            // A database is not in a database, so both are about the server - the same answer
            // `Sql::demands` gives `CREATE DATABASE`, and for the same reason.
            Self::CreateDatabase(_) => Guard::Needs(Privilege::Create, ObjectRef::Server),
            Self::DropDatabase(_) => Guard::Needs(Privilege::Drop, ObjectRef::Server),

            // Every one of these is a peer, and being a peer is the whole requirement. What
            // proves it is a client certificate this node's peer CA signed, checked during the
            // handshake and before a byte of HTTP was read - so a request that reaches here
            // with a username and password, however privileged, is refused.
            Self::PeerQuery
            | Self::PeerRecords
            | Self::PeerDigest
            | Self::PeerFragments
            | Self::PeerFragment
            | Self::PeerKeys
            | Self::PeerSchema
            | Self::PeerNextRecord
            | Self::PeerImport
            | Self::PeerDelete
            | Self::PeerIntern
            | Self::PeerAllocate
            | Self::PeerSchemaStepDown
            | Self::PeerReserve
            | Self::PeerDdl
            | Self::PeerRaft
            | Self::PeerFragmentPut
            | Self::PeerKeysPut
            | Self::PeerEpoch
            | Self::PeerLoad
            | Self::PeerFloors
            | Self::PeerFloorsPut
            | Self::PeerRepaired => Guard::Node,
        }
    }
}

fn resolve<'a>(method: &str, segments: &[&'a str]) -> Option<Target<'a>> {
    Some(match (method, segments) {
        ("GET", ["health"]) => Target::Health,
        ("GET", ["ready"]) => Target::Ready,
        ("GET", ["metrics"]) => Target::Metrics,
        ("GET", ["schema"]) => Target::Schema,
        ("GET", ["verify"]) => Target::Verify,
        ("GET", ["table", t, "records"]) => Target::Records(t),
        ("POST", ["table", t, "query"]) => Target::Query(t),
        ("POST", ["sql"]) => Target::Sql,
        ("GET", ["watch"]) => Target::Watch,
        ("POST", ["table", t, "import"]) => Target::Import(t),
        ("POST", ["table", t, "delete"]) => Target::DeleteRecords(t),
        ("POST", ["table", t]) => Target::CreateTable(t),
        ("POST", ["table", t, "field", f]) => Target::CreateField(t, f),
        ("DELETE", ["table", t]) => Target::DropTable(t),
        ("POST", ["database", d]) => Target::CreateDatabase(d),
        ("DELETE", ["database", d]) => Target::DropDatabase(d),
        ("DELETE", ["table", t, "field", f]) => Target::DropField(t, f),
        ("POST", ["internal", "query"]) => Target::PeerQuery,
        ("POST", ["internal", "records"]) => Target::PeerRecords,
        ("POST", ["internal", "import"]) => Target::PeerImport,
        ("POST", ["internal", "delete"]) => Target::PeerDelete,
        ("POST", ["internal", "intern"]) => Target::PeerIntern,
        ("POST", ["internal", "allocate"]) => Target::PeerAllocate,
        ("POST", ["internal", "schema", "step-down"]) => Target::PeerSchemaStepDown,
        ("POST", ["internal", "reserve"]) => Target::PeerReserve,
        ("POST", ["internal", "next-record"]) => Target::PeerNextRecord,
        ("POST", ["internal", "ddl"]) => Target::PeerDdl,
        ("POST", ["internal", "digest"]) => Target::PeerDigest,
        ("POST", ["internal", "raft"]) => Target::PeerRaft,
        ("POST", ["internal", "fragments"]) => Target::PeerFragments,
        ("POST", ["internal", "fragment"]) => Target::PeerFragment,
        ("POST", ["internal", "fragment", "put"]) => Target::PeerFragmentPut,
        ("POST", ["internal", "keys"]) => Target::PeerKeys,
        ("POST", ["internal", "keys", "put"]) => Target::PeerKeysPut,
        ("POST", ["internal", "repaired"]) => Target::PeerRepaired,
        ("POST", ["internal", "schema"]) => Target::PeerSchema,
        ("POST", ["internal", "epoch"]) => Target::PeerEpoch,
        ("POST", ["internal", "load"]) => Target::PeerLoad,
        ("POST", ["internal", "floors"]) => Target::PeerFloors,
        ("POST", ["internal", "floors", "put"]) => Target::PeerFloorsPut,
        ("POST", ["repair"]) => Target::Repair,
        ("GET", ["cluster", "topology"]) => Target::ClusterTopology,
        ("POST", ["admin", "cluster", "split"]) => Target::ClusterSplit,
        ("POST", ["admin", "cluster", "merge"]) => Target::ClusterMerge,
        ("POST", ["admin", "cluster", "node"]) => Target::ClusterAddNode,
        ("POST", ["admin", "cluster", "admit"]) => Target::ClusterAdmit,
        ("POST", ["admin", "cluster", "drain"]) => Target::ClusterDrain,
        ("DELETE", ["admin", "cluster", "node"]) => Target::ClusterRemove,
        ("POST", ["admin", "cluster", "move"]) => Target::ClusterMove,
        ("POST", ["admin", "cluster", "cancel"]) => Target::ClusterCancel,
        ("POST", ["admin", "cluster", "rebalance"]) => Target::ClusterRebalance,
        ("POST", ["admin", "cluster", "schema-leader"]) => Target::ClusterSchemaLeader,
        ("POST", ["admin", "backup"]) => Target::Backup,
        _ => return None,
    })
}

/// Whether this request is one that could run long enough to be worth watching for a hang-up.
///
/// A query, and a peer's query. Everything else is bounded by the body the client already
/// sent, and spawning a watchdog thread for work that finishes in microseconds costs more than
/// the work. The peer case matters more than it looks: a coordinator that gives up drops the
/// connection, and this is what turns that into the owner stopping work rather than finishing
/// an answer nobody is left to read.
pub fn may_run_long(req: &Request) -> bool {
    // Borrowed back out of the decoded segments. `resolve` matches on `&str` patterns and has
    // no business knowing whether the text behind them was escaped on the wire.
    let segments = req.segments();
    let borrowed: Vec<&str> = segments.iter().map(std::convert::AsRef::as_ref).collect();
    matches!(
        resolve(req.method.as_str(), &borrowed),
        Some(Target::Query(_) | Target::Sql | Target::PeerQuery)
    )
}

/// A response, and who this server decided asked for it.
///
/// The second half is new. This server has never logged *who* made a request - the resolved role
/// was computed and thrown away - which was tolerable when a credential was an anonymous string
/// and is not once it belongs to a person. An audit line that says a table was dropped, without
/// saying by whom, is half a line.
pub struct Answered {
    /// What to send back.
    pub response: Response,
    /// Set when the answer is a stream rather than a body: the head to write, and what to keep
    /// pushing down it. Owned data rather than a closure, so nothing here grows a lifetime and
    /// the work itself stays a plain function the server calls with the socket in hand.
    pub stream: Option<Watch>,
    /// What to put in the log: the field name - `"user"` or `"node"` - and the name itself.
    ///
    /// The kind is carried rather than guessed from the name. An earlier draft inferred it from
    /// a `node-` prefix, which is a convention nothing enforces and which would have mislabelled
    /// a person unlucky enough to be called `node-ops`.
    pub who: Option<(&'static str, String)>,
}

/// A registered live query: what to re-run, how often to look, and as whom.
///
/// **As whom matters on every push, not just at subscribe.** A `REVOKE` while a stream is open
/// has to cut it off, so the identity is carried here and the grant is checked again each time
/// round rather than once at the top.
pub struct Watch {
    /// The `SELECT` to re-run.
    pub sql: String,
    /// Which database an unqualified name in it means.
    pub database: Option<String>,
    /// The longest to go without looking. On a node with peers this is the whole trigger; on a
    /// node writing alone a commit wakes it sooner.
    pub interval: std::time::Duration,
    /// The identity to check the grant against, every push.
    pub who: big_rbac::Who,
    /// The head to write before the first chunk.
    pub head: big_wire::Streaming,
}

impl From<Response> for Answered {
    fn from(response: Response) -> Self {
        Self { response, who: None, stream: None }
    }
}

/// Routes one request and runs it, or answers `404`, `401` or `403` without running anything.
pub fn dispatch<P: PagerMut + Sync>(ctx: &Ctx<'_, P>, req: &Request) -> Answered {
    let segments = req.segments();
    let borrowed: Vec<&str> = segments.iter().map(std::convert::AsRef::as_ref).collect();
    let Some(target) = resolve(req.method.as_str(), &borrowed) else {
        return Response::failure(404, "no_such_route", "no such route").into();
    };

    let principal = match refuse(ctx, req, target.guard()) {
        Ok(p) => p,
        Err(refusal) => return refusal.into(),
    };
    // Recorded before the handler runs, so that a request which panics still says who made it.
    let who = match &principal {
        Principal::Anonymous => None,
        p @ Principal::Node { .. } => Some(("node", p.display().to_string())),
        p => Some(("user", p.display().to_string())),
    };

    // **Before any body is decoded.** A peer running a different build sends bytes this one
    // would read as something else - a length where a tag was - and the result is not a
    // refusal, it is an answer that is quietly wrong. A peer reading a different cluster file
    // is the same failure one level up: it believes a range belongs somewhere it does not.
    // Both are cheap to check and impossible to notice afterwards.
    if target.is_internal() {
        if let Some(refusal) = mismatched(ctx, req) {
            return Answered { response: refusal, who, stream: None };
        }
    }

    // A subscription is not a body, so it leaves before the arm below that builds one. It is
    // also the one route whose answer is decided here and *produced* by the server, with the
    // socket in hand - see `watch::run`.
    if matches!(target, Target::Watch) {
        return match watch::subscribe(ctx, req, &principal) {
            Ok((response, stream)) => Answered { response, who, stream: Some(stream) },
            Err(refusal) => Answered { response: *refusal, who, stream: None },
        };
    }

    let response = match target {
        Target::Health => health(),
        Target::Ready => ready(ctx),
        Target::Metrics => {
            let mut text = ctx.metrics.render(&ctx.api().metrics());
            crate::metrics::render_keys(&mut text, &ctx.api().key_stats());
            crate::metrics::render_group(&mut text, &ctx.api().group_stats());
            crate::metrics::render_cluster(&mut text, &ctx.cluster.counters(), ctx.balance.enabled);
            let (verifications, hits, throttled) = ctx.auth.counters();
            crate::metrics::render_auth(&mut text, verifications, hits, throttled);
            Response::text(
                // The version suffix is part of the contract: a scraper reads it to decide how
                // to parse, and omitting it makes some of them guess.
                "text/plain; version=0.0.4; charset=utf-8",
                text,
            )
        }
        Target::Schema => Response::ok(json::schema(&ctx.cluster.schema())),
        Target::Verify => Response::ok(json::verify(&ctx.cluster.verify())),
        Target::Query(t) => query(ctx, req, t),
        Target::Sql => sql(ctx, req, &principal),
        // Handled above: it answers with a stream rather than a body.
        Target::Watch => unreachable!("a subscription leaves before this match"),
        Target::Records(t) => records(ctx, req, t),
        Target::Import(t) => import(ctx, req, t),
        Target::DeleteRecords(t) => delete(ctx, req, t),
        Target::CreateTable(t) => create_table(ctx, req, t),
        Target::CreateField(t, f) => create_field(ctx, req, t, f),
        Target::DropTable(t) => drop_table(ctx, t),
        Target::CreateDatabase(d) => create_database(ctx, d),
        Target::DropDatabase(d) => drop_database(ctx, req, d),
        Target::DropField(t, f) => drop_field(ctx, t, f),
        Target::PeerQuery => peer_query(ctx, req),
        Target::PeerRecords => peer_records(ctx, req),
        Target::PeerImport => peer_import(ctx, req),
        Target::PeerDelete => peer_delete(ctx, req),
        Target::PeerIntern => peer_intern(ctx, req),
        Target::PeerAllocate => peer_allocate(ctx, req),
        Target::PeerSchemaStepDown => peer_schema_step_down(ctx, req),
        Target::PeerReserve => peer_reserve(ctx, req),
        Target::PeerNextRecord => peer_next_record(ctx, req),
        Target::PeerDdl => peer_ddl(ctx, req),
        Target::PeerDigest => peer_digest(ctx, req),
        Target::PeerRaft => peer_raft(ctx, req),
        Target::PeerFragments => peer_fragments(ctx, req),
        Target::PeerFragment => peer_fragment(ctx, req),
        Target::PeerFragmentPut => peer_fragment_put(ctx, req),
        Target::PeerKeys => peer_keys(ctx, req),
        Target::PeerKeysPut => peer_keys_put(ctx, req),
        Target::PeerRepaired => peer_repaired(ctx, req),
        Target::PeerEpoch => Response::binary(wire::put_u64_body(ctx.cluster.map().epoch)),
        Target::PeerFloors => Response::binary(wire::put_floors(&ctx.cluster.floors_here())),
        Target::PeerFloorsPut => match wire::get_floors(&req.body) {
            Err(e) => unreadable(&e),
            Ok(floors) => {
                ctx.cluster.raise_floors(&floors);
                Response::binary(wire::put_u64_body(floors.len() as u64))
            }
        },
        Target::PeerLoad => {
            let load = ctx.cluster.load();
            Response::binary(wire::put_load(load.pages.unwrap_or(0), load.frontier))
        }
        Target::PeerSchema => {
            let mut out = Vec::new();
            wire::put_schema(&mut out, &ctx.cluster.schema(), &ctx.cluster.views());
            Response::binary(out)
        }
        Target::Repair => repair(ctx),
        Target::ClusterTopology => cluster_topology(ctx),
        Target::ClusterSplit => cluster_split(ctx, req),
        Target::ClusterMerge => cluster_merge(ctx, req),
        Target::ClusterAddNode => cluster_add_node(ctx, req),
        Target::ClusterAdmit => cluster_member(ctx, req, Membership::Admit),
        Target::ClusterDrain => cluster_member(ctx, req, Membership::Drain),
        Target::ClusterRemove => cluster_member(ctx, req, Membership::Remove),
        Target::ClusterMove => cluster_move(ctx, req),
        Target::ClusterCancel => cluster_cancel(ctx, req),
        Target::ClusterRebalance => cluster_rebalance(ctx, req),
        Target::ClusterSchemaLeader => cluster_schema_leader(ctx, req),
        Target::Backup => backup(ctx, req),
    };
    Answered { response, who, stream: None }
}

/// Who this request is, or the response explaining why it is nobody.
///
/// Returns the principal rather than a yes, which is what lets the per-statement check above be
/// free. The old signature returned `Option<Response>` and threw the resolved role away.
/// A `Response` is a couple of hundred bytes, which clippy would rather see boxed. Not here: the
/// `Err` arm is the refused path, it is taken once per refused request and never in a loop, and
/// boxing it would add an allocation to the path that is already answering "no" - to save moving
/// bytes that are about to be written to a socket anyway.
#[allow(clippy::result_large_err)]
fn refuse<'a, P: PagerMut + Sync>(
    ctx: &Ctx<'_, P>,
    req: &'a Request,
    guard: Guard<'a>,
) -> Result<Principal, Response> {
    // Never authenticated, and never even looked at: the probes have to answer while the
    // database is unhealthy, which is exactly when a credential check might not.
    if guard == Guard::Open {
        return Ok(Principal::Anonymous);
    }

    let presented = req.basic();
    let credential = presented.as_ref().map(|b| big_http_credential(b));
    // **Once.** Whatever the guard turns out to want, the argon2 verification happens here and
    // nowhere else - a second one would cost fifty milliseconds on a path that runs per request.
    let principal = match ctx.auth.authorize(ctx.identity, credential) {
        Outcome::Allowed(p) => p,
        Outcome::Unauthenticated => {
            return Err(Response::failure(
                401,
                "unauthenticated",
                "a username and password are required",
            )
            // Without this header a client cannot tell a 401 it can fix from one it cannot, and
            // every HTTP client library looks for it. `charset` is RFC 7617 and tells a client
            // to encode the credential as UTF-8 rather than latin-1.
            .with_header("WWW-Authenticate", "Basic realm=\"big\", charset=\"UTF-8\""));
        }
        // Not a 401. A 401 tells a client to stop and fix its credentials; this one was never
        // looked at, and retrying is exactly the right thing to do.
        Outcome::Overloaded => {
            return Err(Response::failure(
                503,
                "busy_authenticating",
                "too many passwords are being checked at once; retry shortly",
            )
            .with_header("Retry-After", 1))
        }
    };

    // **The peer gate, and it goes both ways.** A person cannot reach `/internal/*` however
    // privileged they are, and a node cannot reach a public route however good its certificate
    // is. When authentication is switched off entirely, `Guard::Node` is satisfied by anything -
    // "auth off means allow all" is an existing contract and this extends it rather than
    // carving an exception into it.
    match guard {
        Guard::Node if ctx.auth.is_enabled() && !principal.is_node() => Err(Response::failure(
            403,
            "not_a_peer",
            "this route is reachable only by another node of this cluster, which proves itself \
             with a client certificate rather than with a password",
        )),
        Guard::Needs(..) | Guard::Authenticated if principal.is_node() => Err(Response::failure(
            403,
            "not_a_user",
            "this route is reachable only by a person; a node certificate grants no role",
        )),
        // The privilege the route itself demands. A statement's own demands are checked further
        // in, by `Cluster::run`, against the same resolver - this is the half that can be
        // decided from the path alone, and it is the only half the REST routes have.
        Guard::Needs(privilege, on) => {
            // A bare table name means the request's database, which only this layer can see.
            let asked = req.param("database");
            let database = asked.as_deref().unwrap_or(big_db::DEFAULT_DATABASE_NAME);
            let on = match on {
                ObjectRef::Table { database: "", table } => ObjectRef::Table { database, table },
                ObjectRef::Database("") => ObjectRef::Database(database),
                other => other,
            };
            if ctx.api().allows(&principal.who(), Demand::new(privilege, on)) {
                Ok(principal)
            } else {
                Err(forbidden(&principal, privilege, on))
            }
        }
        _ => Ok(principal),
    }
}

/// The `403` a principal that does not hold what was needed gets.
///
/// Names the user as well as the privilege and the object. That is deliberate: a `403` is only
/// ever seen by somebody who has already authenticated, so there is nothing here they did not
/// already know - and an operator with two credentials in their shell history needs to be told
/// which one they just used.
fn forbidden(principal: &Principal, privilege: Privilege, on: ObjectRef<'_>) -> Response {
    let where_ = match on {
        ObjectRef::Server => "the server".to_string(),
        ObjectRef::Database(d) => format!("`{d}`"),
        ObjectRef::Table { database, table } => format!("`{database}.{table}`"),
    };
    Response::failure(
        403,
        "forbidden",
        &format!("user `{}` does not hold {} on {where_}", principal.display(), privilege.as_str()),
    )
}

/// Borrows a parsed header as the credential `Auth` wants.
fn big_http_credential(b: &crate::request::Basic) -> Credential<'_> {
    Credential { user: &b.user, password: &b.password }
}

/// `None` when this peer is running the same build and reading the same cluster file.
///
/// `409`, because the request is well formed and retrying will not help: something has to
/// change on one of the two machines. The message says what this node expected, because the
/// operator reading it is looking at two of them and needs to know which one to correct.
fn mismatched<P: PagerMut + Sync>(ctx: &Ctx<'_, P>, req: &Request) -> Option<Response> {
    let speaks = req.header(big_cluster::WIRE_HEADER).and_then(|v| v.parse::<u32>().ok());
    if speaks != Some(big_cluster::WIRE_VERSION) {
        let said = speaks.map_or("nothing".to_string(), |v| v.to_string());
        return Some(Response::failure(
            409,
            "wire_version",
            &format!(
                "this node speaks wire version {}; the peer said {said}. Two builds that \
                 disagree about the encoding would read each other's messages as something \
                 else, so nothing is decoded until they agree",
                big_cluster::WIRE_VERSION
            ),
        ));
    }

    let ours = ctx.cluster.config().fingerprint();
    let theirs =
        req.header(big_cluster::CLUSTER_HEADER).and_then(|v| u64::from_str_radix(v, 16).ok());
    if theirs != Some(ours) {
        let said = theirs.map_or("nothing".to_string(), |v| format!("{v:x}"));
        return Some(Response::failure(
            409,
            "cluster_mismatch",
            &format!(
                "this node read a cluster file with fingerprint {ours:x}; the peer said {said}. \
                 Two nodes reading files that disagree about who owns what would each answer \
                 part of every query and neither would say so"
            ),
        ));
    }
    None
}

/// One failure of the cluster, turned into one response.
///
/// A local engine error is classified exactly as it always was - a coordinator that is also an
/// owner is still an owner - and everything else carries the status and the stable code the
/// failure itself decided. The message is not redacted: an owner being unreachable names the
/// node and the shard range on purpose, because "which part of the space went quiet" is the
/// first thing an operator needs and the one thing they cannot work out from a `503`.
fn from_cluster(e: &ClusterError) -> Response {
    match e {
        ClusterError::Local(api) => crate::status::response_for(api),
        other => Response::failure(other.status(), other.code(), &other.to_string()),
    }
}

/// The refusal a node owes when it can no longer prove it serves its own range.
fn not_serving<P: PagerMut + Sync>(ctx: &Ctx<'_, P>) -> Option<Response> {
    ctx.cluster.guard().err().map(|e| from_cluster(&e))
}

/// A peer sent bytes this build cannot read.
///
/// `400`, because it is the request that is wrong, and a stable code so that a coordinator
/// reporting it upward can say which half of the exchange failed. In practice this means two
/// nodes are running different builds, which is the one thing a wire format is allowed to be
/// blunt about.
fn unreadable(e: &wire::WireError) -> Response {
    Response::failure(400, "unreadable_message", &format!("this message is not usable: {e}"))
}

pub(super) fn parse_kind(s: &str) -> Option<FieldKind> {
    Some(match s {
        "int" => FieldKind::Int,
        "signed" => FieldKind::SignedInt,
        "decimal" => FieldKind::Decimal,
        "set" => FieldKind::Set,
        "mutex" => FieldKind::Mutex,
        "bool" => FieldKind::Bool,
        "timequantum" => FieldKind::TimeQuantum,
        "float32" => FieldKind::Float32,
        "float64" => FieldKind::Float64,
        "date" => FieldKind::Date,
        "datetime" => FieldKind::DateTime,
        _ => return None,
    })
}

/// How many planes a kind gets when the field route is not told.
///
/// **Not a constant 32.** A `float64` is the width its name says, and defaulting it to 32 would
/// have made a field that silently rounded every value it was given - the same field spelled
/// `FLOAT64` in a column list, and quietly a different one. A kind whose name carries a width
/// answers with that width; everything else keeps the 32 it always had.
pub(super) fn default_bit_depth(kind: FieldKind) -> u32 {
    match kind {
        FieldKind::Float64 | FieldKind::DateTime => 64,
        _ => 32,
    }
}

/// `?after=<id>&limit=<n>`, both optional.
///
/// Absent means what it always meant: start at the beginning, return everything.
///
/// The error is the message rather than a built `Response`, so the refusal is constructed at
/// the call site. A `Result` whose error is a whole response makes every caller carry one in
/// its return value for a case that is a bad query string.
pub(super) fn page(req: &Request) -> Result<json::Page, String> {
    let num = |name: &str| -> Result<Option<u64>, String> {
        match req.param(name) {
            None => Ok(None),
            Some(v) => {
                v.parse::<u64>().map(Some).map_err(|_| format!("{name} takes a number, got `{v}`"))
            }
        }
    };
    Ok(json::Page { after: num("after")?, limit: num("limit")?.map(|n| n as usize) })
}

/// How many records `GET /table/{t}/records` returns when the caller does not say.
pub(super) const DEFAULT_PAGE: usize = 1_000;
