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

//! One entry point for everything above the engine.
//!
//! What this crate decides, because nothing below it could:
//!
//! - **Who holds the database.** [`Api`] owns it. Anything above holds an `Api`, not a `Db`.
//! - **Where a transaction begins and ends.** A query is one read transaction; an import is
//!   one write transaction over the whole batch. Neither is exposed, so no caller can leave
//!   one open across a network round trip.
//! - **What the schema looks like on the way out.** A snapshot, not a lock guard: serialising
//!   it must never keep a reader inside the engine while bytes go down a socket.
//!
//! Deliberately not decided here: whether any of this is reached over a network. That is the
//! next layer's question, and the answer is still open.

#![deny(unsafe_code)]
#![deny(missing_docs)]

pub mod error;
pub mod explain;
pub mod fact;
pub mod introspect;
pub mod result;
pub mod schema;
pub mod views;

pub use error::{ApiError, Result};
pub use result::{
    date_text, fixed, literal_of, one_cell, result_set, timestamp_text, Datum, ResultSet, Row,
};
pub use schema::{FieldInfo, TableInfo};
pub use views::ViewInfo;

// Everything below appears in a signature on this page, and a type a caller cannot name is a
// type they cannot hold: `query` returns a `Value`, so a function that wraps `query` has no way
// to write its own return type unless `Value` is reachable from here. These crates are not
// published - `big-embed` and `big-http` are the only two that are - so a re-export is the only
// route to them, not merely the convenient one.
pub use big_db::catalog::FragmentMeta;
pub use big_db::catalog::{FieldId, FieldKind, TableEngine, TableId};
// Renamed on the way out: `big_sql::Cell` is a cell of a result *row*, and this is a cell
// of a stored *column*. Two different things that would otherwise share one name here.
pub use big_db::Cell as ColumnCell;
pub use big_db::KeyStats;
pub use big_db::{
    Container, ContainerKey, Durability, FragmentAddr, Granularity, Matches, QueryLimits, RecordId,
    RowId, RowSet,
};
pub use big_exec::{Group, Pair, Projected, Projection, Value};
pub use big_pager::{MemPager, Metrics, MmapPager, PagerMut, DEFAULT_MAPSIZE};
pub use big_plan::{Plan, Rows};
// The SQL surface's two public shapes. `Shape` appears in the return type of `Api::sql`, so a
// caller that renders an answer has to be able to name it.
pub use big_plan::Literal;
pub use big_sql::lower;
pub use big_sql::{
    Absent, Answer, Ask, Cell, Columns, Cut, Format, GroupOrder, Having, JoinSide, Keying, Of,
    OrderBy, Pairing, Probe as SqlProbe, Refused, Scalar, Selected, Shape,
    Statement as SqlStatement, Threshold, Units,
};
pub use big_sql::{
    Acl as SqlAcl, AclObject as SqlAclObject, Alter as SqlAlter, Column as SqlColumn,
    ColumnKind as SqlColumnKind, Ddl as SqlDdl, ExplainMode, Insert as SqlInsert,
    Query as SqlQuery, Select as SqlSelect, Show as SqlShow, Shown as SqlShown, Sql, SqlError,
    RECORD_COLUMN,
};

use big_db::{At, Db};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// What a caller may set on one read, beyond the query itself.
///
/// A struct rather than four more `query_*` methods: these compose, and every combination of
/// them is legitimate. `Default` is the old behaviour exactly - the engine's own memory
/// ceilings, no clock, and nobody able to interrupt - so adding this changed no caller.
///
/// There is deliberately no equivalent for writes. A write is bounded by the batch the client
/// sent, which the edge already caps, and abandoning one half-applied would need the
/// transaction to be reasoned about rather than simply dropped.
#[derive(Clone, Default)]
pub struct QueryOptions {
    /// Memory ceilings. `None` keeps [`QueryLimits::default`].
    pub limits: Option<QueryLimits>,
    /// Wall-clock budget. `None` means the query runs to completion.
    pub timeout: Option<Duration>,
    /// A flag anyone may set to stop the query. Read cooperatively, at the same points the
    /// memory budget is charged.
    pub cancel: Option<Arc<AtomicBool>>,
    /// Which database an unqualified name in the statement means.
    ///
    /// **A property of the request, not of the text.** `POST /sql` answers one statement and
    /// remembers nothing, so there is no session for a `USE` to leave a database in - it
    /// arrives as `?database=`, and `bigctl` is what turns a typed `USE` into one. `None` means
    /// [`big_db::DEFAULT_DATABASE_NAME`], which is what every statement written before
    /// databases existed asked for.
    ///
    /// A name the statement qualified itself is untouched: `sales.orders` in a request against
    /// `ops` means `sales`.
    pub database: Option<String>,
}

impl QueryOptions {
    /// The database an unqualified name in this request means.
    pub fn database(&self) -> &str {
        self.database.as_deref().unwrap_or(big_db::DEFAULT_DATABASE_NAME)
    }
}

/// The options for the next plan of a statement, with the wall-clock already spent taken off.
///
/// **A timeout bounds the statement, not each plan in it.** A statement can make several plans,
/// and handing every one of them the full budget would let a sixteen-plan statement hold a
/// worker and a socket for sixteen times the configured timeout - which is a timeout that no
/// longer bounds a request. An exhausted budget becomes a zero one, and the storage layer's own
/// deadline check refuses it with the error it always gives, rather than a second one meaning
/// the same thing.
///
/// The memory ceiling is deliberately *not* divided: it bounds what one query materialises at
/// once, the plans run one at a time, and splitting it would make each plan's ceiling depend on
/// how many others happen to share the statement.
pub fn remaining(opts: &QueryOptions, started: Instant) -> QueryOptions {
    QueryOptions {
        limits: opts.limits,
        timeout: opts.timeout.map(|t| t.saturating_sub(started.elapsed())),
        cancel: opts.cancel.clone(),
        database: opts.database.clone(),
    }
}

/// The most record ids a semi-join expands into before it is refused.
///
/// **The cost of this expansion is the number of ids, and it is paid in bit-plane reads.** The
/// outer column is bit-sliced, so `b_id IN (…)` is a union of one equality per id, and each
/// equality costs one read per plane. A set of a few thousand is a query; a set of a few million
/// is a scan wearing a `WHERE`, and it is refused with the number rather than answered slowly.
pub const MAX_SET: u64 = 4_096;

/// Whether a call has a semi-join anywhere under it.
///
/// Its own function because two callers need the question and neither should have to know the
/// name: `Api::sql` and the coordinator resolve one, and an `EXPLAIN` refuses one.
pub fn has_set(call: &big_plan::ast::Call) -> bool {
    fn walk(e: &big_plan::ast::Expr) -> bool {
        match e {
            big_plan::ast::Expr::Call(c) => c.name == "InRecords" || c.args.iter().any(walk),
            big_plan::ast::Expr::Named { value, .. } => walk(value),
            _ => false,
        }
    }
    call.name == "InRecords" || call.args.iter().any(walk)
}

/// Answers the `InRecords` calls inside a statement, turning each into the union of ids it means.
///
/// **This is the one call that names a table other than its own, and it is resolved here rather
/// than in a plan.** A record is a set of bits in one table and the ids in the outer column are
/// coordinates in another, so the inner set has to be *fanned out and merged in full* before the
/// outer question can be asked: a per-node answer is a share of the set, and narrowing one node's
/// records by another node's share of the ids is a smaller answer that looks exactly like a
/// correct one. The same argument a join's arithmetic makes, one step earlier.
///
/// Planning and running are passed in for the reason [`run_probe`] takes them: the caller decides
/// whether a step runs on this node or across every owner, and the resolution is the same either
/// way. What it costs is one extra round trip per `IN (SELECT …)`, said in `EXPLAIN`.
pub fn resolve_sets<E>(
    calls: &mut [big_sql::lower::Ask],
    plan_of: impl Fn(&str, &big_plan::ast::Call) -> core::result::Result<Plan, E>,
    mut run: impl FnMut(&Plan) -> core::result::Result<Value, E>,
    too_many: impl Fn(&str, u64) -> E,
) -> core::result::Result<(), E> {
    for ask in calls.iter_mut() {
        let mut expr = big_plan::ast::Expr::Call(ask.call.clone());
        resolve_expr(&mut expr, &plan_of, &mut run, &too_many)?;
        ask.call = match expr {
            big_plan::ast::Expr::Call(c) => c,
            _ => unreachable!("an `InRecords` only ever replaces a call with a call"),
        };
    }
    Ok(())
}

/// One expression, with every `InRecords` under it replaced by the ids it selected.
fn resolve_expr<E>(
    expr: &mut big_plan::ast::Expr,
    plan_of: &impl Fn(&str, &big_plan::ast::Call) -> core::result::Result<Plan, E>,
    run: &mut impl FnMut(&Plan) -> core::result::Result<Value, E>,
    too_many: &impl Fn(&str, u64) -> E,
) -> core::result::Result<(), E> {
    use big_plan::ast::{Call, Expr, Literal};

    let Expr::Call(call) = expr else {
        if let Expr::Named { value, .. } = expr {
            return resolve_expr(value, plan_of, run, too_many);
        }
        return Ok(());
    };
    if call.name != "InRecords" {
        for arg in &mut call.args {
            resolve_expr(arg, plan_of, run, too_many)?;
        }
        return Ok(());
    }

    // `InRecords(field=<column>, table='<db.table>', <the inner set>)`, in that order: the
    // lowering is the only thing that builds one, so the shape is known rather than searched for.
    let (mut field, mut table, mut inner) = (None, None, None);
    for arg in std::mem::take(&mut call.args) {
        match arg {
            Expr::Named { name, value } if name == "field" => match *value {
                Expr::Ident(f) => field = Some(f),
                _ => return Ok(()),
            },
            Expr::Named { name, value } if name == "table" => match *value {
                Expr::Literal(Literal::Str(t)) => table = Some(t),
                _ => return Ok(()),
            },
            other => inner = Some(other),
        }
    }
    let (Some(field), Some(table), Some(inner)) = (field, table, inner) else { return Ok(()) };

    // Nested semi-joins resolve innermost first, which is the order they have to run in.
    let mut inner = inner;
    resolve_expr(&mut inner, plan_of, run, too_many)?;

    // **The inner set is asked as itself.** A bitmap call with no aggregate around it is
    // already a `Plan::Rows` to the planner, so there is no wrapper to add - which is the same
    // reason a `WHERE` never grows one.
    let Expr::Call(rows) = inner else { return Ok(()) };
    let ids = match run(&plan_of(&table, &rows)?)? {
        Value::Rows(m) => m,
        // A bare bitmap call is the one thing the planner answers with a record set.
        _ => unreachable!("the inner set of a semi-join is a `Plan::Rows`"),
    };
    if ids.cardinality() > MAX_SET {
        return Err(too_many(&table, ids.cardinality()));
    }

    // **An empty set is an empty answer, not a missing filter.** `Union()` of nothing has no
    // identity the planner will take, so it is written as the complement of everything - the
    // one spelling of "no records" every plan already answers.
    let terms: Vec<Expr> = ids
        .records()
        .map(|id| {
            Expr::Call(Call {
                name: "Row".to_string(),
                args: vec![Expr::Compare {
                    field: field.clone(),
                    op: "=".to_string(),
                    value: Literal::Int(id),
                }],
            })
        })
        .collect();
    *expr = match terms.len() {
        0 => Expr::Call(Call {
            name: "Not".to_string(),
            args: vec![Expr::Call(Call { name: "All".to_string(), args: Vec::new() })],
        }),
        1 => terms.into_iter().next().expect("just measured"),
        _ => Expr::Call(Call { name: "Union".to_string(), args: terms }),
    };
    Ok(())
}

/// Runs one [`SqlProbe`] to convergence, given a way to plan and a way to run.
///
/// **A quantile is a search, not a question.** No plan answers "the value at rank k"; what a
/// plan answers is "how many records hold a value at or below `v`". So the bound moves until
/// the count lands on the rank, and every step is an ordinary `Count` - fanned out and merged
/// like any other, which is why an exact quantile needed no `Plan` variant and no merge arm.
///
/// Planning and running are passed in rather than taken from an [`Api`], because the caller
/// decides where a step runs: on this node, or across every owner. The search is the same
/// either way, and there is one of it.
///
/// The cost is one round trip per step, about the bit depth of the field, plus three to find
/// the range and the population. That is the honest price of holding no values in memory.
/// Generic in the error so that both callers can use it unchanged: this node's failures are
/// [`ApiError`]s and a coordinator's are its own, and the search never makes one of either.
pub fn run_probe<E>(
    probe: &SqlProbe,
    plan_of: impl Fn(&str, &big_plan::ast::Call) -> core::result::Result<Plan, E>,
    mut run: impl FnMut(&Plan) -> core::result::Result<Value, E>,
) -> core::result::Result<Value, E> {
    use big_plan::ast::{Call, Expr, Literal};

    // The records in scope that hold a value in the field at all. Every stored value is at or
    // above zero, so `>= 0` is exactly the field's exists row - and a quantile is over the
    // records that have a value, not over those that merely matched the `WHERE`.
    let bounded = |op: &str, v: u64| {
        Expr::Call(Call {
            name: "Intersect".to_string(),
            args: vec![
                Expr::Call(probe.rows.clone()),
                Expr::Call(Call {
                    name: "Row".to_string(),
                    args: vec![Expr::Compare {
                        field: probe.field.clone(),
                        op: op.to_string(),
                        value: Literal::Int(v),
                    }],
                }),
            ],
        })
    };
    let count_of = |rows: Expr| Call { name: "Count".to_string(), args: vec![rows] };

    let mut ask =
        |call: Call| -> core::result::Result<Value, E> { run(&plan_of(&probe.table, &call)?) };

    let population = ask(count_of(bounded(">=", 0)))?.as_count().unwrap_or(0);
    if population == 0 {
        // No values is no quantile, which is the `null` a `min` over nothing already answers.
        return Ok(Value::Extreme(None));
    }

    // The rank asked for, one-based. `ceil(level * n)`, with the zeroth quantile meaning the
    // smallest value rather than nothing at all.
    let rank = {
        let scaled = u128::from(population) * u128::from(probe.per_mille);
        let ceil = scaled.div_ceil(1_000);
        (ceil.max(1)) as u64
    };

    // The search runs between the smallest and largest values actually present, which is
    // usually far narrower than the field's declared depth.
    let low = ask(Call { name: "Min".to_string(), args: min_max_args(probe, &bounded) })?;
    let high = ask(Call { name: "Max".to_string(), args: min_max_args(probe, &bounded) })?;
    let (Some(Some(mut lo)), Some(Some(mut hi))) = (low.as_extreme(), high.as_extreme()) else {
        return Ok(Value::Extreme(None));
    };

    // Invariant: `count(<= hi) >= rank` and, once `lo` moves, `count(<= lo - 1) < rank`. The
    // loop halves the gap, so it ends after about the bit depth of the range.
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let at_or_below = ask(count_of(bounded("<=", mid)))?.as_count().unwrap_or(0);
        if at_or_below >= rank {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }
    Ok(Value::Extreme(Some(lo)))
}

/// `Min`/`Max` over the records the probe is about, which is where the search begins and ends.
fn min_max_args(
    probe: &SqlProbe,
    bounded: &impl Fn(&str, u64) -> big_plan::ast::Expr,
) -> Vec<big_plan::ast::Expr> {
    vec![
        bounded(">=", 0),
        big_plan::ast::Expr::Named {
            name: "field".to_string(),
            value: Box::new(big_plan::ast::Expr::Ident(probe.field.clone())),
        },
    ]
}

/// One fact to write. Which variant is legal depends on the field's kind, and the engine
/// refuses a mismatch rather than guessing.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Fact<'a> {
    /// A value for an unsigned integer field, stored bit-sliced across `bit_depth` rows.
    Int {
        /// Field name, resolved against the table named in the call.
        field: &'a str,
        /// Which record this fact is about.
        record: RecordId,
        /// The value. Wider than the field's declared `bit_depth` is refused, not truncated.
        value: u64,
    },
    /// A value for a signed integer field. Distinct from `Int` because the two cover different
    /// numbers, and because writing one to the other kind's field silently stores a different
    /// value rather than failing.
    Signed {
        /// Field name, resolved against the table named in the call.
        field: &'a str,
        /// Which record this fact is about.
        record: RecordId,
        /// The value, sign included.
        value: i64,
    },
    /// A value for a float field.
    ///
    /// **Carried as `f64::to_bits`**, for the reason `Rows::CompareFloat` carries its threshold
    /// that way: a fact is compared for equality across the write path and its tests, and
    /// `PartialEq` on a float is not reflexive, so an `f64` here would cost this enum its `Eq`.
    /// The bits are the same number, and the storage layer reads them back before it stores one.
    Float {
        /// Field name, resolved against the table named in the call.
        field: &'a str,
        /// Which record this fact is about.
        record: RecordId,
        /// `f64::to_bits` of the value. A NaN is refused on the way in, not here.
        bits: u64,
    },
    /// A value for a set, mutex or time quantum field: a string that is interned to a row id.
    /// Longer than the engine's key limit is refused rather than truncated, because two keys
    /// that truncate alike would silently merge two rows.
    Key {
        /// Field name, resolved against the table named in the call.
        field: &'a str,
        /// Which record this fact is about.
        record: RecordId,
        /// The key. Interned on write, so the same string always names the same row in every
        /// shard.
        value: &'a str,
    },
    /// A value for a boolean field, which is two rows rather than one: a record is written
    /// into the true row *and* the false row is cleared, so "false" and "absent" stay distinct.
    Bool {
        /// Field name, resolved against the table named in the call.
        field: &'a str,
        /// Which record this fact is about.
        record: RecordId,
        /// The value.
        value: bool,
    },
    /// A key with the moment it happened, for a time quantum field.
    ///
    /// Its own variant rather than a `Key` with an extra field, because the two write different
    /// things: a key sets one bit, and this also writes the views by day that make a window
    /// over the field cheap. Without it a time quantum field can be created and filled and
    /// still have no views for a window to read - which is what it had before this existed.
    Time {
        /// Field name, resolved against the table named in the call.
        field: &'a str,
        /// Which record this fact is about.
        record: RecordId,
        /// The key.
        value: &'a str,
        /// When it happened, in seconds since the epoch.
        unix_seconds: i64,
    },
}

/// One fragment's containers: everything it is.
///
/// A name rather than the tuple, because it appears in a signature and a reader should not
/// have to parse three levels of angle brackets to find out that it is a list of containers.
pub type Containers = Vec<(ContainerKey, Container)>;

/// What one fragment holds, in whichever units its view stores.
///
/// An enum rather than two optional fields, because a bitmap fragment and a column segment are
/// addressed alike and store nothing alike: a repair handed the wrong one would write values
/// where row bits belong. Making the two unrepresentable together is cheaper than checking.
#[derive(Clone, Debug)]
pub enum FragmentData {
    /// Containers of row bits, for every view but the column one.
    Containers(Containers),
    /// What each record holds, by its offset within the shard.
    ///
    /// Records rather than encoded blocks: shipping the encoded form would tie two nodes to the
    /// same codec choice for ever, and the receiver re-encoding with its own is what keeps the
    /// block format an implementation detail rather than a wire format.
    Cells(Vec<(u64, ColumnCell)>),
}

impl FragmentData {
    /// How many units this carries - containers or cells. What a repair reports as progress.
    pub fn len(&self) -> usize {
        match self {
            Self::Containers(c) => c.len(),
            Self::Cells(c) => c.len(),
        }
    }

    /// Whether it carries nothing at all, which is what an absent fragment answers with.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// One key, and the row id it was already given somewhere else.
///
/// Carried alongside a batch rather than inside a [`Fact`] because the same key appears in
/// many facts and its meaning is a property of the field, not of any one record.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct KeyAssignment<'a> {
    /// Field name, resolved against the table named in the call.
    pub field: &'a str,
    /// The key, exactly as it will be written.
    pub key: &'a str,
    /// What it means. Decided by the schema leader; refused here if it contradicts what this
    /// node already holds.
    pub row: RowId,
}

/// A database, and the only handle anything above the engine holds.
///
/// Generic over the pager so the whole facade is exercised in-memory in tests. `Api::open`
/// gives the file-backed one; [`Api::in_memory`] gives the other.
///
/// ```
/// # fn main() -> Result<(), big_embed::ApiError> {
/// use big_embed::{Api, Fact, FieldKind};
///
/// let api = Api::in_memory()?;
/// api.create_table("tx")?;
/// api.create_field("tx", "amount", FieldKind::Int, 20)?;
/// api.create_field("tx", "country", FieldKind::Set, 0)?;
///
/// api.import("tx", &[
///     Fact::Int { field: "amount", record: 1, value: 4_200 },
///     Fact::Key { field: "country", record: 1, value: "GB" },
/// ])?;
///
/// let hits = api.query("tx", r#"Count(Row(country="GB"))"#)?;
/// # Ok(())
/// # }
/// ```
pub struct Api<P: PagerMut> {
    db: Db<P>,
}

impl Api<MemPager> {
    /// A database with no file behind it. Nothing is durable and nothing is locked.
    pub fn in_memory() -> Result<Self> {
        Ok(Self { db: Db::in_memory()? })
    }
}

#[cfg(unix)]
impl Api<MmapPager> {
    /// One process per file: the engine takes an exclusive lock, so a second `Api` on the same
    /// path fails here rather than corrupting anything later.
    pub fn open(path: impl AsRef<std::path::Path>) -> Result<Self> {
        Ok(Self { db: Db::open_path(path)? })
    }

    /// The same, with the address-space reservation named rather than defaulted.
    ///
    /// See [`Db::open_path_sized`]: the reservation is the file's ceiling for the life of the
    /// process, so it belongs to whoever is deciding how many databases this machine runs.
    pub fn open_sized(path: impl AsRef<std::path::Path>, mapsize: u64) -> Result<Self> {
        Ok(Self { db: Db::open_path_sized(path, mapsize)? })
    }
}

/// What a backup did, in the numbers an operator asks for afterwards.
///
/// The transaction id is the point the copy was taken at, which is what makes two backups of
/// the same file comparable and what a restore rehearsal records. `pages` is the source's
/// page count, not the destination's: the copy is compact, so it is smaller by however much
/// the freelist was holding, and reporting the source is what says how much was walked.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Backup {
    /// The transaction the copy was taken at.
    pub txn_id: big_pager::TxnId,
    /// Pages in the source when the walk started.
    pub pages: u64,
}

/// `Sync` because a query fans its fragment scan out across threads. Both pagers qualify.
impl<P: PagerMut + Sync> Api<P> {
    /// Creates a table, returning its id. Names are interned; ids are never reused.
    ///
    /// Takes the default storage engine. See [`Api::create_table_with`].
    pub fn create_table(&self, name: &str) -> Result<TableId> {
        Ok(self.db.create_table(name)?)
    }

    /// The same, with the storage engine named.
    ///
    /// The engine decides what the table writes for every fact and therefore which questions it
    /// answers cheaply - see [`TableEngine`]. It is fixed at creation: creating the same table
    /// again under a different engine is refused rather than ignored.
    pub fn create_table_with(&self, name: &str, engine: TableEngine) -> Result<TableId> {
        Ok(self.db.create_table_with(name, engine)?)
    }

    /// Creates a field of the given kind.
    ///
    /// `bit_depth` is how many bits an [`FieldKind::Int`] value may occupy and is ignored by
    /// the kinds that do not store integers. A value wider than it is refused on write.
    pub fn create_field(
        &self,
        table: &str,
        field: &str,
        kind: FieldKind,
        bit_depth: u32,
    ) -> Result<FieldId> {
        Ok(self.db.create_field(table, field, kind, bit_depth)?)
    }

    /// A decimal field: stored as an integer, compared as a value with `scale` digits after
    /// the point.
    pub fn create_decimal(
        &self,
        table: &str,
        field: &str,
        bit_depth: u32,
        scale: i8,
    ) -> Result<FieldId> {
        Ok(self.db.create_decimal(table, field, bit_depth, scale)?)
    }

    /// A time quantum field: keyed, and additionally written into one view per granularity so
    /// a range of days can be read without touching the days outside it.
    pub fn create_time_quantum(
        &self,
        table: &str,
        field: &str,
        granularity: Vec<Granularity>,
    ) -> Result<FieldId> {
        Ok(self.db.create_time_quantum(table, field, granularity)?)
    }

    /// A snapshot of the schema, owned outright.
    ///
    /// The catalog lives behind a lock; handing out a guard would let a caller hold it while
    /// doing something slow, and the first slow thing above this crate will be a socket.
    pub fn schema(&self) -> Vec<TableInfo> {
        schema::snapshot(&self.db.catalog())
    }

    /// Writes a whole batch in **one** transaction, so it lands entirely or not at all.
    ///
    /// The batch is the unit on purpose. Per-fact transactions would pay the commit cost -
    /// two fsyncs and a full metadata rewrite - once per fact.
    pub fn import(&self, table: &str, facts: &[Fact<'_>]) -> Result<()> {
        let mut w = self.db.write();
        apply(&mut w, table, facts)?;
        w.commit()?;
        Ok(())
    }

    /// Assigns a row id to each key, in one transaction, and hands the ids back.
    ///
    /// Nothing is written about the records; this only fixes what the keys *mean*. It exists
    /// for the schema leader of a cluster, which is the one node allowed to decide that, and
    /// it commits because the decision has to outlive the request that caused it.
    ///
    /// Ids are dense and never reused, so a key already known costs a lookup and no write.
    pub fn intern_keys(&self, table: &str, field: &str, keys: &[&str]) -> Result<Vec<RowId>> {
        let mut w = self.db.write();
        let mut out = Vec::with_capacity(keys.len());
        for key in keys {
            out.push(w.intern_key(table, field, key)?);
        }
        w.commit()?;
        Ok(out)
    }

    /// The same as [`Api::import`], for a caller that has already been told what its keys mean.
    ///
    /// A shard owner in a cluster does not choose row ids: the coordinator resolved them with
    /// the schema leader before any fact was sent, and this is where that answer lands. The
    /// assignments are applied in the *same* transaction as the facts, so a refused batch
    /// leaves neither behind.
    ///
    /// A mapping that contradicts one this node already holds is refused rather than
    /// overwritten. That is the whole point of routing interning through one node, checked at
    /// the node that would otherwise be the one to disagree.
    pub fn import_with_keys(
        &self,
        table: &str,
        keys: &[KeyAssignment<'_>],
        facts: &[Fact<'_>],
    ) -> Result<()> {
        let mut w = self.db.write();
        for k in keys {
            w.assign_key(table, k.field, k.key, k.row)?;
        }
        apply(&mut w, table, facts)?;
        w.commit()?;
        Ok(())
    }

    /// Removes records from every field of a table, in one transaction.
    ///
    /// Returns how many of them existed. Deleting a record that was never written is not an
    /// error - it is a request that was already satisfied - so a retried delete is safe.
    pub fn delete(&self, table: &str, records: &[RecordId]) -> Result<u64> {
        let mut w = self.db.write();
        let n = w.delete(table, records)?;
        w.commit()?;
        Ok(n)
    }

    /// Creates a database, or returns `false` if it was already there.
    pub fn create_database(&self, name: &str) -> Result<bool> {
        let before = self.db.catalog().database(name).is_some();
        self.db.create_database(name)?;
        Ok(!before)
    }

    /// Removes a database and, with `cascade`, every table in it. See
    /// [`big_db::Db::drop_database`].
    pub fn drop_database(&self, name: &str, cascade: bool) -> Result<bool> {
        Ok(self.db.drop_database(name, cascade)?)
    }

    /// Whether a database of this name exists.
    pub fn databases_named(&self, name: &str) -> bool {
        self.db.catalog().database(name).is_some()
    }

    /// Every database, with the tables each holds.
    pub fn databases(&self) -> ResultSet {
        introspect::show_databases(&self.schema())
    }

    /// Makes a role. **`Ok(false)` means nothing changed** - it was already there.
    ///
    /// Roles are administered here rather than in `big-db` on purpose. What a role *is* belongs
    /// to `big-rbac`, and where one is *kept* to the catalog's record chain; a third statement of
    /// the same rule in the storage crate would be a third place for it to drift. What this layer
    /// adds is the two things only it can: the name rule every catalog record is held to, and a
    /// transaction to commit the change in.
    pub fn create_role(&self, name: &str) -> Result<bool> {
        big_db::catalog::check_name(name)?;
        Ok(self.db.transact(|c| {
            let before = c.rbac.role(name).is_some();
            c.rbac.intern_role(name)?;
            Ok(!before)
        })?)
    }

    /// Removes a role and every grant it held. **`Ok(false)` means there was no such role.**
    ///
    /// One transaction, so there is no window in which the role is gone and its privileges are
    /// still answerable. A users file naming it is not consulted - it cannot be, from here - so
    /// whoever held it now holds a name that resolves to nothing, which is no privileges at all
    /// and is the fail-closed direction.
    pub fn drop_role(&self, name: &str) -> Result<bool> {
        Ok(self.db.transact(|c| Ok(c.rbac.drop_role(name)?))?)
    }

    /// Every role, in name order. What `SHOW ROLES` lists.
    ///
    /// [`big_rbac::SUPERUSER`] is prepended rather than stored, the way `default` is prepended to
    /// a listing of databases: it is true before anything is written, so a reload that had to
    /// decide whether to put it back is a reload that could get it wrong.
    pub fn roles(&self) -> Vec<String> {
        let mut out = vec![big_rbac::SUPERUSER.to_string()];
        out.extend(self.db.catalog().rbac.roles().map(|r| r.name));
        out
    }

    /// What one role has been granted, as names rather than ids. What `SHOW GRANTS` lists.
    ///
    /// Each entry is the database and table the grant is about - `None` meaning "every" at that
    /// level - and the privileges this build can name. A bit stored by a newer build is left out:
    /// it is kept on disk, but it is not this build's to describe.
    pub fn grants_of(
        &self,
        role: &str,
    ) -> Vec<(Option<String>, Option<String>, Vec<&'static str>)> {
        let catalog = self.db.catalog();
        let Some(id) = catalog.rbac.role(role) else { return Vec::new() };
        catalog
            .rbac
            .of(id)
            .map(|(key, privileges)| {
                let database = (key.database != big_rbac::ANY_DATABASE)
                    .then(|| catalog.database_name(key.database).map(str::to_string))
                    .flatten();
                let table = (key.table != big_rbac::ANY_TABLE)
                    .then(|| catalog.table_by_id(key.table).map(|t| t.name.clone()))
                    .flatten();
                (database, table, privileges.named().map(big_rbac::Privilege::as_str).collect())
            })
            .collect()
    }

    /// Whether `who` may do what `demand` asks.
    ///
    /// **The decision, and the reason it lives at this layer.** It needs two things that are only
    /// together here: the grants, which are in the catalog, and the names in the demand, which
    /// come from a statement. Resolving one against the other is a handful of map lookups over
    /// integers and no I/O - which matters, because this runs once per demand per statement on a
    /// path where re-authenticating would cost a second argon2 hash.
    ///
    /// Fails closed at every step. A role the catalog does not have holds nothing; an object that
    /// does not exist cannot be granted on, so nothing reaches it either.
    pub fn allows(&self, who: &big_rbac::Who, demand: big_rbac::Demand<'_>) -> bool {
        use big_rbac::{ObjectRef, Who, ANY_DATABASE, ANY_TABLE};
        match who {
            // Auth is off, or the caller is a peer applying something already ruled on.
            Who::Trusted => return true,
            _ if who.is_superuser() => return true,
            Who::Role(_) => {}
        }
        let Who::Role(role) = who else { return true };
        let catalog = self.db.catalog();
        let Some(role) = catalog.rbac.role(role) else { return false };
        // **A name that resolves to nothing widens rather than refusing.** A grant is filed under
        // the id of an object that exists, so a table nobody has created cannot have one of its
        // own - but a role holding `sales.*` holds it over every table in `sales`, including the
        // one somebody just mistyped. Narrowing to the level that *does* resolve is what lets
        // that caller get the honest `404` instead of a `403` about a privilege they have.
        //
        // It leaks nothing: a caller without the wider grant is refused either way, and learns
        // only that they were refused - which they already knew.
        let (database, table) = match demand.on {
            ObjectRef::Server => (ANY_DATABASE, ANY_TABLE),
            ObjectRef::Database(name) => match catalog.database(name) {
                Some(id) => (id, ANY_TABLE),
                None => (ANY_DATABASE, ANY_TABLE),
            },
            ObjectRef::Table { database, table } => match catalog.database(database) {
                None => (ANY_DATABASE, ANY_TABLE),
                Some(database) => match catalog.table(database, table) {
                    Some(t) => (database, t.id),
                    None => (database, ANY_TABLE),
                },
            },
        };
        catalog.rbac.allows(role, demand.privilege, database, table)
    }

    /// What one role holds on exactly this object, with no widening applied.
    ///
    /// The raw stored mask, which is what a `GRANT` has to read before it can compute the mask to
    /// store: the union across levels is the *decision*'s business, and adding a privilege to the
    /// union rather than to the entry would write a grant nobody asked for.
    pub fn granted(&self, role: &str, object: &big_rbac::Object) -> Result<big_rbac::Privileges> {
        use big_rbac::{GrantKey, Object, ANY_DATABASE, ANY_TABLE};
        let catalog = self.db.catalog();
        let Some(role) = catalog.rbac.role(role) else { return Ok(big_rbac::Privileges::empty()) };
        let key = match object {
            Object::Server => GrantKey { role, database: ANY_DATABASE, table: ANY_TABLE },
            Object::Database(name) => {
                let Some(d) = catalog.database(name) else {
                    return Err(big_db::DbError::UnknownDatabase(name.clone()).into());
                };
                GrantKey { role, database: d, table: ANY_TABLE }
            }
            Object::Table { database, table } => {
                let Some(d) = catalog.database(database) else {
                    return Err(big_db::DbError::UnknownDatabase(database.clone()).into());
                };
                let Some(t) = catalog.table(d, table) else {
                    return Err(big_db::DbError::UnknownTable(table.clone()).into());
                };
                GrantKey { role, database: d, table: t.id }
            }
        };
        Ok(catalog.rbac.get(key))
    }

    /// Sets one role's privileges on one object to exactly `privileges`.
    ///
    /// **Absolute rather than a change to what is there**, which is what makes a grant safe to
    /// replicate: `GRANT` and `REVOKE` each read the current mask and compute the result once, so
    /// what travels to another node is the answer rather than the arithmetic. See
    /// [`big_rbac::Grants::set`].
    ///
    /// `None` for `database` means every database; `None` for `table` every table in the database
    /// named. Both halves must already exist - a grant on a table nobody has created has no id to
    /// hang on, and inventing one would resurrect the moment somebody used that name.
    pub fn set_grant(
        &self,
        role: &str,
        database: Option<&str>,
        table: Option<&str>,
        privileges: big_rbac::Privileges,
    ) -> Result<()> {
        use big_rbac::{GrantKey, RbacError, ANY_DATABASE, ANY_TABLE, SUPERUSER};
        if role == SUPERUSER {
            return Err(big_db::DbError::from(RbacError::ReservedRole(role.to_string())).into());
        }
        Ok(self.db.transact(|c| {
            let role_id =
                c.rbac.role(role).ok_or_else(|| RbacError::UnknownRole(role.to_string()))?;
            let database_id = match database {
                None => ANY_DATABASE,
                Some(name) => c
                    .database(name)
                    .ok_or_else(|| big_db::DbError::UnknownDatabase(name.to_string()))?,
            };
            let table_id = match table {
                None => ANY_TABLE,
                // A table is only nameable inside a database, so `ON *.tbl` is not a shape a
                // caller is allowed to have built.
                Some(name) if database_id == ANY_DATABASE => {
                    return Err(big_db::DbError::UnknownTable(name.to_string()))
                }
                Some(name) => {
                    c.table(database_id, name)
                        .ok_or_else(|| big_db::DbError::UnknownTable(name.to_string()))?
                        .id
                }
            };
            c.rbac.set(
                GrantKey { role: role_id, database: database_id, table: table_id },
                privileges,
            )?;
            Ok(())
        })?)
    }

    /// Stores a `SELECT` under a name. **`Ok(false)` means nothing changed** - the name already
    /// held this exact statement.
    ///
    /// Changed rather than created, because a replace changes what every statement reading
    /// through the view means, and answering `0` for that would report the most consequential
    /// form of this statement as a no-op.
    ///
    /// **The body is validated before it is stored.** Its shape was decided by the parser at
    /// `CREATE VIEW`; what this adds is the half that needs a catalog - the table or view it
    /// reads has to exist. A view over nothing is a statement that parses and cannot be read,
    /// and finding that out at `CREATE` is the difference between a typo and a trap. It is also
    /// what makes a cycle unrepresentable, since a view can only name what is already there.
    pub fn create_view(&self, name: &str, body: &str, or_replace: bool) -> Result<bool> {
        let base = {
            let mut parsed = big_sql::parse(body)?;
            // **In the view's own database, not the request's.** A view created as `sales.big`
            // over a bare `orders` names `sales.orders`, and that is the table that has to
            // exist - the same rule `views::body_of` applies when the view is later read. A
            // check against the wrong database would refuse a legal view, or worse, accept one
            // because a table of that name happened to exist somewhere else.
            big_sql::qualify(&mut parsed, big_db::TableRef::parse(name).database);
            let big_sql::Parsed::Query(q) = &parsed else {
                return Err(ApiError::Sql(big_sql::SqlError::Refused {
                    what: big_sql::Refused::ViewBody,
                    at: 0,
                }));
            };
            // One branch, one source: the parser refused a `UNION` and a `JOIN` already.
            q.branches[0].from.qualified()
        };
        let catalog = self.db.catalog();
        if catalog.lookup(&base).is_none() && catalog.lookup_saved_query(&base).is_none() {
            return Err(ApiError::Db(big_db::DbError::UnknownTable(base)));
        }
        let unchanged = catalog.lookup_saved_query(name).is_some_and(|q| q.text == body);
        drop(catalog);
        self.db.create_view(name, body, or_replace)?;
        Ok(!unchanged)
    }

    /// Forgets a view. `Ok(false)` means there was no such view.
    pub fn drop_view(&self, name: &str) -> Result<bool> {
        Ok(self.db.drop_view(name)?)
    }

    /// Every view this node holds, with the statement each carries.
    pub fn views(&self) -> Vec<ViewInfo> {
        let catalog = self.db.catalog();
        catalog
            .saved_queries()
            .map(|q| ViewInfo {
                database: catalog
                    .database_name(q.database)
                    .unwrap_or(big_sql::DEFAULT_DATABASE)
                    .to_string(),
                name: q.name.clone(),
                text: q.text.clone(),
            })
            .collect()
    }

    /// Removes a table, its fields, its row keys and the pages behind them.
    ///
    /// `Ok(false)` means there was no such table.
    pub fn drop_table(&self, table: &str) -> Result<bool> {
        Ok(self.db.drop_table(table)?)
    }

    /// Removes one field of a table and the pages behind it.
    pub fn drop_field(&self, table: &str, field: &str) -> Result<bool> {
        Ok(self.db.drop_field(table, field)?)
    }

    /// Parses, plans and runs one query inside one read transaction.
    pub fn query(&self, table: &str, text: &str) -> Result<Value> {
        self.query_with(table, text, &QueryOptions::default())
    }

    /// The same, under a caller's limits, deadline and cancellation flag.
    ///
    /// The transaction is built here rather than handed in, because the point of this facade
    /// is that a read transaction never outlives one call - and a caller holding one across a
    /// network round trip is the failure mode that would follow from relaxing it.
    pub fn query_with(&self, table: &str, text: &str, opts: &QueryOptions) -> Result<Value> {
        let mut read = self.db.read();
        if let Some(limits) = opts.limits {
            read = read.with_limits(limits);
        }
        if let Some(timeout) = opts.timeout {
            read = read.with_deadline(timeout);
        }
        if let Some(cancel) = &opts.cancel {
            read = read.with_cancel(Arc::clone(cancel));
        }
        Ok(big_exec::query(&read, table, text)?)
    }

    /// Parses, plans and runs one SQL statement inside one read transaction.
    ///
    /// The table comes from `FROM` rather than from the caller, which is the one structural
    /// difference from [`Api::query_with`] and the reason the HTTP route for this is `/sql`
    /// rather than one hanging off a table.
    ///
    /// The [`Shape`] that comes back with the answer is how the caller turns it into columns
    /// and rows. It is deliberately not applied here: a coordinator merges several nodes'
    /// answers before anything is rendered, and `count(DISTINCT ...)` counts groups that only
    /// exist once the merge is done.
    pub fn sql(&self, text: &str, opts: &QueryOptions) -> Result<(Vec<Value>, Answer)> {
        // Against the request's database, so an unqualified name means what `?database=` said.
        // Taken from `opts` rather than from a second parameter because it belongs with the
        // other things that are true of the request and not of the text.
        let started = Instant::now();
        // **The semi-joins first, because a call that names another table cannot be planned
        // until that table has answered.** Each is one extra round trip, and it is the whole
        // cost of `IN (SELECT …)`: what comes back is a set of ids, and the outer call is
        // narrowed by the union they mean.
        let (plans, probes, answer) = match self.translate_in(text, opts.database())? {
            big_sql::Sql::Query(mut statement) => {
                {
                    let catalog = self.db.catalog();
                    let schema = big_exec::CatalogSchema(&catalog);
                    resolve_sets(
                        &mut statement.calls,
                        |t, c| big_plan::plan(t, c, &schema).map_err(|e| ApiError::Query(e.into())),
                        |p| self.execute(p, &remaining(opts, started)),
                        |_, _| {
                            ApiError::Sql(big_sql::SqlError::Refused {
                                what: big_sql::Refused::SetTooLarge,
                                at: 0,
                            })
                        },
                    )?;
                }
                self.plan_statement(statement)?
            }
            // Every other statement has no calls to resolve, and says so through the same
            // refusals `plan_sql_in` gives. Routed through it rather than repeated here.
            _ => self.plan_sql_in(text, opts.database())?,
        };
        // One answer per plan, in the order the shape names them. Run in sequence rather than
        // concurrently: each already fans out across every fragment this node holds, and a
        // second layer of parallelism would contend with the first for the same threads.
        let mut values = Vec::with_capacity(plans.len());
        for plan in &plans {
            values.push(self.execute(plan, &remaining(opts, started))?);
        }
        // Then the searches, whose answers sit after the calls' - which is the order the shape
        // names them in.
        let catalog = self.db.catalog();
        let schema = big_exec::CatalogSchema(&catalog);
        for probe in &probes {
            values.push(run_probe(
                probe,
                |t, c| big_plan::plan(t, c, &schema).map_err(|e| ApiError::Query(e.into())),
                |p| self.execute(p, &remaining(opts, started)),
            )?);
        }
        Ok((values, answer))
    }

    /// Translates and resolves a SQL statement against this node's schema, without running it.
    ///
    /// [`Api::plan`] for the other surface, and split out for the same reason: a coordinator
    /// plans once and runs the plan in several places. What travels is still the plan, so a
    /// node answering a fanned-out SQL statement never sees SQL - and cannot disagree with the
    /// coordinator about what was asked.
    pub fn plan_sql(&self, text: &str) -> Result<(Vec<Plan>, Vec<SqlProbe>, Answer)> {
        self.plan_sql_in(text, big_sql::DEFAULT_DATABASE)
    }

    /// The same, against the database an unqualified name in this request means.
    pub fn plan_sql_in(
        &self,
        text: &str,
        database: &str,
    ) -> Result<(Vec<Plan>, Vec<SqlProbe>, Answer)> {
        // Through `Api::translate_in` and not `big_sql`'s, so a statement reading a view is
        // expanded here too. The un-clustered path and the coordinator's have to answer the
        // same question, and going straight to `big-sql` is how they would stop doing that.
        match self.translate_in(text, database)? {
            big_sql::Sql::Query(s) => self.plan_statement(s),
            // The three statements that are not questions have no plan to resolve: a schema
            // change goes to the leader, an insert goes to the shard owners, and a listing is
            // already in this node's catalog. Reachable only through the un-clustered path; a
            // coordinator classifies first - see `Api::translate`.
            big_sql::Sql::Ddl(_)
            | big_sql::Sql::Insert(_)
            | big_sql::Sql::Show(_)
            | big_sql::Sql::Acl(_) => Err(ApiError::Sql(big_sql::SqlError::Refused {
                what: big_sql::Refused::Write,
                at: 0,
            })),
            // ...and an `EXPLAIN`, which is a statement *about* a statement: there is no plan
            // for this to answer with, because the whole of what it asks for is that nothing
            // runs. The surface that answers one is `Cluster::sql`, which builds rows.
            //
            // **Its own refusal, not the one above.** That one says this surface does not
            // write, a sentence which is simply untrue of `EXPLAIN SELECT count(*) FROM t` -
            // and a caller who reads it is told the wrong thing about a statement that is
            // fine. What is refused here is the question, not the text.
            big_sql::Sql::Explain { .. } => Err(ApiError::Sql(big_sql::SqlError::Refused {
                what: big_sql::Refused::ExplainRows,
                at: 0,
            })),
        }
    }

    /// `DESCRIBE t`: this node's fields for one table, as rows - or, for a view, the columns it
    /// exposes.
    ///
    /// Over the snapshot rather than the catalog directly, which is what makes the listings
    /// pure functions - and testable with neither a pager nor a socket.
    pub fn describe(&self, table: &str) -> Result<ResultSet> {
        introspect::describe(&self.schema(), &self.views(), table)
    }

    /// `SHOW TABLES`: every table and view this node holds.
    pub fn show_tables(&self) -> ResultSet {
        introspect::show_tables(&self.schema(), &self.views(), None)
    }

    /// `SHOW VIEWS`: every view this node holds, with the statement each carries.
    pub fn show_views(&self) -> ResultSet {
        introspect::show_views(&self.views(), None)
    }

    /// `SHOW CREATE [TABLE | VIEW] t`: the statement that would recreate one object.
    pub fn show_create(&self, table: &str) -> Result<ResultSet> {
        introspect::show_create(&self.schema(), &self.views(), table, false)
    }

    /// Parses and translates a statement without resolving it against the schema.
    ///
    /// The door a coordinator uses, because what to *do* with a statement depends on which kind
    /// it is: a query is planned here and fanned out, a schema change goes to the leader. Doing
    /// it in one place means neither caller decides that by looking at the text.
    /// **Three steps rather than one call, because the middle one needs a catalog.** `big-sql`
    /// parses `FROM v` as an ordinary source and never learns what a view is - that is what
    /// keeps it schema-free. Substituting the statement a view holds happens here, on the parse
    /// tree, between filling in the request's database and lowering. See [`crate::views`].
    pub fn translate_in(&self, text: &str, database: &str) -> Result<big_sql::Sql> {
        let mut parsed = big_sql::parse(text)?;
        // Qualified first: an expansion looks a source up by `(database, name)`, and a name
        // still carrying `None` would be looked up in the wrong one.
        big_sql::qualify(&mut parsed, database);
        views::expand(&mut parsed, &self.db.catalog())?;
        Ok(big_sql::finish(parsed)?)
    }

    /// The same, against the default database. See [`Api::translate_in`] for the request-scoped
    /// form, which is what an edge with a `?database=` uses.
    pub fn translate(&self, text: &str) -> Result<big_sql::Sql> {
        self.translate_in(text, big_sql::DEFAULT_DATABASE)
    }

    /// Resolves an already-translated query. See [`Api::plan_sql`].
    pub fn plan_statement(
        &self,
        statement: SqlStatement,
    ) -> Result<(Vec<Plan>, Vec<SqlProbe>, Answer)> {
        let catalog = self.db.catalog();
        let schema = big_exec::CatalogSchema(&catalog);
        let sql = |e: big_plan::PlanError| ApiError::Sql(big_sql::SqlError::Plan(e));
        // Every call is resolved before any of them runs, so a statement whose second aggregate
        // names a field that is not there is refused whole rather than half-answered.
        let plans = statement
            .calls
            .iter()
            .map(|ask| big_plan::plan(&ask.table, &ask.call, &schema))
            .collect::<core::result::Result<Vec<Plan>, _>>()
            .map_err(sql)?;
        // The shape's own trip through the schema, and the only part of it that needs one: a
        // `HAVING sum(price) >= 100.00` compares against a number the field stores in units,
        // and the conversion is the planner's, not a second copy of it. Done here so that what
        // travels to a peer and what renders the answer are both already resolved.
        let answer = Answer {
            shape: statement.answer.shape.resolve(&schema).map_err(sql)?,
            format: statement.answer.format,
            calls: statement.answer.calls,
        };
        Ok((plans, statement.probes, answer))
    }

    /// Resolves query text against this node's schema, without running it.
    ///
    /// Pure, and cheap: `big-plan` links no pager, so a query that will not type-check is
    /// refused here rather than after a network round trip. It is the first half of what
    /// [`Api::query_with`] does, split out for a coordinator that has to plan once and run the
    /// plan in several places.
    pub fn plan(&self, table: &str, text: &str) -> Result<Plan> {
        // A planning failure is a query failure: it travels as one so that a client sees the
        // same code whether the query was refused here or one layer down.
        let plan = |e: big_plan::PlanError| ApiError::Query(e.into());
        let call = big_plan::parse(text).map_err(plan)?;
        let catalog = self.db.catalog();
        big_plan::plan(table, &call, &big_exec::CatalogSchema(&catalog)).map_err(plan)
    }

    /// Resolves a call somebody else built, which is what a probe's every step needs.
    pub fn plan_call(&self, table: &str, call: &big_plan::ast::Call) -> Result<Plan> {
        let catalog = self.db.catalog();
        big_plan::plan(table, call, &big_exec::CatalogSchema(&catalog))
            .map_err(|e| ApiError::Query(e.into()))
    }

    /// The row a key already means here, or `None` if this node has never been told.
    ///
    /// The read side of the cache the schema leader fills. Row ids are immutable and never
    /// reused, so a hit can never be stale - a miss is a round trip, not a wrong answer.
    pub fn key_row(&self, table: &str, field: &str, key: &str) -> Option<RowId> {
        self.db.read().key_row(table, field, key)
    }

    /// Runs a plan somebody else made, inside one read transaction.
    ///
    /// [`Api::query_with`] is this plus a parse. The split exists for a coordinator: it plans
    /// once, against its own schema, and sends the *plan* to every shard owner. Re-parsing the
    /// text per node would let two nodes disagree about what was asked, which is the class of
    /// bug that cannot be seen in a result.
    pub fn execute(&self, plan: &Plan, opts: &QueryOptions) -> Result<Value> {
        let mut read = self.db.read();
        if let Some(limits) = opts.limits {
            read = read.with_limits(limits);
        }
        if let Some(timeout) = opts.timeout {
            read = read.with_deadline(timeout);
        }
        if let Some(cancel) = &opts.cancel {
            read = read.with_cancel(Arc::clone(cancel));
        }
        Ok(big_exec::execute(&read, plan)?)
    }

    // ----------------------------------------------------------------------------------
    // Repair
    //
    // What one copy of a range needs in order to become the same database as another. Not a
    // general-purpose surface: it exists because a copy that missed a write has to be able to
    // catch up without being stopped, and every one of these is addressed by name so that two
    // nodes never have to agree about an id.
    // ----------------------------------------------------------------------------------

    /// Every fragment of a table, with the count that stands in for its contents.
    pub fn fragments(&self, table: &str) -> Result<Vec<(FragmentAddr, u64)>> {
        Ok(self.db.read().fragments(table)?)
    }

    /// One fragment's contents, and the zone map that has to travel with them.
    ///
    /// Which units come back is decided by the address, not by the caller: a segment answers in
    /// cells and everything else in containers.
    pub fn fragment(&self, addr: &FragmentAddr) -> Result<(Option<FragmentMeta>, FragmentData)> {
        let read = self.db.read();
        let data = if addr.view_id == big_db::COLUMN_VIEW && addr.view.is_none() {
            FragmentData::Cells(read.segment_cells(addr)?)
        } else {
            FragmentData::Containers(read.fragment_containers(addr)?)
        };
        Ok((read.fragment_meta(addr), data))
    }

    /// Replaces one fragment outright, in its own transaction.
    ///
    /// A copy rather than a merge, because the node this came from is the one that serves the
    /// range and is therefore the truth. See [`big_db::DbWrite::replace_fragment`].
    pub fn replace_fragment(
        &self,
        addr: &FragmentAddr,
        meta: FragmentMeta,
        data: &FragmentData,
    ) -> Result<()> {
        let mut w = self.db.write();
        match data {
            FragmentData::Containers(c) => w.replace_fragment(addr, meta, c)?,
            FragmentData::Cells(c) => w.replace_segment(addr, meta, c)?,
        }
        w.commit()?;
        Ok(())
    }

    /// Every row key of a table, for telling a copy what it means.
    pub fn row_keys(&self, table: &str) -> Result<Vec<(String, String, RowId)>> {
        Ok(self.db.read().row_keys(table)?)
    }

    /// Records what a batch of keys mean, in one transaction, without writing any facts.
    pub fn assign_keys(&self, table: &str, keys: &[KeyAssignment<'_>]) -> Result<()> {
        let mut w = self.db.write();
        for k in keys {
            w.assign_key(table, k.field, k.key, k.row)?;
        }
        w.commit()?;
        Ok(())
    }

    /// How many records a table holds.
    ///
    /// Reachable through `query` as `Count(All())` and routed to the same place, so this is a
    /// convenience rather than a capability. It is here because "how big is this table" is a
    /// question an embedding caller asks without wanting to compose a query for it, and
    /// because a name is what makes it discoverable.
    pub fn count(&self, table: &str) -> Result<u64> {
        Ok(self.db.read().count_all(table)?)
    }

    /// A page of record ids from a table, ascending, resuming strictly after `after`.
    ///
    /// The cursor, and there is no cursor object: the last id of a page is everything the next
    /// page needs, so nothing is held between calls and a client that walks away mid-scan costs
    /// nothing. `after` of `u64::MAX` saturates rather than wrapping - it is a legal record id
    /// whose successor is not.
    ///
    /// Reads a shard at a time and stops at the first one that fills the page, so the memory
    /// this costs is bounded by a shard rather than by the size of the table.
    pub fn records(
        &self,
        table: &str,
        after: Option<RecordId>,
        limit: usize,
    ) -> Result<Vec<RecordId>> {
        let from = after.map_or(0, |a| a.saturating_add(1));
        Ok(self.db.read().scan_records(table, from, limit)?)
    }

    /// The highest record id this node holds for a table, or `None` when it holds none.
    ///
    /// One shard's work rather than the table's - see `DbRead::max_record`. This is what a
    /// record id is allocated above, and in a cluster it is one owner's share of the answer.
    pub fn max_record(&self, table: &str) -> Result<Option<RecordId>> {
        Ok(self.db.read().max_record(table)?)
    }

    /// What a commit currently promises.
    pub fn durability(&self) -> Durability {
        self.db.durability()
    }

    /// Changes what a commit promises, from now on.
    ///
    /// The caller this exists for is a bulk load: relax, load, tighten. Tightening flushes
    /// everything written under the looser setting before it returns, so the pair brackets the
    /// load rather than leaving a hole at the end of it.
    pub fn set_durability(&self, next: Durability) -> Result<()> {
        Ok(self.db.set_durability(next)?)
    }

    /// What the pager knows about its own file: free pages, the reclaim horizon, live readers.
    ///
    /// Exposed here rather than left to `db()` because an operator reading metrics is not
    /// "reaching for something the facade does not offer yet" - it is one of the things a
    /// facade is for, and routing it through the escape hatch would have made every metrics
    /// scrape a reason to keep the escape hatch.
    pub fn metrics(&self) -> Metrics {
        self.db.store().metrics()
    }

    /// What the row-key dictionary costs this process. See [`Db::key_stats`].
    pub fn key_stats(&self) -> KeyStats {
        self.db.key_stats()
    }

    /// Caps how many row keys this database will invent. See [`Db::set_key_limit`].
    pub fn set_key_limit(&self, limit: Option<usize>) {
        self.db.set_key_limit(limit)
    }

    /// Writes a consistent, compact copy of this node's file to `path`, **while serving**.
    ///
    /// The walk holds a read transaction from start to finish, so what lands is one
    /// transaction's worth of state and nothing half-written. Writers keep committing
    /// throughout; they simply cannot reuse a page the walk can still see, which shows up as
    /// `pages_pending_reclaim_reader` climbing for the duration and falling back afterwards.
    ///
    /// **This is one node's file, not a cluster.** A fanned-out database is backed up by
    /// calling this on every node, and the results are not a cluster-wide snapshot: each was
    /// taken at that node's own transaction. Nothing here can offer more than that, for the
    /// same reason a fanned-out read cannot - see `docs/clustering.md`.
    ///
    /// The destination must not exist. A backup that can overwrite is a backup that can destroy
    /// the previous one.
    #[cfg(unix)]
    pub fn backup_to(&self, path: impl AsRef<std::path::Path>) -> Result<Backup> {
        let before = self.metrics();
        // The destination reserves what the source reserved rather than the default terabyte:
        // the live data cannot be larger than the file it came out of, and an operator who
        // chose a ceiling meant it for this process rather than for this call.
        let mapsize = self
            .db
            .store()
            .pager()
            .capacity()
            .map_or(DEFAULT_MAPSIZE, |pages| pages * big_pager::PAGE_SIZE as u64);
        self.db.backup_to_sized(path, mapsize)?;
        Ok(Backup { txn_id: before.txn_id, pages: before.page_count })
    }

    /// The database underneath, for callers that need something this facade does not offer yet.
    ///
    /// Present so that a missing method is an inconvenience rather than a wall, and narrow
    /// enough that reaching for it is visible in review.
    ///
    /// **Behind the `unstable` feature, and it has no callers.** `Db` belongs to a crate that
    /// is not published, so every one of its methods and every type in their signatures was
    /// part of this crate's public surface by accident - a semver commitment on the whole
    /// engine, made by a single accessor nobody was using. The feature keeps the escape hatch
    /// for whoever eventually needs it and makes reaching for it an explicit opt-out of the
    /// version guarantee rather than the default.
    #[cfg(feature = "unstable")]
    pub fn db(&self) -> &Db<P> {
        &self.db
    }
}

/// Applies a batch of facts to an open transaction, resolving each field it names **once**.
///
/// A `Fact` carries its field as a name, and the setters that take a name resolve it - two
/// catalog lookups and a clone of a `FieldDef`, which owns a `String` and a `Vec` - on every
/// call. A batch is the unit of this API precisely because it is large, so that was two
/// allocations per fact to re-learn something fixed for the length of the transaction. A batch
/// names a handful of fields, so the cache below is a handful of entries scanned linearly:
/// comparing a few short names beats hashing them, and beats resolving them by a wide margin.
/// One field of the batch, resolved once, with what it has been told so far.
struct Resolved<'f> {
    /// The name as the *first* fact of this field spelled it, kept so the next fact can be
    /// matched against it. See [`same_slice`].
    name: &'f str,
    at: At,
    /// Row ids for the keys this batch has named at this field, resolved on first use.
    ///
    /// Interning is idempotent: a key already known is a scope lookup and a string comparison
    /// that return the id they returned last time, and a load names the same few hundred keys
    /// over and over.
    ///
    /// **Per field, not one map keyed by `(field, key)`.** The flat version was measured
    /// hashing the field's index with SipHash at **1.25% of the daemon** - more than it spent
    /// hashing the key string beside it, to distinguish four fields. Nesting the map is the
    /// same lookup with that half of the key already answered by which map is being asked.
    ///
    /// **A map, and it has to be a map.** This was a `Vec` scanned linearly, on the reasoning
    /// that a keyed field draws from a small alphabet. It does - and "small" was 256 categories,
    /// so every fact compared its key against up to 256 strings. A profile put
    /// `__memcmp_avx2_movbe` at **38% of the daemon** and the stacks led here.
    rows: std::collections::HashMap<&'f str, RowId>,
}

/// Whether two `&str` are literally the same slice: one address, one length.
///
/// **A pointer comparison standing in for a string comparison, and it is allowed to say no when
/// the answer is yes.** Every caller below falls back to a real comparison when this misses, so
/// the only thing it can cost is the two instructions it took to ask.
///
/// It hits because of where the names come from. `/import` resolves each line's field against
/// the schema and hands the fact the schema's *own* copy of the name, so every fact naming
/// `amount` carries the identical slice - and matching a fact to its resolved field becomes an
/// address compare instead of `__memcmp_avx2_movbe`, which a call graph put at 2% of the daemon
/// doing nothing else.
fn same_slice(a: &str, b: &str) -> bool {
    a.as_ptr() == b.as_ptr() && a.len() == b.len()
}

fn apply<'f, P: big_pager::PagerMut>(
    w: &mut big_db::DbWrite<'_, P>,
    table: &str,
    facts: &[Fact<'f>],
) -> Result<()> {
    let mut at: Vec<Resolved<'f>> = Vec::new();
    for fact in facts {
        let name = fact.field();
        let i = match at.iter().position(|r| same_slice(r.name, name)) {
            Some(i) => i,
            // Either a caller that spells the name afresh per fact, or a field not seen yet.
            // Both are answered by comparing the strings; only the second resolves anything.
            None => match at.iter().position(|r| r.name == name) {
                Some(i) => i,
                None => {
                    let resolved = Resolved {
                        name,
                        at: w.at(table, name)?,
                        rows: std::collections::HashMap::new(),
                    };
                    at.push(resolved);
                    at.len() - 1
                }
            },
        };
        match fact {
            Fact::Int { record, value, .. } => w.set_int_at(&at[i].at, *record, *value)?,
            Fact::Signed { record, value, .. } => w.set_signed_at(&at[i].at, *record, *value)?,
            Fact::Float { record, bits, .. } => {
                w.set_float_at(&at[i].at, *record, f64::from_bits(*bits))?
            }
            Fact::Bool { record, value, .. } => w.set_bool_at(&at[i].at, *record, *value)?,
            Fact::Key { record, value, .. } => {
                // `copied()` ends the borrow of the cache before the miss goes on to write to
                // it, which is what lets the lookup and the insert share one `at[i]`.
                let row = match at[i].rows.get(*value).copied() {
                    Some(row) => row,
                    None => {
                        let row = w.intern_key_at(&at[i].at, value)?;
                        at[i].rows.insert(value, row);
                        row
                    }
                };
                w.set_row_at(&at[i].at, *record, row)?;
            }
            Fact::Time { record, value, unix_seconds, .. } => {
                w.set_time_at(&at[i].at, *record, value, *unix_seconds)?;
            }
        }
    }
    Ok(())
}

impl<'a> Fact<'a> {
    /// The field this fact is about, whatever kind it is.
    ///
    /// Tied to the batch rather than to the borrow of this one fact, which is what lets a caller
    /// hold the name while it goes on reading the batch - `apply` keeps one per distinct field.
    pub fn field(&self) -> &'a str {
        match self {
            Fact::Int { field, .. }
            | Fact::Signed { field, .. }
            | Fact::Float { field, .. }
            | Fact::Bool { field, .. }
            | Fact::Key { field, .. }
            | Fact::Time { field, .. } => field,
        }
    }
}
