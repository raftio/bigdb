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

//! An explanation, as the rows a result set is.
//!
//! **Only the rows.** What an `EXPLAIN` says is [`big_sql::explain::explained`]'s to decide, and
//! deliberately so: it is dialect surface, it changes when the dialect does, and it is checked
//! by the corpus that checks every other thing this dialect prints. This module knows one thing
//! that crate cannot - what a [`ResultSet`] is - and does nothing else.
//!
//! # One row per line, never one cell of many lines
//!
//! A tree is many lines, and a result set is the only shape this surface answers with. Put the
//! tree in one cell and two of the three formats ruin it: TSV escapes the newlines and JSON
//! turns them into `\n`, so what a person reads is one unreadable string. So the tree is *rows*,
//! one line each, under a column called `explain` - which is what ClickHouse answers with, and
//! what makes `bigctl` need no change at all to render one.

use crate::result::{Datum, ResultSet};
use big_sql::ExplainMode;

/// What there is to explain, and one resolved search. Re-exported rather than redefined: a
/// coordinator reaches the SQL surface through this crate and links none of it itself.
pub use big_sql::explain::{Explained, Probed};

/// The one column an explanation answers under.
pub const COLUMN: &str = "explain";

/// One row per line of what [`big_sql::explain::explained`] wrote.
///
/// **The blank-line filter is deliberately redundant.** [`big_sql::explain::explained`] strips
/// blank lines itself, so this one drops nothing today - it is not a check, and reading it as
/// one is how a guarantee ends up believed rather than held. It stays because a blank row is
/// the single defect this surface cannot show: the test corpora end an expected block at the
/// first blank line, so one row of nothing would truncate a case rather than fail it. The
/// assertion that the printers never produce one lives where it can still fail, over arbitrary
/// input in `fuzz/fuzz_targets/parse_sql.rs`.
pub fn result_set(mode: ExplainMode, what: &Explained<'_>) -> ResultSet {
    ResultSet {
        columns: vec![COLUMN.to_string()],
        rows: big_sql::explain::explained(mode, what)
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| vec![Datum::Text(l.to_string())])
            .collect(),
    }
}

/// The `SETTINGS` line, carrying the numbers that will actually be enforced.
///
/// **Effective, not as written, and that is the whole point of printing it.** A statement may
/// only lower what the operator configured, so what a client asked for and what it gets are two
/// different numbers whenever it asked for more. Clamping silently is only defensible because
/// this line exists: `EXPLAIN` is where somebody finds out which of the two won, without having
/// to know what the server was started with.
///
/// Prepended rather than appended, because it is a fact about the whole statement and the tree
/// below it is the answer to one. Every key is printed, including the ones the statement did not
/// write - a limit it inherited bounds it exactly as much as one it chose, and leaving those out
/// would make an inherited ceiling the one thing an explanation hides.
pub fn with_settings(mut set: ResultSet, opts: &crate::QueryOptions) -> ResultSet {
    let limits = opts.limits.unwrap_or_default();
    // Printed exactly, including a zero. `SETTINGS max_execution_time = 0` is a statement that
    // will be refused the moment it reaches a fragment, and rounding that up to `1s` here would
    // make the one line whose job is to say what will be enforced the line that lies about it.
    let seconds = opts.timeout.map_or_else(|| "none".to_string(), |t| format!("{}s", t.as_secs()));
    let line = format!(
        "settings max_execution_time={seconds} max_memory_usage={} max_result_rows={}",
        limits.max_bytes, limits.max_records
    );
    set.rows.insert(0, vec![Datum::Text(line)]);
    set
}
