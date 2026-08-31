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
//! assert_eq!(s.answer.shape, Shape::Row { cells: vec![Cell {
//!     column: "count".to_string(),
//!     of: Of::Value { plan: 0 },
//! }] });
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
pub mod lex;
pub mod lower;
pub mod parse;
pub mod shape;

pub use ast::{Query, Select};
pub use ddl::Ddl;
pub use error::{Refused, Result, SqlError};
pub use lower::{lower, Ask, Probe, Statement, MAX_CALLS, MAX_PROJECTION};
pub use parse::{parse, Parsed};
pub use shape::{
    Absent, Answer, Cell, Cut, Format, GroupOrder, Having, Of, OrderBy, Pairing, Shape, Threshold,
};

/// One statement, translated.
///
/// Two variants because they are two different things downstream: a query becomes calls a
/// planner resolves and a coordinator fans out, while a schema change is decided at one node,
/// applied everywhere, and answered with a number. A caller that could not tell them apart
/// would have to ask.
// The two variants are far apart in size, and boxing the large one would be the wrong trade: a
// `Sql` exists once per statement and is destructured immediately, so the allocation would be
// pure cost against 320 bytes of stack that the caller was going to hold anyway.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Sql {
    Query(Statement),
    Ddl(Ddl),
}

/// Parses one statement and translates it, which is the whole of this crate's job.
pub fn translate(text: &str) -> Result<Sql> {
    Ok(match parse(text)? {
        Parsed::Query(q) => Sql::Query(lower(&q)?),
        Parsed::Ddl(d) => Sql::Ddl(d),
    })
}
