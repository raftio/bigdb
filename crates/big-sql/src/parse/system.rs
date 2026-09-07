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

//! `SELECT * FROM system.tables`: the catalog, asked about the way everything else is.
//!
//! # Why this is parsed apart from `select`
//!
//! **`*` means two different things, and the `FROM` is what decides which.** Over a table it is
//! the record id - the address a fact is written to - because a record here has no row of values
//! to hand back. Over a system view it means every column, the way it does in every other
//! dialect, because a system view *is* rows. One token, two readings, so the reading has to be
//! chosen before the select list is interpreted rather than after.
//!
//! That is why [`Parser::names_system`] looks ahead to the `FROM` first. It is the only lookahead
//! in this parser and it earns its place: the alternative is parsing the list one way and
//! reinterpreting it later, which is how `*` comes to mean whichever thing the last edit assumed.
//!
//! # What it will not do
//!
//! A system view answers in full. The one clause it takes is `WHERE database = '…'`, and on
//! `system.columns` also `WHERE table = '…'` - not as a general filter, but because those two are
//! *already the parameters* of the introspection functions underneath (`SHOW TABLES FROM d` and
//! `DESCRIBE t` pass exactly them). Anything else - a join, a grouping, an ordering, a limit - is
//! refused at its own keyword, because the answer is a `ResultSet` built from the catalog rather
//! than a shape over plans, and filtering one would mean a second filter engine over rows.

use super::Parser;
use crate::error::{Refused, Result};
use crate::lex::Tok;
use crate::show::{Show, Shown, SystemView, SYSTEM_DATABASE};

impl Parser<'_> {
    /// Whether this statement's `FROM` names the `system` database.
    ///
    /// A scan rather than a parse, and bounded by the statement: it stops at the first `FROM`,
    /// of which there is exactly one here because this dialect has no subqueries to hide a
    /// second in. `INSERT ... SELECT` never reaches this - it is dispatched on `INSERT` - so the
    /// only `FROM` this can find is the one whose meaning it is deciding.
    pub(super) fn names_system(&self) -> bool {
        let mut i = self.i;
        while i < self.t.len() {
            if let Tok::Word(w) = &self.t[i].tok {
                if w.eq_ignore_ascii_case("FROM") {
                    return matches!(
                        (self.t.get(i + 1).map(|t| &t.tok), self.t.get(i + 2).map(|t| &t.tok)),
                        (Some(Tok::Word(db)), Some(Tok::Dot)) if db.eq_ignore_ascii_case(SYSTEM_DATABASE)
                    );
                }
            }
            i += 1;
        }
        false
    }

    /// `SELECT <list> FROM system.<view> [WHERE <one equality>] [FORMAT f] [SETTINGS ...]`,
    /// with `SELECT` already consumed.
    pub(super) fn system_select(&mut self) -> Result<Show> {
        let columns = self.system_columns()?;
        self.expect_word("FROM", "FROM after the select list")?;

        // `system` and the dot, both already established by `names_system`.
        self.i += 2;
        let at = self.at();
        let name = self.bare_ident("a view name after `system.`")?;
        let Some(view) = SystemView::of(&name) else {
            return Err(self.refuse_at(Refused::SystemTable, at));
        };

        let (database, table) = self.system_filter(view)?;
        let format = self.format()?;

        // **No `SETTINGS` here, on purpose.** The clause bounds what a statement spends, and
        // this one spends nothing measurable: the answer is built from the catalog already in
        // memory, never reaching `DbRead` and so never meeting a deadline or a record ceiling.
        // Accepting it would be exactly the silent ignore `Refused::Setting` exists to prevent,
        // so it falls into the refusal below with every other clause these views do not take.
        //
        // Everything left is refused at the word that names it rather than by falling off the
        // grammar, which is the rule the rest of this parser follows. `WHERE` is not among them
        // because `system_filter` has already had it and refused whatever it could not take.
        if self.peek().is_some() {
            return Err(self.refuse(Refused::SystemClause));
        }
        Ok(Show { what: Shown::System { view, database, table, columns }, format })
    }

    /// The select list: `*`, or the columns to keep.
    ///
    /// Bare names only. An expression, an aggregate or an alias over a system view would be a
    /// second evaluator over a `ResultSet`, which is the thing this whole variant exists not to
    /// need - so each is refused here rather than accepted into a shape that cannot carry it.
    fn system_columns(&mut self) -> Result<Option<Vec<String>>> {
        if self.eat(&Tok::Star) {
            return Ok(None);
        }
        let mut columns = vec![self.system_column()?];
        while self.eat(&Tok::Comma) {
            columns.push(self.system_column()?);
        }
        Ok(Some(columns))
    }

    /// One bare column name, or the refusal that says only bare names are read here.
    fn system_column(&mut self) -> Result<String> {
        let at = self.at();
        let name = self.bare_ident("a column name, or `*`")?;
        // A bracket after the name is a call - `count(*)`, `lower(name)` - and a dot is a
        // qualified name, which needs a join to be about. Both are refused at the token that
        // made it more than a name.
        if matches!(self.peek(), Some(Tok::LParen) | Some(Tok::Dot)) || self.word_is("AS") {
            return Err(self.refuse_at(Refused::SystemClause, at));
        }
        Ok(name)
    }

    /// The one `WHERE` these views take: `database = '…'`, and `table = '…'` on `system.columns`.
    ///
    /// Not a general filter and not the beginning of one. These two are the *parameters the
    /// introspection already has* - `SHOW TABLES FROM d` and `DESCRIBE t` pass exactly them - so
    /// accepting them costs no filter engine and no second code path. Anything else is refused
    /// with the two spellings named, because a client that cannot filter here can filter in the
    /// row set it got back, and a client that thinks it filtered and did not cannot.
    fn system_filter(&mut self, view: SystemView) -> Result<(Option<String>, Option<String>)> {
        let mut database = None;
        let mut table = None;
        if !self.eat_word("WHERE") {
            return Ok((None, None));
        }
        loop {
            let at = self.at();
            let column = self.bare_ident("`database` or `table`")?;
            if !self.eat(&Tok::Op("=")) {
                return Err(self.refuse_at(Refused::SystemClause, at));
            }
            let value = match self.peek() {
                Some(Tok::Str(s)) => {
                    let s = s.clone();
                    self.i += 1;
                    s
                }
                _ => return Err(self.refuse_at(Refused::SystemClause, at)),
            };
            match column.to_ascii_lowercase().as_str() {
                "database" => database = Some(value),
                // Only where it means something. On `system.databases` there is no table column
                // to compare, and a filter naming one would select nothing while reading as
                // though it had selected something.
                "table" if matches!(view, SystemView::Columns | SystemView::Parts) => {
                    table = Some(value);
                }
                _ => return Err(self.refuse_at(Refused::SystemClause, at)),
            }
            if !self.eat_word("AND") {
                break;
            }
        }
        Ok((database, table))
    }
}
