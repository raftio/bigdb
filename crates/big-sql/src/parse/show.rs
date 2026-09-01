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

//! `DESCRIBE` and `SHOW`: the catalog, asked about in SQL.

use super::Parser;
use crate::error::{Refused, Result};
use crate::shape::Format;
use crate::show::{Show, Shown};

impl Parser<'_> {
    /// `DESCRIBE`/`DESC`/`SHOW ...`, with nothing consumed yet.
    ///
    /// Nothing is consumed on the way in because the leading word is one of three rather than
    /// one, and reading it here keeps the dispatch in `parse` a table of names.
    pub(super) fn show(&mut self) -> Result<Show> {
        let what = if self.eat_word("DESCRIBE") || self.eat_word("DESC") {
            // `DESCRIBE TABLE t` is ClickHouse's spelling and `DESCRIBE t` is everyone else's.
            self.eat_word("TABLE");
            Shown::Columns { table: self.bare_ident("a table name")? }
        } else {
            self.expect_word("SHOW", "SHOW")?;
            self.show_what()?
        };

        let format = self.format()?;
        if self.peek().is_some() {
            return Err(self.syntax("the end of the statement"));
        }
        Ok(Show { what, format })
    }

    /// What follows `SHOW`.
    fn show_what(&mut self) -> Result<Shown> {
        if self.eat_word("TABLES") {
            return Ok(Shown::Tables);
        }
        if self.eat_word("COLUMNS") || self.eat_word("FIELDS") {
            self.expect_word("FROM", "FROM and a table name")?;
            return Ok(Shown::Columns { table: self.bare_ident("a table name")? });
        }
        if self.eat_word("CREATE") {
            self.eat_word("TABLE");
            return Ok(Shown::Create { table: self.bare_ident("a table name")? });
        }
        // `SHOW DATABASES` and `SHOW SCHEMAS` ask about a level that does not exist here, which
        // is a different answer from "not supported".
        if self.word_is("DATABASES") || self.word_is("SCHEMAS") {
            return Err(self.refuse(Refused::Database));
        }
        // `SHOW INDEX`, `SHOW GRANTS`, `SHOW PROCESSLIST`: each is a surface of its own, and
        // none of them is one this statement can grow by accident.
        Err(self.syntax("TABLES, COLUMNS FROM a table, or CREATE TABLE"))
    }

    /// The trailing `FORMAT <name>`, or the default.
    ///
    /// Here rather than in `select` because both statements end with it and it means the same
    /// thing in both: which bytes the same answer is written as.
    pub(super) fn format(&mut self) -> Result<Format> {
        if !self.eat_word("FORMAT") {
            return Ok(Format::default());
        }
        let at = self.at();
        let name = self.bare_ident("a format name after FORMAT")?;
        Format::of(&name).ok_or_else(|| self.refuse_at(Refused::Format, at))
    }
}
