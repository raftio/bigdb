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

//! `INSERT INTO t (...) VALUES (...)`.

use super::Parser;
use crate::error::{Refused, Result};
use crate::insert::{Insert, MAX_INSERT_ROWS, RECORD_COLUMN};
use crate::lex::Tok;
use big_plan::Literal;

impl Parser<'_> {
    /// `INSERT [INTO] <table> ( <column>, ... ) VALUES ( <literal>, ... ) [, ...]`, with
    /// `INSERT` consumed.
    ///
    /// Every refusal here is judged before a single value is read, and each points at the thing
    /// that caused it: a missing column list at the table name, a missing `id` at the closing
    /// bracket of the list that should have held one, and a `SELECT` at the `SELECT`.
    pub(super) fn insert(&mut self) -> Result<Insert> {
        self.eat_word("INTO");
        let table = self.bare_ident("a table name")?;

        // `INSERT INTO t SELECT ...` is a whole statement's worth of meaning, and answering it
        // with "expected (" would be answering a question nobody asked.
        if self.word_is("SELECT") || self.word_is("WITH") {
            return Err(self.refuse(Refused::InsertSelect));
        }
        if !self.eat(&Tok::LParen) {
            return Err(self.refuse(Refused::InsertColumns));
        }

        let mut columns = Vec::new();
        loop {
            columns.push(self.bare_ident("a column name")?);
            if self.eat(&Tok::Comma) {
                continue;
            }
            self.expect(&Tok::RParen, ", or ) after the columns")?;
            break;
        }

        // `None` is a statement leaving the record to the server, which is a different
        // statement rather than a malformed one - see [`Insert::id_at`].
        let id_at = columns.iter().position(|c| c.eq_ignore_ascii_case(RECORD_COLUMN));

        // `VALUE` is MySQL's spelling of the same word and means the same thing.
        if !self.eat_word("VALUES") && !self.eat_word("VALUE") {
            if self.word_is("SELECT") {
                return Err(self.refuse(Refused::InsertSelect));
            }
            return Err(self.syntax("VALUES"));
        }

        let mut rows = Vec::new();
        loop {
            rows.push(self.tuple(columns.len(), id_at)?);
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        // Counted after the rows are read rather than as they arrive, so the refusal says how
        // many were written rather than stopping at ten thousand and one.
        if rows.len() > MAX_INSERT_ROWS {
            return Err(self.refuse(Refused::InsertSize));
        }

        if self.peek().is_some() {
            return Err(self.syntax("the end of the statement"));
        }
        Ok(Insert { table, columns, id_at, rows })
    }

    /// One `( <literal>, ... )`, as wide as the column list.
    ///
    /// A width that does not match is a syntax error and not a refusal: nothing about the
    /// engine makes it impossible, the statement is simply not saying what it means.
    fn tuple(&mut self, width: usize, id_at: Option<usize>) -> Result<Vec<Literal>> {
        self.expect(&Tok::LParen, "( and the values for a row")?;
        let mut values = Vec::with_capacity(width);
        loop {
            let at = self.at();
            let value = self.literal("a value")?;
            // Judged at the value, where a schema is not needed to know better: a record id is
            // a whole number on every write path this engine has.
            if Some(values.len()) == id_at && !matches!(value, Literal::Int(_)) {
                return Err(self.refuse_at(Refused::InsertId, at));
            }
            values.push(value);
            if self.eat(&Tok::Comma) {
                continue;
            }
            self.expect(&Tok::RParen, ", or ) after the values")?;
            break;
        }
        if values.len() != width {
            return Err(self.syntax("one value per column"));
        }
        Ok(values)
    }
}
