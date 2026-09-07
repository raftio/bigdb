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
use crate::ast::Proj;
use crate::error::{Refused, Result};
use crate::insert::{Insert, Source, MAX_INSERT_ROWS, RECORD_COLUMN};
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
        // Before `INTO`, because `INSERT OVERWRITE TABLE t` writes neither word in the place
        // this expects one. It is a truncate and an insert, and nothing wraps two of those in
        // one transaction - so the sentence offers the two statements, and the atomic spelling.
        if self.word_is("OVERWRITE") {
            return Err(self.refuse(Refused::Overwrite));
        }
        self.eat_word("INTO");
        let (database, table) = self.table_ref("a table name")?;

        // `INSERT INTO t SELECT ...` with no column list would be positional against a field
        // order the statement does not carry, which is the same refusal `VALUES` earns.
        if self.word_is("SELECT") || self.word_is("WITH") {
            return Err(self.refuse(Refused::InsertColumns));
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

        // `SELECT` here reads the values out of the table instead of out of the statement.
        if self.word_is("SELECT") {
            let source = self.insert_select(&database, &table, columns.len(), id_at)?;
            if self.peek().is_some() {
                return Err(self.syntax("the end of the statement"));
            }
            return Ok(Insert { database, table, columns, id_at, source });
        }

        // `VALUE` is MySQL's spelling of the same word and means the same thing.
        if !self.eat_word("VALUES") && !self.eat_word("VALUE") {
            return Err(self.syntax("VALUES or SELECT"));
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
        Ok(Insert { database, table, columns, id_at, source: Source::Values(rows) })
    }

    /// The `SELECT` an `INSERT` reads its values from, with `SELECT` still ahead.
    ///
    /// Three things are checked here and each is a different mistake:
    ///
    /// - **The shape has to be a projection.** Most answers on this surface are numbers *about*
    ///   a set of records, and `SELECT *` answers with record ids because a record has no row of
    ///   values to read out. A projection is the one shape that reads stored values back per
    ///   record, so it is the one shape there is anything to write.
    /// - **The source table cannot be the target.** Reading and writing one table in a single
    ///   statement has no snapshot under it here: the records this writes would be visible to
    ///   the read that is still running, and the statement would feed itself.
    /// - **The widths have to match**, which is a syntax error rather than a refusal - nothing
    ///   about the engine makes it impossible, the statement is simply not saying what it means.
    fn insert_select(
        &mut self,
        database: &Option<String>,
        table: &str,
        columns: usize,
        id_at: Option<usize>,
    ) -> Result<Source> {
        let at = self.at();
        // `SELECT` is consumed here rather than by `select`, which starts at the list - the same
        // way `CREATE VIEW` reads its body.
        self.i += 1;
        let select = self.select()?;

        // **Values, not identities.** What the write path needs of a source is that every cell
        // it produces is a value a fact can hold - which a projection's cells are, and which a
        // grouped answer's cells are just as much: a key is the string a keyed column was
        // interned from, and a count is a number.
        //
        // So the rule is not "a projection". It is `SELECT *`, which answers with *record ids*
        // because a record has no row of values to read out - an id is the address a fact is
        // written to rather than something stored in one, and writing those into a column would
        // put this engine's own coordinates into a user's data.
        //
        // A cell that is a value but not an exact one - the float an `avg` is - is refused
        // where it is read rather than here, by `literal_of`, with the sentence that says what
        // to write instead. This layer has no schema and cannot tell which those are.
        let has_star = select.items.iter().any(|item| matches!(item.leaf(), Proj::Star));
        if has_star || select.items.is_empty() {
            return Err(self.refuse_at(Refused::InsertSelect, at));
        }

        // The same table on both sides, whichever way each was qualified. Compared on the pair
        // rather than the written text, so `INSERT INTO sales.t ... FROM t` under
        // `?database=sales` is caught too.
        if select.from.table == table && select.from.database == *database {
            return Err(self.refuse_at(Refused::InsertSelfRead, at));
        }

        // An id column would have to be read out of the source, and `SELECT _record_id` is not
        // a projection this engine has - a record id is the address a fact is written to rather
        // than a value stored in one. So this form always allocates.
        if id_at.is_some() {
            return Err(self.refuse_at(Refused::InsertSelect, at));
        }
        if select.items.len() != columns {
            return Err(self.syntax("as many selected columns as the INSERT names"));
        }
        Ok(Source::Select(Box::new(select)))
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
