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

//! `KILL QUERY '<id>'`: one running query, named and stopped.

use super::Parser;
use crate::error::{Refused, Result};
use crate::lex::Tok;

impl Parser<'_> {
    /// `KILL [QUERY] '<id>'` and `KILL QUERY WHERE query_id = '<id>'`, with `KILL` consumed.
    ///
    /// Two spellings because ClickHouse writes the second and everybody else writes the first,
    /// and they name the same one query. What is **not** accepted is a predicate over anything
    /// else - a user, a table, an elapsed time - because that kills a set nobody named, and the
    /// set it kills depends on what happened to be running when it arrived.
    pub(super) fn kill(&mut self) -> Result<String> {
        // `KILL MUTATION` names work this engine does not queue: a schema change is applied and
        // finished, and there is no background rewrite to cancel.
        if self.word_is("MUTATION") || self.word_is("CONNECTION") {
            return Err(self.refuse(Refused::KillTarget));
        }
        self.eat_word("QUERY");

        if self.eat_word("WHERE") {
            let at = self.at();
            let column = self.bare_ident("query_id")?;
            if !column.eq_ignore_ascii_case("query_id") || !self.eat(&Tok::Op("=")) {
                return Err(self.refuse_at(Refused::KillTarget, at));
            }
            let id = self.quoted_id()?;
            // A second term would make this a predicate over a set again, which is the whole of
            // what this refuses.
            if self.peek().is_some() {
                return Err(self.refuse(Refused::KillTarget));
            }
            return Ok(id);
        }

        let id = self.quoted_id()?;
        if self.peek().is_some() {
            return Err(self.refuse(Refused::KillTarget));
        }
        Ok(id)
    }

    /// The id, which is a string because it carries a node name and a slash.
    fn quoted_id(&mut self) -> Result<String> {
        match self.peek() {
            Some(Tok::Str(s)) => {
                let s = s.clone();
                self.i += 1;
                Ok(s)
            }
            _ => Err(self.refuse(Refused::KillTarget)),
        }
    }
}
