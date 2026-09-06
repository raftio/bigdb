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

//! `DELETE FROM t WHERE ...`, which is a `WHERE` and a table and deliberately nothing else.

use super::Parser;
use crate::ast::Delete;
use crate::error::{Refused, Result};
use crate::lex::Tok;

impl Parser<'_> {
    /// `DELETE FROM <table> WHERE <cond>`, with `DELETE` already consumed.
    pub(super) fn delete(&mut self) -> Result<Delete> {
        self.expect_word("FROM", "FROM after DELETE")?;
        let at = self.at();
        let (database, table) = self.table_ref("a table name")?;

        // Refused at the word that names it, before `WHERE` is looked for, so somebody who wrote
        // one of these is told what this engine does rather than "expected WHERE".
        if self.word_is("USING") || self.peek() == Some(&Tok::Comma) {
            // A second table in a delete selects the records to remove by joining - which needs
            // the pairing this engine does not do. The set form of the same question is the one
            // that works here, and `Refused::Joins` already names it.
            return Err(self.refuse(Refused::Joins));
        }

        // **No `WHERE` is its own refusal, not an empty filter.** Every record is what
        // `TRUNCATE TABLE` frees by key; clearing them one id at a time is the same answer at the
        // worst possible price, so the statement is pointed at the cheap spelling instead of
        // being answered slowly.
        if !self.eat_word("WHERE") {
            return Err(self.refuse_at(Refused::DeleteAll, at));
        }
        let filter = self.cond()?;

        // MySQL takes `ORDER BY` and `LIMIT` on a delete, meaning "remove some of them". There is
        // no order to take the first of here - a record id is an address, and which records a
        // predicate selects is a set - so a partial delete would remove an arbitrary subset while
        // reading as though it had chosen one.
        if self.word_is("ORDER") || self.word_is("LIMIT") {
            return Err(self.refuse(Refused::DeleteAll));
        }
        // **A delete is the one write that takes `SETTINGS`**, because it is the one with a
        // ceiling of its own: `max_delete_records` bounds the half no deadline can reach. Read
        // here rather than in `union` because a delete never goes through it - and a key nothing
        // could carry would be exactly the empty promise `Refused::Setting` refuses other keys
        // for making.
        self.settings = self.settings()?;
        if self.peek().is_some() {
            return Err(self.syntax("the end of the statement"));
        }
        Ok(Delete { database, table, filter, at })
    }
}
