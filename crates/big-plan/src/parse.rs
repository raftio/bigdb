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

//! Text to [`Call`]. One syntactic form, so one small recursive descent.
//!
//! The grammar is deliberately tiny:
//!
//! ```text
//! call    := ident '(' args? ')'
//! args    := expr (',' expr)*
//! expr    := call | compare | named | literal | ident
//! compare := ident op literal
//! named   := ident '=' literal          // only when the value is not a comparison
//! op      := '>' | '>=' | '<' | '<=' | '==' | '!='
//! ```
//!
//! `=` is ambiguous between a named argument and an equality test, and the ambiguity is not
//! resolvable here: `field=amount` names an argument, `country="GB"` tests a value. Both are
//! parsed as [`Expr::Named`] and told apart in [`mod@crate::plan`], where the schema is available
//! to say which one `country` is. Guessing at parse time is how a language ends up with rules
//! nobody can state.

use crate::ast::{Call, Expr, Literal};
use crate::error::{PlanError, Result};

/// How deep a query may nest before it is refused.
///
/// **This is a bound on the stack, not a taste in queries.** The parser is recursive descent,
/// so nesting depth is call depth, and without a limit `Count(Union(Union(...` overflows the
/// stack and *aborts the process* - which on `big serve` means every other request in flight dies
/// with it. A stack overflow is not a panic and cannot be caught, so it cannot be turned into
/// a `Result` after the fact; the only fix is to never recurse that far.
///
/// 128 is far past anything a person writes and far short of where the default stack runs out.
/// Measured, not guessed: 1,000 levels parse fine on a main thread and 10,000 abort, so the
/// limit sits two orders of magnitude below the observed cliff. The margin is deliberate -
/// `big serve` runs queries on pool threads, whose stacks are smaller than the main thread's.
pub const MAX_DEPTH: usize = 128;

pub fn parse(input: &str) -> Result<Call> {
    let mut p = Parser { s: input.as_bytes(), i: 0, depth: 0 };
    p.space();
    let call = p.call()?;
    p.space();
    if p.i < p.s.len() {
        return Err(PlanError::TrailingInput { at: p.i });
    }
    Ok(call)
}

struct Parser<'a> {
    s: &'a [u8],
    i: usize,
    /// Call depth of the recursive descent, checked at every point that recurses.
    depth: usize,
}

impl<'a> Parser<'a> {
    fn space(&mut self) {
        while self.i < self.s.len() && self.s[self.i].is_ascii_whitespace() {
            self.i += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }

    fn eat(&mut self, c: u8) -> bool {
        if self.peek() == Some(c) {
            self.i += 1;
            true
        } else {
            false
        }
    }

    fn here(&self) -> String {
        match self.peek() {
            Some(c) => (c as char).to_string(),
            None => "end of input".to_string(),
        }
    }

    fn expect(&mut self, c: u8, want: &'static str) -> Result<()> {
        if self.eat(c) {
            Ok(())
        } else {
            Err(PlanError::Unexpected { at: self.i, found: self.here(), want })
        }
    }

    fn ident(&mut self) -> Result<String> {
        let start = self.i;
        while self.peek().is_some_and(|c| c.is_ascii_alphanumeric() || c == b'_') {
            self.i += 1;
        }
        if self.i == start {
            return Err(PlanError::Unexpected {
                at: self.i,
                found: self.here(),
                want: "an identifier",
            });
        }
        Ok(String::from_utf8_lossy(&self.s[start..self.i]).into_owned())
    }

    fn call(&mut self) -> Result<Call> {
        let name = self.ident()?;
        self.call_tail(name)
    }

    /// The `( args )` half, once the name is already in hand.
    ///
    /// The depth guard is here rather than in `expr` because this is the only place that can
    /// recurse without consuming a byte of grammar per level: one `(` buys a whole frame.
    fn call_tail(&mut self, name: String) -> Result<Call> {
        self.enter()?;
        self.space();
        self.expect(b'(', "`(`")?;
        let mut args = Vec::new();
        self.space();
        if !self.eat(b')') {
            loop {
                self.space();
                args.push(self.expr()?);
                self.space();
                if self.eat(b',') {
                    continue;
                }
                self.expect(b')', "`,` or `)`")?;
                break;
            }
        }
        self.leave();
        Ok(Call { name, args })
    }

    /// Enters one level of nesting, or refuses.
    ///
    /// Paired by hand with `leave` rather than by a guard type, because a guard would have to
    /// hold `&mut self` for the whole body it is guarding and nothing else could then parse.
    /// An early `?` leaves the counter high, which is harmless: the whole parse is already
    /// failing and this `Parser` is dropped without being read again.
    fn enter(&mut self) -> Result<()> {
        if self.depth >= MAX_DEPTH {
            return Err(PlanError::TooDeep { at: self.i, limit: MAX_DEPTH });
        }
        self.depth += 1;
        Ok(())
    }

    fn leave(&mut self) {
        self.depth -= 1;
    }

    fn expr(&mut self) -> Result<Expr> {
        match self.peek() {
            Some(b'"') => Ok(Expr::Literal(self.string()?)),
            Some(c) if c.is_ascii_digit() || self.starts_number(c) => {
                Ok(Expr::Literal(self.number()?))
            }
            _ => self.ident_led(),
        }
    }

    /// Something starting with an identifier: a call, a comparison, a named argument, or a
    /// bare name. Which one is only known after the identifier has been read, so the position
    /// is saved and restored rather than guessed at.
    fn ident_led(&mut self) -> Result<Expr> {
        let name = self.ident()?;
        let save = self.i;
        self.space();

        if self.peek() == Some(b'(') {
            return Ok(Expr::Call(self.call_tail(name)?));
        }

        if let Some(op) = self.operator() {
            self.space();
            // `=` takes any expression, so an argument can be another call. Every other
            // operator is a value comparison and only a literal makes sense on the right.
            if op == "=" {
                let value = self.expr()?;
                return Ok(Expr::Named { name, value: Box::new(value) });
            }
            let value = self.literal()?;
            return Ok(Expr::Compare { field: name, op, value });
        }

        self.i = save;
        // `true` and `false` belong to the literal grammar, not to the space of names. Without
        // this they read as bare identifiers, and `Row(active=true)` stops being a boolean.
        Ok(match name.as_str() {
            "true" => Expr::Literal(Literal::Bool(true)),
            "false" => Expr::Literal(Literal::Bool(false)),
            _ => Expr::Ident(name),
        })
    }

    /// Longest match first, so `>=` never reads as `>` followed by nonsense.
    fn operator(&mut self) -> Option<String> {
        for op in [">=", "<=", "==", "!=", ">", "<", "="] {
            if self.s[self.i..].starts_with(op.as_bytes()) {
                self.i += op.len();
                return Some(op.to_string());
            }
        }
        None
    }

    fn literal(&mut self) -> Result<Literal> {
        match self.peek() {
            Some(b'"') => self.string(),
            Some(c) if c.is_ascii_digit() || self.starts_number(c) => self.number(),
            _ => {
                let save = self.i;
                let word = self.ident()?;
                match word.as_str() {
                    "true" => Ok(Literal::Bool(true)),
                    "false" => Ok(Literal::Bool(false)),
                    _ => {
                        self.i = save;
                        Err(PlanError::Unexpected {
                            at: self.i,
                            found: word,
                            want: "a number, string, or true/false",
                        })
                    }
                }
            }
        }
    }

    /// Whether `c` at the current position begins a number rather than a name.
    ///
    /// Only `-` is ambiguous, and only because there is no arithmetic in this language: a minus
    /// can never be an operator here, so a minus followed by a digit is always a sign.
    fn starts_number(&self, c: u8) -> bool {
        c == b'-' && self.s.get(self.i + 1).is_some_and(|c| c.is_ascii_digit())
    }

    fn number(&mut self) -> Result<Literal> {
        let start = self.i;
        let negative = self.peek() == Some(b'-');
        if negative {
            self.i += 1;
        }
        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            self.i += 1;
        }
        // From after the sign: the digits are parsed on their own and the sign reapplied, so a
        // stray `-` can never reach `parse` and turn a range error into a syntax error.
        let digits_at = start + usize::from(negative);
        let whole = String::from_utf8_lossy(&self.s[digits_at..self.i]).into_owned();

        // A point followed by a digit continues the number. A point followed by anything else
        // is not part of it, so `Row(a > 1)` is never derailed by whatever comes next.
        if self.peek() == Some(b'.') && self.s.get(self.i + 1).is_some_and(|c| c.is_ascii_digit()) {
            self.i += 1;
            let frac_start = self.i;
            while self.peek().is_some_and(|c| c.is_ascii_digit()) {
                self.i += 1;
            }
            let frac = String::from_utf8_lossy(&self.s[frac_start..self.i]).into_owned();
            let scale =
                u8::try_from(frac.len()).map_err(|_| PlanError::NumberTooLarge { at: start })?;
            let units = format!("{whole}{frac}")
                .parse::<u64>()
                .map_err(|_| PlanError::NumberTooLarge { at: start })?;
            // A negative fractional number used to be refused here, because a decimal field is
            // unsigned and there was nowhere else for one to go. A float field holds one
            // perfectly well, and a lexer cannot see which kind of field a value is headed for -
            // so the shape is read and the refusal moved to `to_units`, which knows.
            if negative {
                let units = i64::try_from(units)
                    .map(|v| -v)
                    .map_err(|_| PlanError::NumberTooLarge { at: start })?;
                return Ok(Literal::Sdec { units, scale });
            }
            return Ok(Literal::Dec { units, scale });
        }

        if negative {
            return format!("-{whole}")
                .parse::<i64>()
                .map(Literal::Sint)
                .map_err(|_| PlanError::NumberTooLarge { at: start });
        }
        whole.parse::<u64>().map(Literal::Int).map_err(|_| PlanError::NumberTooLarge { at: start })
    }

    /// No escapes. A row key with a quote in it is a problem worth solving on purpose later,
    /// not by half-implementing backslashes now.
    fn string(&mut self) -> Result<Literal> {
        let open = self.i;
        self.expect(b'"', "`\"`")?;
        let start = self.i;
        while self.peek().is_some_and(|c| c != b'"') {
            self.i += 1;
        }
        if self.peek().is_none() {
            return Err(PlanError::UnterminatedString { at: open });
        }
        let text = String::from_utf8_lossy(&self.s[start..self.i]).into_owned();
        self.i += 1;
        Ok(Literal::Str(text))
    }
}
