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

//! `UPDATE t SET c = v WHERE ...`, which is an assignment list and the `WHERE` a delete takes.

use super::Parser;
use crate::ast::Update;
use crate::error::{Refused, Result};
use crate::lex::Tok;

impl Parser<'_> {
    /// `UPDATE <table> SET <col> = <literal> [, ...] WHERE <cond>`, with `UPDATE` consumed.
    pub(super) fn update(&mut self) -> Result<Update> {
        let at = self.at();
        let (database, table) = self.table_ref("a table name")?;
        self.expect_word("SET", "SET after the table name")?;

        let mut assignments = Vec::new();
        loop {
            let column_at = self.at();
            let column = self.bare_ident("a column name")?;
            // The record id is the address a fact is written to rather than a value stored in a
            // column, so moving a record to a different id is not an update - it is writing a
            // different record. Refused where it is named, with the sentence that already
            // explains what `_record_id` is.
            if column == crate::RECORD_COLUMN {
                return Err(self.refuse_at(Refused::IdColumn, column_at));
            }
            if !self.eat(&Tok::Op("=")) {
                return Err(self.syntax("= after the column name"));
            }
            // **Literals only.** `SET amount = amount + 1` is a read-modify-write per record and
            // there is no plan for one; refused at the token that is not a literal, reusing the
            // sentence about what an expression may name.
            // **A bare word here is a column name, and that makes this an expression.**
            // `SET amount = amount + 1` needs each record's current value read back, arithmetic
            // done on it and the result written - a read-modify-write per record, which is not a
            // plan this engine has. It cannot be caught *before* `literal` runs, because `true`,
            // `false` and a `WITH` binding are words too; so the refusal is made out of the
            // failure, which is the one place the two can be told apart.
            let word_at_value = matches!(self.peek(), Some(Tok::Word(_)));
            let value = match self.literal("a value") {
                Ok(v) => v,
                Err(_) if word_at_value => return Err(self.refuse(Refused::Expression)),
                Err(e) => return Err(e),
            };
            // And arithmetic after a literal is the same construct written the other way round.
            if matches!(self.peek(), Some(Tok::Arith(_))) {
                return Err(self.refuse(Refused::Expression));
            }
            assignments.push((column, value));
            if !self.eat(&Tok::Comma) {
                break;
            }
        }

        // No `WHERE` names every record. Unlike a delete there is no cheaper spelling to point
        // at - `TRUNCATE` empties rather than rewrites - so it is refused for what it is: a
        // statement that would rewrite a whole table without saying so.
        if !self.eat_word("WHERE") {
            return Err(self.refuse_at(Refused::DeleteAll, at));
        }
        let filter = self.cond()?;

        if self.word_is("ORDER") || self.word_is("LIMIT") {
            return Err(self.refuse(Refused::DeleteAll));
        }
        self.settings = self.settings()?;
        if self.peek().is_some() {
            return Err(self.syntax("the end of the statement"));
        }
        Ok(Update { database, table, assignments, filter, at })
    }
}
