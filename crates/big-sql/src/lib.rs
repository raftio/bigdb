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

//! SQL for a bitmap engine, which is to say a translation into the query language it already
//! has.
//!
//! [`translate`] turns one `SELECT` into [`big_plan::ast::Call`]s — the same AST the query
//! language's own parser produces — and a [`Shape`] saying how their answers become columns and
//! rows. The planner then resolves each call exactly as it resolves text somebody typed: same
//! schema lookup, same type rules, same error codes, same `Plan`.
//!
//! **This crate cannot express anything the query language cannot, because the only thing it
//! can produce is the query language.** That is not a discipline anyone has to keep; it is the
//! type of [`Statement::calls`]. It is also the answer to the objection that a SQL surface on an
//! engine with no joins promises joins: what a statement is allowed to say is bounded by what
//! the planner will resolve, and everything outside that boundary is refused by name, before
//! anything runs, with a sentence saying what exists instead. See [`Refused`].
//!
//! A statement usually means one call. It means several when the select list asks several
//! questions of the same records — `SELECT count(*), sum(amount)` — and each of them is planned,
//! fanned out and merged exactly as if it had been written alone. The list is what the statement
//! costs in round trips, which is why it is capped at [`MAX_CALLS`] rather than left open.
//!
//! ```
//! # use big_sql::{translate, Cell, Of, Shape, Sql};
//! let Sql::Query(s) = translate("SELECT count(*) FROM tx WHERE amount >= 500").unwrap() else {
//!     panic!("a SELECT is a query")
//! };
//! assert_eq!(s.calls[0].table, "tx");
//! assert_eq!(s.answer.shape, Shape::row(vec![Cell::plain("count", Of::Value { plan: 0 })]));
//! // The call is `Count(Row(amount >= 500))`, which is what a user would have written.
//! assert_eq!(s.calls[0].call.name, "Count");
//! ```
//!
//! No schema is needed to translate, so every test in this crate runs at the speed of a parser
//! test: no file, no mapping, no pager. Whether `amount` exists is the planner's question and
//! is asked one layer up, which is what keeps "this is not a statement", "this engine does not
//! answer that" and "there is no such field" from arriving as the same error.

#![deny(unsafe_code)]

pub mod acl;
pub mod ast;
pub mod ddl;
pub mod delete;
pub mod error;
pub mod explain;
pub mod insert;
pub mod lex;
pub mod lower;
pub mod parse;
pub mod render;
pub mod scalar;
pub mod settings;
pub mod shape;
pub mod show;
pub mod update;

pub use acl::{Acl, AclObject};
pub use ast::{ExplainMode, Query, Select};
// The privilege vocabulary, re-exported so a caller reading `Sql::demands` can name what it
// hands back without depending on `big-rbac` directly.
pub use big_rbac::{Demand, Object, ObjectRef, Privilege, Privileges};
pub use ddl::{Alter, Column, ColumnKind, Ddl, MAX_VIEW_DEPTH};
pub use delete::Delete;
pub use error::{Refused, Result, SqlError};
pub use insert::{Insert, MAX_INSERT_ROWS, RECORD_COLUMN};
pub use lower::{lower, Ask, Probe, Statement, MAX_CALLS};
pub use parse::{parse, Parsed};
pub use scalar::{BinOp, Func, Scalar, UnOp};
pub use settings::Settings;
pub use shape::{
    Absent, Answer, Cell, Columns, Cut, Format, GroupOrder, Having, JoinSide, Keying, Of, Operand,
    OrderBy, Pairing, Selected, Shape, Threshold, Units,
};
pub use show::{Show, Shown, SystemView};
pub use update::Update;

/// One statement, translated.
///
/// Six variants because they are six different things downstream, and the differences are not
/// cosmetic:
///
/// | | what it is | what it demands |
/// |---|---|---|
/// | `Query` | calls a planner resolves and a coordinator fans out | `SELECT` on each table read |
/// | `Insert` | literals for the layer that holds a schema to turn into facts | `INSERT`, and `SELECT` when the values come from a query |
/// | `Show` | a question the catalog already holds the answer to | `SELECT` on the object named, or nothing for a listing |
/// | `Ddl` | a change decided at one node and applied everywhere | `CREATE`, `ALTER` or `DROP` on what it names |
/// | `Acl` | a change to who may do what, replicated the same way | `ROLES` on the server |
/// | `Explain` | a description of one of the five above, run nowhere | whatever it wraps |
///
/// That last column is why this is an enum rather than a `Statement` with more fields, and it is
/// [`Sql::demands`] rather than prose: a caller asks the statement what it needs instead of
/// writing a seventh match over these variants, and a further kind of statement cannot be added
/// without that one match failing to compile.
// The variants are far apart in size, and boxing the large one would be the wrong trade: a
// `Sql` exists once per statement and is destructured immediately, so the allocation would be
// pure cost against 320 bytes of stack that the caller was going to hold anyway. `Explain` is
// boxed for the one reason that is not a trade - a variant holding its own enum has no size
// without it - and pays for that allocation only when somebody writes `EXPLAIN`.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Sql {
    Query(Statement),
    Insert(Insert),
    Show(Show),
    Ddl(Ddl),
    /// `GRANT`, `REVOKE`, `CREATE ROLE`, `DROP ROLE`.
    ///
    /// Nothing is lowered: an ACL statement names no column and reads no table, so there is no
    /// plan to make. It arrives at the executor as it was written.
    Acl(Acl),
    /// `DELETE FROM t WHERE ...`: the records a condition selects, cleared from every field.
    ///
    /// **A write whose first half is a read**, which is why it is not an `Insert` and not a
    /// `Ddl`. The call it carries is resolved and fanned out exactly as a `SELECT`'s is - the
    /// coordinator even resolves an `IN (SELECT ...)` inside it the same way - and only then does
    /// anything get cleared. See [`crate::delete`] for what that costs and what it does not
    /// promise.
    Delete(Delete),
    /// `UPDATE t SET c = v WHERE ...`: a new fact written where an old one was.
    ///
    /// **A write whose first half is a read**, like [`Sql::Delete`], and it demands *both*
    /// `Insert` and `Delete`: writing the new fact and clearing the old is what an update is
    /// here, not an implementation detail of one. See [`crate::update`] for which columns it
    /// works on and why the others are refused rather than emulated.
    Update(Update),
    /// `KILL QUERY '<id>'`: one running query, named and stopped.
    ///
    /// **Answered at the edge rather than by the cluster**, because the flag lives where it is
    /// minted - one per connection, in the server - and a registry any lower would be a map the
    /// layer that makes the flag has to reach down into. That is the one place this surface
    /// answers a statement outside `Cluster::run`, and it is deliberate.
    Kill(String),
    /// `EXPLAIN <statement>`: what the statement would do, having done none of it.
    ///
    /// **A wrapper rather than a sixth kind of work.** The five above say what a statement
    /// does; this one does none of it - no plan is run, no fact is written, no schema is
    /// changed. What it *demands of the caller* is a separate question, and the answer is not
    /// "a read": see [`Sql::demands`], which looks inside.
    Explain {
        /// Which half was asked for.
        mode: ExplainMode,
        /// The statement being described, which is never itself an `EXPLAIN`.
        inner: Box<Sql>,
    },
    /// `<statement> SETTINGS max_execution_time = 30`: what this statement may spend.
    ///
    /// **A wrapper, and not a seventh kind of work.** It changes nothing about what the
    /// statement is or what it answers - only the budget the executor gives it, which is why
    /// every match over `Sql` delegates through it rather than deciding anything of its own.
    /// What it demands is the inner statement's demands: a limit is not an authority.
    Settings {
        /// The limits written on the statement, never empty.
        settings: Settings,
        /// The statement being bounded.
        inner: Box<Sql>,
    },
}

impl Sql {
    /// Every privilege this statement needs, on every object it needs one for.
    ///
    /// **An `AND`, never an `OR`.** A join over two tables needs `Select` on both, and holding it
    /// on one is not most of the way to being allowed. The caller checks all of them or refuses.
    ///
    /// Deliberately here, next to the variants, rather than at the edge that enforces it: an
    /// edge deciding what a statement costs by matching on the AST is how the rule ends up
    /// written twice and enforced once. The match below is exhaustive, so a statement kind added
    /// later cannot be added without answering for what it demands.
    ///
    /// Borrowed from the statement rather than owned, because a demand is made and answered
    /// within one statement and never stored - owning the names would allocate twice per table
    /// on the path that runs once per query.
    ///
    /// **What a listing does not demand.** `SHOW DATABASES` and `SHOW TABLES` demand nothing:
    /// they answer with names, the route floor has already established that the caller is
    /// somebody, and filtering a listing down to what the reader may query is a feature this
    /// surface does not have yet. When it does, it belongs here. `SHOW GRANTS FOR <role>` is the
    /// exception, because reading somebody else's privileges is an administrative act.
    ///
    /// The recursion through `EXPLAIN` terminates because the parser refuses `EXPLAIN EXPLAIN`,
    /// and would still terminate on a nesting some other build wrote: each step unwraps one
    /// `Box`.
    #[must_use]
    pub fn demands(&self) -> Vec<Demand<'_>> {
        let mut out = Vec::new();
        self.collect_demands(&mut out);
        // The same table named twice is one demand: `gates.rs` compares an `EXPLAIN` against the
        // statement it wraps, so the order and the repeats have to be a property of the
        // statement rather than of how the walk happened to run.
        out.dedup();
        out
    }

    fn collect_demands<'a>(&'a self, out: &mut Vec<Demand<'a>>) {
        match self {
            Self::Query(statement) => {
                for table in statement.tables() {
                    out.push(Demand::new(Privilege::Select, object_of(table)));
                }
            }
            Self::Insert(insert) => {
                out.push(Demand::new(
                    Privilege::Insert,
                    table_ref(insert.database.as_deref(), &insert.table),
                ));
                // `INSERT ... SELECT` reads before it writes, and the read is a read: without
                // this the refusal would land after the coordinator had already taken a run of
                // record ids from the leader.
                if let insert::Source::Select(select) = &insert.source {
                    out.push(Demand::new(
                        Privilege::Select,
                        table_ref(select.from.database.as_deref(), &select.from.table),
                    ));
                    for join in &select.joins {
                        out.push(Demand::new(
                            Privilege::Select,
                            table_ref(join.source.database.as_deref(), &join.source.table),
                        ));
                    }
                }
            }
            Self::Show(show) => match &show.what {
                Shown::Columns { database, table } | Shown::Create { database, table, .. } => {
                    out.push(Demand::new(Privilege::Select, table_ref(database.as_deref(), table)));
                }
                // Listings of names. See the note above.
                Shown::Tables { .. } | Shown::Views { .. } | Shown::Databases => {}
                // Somebody else's privileges is an administrative question; your own is not.
                Shown::Grants { role: Some(_) } | Shown::Roles => {
                    out.push(Demand::server(Privilege::Roles));
                }
                Shown::Grants { role: None } => {}
                // **Nothing**, which is the same answer `SHOW TABLES` gives and for the same
                // reason: these are listings of names, the route floor has already established
                // that the caller is somebody, and narrowing a listing to what the reader may
                // query is a feature this surface does not have yet. Demanding `SELECT` on every
                // table `system.columns` names is not a stricter version of that - it is a
                // different statement, one nobody could hold the privileges for.
                Shown::System { .. } => {}
                // Somebody else's running statements is an administrative question, and the text
                // of one can name tables the reader may not read. `Operate` is the privilege
                // that guards operating the process, which is what this is a view of.
                Shown::Processlist => out.push(Demand::server(Privilege::Operate)),
            },
            Self::Ddl(ddl) => out.push(ddl_demand(ddl)),
            // Stopping somebody else's query is operating the server, which is what
            // `Privilege::Operate` guards. A rule that let a caller kill *their own* without it
            // cannot be written here: `demands` is static and does not know whose query an id
            // names. The simple rule is the one that can be checked before anything runs.
            Self::Kill(_) => out.push(Demand::server(Privilege::Operate)),
            Self::Update(update) => {
                // **Both**, because both is what it does. An update writes the new fact and
                // clears the old one, so a credential that may add facts but not remove them is
                // not most of the way to being allowed - it is missing half the operation. It
                // also avoids a ninth `Privilege`, which would change `Privilege::ALL` and with
                // it the RBAC bitmask already stored in every catalog.
                let object = table_ref(update.database.as_deref(), &update.table);
                out.push(Demand::new(Privilege::Insert, object));
                out.push(Demand::new(Privilege::Delete, object));
                for table in &update.reads {
                    out.push(Demand::new(Privilege::Select, object_of(table)));
                }
            }
            Self::Delete(delete) => {
                out.push(Demand::new(
                    Privilege::Delete,
                    table_ref(delete.database.as_deref(), &delete.table),
                ));
                // **A read is a read, wherever it appears.** An `IN (SELECT ... FROM other)`
                // inside the filter reads `other` before this table is narrowed by what came
                // back, so without this a caller could learn which ids `other` holds by deleting
                // from a table they may delete from. The same argument `INSERT ... SELECT` makes
                // one variant up.
                for table in &delete.reads {
                    out.push(Demand::new(Privilege::Select, object_of(table)));
                }
            }
            // Every form of it administers roles, which is one privilege held on the server or
            // nowhere - see `Privilege::Roles` for why it does not divide by database.
            Self::Acl(_) => out.push(Demand::server(Privilege::Roles)),
            // Inherited rather than read as a read. `EXPLAIN` runs nothing, but an authority
            // decided from a *wrapper keyword* rather than from what the statement is about is
            // the shape that goes wrong the first time an explained statement has to read
            // something to say anything useful. This way round it fails closed, and it is what
            // ClickHouse does.
            Self::Explain { inner, .. } => inner.collect_demands(out),
            // A budget is not an authority. Narrowing what a statement may spend cannot widen
            // what it may reach, so the demands are exactly the inner statement's.
            Self::Settings { inner, .. } => inner.collect_demands(out),
        }
    }
}

/// The object a `Ddl` is about, and the privilege it needs over it.
fn ddl_demand(ddl: &Ddl) -> Demand<'_> {
    match ddl {
        // A database is not *in* a database, so creating or dropping one is about the server.
        Ddl::CreateDatabase { .. } => Demand::server(Privilege::Create),
        Ddl::DropDatabase { .. } => Demand::server(Privilege::Drop),
        // Creating is granted a level up from the thing created: there is no table yet to hold
        // the privilege, so it is held over the database that will hold the table.
        Ddl::CreateTable { database, .. } | Ddl::CreateView { database, .. } => {
            Demand::new(Privilege::Create, database_ref(database.as_deref()))
        }
        Ddl::AlterTable { database, table, .. } => {
            Demand::new(Privilege::Alter, table_ref(database.as_deref(), table))
        }
        // `Drop` and not `Alter`, because what it costs the caller is the data. `Privilege::Drop`
        // is documented as the one that destroys, and emptying a table destroys exactly as much
        // as dropping it - the declaration that survives is not the part anybody minds losing.
        Ddl::DropTable { database, table, .. } | Ddl::TruncateTable { database, table, .. } => {
            Demand::new(Privilege::Drop, table_ref(database.as_deref(), table))
        }
        Ddl::DropView { database, name, .. } => {
            Demand::new(Privilege::Drop, table_ref(database.as_deref(), name))
        }
    }
}

/// A `db.table` or bare `table` string, as the object it names.
///
/// Split on the first `.`, which is safe because `Refused::ThreePartName` means there is at most
/// one and because a name holding a `.` is refused by the catalog on the way in.
fn object_of(qualified: &str) -> ObjectRef<'_> {
    match qualified.split_once('.') {
        Some((database, table)) => ObjectRef::Table { database, table },
        None => ObjectRef::Table { database: DEFAULT_DATABASE, table: qualified },
    }
}

/// A table whose database may not have been filled in, which means the request was against the
/// default one - `qualify` returns early in exactly that case.
fn table_ref<'a>(database: Option<&'a str>, table: &'a str) -> ObjectRef<'a> {
    ObjectRef::Table { database: database.unwrap_or(DEFAULT_DATABASE), table }
}

/// A whole database, whose name may not have been filled in for the same reason.
fn database_ref(database: Option<&str>) -> ObjectRef<'_> {
    ObjectRef::Database(database.unwrap_or(DEFAULT_DATABASE))
}

/// The database an unqualified name means when the request did not say.
///
/// Kept here rather than imported from `big-db` because this crate links no storage: the two
/// are checked against each other by a test in `big-embed`, which sees both.
pub const DEFAULT_DATABASE: &str = "default";

/// Parses one statement and translates it, which is the whole of this crate's job.
///
/// Only a query is lowered. The other three are already what they mean: a schema change names
/// its columns, an insert carries its literals, and a question about the catalog is a question
/// - none of them has a plan to be turned into, which is why none of them is a `Statement`.
pub fn translate(text: &str) -> Result<Sql> {
    translate_in(text, DEFAULT_DATABASE)
}

/// The same, against the database an unqualified name in this request means.
///
/// **The default is applied here, once, before anything is lowered.** A table name reaches the
/// planner, the shape and the executor as one string, and filling the database in at each of
/// those would be three chances for them to disagree about which table a statement was about.
/// So it happens on the parse tree, and everything downstream sees names that are already
/// whatever they are going to be.
///
/// A name the statement qualified itself is untouched: `sales.orders` in a request against
/// `ops` means `sales`. And a request against the default database changes nothing at all, so
/// the common case still carries the bare name somebody wrote.
pub fn translate_in(text: &str, database: &str) -> Result<Sql> {
    let mut parsed = parse(text)?;
    qualify(&mut parsed, database);
    finish(parsed)
}

/// Fills in the database every name in a parse tree did not carry.
///
/// Public because the step after it is not always [`finish`]. A caller holding a catalog expands
/// views between the two - a view is a name that is not a table, which only a catalog knows -
/// and it has to run against names that are already qualified, or a view in `sales` and a table
/// in `sales` would be looked up under different databases.
///
/// A request against the default database is left exactly as written. Not an optimisation: an
/// unqualified name already resolves in the default database, so filling it in would change
/// every name in the common case to say what it already meant.
///
/// A name the statement qualified itself is untouched: `sales.orders` in a request against `ops`
/// means `sales`.
pub fn qualify(parsed: &mut Parsed, database: &str) {
    if database == DEFAULT_DATABASE {
        return;
    }
    match parsed {
        Parsed::Query(q) => {
            for select in &mut q.branches {
                select.from.database.get_or_insert_with(|| database.to_string());
                for join in &mut select.joins {
                    join.source.database.get_or_insert_with(|| database.to_string());
                }
            }
        }
        Parsed::Insert(i) => {
            i.database.get_or_insert_with(|| database.to_string());
        }
        Parsed::Show(s) => s.what.fill_database(database),
        Parsed::Ddl(d) => d.fill_database(database),
        Parsed::Delete(d) => {
            d.database.get_or_insert_with(|| database.to_string());
        }
        Parsed::Update(u) => {
            u.database.get_or_insert_with(|| database.to_string());
        }
        // An id names a query, not a table, so there is no database to fill in.
        Parsed::Kill(_) => {}
        Parsed::Acl(a) => a.fill_database(database),
        // The names an `EXPLAIN` describes are the inner statement's, and they mean what they
        // would have meant had it been run - so this is the same walk, one level down.
        Parsed::Explain { inner, .. } => qualify(inner, database),
        // The names a bounded statement is about are its own; the clause names no table.
        Parsed::Settings { inner, .. } => qualify(inner, database),
    }
}

/// Lowers a parse tree whose names are already whatever they are going to be.
///
/// The half of [`translate_in`] after [`qualify`], split out so a caller that has to do
/// something in between - expanding a view - runs the same lowering rather than its own.
pub fn finish(parsed: Parsed) -> Result<Sql> {
    Ok(match parsed {
        Parsed::Query(q) => Sql::Query(lower(&q)?),
        Parsed::Insert(i) => Sql::Insert(i),
        Parsed::Show(s) => Sql::Show(s),
        Parsed::Ddl(d) => Sql::Ddl(d),
        Parsed::Delete(d) => Sql::Delete(lower::delete::lower(&d)),
        Parsed::Update(u) => Sql::Update(lower::delete::update(&u)),
        Parsed::Kill(id) => Sql::Kill(id),
        Parsed::Acl(a) => Sql::Acl(a),
        // Lowered exactly as it would have been unwrapped, so what is described is what would
        // have run - including the refusals. `EXPLAIN` of a statement this engine will not
        // answer fails with that statement's own refusal, which is the useful answer.
        Parsed::Explain { mode, inner } => Sql::Explain { mode, inner: Box::new(finish(*inner)?) },
        // Lowered exactly as the unbounded statement would have been, because it is the same
        // statement: what the clause changes is what the executor gives it, not what it asks.
        Parsed::Settings { settings, inner } => {
            let inner = finish(*inner)?;
            // **A key that this statement has nothing to spend on is refused, not ignored.**
            // `max_delete_records` bounds the half of a delete no deadline reaches; on a query
            // there is no such half, so accepting it would be the silent drop `Refused::Setting`
            // exists to prevent - said about a key that is spelled right and means nothing here.
            // The other three bound reads, and a delete does a read, so none of them is refused
            // the other way round.
            if settings.max_delete_records.is_some() && !matches!(inner, Sql::Delete(_)) {
                return Err(SqlError::Refused { what: Refused::Setting, at: 0 });
            }
            Sql::Settings { settings, inner: Box::new(inner) }
        }
    })
}
