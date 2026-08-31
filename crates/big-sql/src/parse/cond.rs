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

//! The `WHERE` grammar, and the depth bound that keeps a recursive descent off the stack's end.

use super::Parser;
use super::MAX_DEPTH;
use crate::ast::Cond;
use crate::error::{Refused, Result, SqlError};
use crate::lex::Tok;
use big_plan::Literal;

impl Parser<'_> {
    pub(super) fn enter(&mut self) -> Result<()> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(SqlError::TooDeep { at: self.at(), limit: MAX_DEPTH });
        }
        Ok(())
    }

    pub(super) fn leave(&mut self) {
        self.depth -= 1;
    }

    pub(super) fn cond(&mut self) -> Result<Cond> {
        self.enter()?;
        let mut left = self.conj()?;
        while self.eat_word("OR") {
            left = Cond::Or(Box::new(left), Box::new(self.conj()?));
        }
        self.leave();
        Ok(left)
    }

    pub(super) fn conj(&mut self) -> Result<Cond> {
        self.enter()?;
        let mut left = self.neg()?;
        while self.eat_word("AND") {
            left = Cond::And(Box::new(left), Box::new(self.neg()?));
        }
        self.leave();
        Ok(left)
    }

    pub(super) fn neg(&mut self) -> Result<Cond> {
        self.enter()?;
        let out = if self.eat_word("NOT") {
            Cond::Not(Box::new(self.neg()?))
        } else if self.peek() == Some(&Tok::LParen) {
            // A `(` that begins a select is a subquery; one that begins anything else is a
            // parenthesised condition.
            if self.word_at_is(1, "SELECT") {
                return Err(self.refuse(Refused::Subquery));
            }
            self.i += 1;
            let inner = self.cond()?;
            self.expect(&Tok::RParen, ") to close the condition")?;
            inner
        } else {
            self.predicate()?
        };
        self.leave();
        Ok(out)
    }

    /// Whether the current token closes the term being parsed rather than continuing it.
    ///
    /// What tells `WHERE active AND amount > 5` from `WHERE country LIKE 'G%'`: both are a
    /// column followed by a word, and only the second is a comparison this engine has no set
    /// operation for.
    fn ends_a_term(&self) -> bool {
        const ENDS: [&str; 8] =
            ["AND", "OR", "GROUP", "HAVING", "ORDER", "LIMIT", "OFFSET", "FORMAT"];
        match self.peek() {
            // The end of the statement, a closing bracket, or the comma between an `-If`'s two
            // arguments.
            None | Some(Tok::RParen) | Some(Tok::Comma) => true,
            Some(Tok::Word(_)) => ENDS.iter().any(|k| self.word_is(k)),
            _ => false,
        }
    }

    pub(super) fn predicate(&mut self) -> Result<Cond> {
        let field = self.name("a column name")?;

        if self.word_is("IS") {
            return Err(self.refuse(Refused::Null));
        }
        // `NOT IN` and `NOT BETWEEN` negate the whole term, which is what `Not` already is.
        let negated =
            self.word_is("NOT") && (self.word_at_is(1, "IN") || self.word_at_is(1, "BETWEEN"));
        if negated {
            self.i += 1;
        }

        let inner = if self.eat_word("IN") {
            self.expect(&Tok::LParen, "( after IN")?;
            if self.word_is("SELECT") {
                return Err(self.refuse(Refused::Subquery));
            }
            let mut values = vec![self.literal("a value inside IN")?];
            while self.eat(&Tok::Comma) {
                values.push(self.literal("a value inside IN")?);
            }
            self.expect(&Tok::RParen, ") to close IN")?;
            Cond::In { field, values }
        } else if self.eat_word("BETWEEN") {
            let low = self.literal("a lower bound")?;
            self.expect_word("AND", "AND between the two bounds")?;
            let high = self.literal("an upper bound")?;
            Cond::Between { field, low, high }
        } else {
            match self.peek() {
                Some(Tok::Op(op)) => {
                    let op = *op;
                    self.i += 1;
                    Cond::Cmp { field, op, value: self.literal("a value to compare against")? }
                }
                // A word here is `LIKE`, `SIMILAR`, `GLOB` or another comparison this engine
                // has no set operation for. Refused as a predicate rather than as syntax.
                //
                // `AND`, `OR` and the clause keywords are the exception: they end the term
                // rather than continue it, so a column standing alone in front of one is the
                // bare boolean below.
                // **A column standing on its own is `= TRUE`.** `WHERE active` is how
                // ClickHouse spells it and how `countIf(active)` reads, and a boolean field is
                // the only kind that could mean anything here - which the planner enforces,
                // because it is the layer that knows the field's class.
                _ if self.ends_a_term() => Cond::Cmp { field, op: "=", value: Literal::Bool(true) },
                Some(Tok::Word(_)) => return Err(self.refuse(Refused::Predicate)),
                _ => return Err(self.syntax("a comparison after the column")),
            }
        };

        Ok(if negated { Cond::Not(Box::new(inner)) } else { inner })
    }
}
