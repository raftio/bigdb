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
//! assert_eq!(s.answer.shape, Shape::Row {
//!     cells: vec![Cell::plain("count", Of::Value { plan: 0 })],
//! });
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

pub use ast::{Query, Select};
pub use ddl::{Alter, Column, ColumnKind, Ddl};
pub use error::{Refused, Result, SqlError};
pub use insert::{Insert, MAX_INSERT_ROWS, RECORD_COLUMN};
pub use lower::{lower, Ask, Probe, Statement, MAX_CALLS, MAX_PROJECTION};
pub use parse::{parse, Parsed};
pub use shape::{
    Absent, Answer, Cell, Cut, Format, GroupOrder, Having, JoinSide, Keying, Of, OrderBy, Pairing,
    Selected, Shape, Threshold, Units,
};
pub use show::{Show, Shown};

/// One statement, translated.
///
/// Four variants because they are four different things downstream, and the differences are not
/// cosmetic:
///
/// | | what it is | what it costs the caller |
/// |---|---|---|
/// | `Query` | calls a planner resolves and a coordinator fans out | a read |
/// | `Insert` | literals for the layer that holds a schema to turn into facts | a write |
/// | `Show` | a question the catalog already holds the answer to | a read |
/// | `Ddl` | a change decided at one node and applied everywhere | an admin |
///
/// That last column is why this is an enum rather than a `Statement` with more fields: the edge
/// matches on it to decide which role a statement needs, and a fifth kind of statement cannot
/// be added without every such match failing to compile.
// The variants are far apart in size, and boxing the large one would be the wrong trade: a
// `Sql` exists once per statement and is destructured immediately, so the allocation would be
// pure cost against 320 bytes of stack that the caller was going to hold anyway.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Sql {
    Query(Statement),
    Insert(Insert),
    Show(Show),
    Ddl(Ddl),
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
    // A request against the default database is left exactly as written. Not an optimisation:
    // an unqualified name already resolves in the default database, so filling it in would
    // change every name in the common case to say what it already meant.
    if database != DEFAULT_DATABASE {
        match &mut parsed {
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
        }
    }
    Ok(match parsed {
        Parsed::Query(q) => Sql::Query(lower(&q)?),
        Parsed::Insert(i) => Sql::Insert(i),
        Parsed::Show(s) => Sql::Show(s),
        Parsed::Ddl(d) => Sql::Ddl(d),
    })
}
