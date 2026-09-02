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

pub mod ast;
pub mod ddl;
pub mod error;
pub mod explain;
pub mod insert;
pub mod lex;
pub mod lower;
pub mod parse;
pub mod render;
pub mod shape;
pub mod show;

pub use ast::{ExplainMode, Query, Select, TimeOp};
pub use ddl::{Alter, Column, ColumnKind, Ddl, MAX_VIEW_DEPTH};
pub use error::{Refused, Result, SqlError};
pub use insert::{Insert, MAX_INSERT_ROWS, RECORD_COLUMN};
pub use lower::{lower, Ask, Probe, Statement, MAX_CALLS};
pub use parse::{parse, Parsed};
pub use shape::{
    Absent, Answer, Cell, Columns, Cut, Format, GroupOrder, Having, JoinSide, Keying, Of, Operand,
    OrderBy, Pairing, Selected, Shape, Threshold, Units,
};
pub use show::{Show, Shown};

/// One statement, translated.
///
/// Five variants because they are five different things downstream, and the differences are not
/// cosmetic:
///
/// | | what it is | what it costs the caller |
/// |---|---|---|
/// | `Query` | calls a planner resolves and a coordinator fans out | a read |
/// | `Insert` | literals for the layer that holds a schema to turn into facts | a write |
/// | `Show` | a question the catalog already holds the answer to | a read |
/// | `Ddl` | a change decided at one node and applied everywhere | an admin |
/// | `Explain` | a description of one of the four above, run nowhere | whatever it wraps |
///
/// That last column is why this is an enum rather than a `Statement` with more fields, and it is
/// [`Sql::authority`] rather than prose: a caller asks the statement what it costs instead of
/// writing a sixth match over these variants, and a further kind of statement cannot be added
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
    /// `EXPLAIN <statement>`: what the statement would do, having done none of it.
    ///
    /// **A wrapper rather than a fifth kind of work.** The four above say what a statement
    /// does; this one does none of it - no plan is run, no fact is written, no schema is
    /// changed. What it *costs the caller* is a separate question, and the answer is not "a
    /// read": see [`Sql::authority`], which looks inside.
    Explain {
        /// Which half was asked for.
        mode: ExplainMode,
        /// The statement being described, which is never itself an `EXPLAIN`.
        inner: Box<Sql>,
    },
}

/// What a statement demands of whoever sent it.
///
/// **The last column of [`Sql`]'s table, as code rather than as prose.** That column was a
/// promise two other crates were each keeping by hand - the HTTP edge turned a statement into
/// the role a token needs, the un-clustered door turned one into a refusal - and two
/// hand-written copies of one rule are one rule plus the day they disagree. It lives here, in
/// the file where a variant can be added, so adding one fails to compile in exactly one place.
///
/// Deliberately *not* `big-http`'s `Role`. This says what a statement is; a role says what a
/// credential holds. They happen to map one to one, and that mapping is the edge's to write -
/// there is no reason for a dialect crate to learn how a token file is spelled.
///
/// Ordered, weakest first: an authority contains the ones below it, which is what lets an edge
/// compare one against the floor its route already checked.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Authority {
    /// Reads what is already there: a `SELECT`, a `DESCRIBE`, a `SHOW`.
    Read,
    /// Writes facts, and changes no schema.
    Write,
    /// Changes the schema.
    Admin,
}

impl Sql {
    /// What this statement demands of whoever sent it.
    ///
    /// **`EXPLAIN` inherits rather than reading as a read.** It runs nothing, so on the letter
    /// of it an `EXPLAIN CREATE TABLE` is harmless - today it renders a parse tree the caller
    /// already holds and reads no catalog at all. The rule is here anyway, because the
    /// alternative decides an authority from a *wrapper keyword* rather than from what the
    /// statement is about, and that is the shape that goes wrong the first time an explained
    /// statement has to read something to say anything useful. An authority says which class of
    /// statement a credential may name, not only which it may run - and this way round it fails
    /// closed. It is also what ClickHouse does, whose `EXPLAIN` checks the access the explained
    /// query would have needed rather than a permission of its own.
    ///
    /// The recursion terminates because the parser refuses `EXPLAIN EXPLAIN`, and would still
    /// terminate on a nesting some other build wrote: each step unwraps one `Box`.
    #[must_use]
    pub fn authority(&self) -> Authority {
        match self {
            Self::Ddl(_) => Authority::Admin,
            Self::Insert(_) => Authority::Write,
            Self::Explain { inner, .. } => inner.authority(),
            // A `SELECT` and a `DESCRIBE` both read, which is the floor every surface starts at.
            Self::Query(_) | Self::Show(_) => Authority::Read,
        }
    }
}

/// The database an unqualified name means when the request did not say.
///
/// Kept here rather than imported from `big-db` because this crate links no storage: the two
/// are checked against each other by a test in `big-api`, which sees both.
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
        // The names an `EXPLAIN` describes are the inner statement's, and they mean what they
        // would have meant had it been run - so this is the same walk, one level down.
        Parsed::Explain { inner, .. } => qualify(inner, database),
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
        // Lowered exactly as it would have been unwrapped, so what is described is what would
        // have run - including the refusals. `EXPLAIN` of a statement this engine will not
        // answer fails with that statement's own refusal, which is the useful answer.
        Parsed::Explain { mode, inner } => Sql::Explain { mode, inner: Box::new(finish(*inner)?) },
    })
}
