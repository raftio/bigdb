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

//! The one statement on this surface that changes the schema rather than reading it.
//!
//! # Why this is a variant and not a `Statement` with no calls
//!
//! Everything else `big-sql` emits is a query-language call that a planner resolves, fans out
//! and merges. A schema change is none of those: it is decided at one node, applied everywhere,
//! and answered with a number rather than a result set. Folding it into [`crate::Statement`]
//! would mean every caller of a statement having to ask whether this one was really a query.
//!
//! # Why the engine is a string here
//!
//! For the same reason this crate never asks whether a column exists: it links no storage crate,
//! and an engine name it has never heard of is the layer above's business. Translating is about
//! *shape*, and validating a name against the engines a build actually has would put a second
//! copy of that list here to drift from the first.

/// A schema change written in SQL.
///
/// Deliberately one variant. `DROP`, `ALTER` and column definitions are still refused by name -
/// see [`crate::Refused::Write`] - because each is a design decision of its own and a surface
/// that grew them by accident would be a surface nobody chose.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Ddl {
    /// `CREATE TABLE <name> [ENGINE = <engine>]`.
    ///
    /// No column list. Fields are declared separately here, exactly as they are over HTTP and in
    /// `bigc` - a table and its fields are two statements because they are two requests, and
    /// inventing a column syntax would mean inventing a mapping from SQL types onto field kinds
    /// that have no SQL analogue at all: a set, a mutex, a time quantum.
    CreateTable {
        table: String,
        /// `None` when the statement did not say, which means the server's default rather than
        /// any particular engine.
        engine: Option<String>,
    },
}
