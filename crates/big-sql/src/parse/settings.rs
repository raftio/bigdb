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

//! The trailing `SETTINGS k = v`, which is what this dialect has instead of a session.
//!
//! Written as the twin of [`Parser::format`](super::Parser::format), and for the same reason
//! that one is factored out of `select`: it is a trailing clause whose meaning does not depend
//! on the statement it trails. The difference is where each may appear - `FORMAT` changes the
//! bytes of any answer, and `SETTINGS` bounds a statement that has something to spend.

use super::Parser;
use crate::error::{Refused, Result};
use crate::lex::Tok;
use crate::settings::Settings;

impl Parser<'_> {
    /// `SETTINGS k = v [, k = v]*`, or the empty set.
    ///
    /// # Why it goes after `FORMAT` and not before it
    ///
    /// ClickHouse writes `SETTINGS` before `FORMAT`; this reads it after, and there is one
    /// position rather than two so that a statement cannot carry two of them. The clause is
    /// read once for the whole statement, after the last `UNION ALL` branch - a budget is
    /// something the statement spends together, and a per-branch limit would be a number that
    /// bounds a part of an answer nobody receives a part of.
    ///
    /// # A key is refused rather than ignored
    ///
    /// Which is the opposite of what ClickHouse does, and deliberately. There are no bind
    /// parameters in this dialect, so a key here was *typed* - and a misspelled
    /// `max_execution_tim` that is quietly dropped is a query running without the bound its
    /// author believes it has, which reads exactly like a query that was bounded and slow. The
    /// same argument [`Refused::Round`] makes about a value that cannot be produced.
    pub(super) fn settings(&mut self) -> Result<Settings> {
        let mut settings = Settings::default();
        if !self.eat_word("SETTINGS") {
            return Ok(settings);
        }
        loop {
            let at = self.at();
            let key = self.bare_ident("a setting name after SETTINGS")?;
            self.expect(&Tok::Op("="), "= and a value after the setting name")?;
            let value = self.small_number("a whole number for the setting")?;

            // Matched against the struct's own fields, so a key that parses is a key something
            // downstream reads. `max_threads`, `max_block_size` and `join_algorithm` fall
            // through to the refusal on purpose: each names a knob this engine does not have,
            // and accepting one would be a promise nothing keeps.
            let slot = match key.to_ascii_lowercase().as_str() {
                "max_execution_time" => &mut settings.max_execution_time,
                "max_memory_usage" => &mut settings.max_memory_usage,
                "max_result_rows" => &mut settings.max_result_rows,
                "max_delete_records" => &mut settings.max_delete_records,
                _ => return Err(self.refuse_at(Refused::Setting, at)),
            };
            // The last one written wins, rather than the first or a refusal. Repeating a key is
            // not ambiguous - a statement is read left to right and the reader of it expects the
            // same - and it is what makes a query built by string concatenation able to append
            // an override.
            *slot = Some(value);

            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        Ok(settings)
    }
}
