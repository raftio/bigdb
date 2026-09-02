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

//! The expression half of a select-list entry.
//!
//! Precedence climbing over [`crate::scalar::BinOp`], with one wrinkle that is this dialect's
//! rather than the algorithm's: **the operand at the bottom may be an aggregate**. `avg(x)` and
//! `x` are both "the one number this entry is about", so both are parsed here into the same
//! leaf, and which of them it was is carried out in a [`Leaf`] for the caller to plan. That is
//! what lets `round(avg(amount), 2)` and `round(amount, 2)` share every line of this file.
//!
//! What is *not* here is a second expression grammar for `WHERE`. A condition selects a set
//! before any value has been read, so it has nothing these operators could apply to - see
//! [`Refused::ScalarFilter`], which this phase leaves exactly as it was.

use super::Parser;
use crate::ast::Proj;
use crate::error::{Refused, Result, SqlError};
use crate::lex::Tok;
use crate::scalar::{BinOp, Func, Scalar, UnOp};

/// What the one leaf of an expression turned out to be.
///
/// The parser does not plan anything, so an aggregate leaf is handed back as the [`Proj`] it
/// would have been on its own, plus the condition an `-If` or a `FILTER` gave it.
pub(super) struct Leaf {
    /// The projection the leaf is, as though the expression around it were not there.
    pub proj: Proj,
    /// `FILTER (WHERE ...)` or an `-If` suffix, when the leaf was an aggregate that carried one.
    pub filter: Option<crate::ast::Cond>,
}

/// One parsed expression: the tree, and the leaves it named.
pub(super) struct Parsed {
    /// The expression, with [`Scalar::Value`] standing in for the leaf.
    pub expr: Scalar,
    /// Every leaf, in the order met. Exactly one is legal - see [`Parser::item`].
    pub leaves: Vec<Leaf>,
}

impl Parser<'_> {
    /// A full expression, lowest precedence first.
    pub(super) fn expr(&mut self) -> Result<Parsed> {
        let mut leaves = Vec::new();
        let expr = self.climb(0, &mut leaves)?;
        Ok(Parsed { expr, leaves })
    }

    /// Precedence climbing: parse a unary, then absorb every operator that binds at least as
    /// tightly as `min`.
    fn climb(&mut self, min: u8, leaves: &mut Vec<Leaf>) -> Result<Scalar> {
        let mut left = self.unary(leaves)?;
        while let Some(op) = self.infix() {
            if op.precedence() < min {
                break;
            }
            self.i += 1;
            // Left-associative, so the right-hand side stops at anything binding as loosely as
            // this one: `a - b - c` is `(a - b) - c` and not `a - (b - c)`.
            let right = self.climb(op.precedence() + 1, leaves)?;
            left = Scalar::Binary { op, left: Box::new(left), right: Box::new(right) };
        }
        Ok(left)
    }

    /// The infix operator being looked at, without consuming it.
    ///
    /// `*` is [`Tok::Star`] because a select list uses it for "every column"; here, after an
    /// operand, there is no other reading of it than multiplication.
    fn infix(&self) -> Option<BinOp> {
        match self.peek()? {
            Tok::Op(o) => BinOp::compare(o),
            Tok::Arith(o) => BinOp::arith(o),
            Tok::Star => Some(BinOp::Mul),
            Tok::Word(w) if w.eq_ignore_ascii_case("AND") => Some(BinOp::And),
            Tok::Word(w) if w.eq_ignore_ascii_case("OR") => Some(BinOp::Or),
            _ => None,
        }
    }

    /// `-x`, `NOT x`, or a primary.
    fn unary(&mut self, leaves: &mut Vec<Leaf>) -> Result<Scalar> {
        if matches!(self.peek(), Some(Tok::Arith("-"))) {
            self.i += 1;
            // A `-` in front of a number is the number's sign and folds into it, which keeps
            // `-1` one literal rather than a negation of one and keeps the tree comparable to
            // what the corpus pins.
            if let Some(Tok::Num(n)) = self.peek() {
                let at = self.at();
                let n = crate::lex::negate(n, at)?;
                self.i += 1;
                return Ok(Scalar::Literal(n));
            }
            let arg = self.unary(leaves)?;
            return Ok(Scalar::Unary { op: UnOp::Neg, arg: Box::new(arg) });
        }
        if self.word_is("NOT") {
            self.i += 1;
            let arg = self.unary(leaves)?;
            return Ok(Scalar::Unary { op: UnOp::Not, arg: Box::new(arg) });
        }
        self.primary(leaves)
    }

    /// A literal, a bracketed expression, a `CASE`, a `CAST`, a call, or the column.
    fn primary(&mut self, leaves: &mut Vec<Leaf>) -> Result<Scalar> {
        let at = self.at();

        if self.eat(&Tok::LParen) {
            let inner = self.climb(0, leaves)?;
            self.expect(&Tok::RParen, ") to close the expression")?;
            return Ok(inner);
        }
        if matches!(self.peek(), Some(Tok::Num(_)) | Some(Tok::Str(_))) {
            return Ok(Scalar::Literal(self.literal("a value")?));
        }
        if self.word_is("CASE") {
            return self.case(leaves);
        }
        if self.word_is("CAST") {
            return self.cast(leaves);
        }
        if self.word_is("NULL") {
            return Err(self.refuse(Refused::Null));
        }
        if self.eat_word("TRUE") {
            return Ok(Scalar::Literal(big_plan::Literal::Bool(true)));
        }
        if self.eat_word("FALSE") {
            return Ok(Scalar::Literal(big_plan::Literal::Bool(false)));
        }

        // A name: a column, an aggregate, or a scalar call. Which one depends on what follows,
        // and only a `(` can make it a call.
        let name = self.name("a column, a value or a function")?;
        if name.qualifier.is_none() {
            // An aggregate is a leaf, not a call: it produces the one number this entry is
            // about, and everything around it is applied to that number.
            if let Some(a) = self.aggregate(&name.column)? {
                leaves.push(Leaf { proj: a.proj, filter: a.filter });
                return Ok(Scalar::Value);
            }
            if self.peek() == Some(&Tok::LParen) {
                return self.call(&name.column, at, leaves);
            }
        }
        if self.peek() == Some(&Tok::LParen) {
            // `a.count(*)` is not a spelling of anything: a qualified name is a column of a
            // named table, and a table has no methods.
            return Err(self.syntax("a column, not a call on a qualified name"));
        }
        leaves.push(Leaf { proj: Proj::Column(name), filter: None });
        Ok(Scalar::Value)
    }

    /// A scalar call, with the name read and the `(` still ahead.
    fn call(&mut self, name: &str, at: usize, leaves: &mut Vec<Leaf>) -> Result<Scalar> {
        // `now()` reads nothing, so it is a constant rather than a call over one. Kept apart
        // here because it is the only zero-argument name in the dialect.
        if name.eq_ignore_ascii_case("now") {
            self.i += 1;
            self.expect(&Tok::RParen, ") after now(")?;
            return Ok(Scalar::Now { unix_seconds: self.now });
        }
        // `toStartOfInterval` is ClickHouse's spelling of `date_trunc` and is refused by name
        // rather than by absence, so somebody who wrote it is told which spelling this dialect
        // takes. Checked before the table so the sentence survives the function existing.
        if name.eq_ignore_ascii_case("toStartOfInterval") {
            return Err(self.refuse_at(Refused::Interval, at));
        }
        // `if` and `multiIf` are `CASE WHEN` with the brackets moved, so they are parsed into
        // one tree rather than two. One evaluator, one printer, and no pair of spellings that
        // can come to disagree about what an absent `ELSE` answers.
        if name.eq_ignore_ascii_case("if") || name.eq_ignore_ascii_case("multiIf") {
            return self.if_call(at, leaves);
        }
        let Some(func) = Func::of(name) else {
            return Err(self.refuse_at(super::item::unsupported_call(name), at));
        };
        self.i += 1;

        let mut args = Vec::new();
        if !self.eat(&Tok::RParen) {
            loop {
                args.push(self.climb(0, leaves)?);
                if self.eat(&Tok::Comma) {
                    continue;
                }
                self.expect(&Tok::RParen, ", or ) to close the call")?;
                break;
            }
        }
        func.check_arity(args.len(), at)?;
        self.checked_unit(func, &args, at)?;
        Ok(Scalar::Call { func, args })
    }

    /// `if(cond, then, else)` and `multiIf(c1, v1, ..., else)`, with the `(` still ahead.
    ///
    /// One odd argument at the end is the `ELSE`; an even count has none, which SQL says
    /// answers null. `if` is `multiIf` with one pair, so both are read by the same loop.
    fn if_call(&mut self, at: usize, leaves: &mut Vec<Leaf>) -> Result<Scalar> {
        self.i += 1;
        let mut args = Vec::new();
        loop {
            args.push(self.climb(0, leaves)?);
            if self.eat(&Tok::Comma) {
                continue;
            }
            self.expect(&Tok::RParen, ", or ) to close the call")?;
            break;
        }
        // Two is `if(cond, then)`, which every dialect writes with an `else` or not at all.
        if args.len() < 3 {
            return Err(SqlError::Refused { what: Refused::Shape, at });
        }
        let default = if args.len() % 2 == 1 { args.pop().map(Box::new) } else { None };
        let mut arms = Vec::new();
        let mut rest = args.into_iter();
        while let (Some(when), Some(then)) = (rest.next(), rest.next()) {
            arms.push((when, then));
        }
        Ok(Scalar::Case { arms, default })
    }

    /// The calendar boundary a time function names, checked where it is written.
    ///
    /// `date_trunc('month', ts)` takes its unit as a quoted string, and a unit the calendar
    /// does not have is [`Refused::TruncUnit`] here rather than a null three layers down. A
    /// bare word is refused too, for the reason the old parser gave: `date_trunc(month, ts)`
    /// reads as two columns everywhere else in this dialect.
    fn checked_unit(&self, func: Func, args: &[Scalar], at: usize) -> Result<()> {
        if !matches!(func, Func::DateTrunc | Func::DateDiff | Func::DateAdd | Func::DateSub) {
            return Ok(());
        }
        let Some(Scalar::Literal(big_plan::Literal::Str(u))) = args.first() else {
            return Err(SqlError::Syntax {
                at,
                found: self.here(),
                want: "a quoted unit, like 'month'",
            });
        };
        match big_civil::Unit::parse(u) {
            Some(_) => Ok(()),
            None => Err(SqlError::Refused { what: Refused::TruncUnit, at }),
        }
    }

    /// `CAST(<expr> AS <type>)`, which is the three `to*` conversions under standard syntax.
    fn cast(&mut self, leaves: &mut Vec<Leaf>) -> Result<Scalar> {
        let at = self.at();
        self.i += 1;
        self.expect(&Tok::LParen, "( after CAST")?;
        let inner = self.climb(0, leaves)?;
        self.expect_word("AS", "AS inside CAST")?;
        let ty = self.bare_ident("a type name")?;
        // A width in brackets is accepted and ignored: `CAST(x AS DECIMAL(10, 2))` asks for a
        // representation, and what this can do is read the value as a number. The digits it
        // keeps are the field's, and nothing here can change them.
        if self.eat(&Tok::LParen) {
            while !self.eat(&Tok::RParen) {
                if self.peek().is_none() {
                    return Err(self.syntax(") to close the type"));
                }
                self.i += 1;
            }
        }
        self.expect(&Tok::RParen, ") to close CAST")?;

        let func = match cast_target(&ty) {
            Some(f) => f,
            // A type this engine has no reading for: a blob, a JSON document, an array. The
            // sentence for it is the column-type one, which names what does exist.
            None => return Err(SqlError::Refused { what: Refused::ColumnType, at }),
        };
        Ok(Scalar::Call { func, args: vec![inner] })
    }

    /// `CASE WHEN <cond> THEN <value> [...] [ELSE <value>] END`.
    ///
    /// Only the searched form. `CASE <expr> WHEN <value> THEN ...` is the same statement with
    /// the comparison factored out, and accepting both would mean two trees that have to answer
    /// identically - so the simple form is refused with the shape sentence rather than silently
    /// read as the other one.
    fn case(&mut self, leaves: &mut Vec<Leaf>) -> Result<Scalar> {
        let at = self.at();
        self.i += 1;
        let mut arms = Vec::new();
        while self.eat_word("WHEN") {
            let when = self.climb(0, leaves)?;
            self.expect_word("THEN", "THEN after the condition")?;
            let then = self.climb(0, leaves)?;
            arms.push((when, then));
        }
        if arms.is_empty() {
            // `CASE amount WHEN 500 THEN ...`: the simple form, which is the searched one with
            // the comparison factored out. Refused by name rather than as a syntax error, so
            // that somebody who wrote perfectly good SQL is told which spelling to use.
            return Err(SqlError::Refused { what: Refused::Case, at });
        }
        let default =
            if self.eat_word("ELSE") { Some(Box::new(self.climb(0, leaves)?)) } else { None };
        self.expect_word("END", "END to close CASE")?;
        Ok(Scalar::Case { arms, default })
    }
}

/// The conversion a `CAST` target names, under every spelling this dialect's column list takes.
///
/// The same table as `Parser::column_type` reads, collapsed: a cast asks how to *read* a value,
/// and `TINYINT` and `BIGINT` are one answer to that even though they are two fields.
fn cast_target(ty: &str) -> Option<Func> {
    const INTS: [&str; 10] = [
        "int", "integer", "tinyint", "smallint", "bigint", "uint", "signed", "int32", "int64",
        "uint64",
    ];
    const FLOATS: [&str; 7] =
        ["float", "real", "double", "float32", "float64", "decimal", "numeric"];
    const TEXTS: [&str; 4] = ["text", "varchar", "char", "string"];

    if INTS.iter().any(|t| ty.eq_ignore_ascii_case(t)) {
        return Some(Func::ToInt);
    }
    if FLOATS.iter().any(|t| ty.eq_ignore_ascii_case(t)) {
        return Some(Func::ToFloat);
    }
    if TEXTS.iter().any(|t| ty.eq_ignore_ascii_case(t)) {
        return Some(Func::ToStr);
    }
    if ty.eq_ignore_ascii_case("date") {
        return Some(Func::ToDate);
    }
    None
}
