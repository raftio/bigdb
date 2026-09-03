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

// Glob rather than a list, because the list would be the route table written a fourth time -
// after the doc comment above, `Target`, and `dispatch` - and the one that drifts is the one
// nothing checks. A handler that is not routed is an unused-function warning, which is the
// check that list was pretending to be.
use admin::*;
use backup::*;
use ops::*;
use peer::*;
use query::*;

use crate::auth::{Auth, Outcome, Role};
use crate::metrics::ServerMetrics;
use crate::{json, Request, Response};
use big_cluster::wire::{self, OwnedFact};
use big_cluster::{Cluster, ClusterError};
use big_db::catalog::FieldKind;
use big_embed::{Api, Fact, FieldInfo, QueryOptions};
use big_pager::PagerMut;
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
    /// Whether a backup is already walking this node's file. Shared with the server rather
    /// than owned here, because a `Ctx` lives for one request and the flag has to outlive it.
    pub backup_running: &'a AtomicBool,
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
    Repair,
    Backup,
}

impl Target<'_> {
    /// Whether this route is one node talking to another.
    ///
    /// The two stamps below are checked for exactly these, because they are the only requests
    /// where the sender is supposed to be running the same build and reading the same file. A
    /// client has no business sending either and is not asked for them.
    fn is_internal(&self) -> bool {
        matches!(
            self,
            Self::PeerQuery
                | Self::PeerRecords
                | Self::PeerDigest
                | Self::PeerImport
                | Self::PeerDelete
                | Self::PeerIntern
                | Self::PeerAllocate
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

    /// The least role that may reach this route. `None` means no credential is required.
    fn role(&self) -> Option<Role> {
        match self {
            Self::Health | Self::Ready => None,
            Self::Metrics
            | Self::Schema
            | Self::Query(_)
            | Self::Sql
            | Self::Records(_)
            | Self::Verify => Some(Role::Read),
            Self::Import(_) | Self::DeleteRecords(_) => Some(Role::Write),
            Self::CreateTable(_)
            | Self::CreateField(..)
            | Self::DropTable(_)
            | Self::DropField(..)
            // A database is a schema change like any other. The powers have to move together:
            // a weaker one that could drop a database would be a way round the stronger one
            // that guards dropping the tables in it.
            | Self::CreateDatabase(_)
            | Self::DropDatabase(_) => Some(Role::Admin),

            // A peer is a client with a token, not a trusted origin. Each of these needs what
            // the public route it serves needs, and interning is a write because it commits: a
            // read token that could assign row ids would be a read token that can change what
            // every other node means by a string.
            Self::PeerQuery
            | Self::PeerRecords
            | Self::PeerDigest
            | Self::PeerFragments
            | Self::PeerFragment
            | Self::PeerKeys
            | Self::PeerSchema => Some(Role::Read),
            Self::PeerImport | Self::PeerDelete | Self::PeerIntern | Self::PeerAllocate => {
                Some(Role::Write)
            }
            // Reading how far a table's ids reach is a read, and it is asked of every node
            // rather than of the leader.
            Self::PeerNextRecord => Some(Role::Read),
            Self::PeerDdl => Some(Role::Admin),
            // The agreement decides which node serves which range. A credential that can vote
            // is a credential that can decide where every read goes, which is more than write.
            //
            // Replacing a fragment outright is the same size of power: it is not writing a
            // fact, it is replacing what a node holds. So is saying a copy has caught up,
            // which is what lets that copy start answering reads.
            Self::PeerRaft
            | Self::PeerFragmentPut
            | Self::PeerKeysPut
            | Self::PeerRepaired
            | Self::Repair => Some(Role::Admin),
            // Reading every live page and writing it somewhere the operator named. `admin`
            // rather than `read` because what it produces is a second copy of the whole
            // database, and a credential that can make one is a credential that can carry the
            // data out of here.
            Self::Backup => Some(Role::Admin),
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
        ("POST", ["repair"]) => Target::Repair,
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

/// Routes one request and runs it, or answers `404`, `401` or `403` without running anything.
pub fn dispatch<P: PagerMut + Sync>(ctx: &Ctx<'_, P>, req: &Request) -> Response {
    let segments = req.segments();
    let borrowed: Vec<&str> = segments.iter().map(std::convert::AsRef::as_ref).collect();
    let Some(target) = resolve(req.method.as_str(), &borrowed) else {
        return Response::failure(404, "no_such_route", "no such route");
    };

    if let Some(refusal) = refuse(ctx.auth, req, target.role()) {
        return refusal;
    }

    // **Before any body is decoded.** A peer running a different build sends bytes this one
    // would read as something else - a length where a tag was - and the result is not a
    // refusal, it is an answer that is quietly wrong. A peer reading a different cluster file
    // is the same failure one level up: it believes a range belongs somewhere it does not.
    // Both are cheap to check and impossible to notice afterwards.
    if target.is_internal() {
        if let Some(refusal) = mismatched(ctx, req) {
            return refusal;
        }
    }

    match target {
        Target::Health => health(),
        Target::Ready => ready(ctx),
        Target::Metrics => {
            let mut text = ctx.metrics.render(&ctx.api().metrics());
            crate::metrics::render_keys(&mut text, &ctx.api().key_stats());
            crate::metrics::render_cluster(&mut text, &ctx.cluster.counters());
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
        Target::Sql => sql(ctx, req),
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
        Target::PeerNextRecord => peer_next_record(ctx, req),
        Target::PeerDdl => peer_ddl(ctx, req),
        Target::PeerDigest => peer_digest(ctx),
        Target::PeerRaft => peer_raft(ctx, req),
        Target::PeerFragments => peer_fragments(ctx, req),
        Target::PeerFragment => peer_fragment(ctx, req),
        Target::PeerFragmentPut => peer_fragment_put(ctx, req),
        Target::PeerKeys => peer_keys(ctx, req),
        Target::PeerKeysPut => peer_keys_put(ctx, req),
        Target::PeerRepaired => peer_repaired(ctx, req),
        Target::PeerSchema => {
            let mut out = Vec::new();
            wire::put_schema(&mut out, &ctx.cluster.schema(), &ctx.cluster.views());
            Response::binary(out)
        }
        Target::Repair => repair(ctx),
        Target::Backup => backup(ctx, req),
    }
}

/// `None` when the request may proceed.
/// The same check, for a route whose static role is only a floor.
///
/// `POST /sql` is authorised as `read` because that is what almost every statement needs, and
/// the check runs before the body is decoded. A statement that changes the schema needs more,
/// and this is how it asks - see `routes::query::sql`.
pub(super) fn require(auth: &Auth, req: &Request, needed: Role) -> Option<Response> {
    refuse(auth, req, Some(needed))
}

fn refuse(auth: &Auth, req: &Request, needed: Option<Role>) -> Option<Response> {
    let needed = needed?;
    match auth.authorize(req.bearer(), needed) {
        Outcome::Allowed(_) => None,
        Outcome::Unauthenticated => Some(
            Response::failure(401, "unauthenticated", "a bearer token is required")
                // Without this header a client cannot tell a 401 it can fix from one it
                // cannot, and every HTTP client library looks for it.
                .with_header("WWW-Authenticate", "Bearer realm=\"big\""),
        ),
        // Deliberately says what was needed. Hiding it does not stop anyone holding a valid
        // token from finding out, and it stops a legitimate operator from understanding why.
        Outcome::Forbidden { held, needed } => Some(Response::failure(
            403,
            "forbidden",
            &format!("this token is `{}`; this route needs `{}`", held.as_str(), needed.as_str()),
        )),
    }
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
        ClusterError::Local(api) => Response::from_error(api),
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
