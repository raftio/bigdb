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

//! Rows written literally.
//!
//! # Why this is literals and not facts
//!
//! What a written value *means* for a field is the field's kind to decide: `'GB'` is a key to
//! intern on a set field and a mistake on an integer one, `12.50` is 1250 units on a decimal of
//! scale 2, and `now@1750000000` is a key and a moment. Deciding any of that here would be this
//! crate reading a schema, which is the one thing it does not do - so an `Insert` carries the
//! literals exactly as written and `big_embed::fact::from_literal` turns each into a fact
//! against the `FieldInfo` it is for. That is the same function `POST /table/{t}/import` reads
//! its lines with, so the two write paths cannot come to disagree about what a value means.
//!
//! # Why the record id is a column, and what happens when it is not written
//!
//! A fact is a bit at `(row, record)`: the record id is the address it is written to rather than
//! a key generated for it, and `shard_of(record)` is also which node owns it. So it is a column
//! here - `SELECT *` answers with nothing else, and an ETL that already has its own ids writes
//! them straight through.
//!
//! A statement that omits it is answered by allocating one, and **that allocation is not this
//! crate's**: "one past the highest" is a question with one right answer per cluster, and two
//! coordinators computing it independently would compute the same number and write two records
//! into one. The schema leader allocates, exactly as it interns keys, and a leader that cannot
//! be reached stops the statement rather than guessing. See `big_cluster::Cluster::allocate`.

use crate::ast::Select;
use big_plan::Literal;

/// Where the values an `INSERT` writes come from.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Source {
    /// `VALUES (...), (...)`: literals, exactly as written.
    Values(Vec<Vec<Literal>>),
    /// `SELECT <columns> FROM <table> [WHERE ...] [LIMIT n]`: a **projection**, whose values
    /// become the facts.
    ///
    /// # Why only a projection
    ///
    /// Most answers here are numbers *about* a set of records rather than records to copy, and
    /// `SELECT *` answers with record ids because a record has no row of values to read out.
    /// A projection is the one shape that reads stored values back per record - see
    /// `crate::lower::ungrouped` - so it is the one shape there is anything to write. Everything
    /// else earns [`crate::Refused::InsertSelect`], whose sentence says so.
    Select(Box<Select>),
}

/// One `INSERT INTO t (...) VALUES (...)`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Insert {
    /// `sales` in `INSERT INTO sales.orders`. `None` means the request's default database.
    pub database: Option<String>,
    pub table: String,
    /// The columns named, in the order written, including `id`.
    pub columns: Vec<String>,
    /// Which of `columns` is the record id, and `None` when the statement did not name one.
    ///
    /// Found by the parser rather than by every reader. `None` is what makes the layer above
    /// allocate: it is the whole of the difference between the two forms, so nothing downstream
    /// has to search the column list to find out which one it was given.
    pub id_at: Option<usize>,
    /// Where the values come from: literals, or a query that reads them.
    pub source: Source,
}

impl Insert {
    /// The literal rows, which is empty for a statement whose values come from a query.
    ///
    /// A method rather than a field so that the two sources are one type to everything that
    /// only cares about the written form - `EXPLAIN`, the row ceiling, the tests - while the
    /// layer that executes still has to look at [`Insert::source`] and decide.
    pub fn values(&self) -> &[Vec<Literal>] {
        match &self.source {
            Source::Values(rows) => rows,
            Source::Select(_) => &[],
        }
    }

    /// The query the values come from, when they come from one.
    pub fn select(&self) -> Option<&Select> {
        match &self.source {
            Source::Select(s) => Some(s),
            Source::Values(_) => None,
        }
    }

    /// The record one row is about, or `None` when the statement left that to the server.
    ///
    /// Never `Some` of something that is not a whole number: the parser refused any other
    /// literal in that column, which it can judge without a schema, since a record id is a
    /// number on every write path this engine has.
    pub fn record(&self, row: &[Literal]) -> Option<u64> {
        match row[self.id_at?] {
            Literal::Int(n) => Some(n),
            // The parser is the one gate, and it only lets `Int` through.
            _ => unreachable!("a record id is a whole number, refused at the value otherwise"),
        }
    }

    /// The columns of one row that are fields, with their values - which is everything but the
    /// id.
    pub fn facts<'a>(&'a self, row: &'a [Literal]) -> impl Iterator<Item = (&'a str, &'a Literal)> {
        self.columns
            .iter()
            .zip(row)
            .enumerate()
            .filter(|(i, _)| Some(*i) != self.id_at)
            .map(|(_, (name, value))| (name.as_str(), value))
    }

    /// How many facts this statement writes, which is what it costs.
    pub fn fact_count(&self) -> usize {
        self.values().len() * self.field_count()
    }

    /// How many of the columns are fields, which is all of them but the id.
    pub fn field_count(&self) -> usize {
        self.columns.len() - usize::from(self.id_at.is_some())
    }
}

/// How many rows one `INSERT` may carry.
///
/// **A bound on what one statement holds in memory, not a taste in batch sizes.** The text is
/// lexed into tokens and then parsed into literals before the first fact is written, so the
/// rows are resident twice over; `POST /table/{t}/import` is the route for volume, and it holds
/// one line at a time.
///
/// **It is not the bound that usually bites, and it is not meant to be.** `big_http::MAX_BODY`
/// caps a request at 8 MiB, checked against `Content-Length` before a byte is read, so the text
/// this parser is handed is already bounded - and 8 MiB of the smallest rows there are,
/// `(1),(2),(3)...`, is about seven hundred thousand of them. This sits above that on purpose:
/// a count of rows says nothing about what they cost, since ten thousand rows of one small
/// integer and ten thousand rows of eighty columns are the same number and nothing like the
/// same memory. What it is for is the pathological narrow statement, where the byte cap alone
/// still admits a great many rows.
///
/// The number matters, so here it is measured: seven hundred thousand of those smallest rows
/// took one server from 3.4 MB resident to 239 MB, an amplification of about thirty-eight times
/// over the text. `MAX_BODY` bounds that per request *in flight*, so the worker pool multiplies
/// it - which is the sum an operator lowering either number is actually doing.
pub const MAX_INSERT_ROWS: usize = 1_000_000;

/// The column that names the record id.
///
/// **Underscored because `id` belongs to whoever is writing the table.** A record id is the
/// engine's own coordinate rather than a key the data chose, and taking the most natural column
/// name in SQL for it would mean a table could not have an `id` field of its own - which is a
/// name almost every schema wants. So the reserved one is spelled where nothing else will be,
/// and `id` stays an ordinary field name.
///
/// The same name `SELECT *` answers under, because they are the same number.
pub const RECORD_COLUMN: &str = "_record_id";
