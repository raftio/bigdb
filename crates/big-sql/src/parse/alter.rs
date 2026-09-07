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

//! `ALTER TABLE` and `DROP TABLE`: the two schema changes that are not a creation.

use super::Parser;
use crate::ddl::Alter;
use crate::error::{Refused, Result};
use crate::lex::Tok;

impl Parser<'_> {
    /// `ALTER TABLE <name> <change> [, <change>]*`, with `ALTER` already consumed.
    ///
    /// ```text
    /// change := ADD [COLUMN] ident type
    ///         | DROP [COLUMN] ident
    /// ```
    ///
    /// Two changes, because two is what the engine has. Everything else `ALTER` can say in SQL
    /// is refused at the keyword that says it, with what to do instead: a kind or a depth is
    /// what a field's bit planes are, a name is what resolves a fact, and an engine is what a
    /// table stored every fact it already holds under.
    pub(super) fn alter_table(&mut self) -> Result<crate::ddl::Ddl> {
        if !self.word_is("TABLE") {
            if self.word_is("MATERIALIZED") {
                return Err(self.refuse(Refused::MaterializedView));
            }
            // `ALTER VIEW` joins `ALTER DATABASE`, `ALTER USER` and `ALTER INDEX`: a change this
            // surface does not have, which is what the `Write` refusal already says. A view
            // carries a name and a statement, and `CREATE OR REPLACE VIEW` changes the statement
            // - so there is nothing left for an `ALTER` to reach.
            return Err(self.refuse(Refused::Write));
        }
        self.i += 1;
        let (database, table) = self.table_ref("a table name")?;

        // `RENAME TO` before the change loop, because it is not one of the changes: the loop
        // composes clauses against one table, and this replaces the table's own name. Two of
        // them in one list would be a list where only the last entry meant anything.
        //
        // `AS` is read beside `TO` for the reason `DOUBLE PRECISION` is read beside `DOUBLE` -
        // it is a word that says what was already meant, and MySQL writes it. `RENAME COLUMN`
        // does not reach here: it has no `TO` in second position, so it falls to `alter_change`
        // and the refusal that is about a field's name.
        if self.word_is("RENAME") && (self.word_at_is(1, "TO") || self.word_at_is(1, "AS")) {
            self.i += 2;
            // Bare, and a qualified name is a syntax error rather than a refusal: a rename that
            // also moved the table across databases is not a construct with a sentence to write,
            // it is a shape this grammar does not have. See `Ddl::RenameTable`.
            let to = self.bare_ident("a new table name")?;
            self.end_of_statement()?;
            return Ok(crate::ddl::Ddl::RenameTable { database, table, to });
        }

        let mut changes = Vec::new();
        loop {
            changes.push(self.alter_change()?);
            if !self.eat(&Tok::Comma) {
                break;
            }
        }

        if self.peek().is_some() {
            return Err(self.syntax("the end of the statement"));
        }
        Ok(crate::ddl::Ddl::AlterTable { database, table, changes })
    }

    /// One clause of an `ALTER TABLE`.
    fn alter_change(&mut self) -> Result<Alter> {
        // Refused before `ADD` and `DROP` are looked for, so that each names the construct that
        // was written rather than "expected ADD or DROP". Somebody who wrote `MODIFY` knows
        // what they meant; what they need is why this engine will not do it.
        // `ALTER TABLE t MODIFY TTL ...` would otherwise be caught by `AlterKind` below, whose
        // sentence is about column kinds and says nothing about retention.
        if (self.word_is("MODIFY") || self.word_is("SET")) && self.word_at_is(1, "TTL") {
            return Err(self.refuse(Refused::DeclarativeTtl));
        }
        // Named before `AlterKind`, whose sentence is about column kinds and says nothing about
        // where compaction actually lives.
        if self.word_is("COMPACT") || self.word_is("FREEZE") {
            return Err(self.refuse(Refused::Optimize));
        }
        if self.word_is("MODIFY") || self.word_is("CHANGE") {
            return Err(self.refuse(Refused::AlterKind));
        }
        if self.word_is("RENAME") {
            return Err(self.refuse(Refused::Rename));
        }
        if self.word_is("ENGINE") {
            return Err(self.refuse(Refused::AlterEngine));
        }

        // `DROP DAYS BEFORE '<date>' ON <column>` before the plain `DROP`, because both start
        // with the same word and only this one continues with `DAYS`.
        if self.word_is("DROP") && self.word_at_is(1, "DAYS") {
            self.i += 2;
            self.expect_word("BEFORE", "BEFORE after DAYS")?;
            let before = match self.peek() {
                Some(Tok::Str(s)) => {
                    let s = s.clone();
                    self.i += 1;
                    s
                }
                _ => return Err(self.syntax("a date in quotes after BEFORE")),
            };
            self.expect_word("ON", "ON and the column name")?;
            let column = self.bare_ident("a column name")?;
            return Ok(Alter::DropDaysBefore { column, before });
        }
        if self.eat_word("ADD") {
            // `ADD COLUMN` and `ADD` are the same clause; the word is optional in every dialect
            // that has it. `ADD CONSTRAINT`, `ADD PRIMARY KEY`, `ADD INDEX` are not, and are
            // refused where a constraint in a column list is.
            if self.at_constraint() || self.word_is("CONSTRAINT") || self.word_is("INDEX") {
                return Err(self.refuse(Refused::Constraint));
            }
            self.eat_word("COLUMN");
            let at = self.at();
            let name = self.bare_ident("a column name")?;
            if name.eq_ignore_ascii_case(crate::insert::RECORD_COLUMN) {
                return Err(self.refuse_at(Refused::IdColumn, at));
            }
            return self.column_type(name).map(Alter::Add);
        }

        if self.eat_word("DROP") {
            if self.at_constraint() || self.word_is("CONSTRAINT") || self.word_is("INDEX") {
                return Err(self.refuse(Refused::Constraint));
            }
            self.eat_word("COLUMN");
            let name = self.bare_ident("a column name")?;
            return Ok(Alter::Drop(name));
        }

        // `ALTER TABLE t ALTER COLUMN a TYPE int` - the standard spelling of what `MODIFY`
        // says, and the one that needs its sentence rather than a syntax error.
        if self.word_is("ALTER") {
            return Err(self.refuse(Refused::AlterKind));
        }
        Err(self.syntax("ADD or DROP"))
    }

    /// `DROP TABLE [IF EXISTS] <name>`, with `DROP` already consumed.
    ///
    /// **The only statement on this surface that destroys anything**, and what it destroys is a
    /// table: `DELETE FROM` is still refused, because a record is the bits set for it across
    /// every field rather than a row to remove. It reaches nothing the `admin` token that
    /// authorises it could not already reach over `DELETE /table/{t}` - a second spelling, not
    /// a second capability.
    ///
    /// One table, not a list. Two drops are two changes, each of which goes to the schema
    /// leader and then to every node, and there is no journal that would let half of a list be
    /// undone - so a list would promise an atomicity nothing below here has.
    pub(super) fn drop_table(&mut self) -> Result<crate::ddl::Ddl> {
        if self.word_is("DATABASE") || self.word_is("SCHEMA") || self.word_is("DATASET") {
            self.i += 1;
            let if_exists = self.if_exists(false)?;
            let name = self.bare_ident("a database name")?;
            // `RESTRICT` is the default, so saying it changes nothing - it is accepted because
            // it is what somebody writes to be explicit about the behaviour they are getting.
            let cascade = self.eat_word("CASCADE");
            if !cascade {
                self.eat_word("RESTRICT");
            }
            if self.peek().is_some() {
                return Err(self.syntax("the end of the statement"));
            }
            // Nothing created it, so nothing drops it - and a `DROP DATABASE system` that
            // answered "nothing dropped" would read as though the views had been there and gone.
            if name.eq_ignore_ascii_case(crate::show::SYSTEM_DATABASE) {
                return Err(self.refuse(Refused::SystemTable));
            }
            return Ok(crate::ddl::Ddl::DropDatabase { name, if_exists, cascade });
        }
        // `MATERIALIZED` before `VIEW`, so `DROP MATERIALIZED VIEW` gets the sentence about the
        // thing it names rather than being read as a plain view drop.
        if self.word_is("MATERIALIZED") {
            return Err(self.refuse(Refused::MaterializedView));
        }
        if self.word_is("VIEW") {
            self.i += 1;
            let if_exists = self.if_exists(false)?;
            let (database, name) = self.table_ref("a view name")?;
            if self.peek().is_some() {
                return Err(self.syntax("the end of the statement"));
            }
            return Ok(crate::ddl::Ddl::DropView { database, name, if_exists });
        }
        if !self.word_is("TABLE") {
            return Err(self.refuse(Refused::Write));
        }
        self.i += 1;
        let if_exists = self.if_exists(false)?;
        let (database, table) = self.table_ref("a table name")?;

        if self.peek().is_some() {
            return Err(self.syntax("the end of the statement"));
        }
        Ok(crate::ddl::Ddl::DropTable { database, table, if_exists })
    }
}

impl Parser<'_> {
    /// `TRUNCATE [TABLE] [IF EXISTS] <name>`, with `TRUNCATE` already consumed.
    ///
    /// `TABLE` is optional because it is optional in MySQL and required in Postgres, and the
    /// word decides nothing here - there is one kind of object this empties. A `TRUNCATE` of
    /// anything else is refused at the word that named it rather than read as a table with a
    /// strange name.
    pub(super) fn truncate_table(&mut self) -> Result<crate::ddl::Ddl> {
        // Named before `TABLE` is looked for, so somebody who wrote one of these is told what
        // this engine has rather than "expected a table name".
        if self.word_is("DATABASE") || self.word_is("SCHEMA") || self.word_is("VIEW") {
            return Err(self.refuse(Refused::Write));
        }
        self.eat_word("TABLE");
        let if_exists = self.if_exists(false)?;
        let (database, table) = self.table_ref("a table name")?;
        // `RESTART IDENTITY` and `CASCADE` are Postgres's, and both would be lies here: there is
        // no sequence to restart - a record id is an address, not a counter anybody may reset -
        // and nothing references a table for a cascade to follow.
        if self.peek().is_some() {
            return Err(self.syntax("the end of the statement"));
        }
        Ok(crate::ddl::Ddl::TruncateTable { database, table, if_exists })
    }

    /// `EXCHANGE TABLES <a> AND <b>`, with `EXCHANGE` already consumed.
    ///
    /// `TABLES` is required, unlike `TRUNCATE`'s optional `TABLE`: there the word decides
    /// nothing because there is one kind of object, and here it is the whole of what tells
    /// `EXCHANGE` apart from a column called `exchange`.
    pub(super) fn exchange_tables(&mut self) -> Result<crate::ddl::Ddl> {
        self.expect_word("TABLES", "TABLES after EXCHANGE")?;
        let (da, a) = self.table_ref("a table name")?;
        self.expect_word("AND", "AND between the two tables")?;
        let (db, b) = self.table_ref("a table name")?;

        // One database, and the disagreement is a syntax error rather than a refusal for the
        // reason a qualified rename target is: swapping two names across databases is not a
        // construct this grammar has a shape for, so there is no sentence about what to write
        // instead beyond writing it the way it is written.
        //
        // Only an *explicit* pair is compared. A bare name means the request's database, which
        // is filled in later by `fill_database` and is the same one for both.
        let database = match (da, db) {
            (Some(x), Some(y)) if !x.eq_ignore_ascii_case(&y) => {
                return Err(self.syntax("both tables in one database"))
            }
            (Some(x), _) | (None, Some(x)) => Some(x),
            (None, None) => None,
        };

        self.end_of_statement()?;
        Ok(crate::ddl::Ddl::ExchangeTables { database, a, b })
    }
}
