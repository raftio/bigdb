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

//! `SELECT * FROM numbers(10)`: a `FROM` that names no table.
//!
//! # Why this forks beside `system.`
//!
//! For the same reason and at the same place: **the `FROM` decides what `*` means.** Over a table
//! it is the record id; over `numbers(n)` it is the one column there is. So the fork happens on a
//! lookahead to the `FROM` before the select list is read, exactly as [`Parser::names_system`]
//! does, rather than by parsing the list one way and reinterpreting it afterwards.
//!
//! # Why it is a `Shown` and not a query
//!
//! No plan produces these rows. A system view's answer is already in the catalog; this one is not
//! anywhere at all - which comes to the same thing on the query path, because there is nothing to
//! fan out and nothing to merge. Making it a query would mean the first `Statement` whose `calls`
//! list is empty under a shape naming plans that do not exist, and that is the fiction the
//! `Shown` family exists to avoid.
//!
//! # The other table functions
//!
//! `s3`, `url` and `file` read something outside this process, which is the external-catalog
//! milestone and not a parser question. `generateRandom` needs both a generator and a way to say
//! what type each column is. All four are refused by name in [`Parser::source`], with
//! `numbers(n)` named as the one that works.

use super::Parser;
use crate::error::{Refused, Result};
use crate::lex::Tok;
use crate::show::{Show, Shown, MAX_NUMBERS};

/// The name that is a table function rather than a table when a `(` follows it.
pub(super) const NUMBERS: &str = "numbers";

impl Parser<'_> {
    /// Whether this statement's `FROM` names `numbers(`.
    ///
    /// A scan, bounded by the statement, stopping at the first `FROM` - of which there is
    /// exactly one, because this dialect has no subquery to hide a second in. The companion to
    /// [`Parser::names_system`], and it has to be a lookahead for the same reason that one is.
    ///
    /// The `(` is half the test. `SELECT * FROM numbers` is a table somebody called `numbers`,
    /// and reading it as this would make a name unusable by surprise.
    pub(super) fn names_numbers(&self) -> bool {
        let mut i = self.i;
        while i < self.t.len() {
            if let Tok::Word(w) = &self.t[i].tok {
                if w.eq_ignore_ascii_case("FROM") {
                    return matches!(
                        (self.t.get(i + 1).map(|t| &t.tok), self.t.get(i + 2).map(|t| &t.tok)),
                        (Some(Tok::Word(f)), Some(Tok::LParen)) if f.eq_ignore_ascii_case(NUMBERS)
                    );
                }
            }
            i += 1;
        }
        false
    }

    /// `SELECT <list> FROM numbers(<n>) [FORMAT f]`, with `SELECT` already consumed.
    pub(super) fn numbers_select(&mut self) -> Result<Show> {
        // The one column there is, so the list may name it or say `*`. Anything else is refused
        // where a system view's select list is, and for the same reason: an expression here
        // would be a second evaluator over a `ResultSet`.
        self.numbers_columns()?;
        self.expect_word("FROM", "FROM after the select list")?;

        // `numbers` and the bracket, both already established by `names_numbers`.
        self.i += 1;
        let at = self.at();
        self.i += 1;
        let n = self.small_number("how many numbers")?;
        if !self.eat(&Tok::RParen) {
            return Err(self.syntax("`)` after the count"));
        }
        // Refused with the number rather than clamped to it. See `MAX_NUMBERS`: the silent
        // clamp a `SETTINGS` value gets is only bearable because `EXPLAIN` prints the figure
        // that was applied, and there is no such line on the way out of here.
        if n > MAX_NUMBERS {
            return Err(self.refuse_at(Refused::NumbersTooLarge, at));
        }

        let format = self.format()?;
        // Every remaining clause is refused at the word that names it, exactly as a system
        // view's are - including `SETTINGS`, which would bound a query and cannot bound this.
        // `WHERE number > 5` is deliberately among them: filtering here would be a second
        // filter engine over rows, and the sequence is already whatever the caller asked for.
        if self.peek().is_some() {
            return Err(self.refuse(Refused::SystemClause));
        }
        Ok(Show { what: Shown::Numbers { n }, format })
    }

    /// `*`, or the one column this has, named.
    fn numbers_columns(&mut self) -> Result<()> {
        if self.eat(&Tok::Star) {
            return Ok(());
        }
        let at = self.at();
        let name = self.bare_ident("a column name, or `*`")?;
        // Not `number` is not a column of this, and neither is a call, a dot or an alias -
        // each of which would make it more than a name.
        if !name.eq_ignore_ascii_case("number")
            || matches!(self.peek(), Some(Tok::LParen) | Some(Tok::Dot))
            || self.word_is("AS")
        {
            return Err(self.refuse_at(Refused::SystemClause, at));
        }
        Ok(())
    }
}
