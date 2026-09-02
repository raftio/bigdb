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

//! Messages gathered into one `INSERT`.
//!
//! # The buffer is the statement
//!
//! There is no list of pending messages anywhere in this crate. A message is rendered into the
//! statement text the moment it arrives and is never looked at again, which is what lets
//! [`Value`] borrow its strings and lets the byte ceiling be checked against the thing actually
//! being sent rather than an estimate of it. The cost of the ceiling is one `String` of scratch,
//! reused, because a row has to be rendered before its length is known.
//!
//! # Two ceilings, and the one that usually stops a batch
//!
//! Rows, because `big_sql` refuses a statement past `MAX_INSERT_ROWS`. Bytes, because
//! `big_http` refuses a body past `MAX_BODY`. **Bytes is the one an ordinary batch reaches**,
//! and the row cap is a backstop for rows so narrow that seven megabytes still holds an
//! unreasonable number of them - see [`crate::DEFAULT_MAX_ROWS`].

use crate::error::Error;
use crate::sql;
use crate::value::Value;

/// What happened to a row offered to a batch.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Fit {
    /// It is in the statement.
    Added,
    /// It did not fit, and the batch holds rows - so flushing and offering it again will work.
    Full,
    /// It did not fit an empty batch, so no amount of flushing will help. Carries the rendered
    /// length, which is what the caller reports.
    TooLarge(usize),
}

/// One `INSERT` under construction.
pub(crate) struct Batch {
    /// `INSERT INTO "t" ("a", "b") VALUES ` and then the tuples.
    statement: String,
    /// Where the tuples begin, so a flush can truncate rather than rebuild the prefix.
    prefix: usize,
    columns: usize,
    rows: usize,
    max_rows: usize,
    max_bytes: usize,
    /// One tuple, rendered before it is known whether it fits. Reused across rows.
    scratch: String,
}

impl Batch {
    /// A batch for one table and one fixed list of columns.
    ///
    /// The column list is fixed because a statement has exactly one, and a producer that let it
    /// vary would be quietly starting a new batch every time a message arrived with a different
    /// shape - which is a second batching rule nobody asked for. Two shapes means two producers.
    pub(crate) fn new(
        table: &str,
        columns: &[&str],
        max_rows: usize,
        max_bytes: usize,
    ) -> Result<Self, Error> {
        if columns.is_empty() {
            return Err(Error::Value("a table needs at least one column to write".to_string()));
        }

        let mut statement = String::with_capacity(64 + columns.len() * 16);
        statement.push_str("INSERT INTO ");
        sql::push_table(table, &mut statement)?;
        statement.push_str(" (");
        for (i, column) in columns.iter().enumerate() {
            sql::check_column(column)?;
            if i > 0 {
                statement.push(',');
            }
            sql::push_ident(column, &mut statement)?;
        }
        statement.push_str(") VALUES ");

        let prefix = statement.len();
        if prefix >= max_bytes {
            return Err(Error::Value(format!(
                "the statement's own header is {prefix} bytes, past the {max_bytes} a request \
                 may carry"
            )));
        }

        Ok(Self {
            statement,
            prefix,
            columns: columns.len(),
            rows: 0,
            max_rows,
            max_bytes,
            scratch: String::with_capacity(128),
        })
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.rows == 0
    }

    pub(crate) fn rows(&self) -> usize {
        self.rows
    }

    /// The byte ceiling, so a refusal can quote the number it was measured against.
    pub(crate) fn max_bytes(&self) -> usize {
        self.max_bytes
    }

    /// The statement as it stands, which is only meaningful when the batch holds rows.
    pub(crate) fn statement(&self) -> &str {
        &self.statement
    }

    /// Drop every row, keeping the header.
    pub(crate) fn clear(&mut self) {
        self.statement.truncate(self.prefix);
        self.rows = 0;
    }

    /// Offer one row.
    ///
    /// Rendering happens before the decision because the byte ceiling is about rendered bytes,
    /// and a row whose values are refused is refused here - where the message that carried them
    /// can still be named - rather than at the server, where it would refuse the whole batch.
    pub(crate) fn offer(&mut self, values: &[Value<'_>]) -> Result<Fit, Error> {
        if values.len() != self.columns {
            return Err(Error::Value(format!(
                "a message of {} values for {} columns",
                values.len(),
                self.columns
            )));
        }

        self.scratch.clear();
        self.scratch.push('(');
        for (i, value) in values.iter().enumerate() {
            if i > 0 {
                self.scratch.push(',');
            }
            sql::push_value(*value, &mut self.scratch)?;
        }
        self.scratch.push(')');

        // The comma that would join this tuple to the previous one is part of what has to fit.
        let separator = usize::from(self.rows > 0);
        let needed = self.scratch.len() + separator;

        if self.rows == 0 && self.prefix + needed > self.max_bytes {
            return Ok(Fit::TooLarge(self.scratch.len()));
        }
        if self.rows >= self.max_rows || self.statement.len() + needed > self.max_bytes {
            return Ok(Fit::Full);
        }

        if separator == 1 {
            self.statement.push(',');
        }
        self.statement.push_str(&self.scratch);
        self.rows += 1;
        Ok(Fit::Added)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{DEFAULT_MAX_BYTES, DEFAULT_MAX_ROWS};

    fn batch(max_rows: usize, max_bytes: usize) -> Batch {
        Batch::new("tx", &["amount", "country"], max_rows, max_bytes).unwrap()
    }

    fn row(amount: u64, country: &str) -> [Value<'_>; 2] {
        [Value::Int(amount), Value::Text(country)]
    }

    #[test]
    fn the_header_names_the_table_and_the_columns_and_never_the_record() {
        let b = batch(100_000, 1 << 20);
        assert_eq!(b.statement(), "INSERT INTO \"tx\" (\"amount\",\"country\") VALUES ");
        assert!(
            !b.statement().contains("_record_id"),
            "an allocating statement must not name a record"
        );
    }

    #[test]
    fn rows_are_joined_with_commas_in_the_order_they_arrived() {
        let mut b = batch(100_000, 1 << 20);
        assert_eq!(b.offer(&row(100, "GB")).unwrap(), Fit::Added);
        assert_eq!(b.offer(&row(900, "US")).unwrap(), Fit::Added);
        assert_eq!(
            b.statement(),
            "INSERT INTO \"tx\" (\"amount\",\"country\") VALUES (100,'GB'),(900,'US')"
        );
        assert_eq!(b.rows(), 2);
    }

    #[test]
    fn a_batch_never_exceeds_its_ceiling() {
        // Set just past one row, so the second cannot fit.
        let one = {
            let mut probe = batch(100_000, 1 << 20);
            probe.offer(&row(1, "GB")).unwrap();
            probe.statement().len()
        };
        let mut b = batch(100_000, one + 4);
        assert_eq!(b.offer(&row(1, "GB")).unwrap(), Fit::Added);
        assert_eq!(b.offer(&row(2, "GB")).unwrap(), Fit::Full);
        assert!(b.statement().len() <= b.max_bytes);
    }

    #[test]
    fn a_full_batch_takes_the_row_again_once_it_is_cleared() {
        // Room for exactly one row, so the second fills the batch.
        let one = {
            let mut probe = batch(100_000, 1 << 20);
            probe.offer(&row(1, "GB")).unwrap();
            probe.statement().len()
        };
        let mut b = batch(100_000, one + 4);
        assert_eq!(b.offer(&row(1, "GB")).unwrap(), Fit::Added);
        assert_eq!(b.offer(&row(2, "US")).unwrap(), Fit::Full);
        b.clear();
        assert!(b.is_empty());
        assert_eq!(b.offer(&row(2, "US")).unwrap(), Fit::Added);
        assert_eq!(b.statement(), "INSERT INTO \"tx\" (\"amount\",\"country\") VALUES (2,'US')");
    }

    #[test]
    fn a_row_too_large_for_an_empty_batch_says_so_rather_than_looping() {
        // `Full` on an empty batch would send a producer round the flush-and-retry loop for
        // ever, because flushing an empty batch changes nothing.
        let mut b = batch(100_000, 64);
        let huge = "x".repeat(200);
        match b.offer(&row(1, &huge)).unwrap() {
            Fit::TooLarge(len) => assert!(len > 64, "{len}"),
            other => panic!("expected TooLarge, got {other:?}"),
        }
    }

    #[test]
    fn a_message_of_the_wrong_width_is_refused_at_the_batch() {
        let mut b = batch(100_000, 1 << 20);
        assert!(b.offer(&[Value::Int(1)]).is_err());
        assert!(b.offer(&[Value::Int(1), Value::Text("GB"), Value::Int(2)]).is_err());
    }

    #[test]
    fn a_column_that_names_the_record_id_is_refused_when_the_batch_is_built() {
        assert!(Batch::new("tx", &["amount", "_record_id"], 10, 1 << 20).is_err());
        assert!(Batch::new("tx", &["_RECORD_ID"], 10, 1 << 20).is_err());
    }

    #[test]
    fn a_table_with_no_columns_is_refused() {
        assert!(Batch::new("tx", &[], 10, 1 << 20).is_err());
    }

    /// The byte ceiling is what stops an ordinary batch, and the row cap is the backstop.
    ///
    /// Worth pinning because it decides which knob a person reaches for. With the defaults, a
    /// batch of a number and a two-letter key fills seven megabytes long before it reaches half
    /// a million rows - so raising `max_rows` alone changes nothing.
    #[test]
    fn the_byte_ceiling_is_what_an_ordinary_batch_reaches_first() {
        let mut b = batch(DEFAULT_MAX_ROWS, DEFAULT_MAX_BYTES);
        let mut rows = 0u64;
        loop {
            match b.offer(&row(rows, "GB")).unwrap() {
                Fit::Added => rows += 1,
                Fit::Full => break,
                Fit::TooLarge(n) => panic!("a row of {n} bytes does not fit an empty batch"),
            }
        }
        assert!(
            rows < DEFAULT_MAX_ROWS as u64,
            "the row cap was reached at {rows}, so bytes never bound the batch"
        );
        assert!(b.statement().len() <= DEFAULT_MAX_BYTES);
        // And the batch is a great deal larger than the 8,000 the old ceiling allowed.
        assert!(rows > 100_000, "only {rows} rows fitted in seven megabytes");
    }

    /// The defaults are numbers under the server's own, and this is where that is checked.
    ///
    /// Deliberately asserted against the constants rather than derived from them: `big-sql` and
    /// `big-http` are `[dev-dependencies]` here, so the shipped crate holds its own numbers and
    /// this test is what notices when the server's move. The same arrangement, for the same
    /// reason, as `the_default_chunk_leaves_room_under_the_ceiling` in `big-bin`.
    #[test]
    // Both sides are constants, and that is the point rather than a mistake: this test exists
    // to stop compiling agreeably on the day one of the four numbers moves.
    #[allow(clippy::assertions_on_constants)]
    fn the_defaults_leave_room_under_the_servers_ceilings() {
        assert!(
            DEFAULT_MAX_ROWS < big_sql::MAX_INSERT_ROWS,
            "{DEFAULT_MAX_ROWS} rows is not under the server's {}",
            big_sql::MAX_INSERT_ROWS
        );
        assert!(
            DEFAULT_MAX_BYTES < big_http::MAX_BODY,
            "{DEFAULT_MAX_BYTES} bytes is not under the server's {}",
            big_http::MAX_BODY
        );
    }
}
