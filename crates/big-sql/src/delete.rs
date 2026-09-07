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

//! `DELETE FROM t WHERE ...`: the records a condition selects, cleared from every field.
//!
//! # Why this lowers to the same call a `SELECT` would
//!
//! A delete here is a *selection* followed by a clear, and the selection is not a special kind of
//! read - it is exactly the read `SELECT * FROM t WHERE ...` performs, lowered by exactly the
//! same `lower::cond::rows`. That is the whole design: there is one place a `WHERE`
//! becomes a set operation, so a predicate cannot come to select one set when it is being counted
//! and a different set when it is being deleted. The corpus pins that by writing both spellings
//! and expecting one tree.
//!
//! # What it is bounded by, and what it is not
//!
//! The **selection** is a read and is bounded like every other one: it runs through `DbRead`, so
//! `SETTINGS max_execution_time = 30` really stops it and `checkpoint` really aborts it.
//!
//! The **clearing** is one transaction and nothing interrupts one - `QueryOptions` says so in as
//! many words, and it is why there is no deadline for a write. So the bound on that half is a
//! *count*, taken from the merged set before a single bit is cleared: a popcount, known before any
//! id is materialised. See `big_embed::MAX_DELETE`.
//!
//! # The gap this does not close
//!
//! Selecting and clearing are two steps with no snapshot between them. A record written after the
//! merge and before the clear survives; one deleted concurrently was never counted. That is the
//! same non-atomicity `INSERT ... SELECT` already lives with and which
//! [`crate::Refused::InsertSelfRead`] is the published sentence about. It is stated rather than
//! papered over: a delete that quietly claimed to be a snapshot would be worse than one that says
//! it is not.

/// `DELETE FROM t WHERE ...`, lowered.
///
/// The parse-tree half is [`crate::ast::Delete`], and the split is [`crate::Query`] and
/// [`crate::Statement`]'s: what the statement *says* carries a `Cond` and byte offsets, and what
/// it *means* carries the query language and nothing else.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Delete {
    /// `sales` in `DELETE FROM sales.orders`. `None` means the request's default database.
    pub database: Option<String>,
    pub table: String,
    /// The records to clear, as the call that selects them.
    ///
    /// A [`big_plan::ast::Call`] rather than a `Plan`, for the reason every other lowering
    /// produces one: this crate holds no schema, so what it can produce is the query language and
    /// the layer with a catalog turns it into a plan. It is also what lets the coordinator resolve
    /// an `IN (SELECT ...)` inside it exactly as it resolves one inside a `SELECT`.
    pub rows: big_plan::ast::Call,
    /// Every *other* table the filter reads, qualified: the ones an `IN (SELECT ... FROM b)`
    /// names.
    ///
    /// Carried rather than recovered by walking [`Delete::rows`], because the walk would be a
    /// second reading of the same fact and this one is about authorisation - a demand computed
    /// by a parser of the query language is a demand that goes wrong quietly. It is collected
    /// from the `Cond` at lowering, where the tables are already named.
    ///
    /// **A read is a read**, so each of these demands `SELECT`. Without it, deleting from a table
    /// you may delete from would be a way to learn which ids another table holds. The same
    /// argument `INSERT ... SELECT` makes.
    pub reads: Vec<String>,
}

impl Delete {
    /// The table this reads and writes, qualified.
    #[must_use]
    pub fn qualified(&self) -> String {
        match &self.database {
            Some(d) => format!("{d}.{}", self.table),
            None => self.table.clone(),
        }
    }
}
