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

//! Arithmetic and function calls over the value a column already gave up.
//!
//! # Why this is not part of the plan
//!
//! Everything here runs on a number that has *already been read*. The plan reads the column it
//! was going to read anyway, at the cost it was going to cost, and each of these is applied to
//! the value on its way into a cell - the same place a decimal has its point put back, and the
//! same place a rounded timestamp has always been truncated.
//!
//! That is what makes an expression evaluator affordable in an engine with no rows. It is also
//! the boundary: a scalar in a `WHERE` would have to be computed per record *before* the filter
//! chose any, and there is nothing below this to compute it with - see
//! [`crate::Refused::ScalarFilter`], which is unchanged.
//!
//! # One leaf
//!
//! A [`Scalar`] holds exactly one [`Scalar::Value`], which stands for the column the projection
//! read. `substring(country, 1, 2)` is one column and a scalar over it; `concat(country, tier)`
//! is two columns in one cell, and a projection is one plan reading one field per column - so
//! it is refused as a shape rather than accepted as an expression. See [`Scalar::leaves`],
//! which is what the parser checks.

use crate::error::{Refused, Result, SqlError};
use big_plan::Literal;

/// A computation over one already-read value.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Scalar {
    /// The value the plan read for this column. The one leaf.
    Value,
    /// A constant, exactly as written.
    Literal(Literal),
    /// `now()`: the instant the statement was read.
    ///
    /// A constant and not a leaf, because it reads no column - which is what lets
    /// `date_diff('day', ts, now())` be one column and a constant rather than two columns. The
    /// moment travels from the parser for the reason [`crate::ast::Proj::Now`] gives: one
    /// statement has one now, whichever node answers it.
    Now {
        /// Seconds since the Unix epoch.
        unix_seconds: i64,
    },
    /// `-x`, and `NOT x`.
    Unary {
        /// Which one.
        op: UnOp,
        /// What it applies to.
        arg: Box<Scalar>,
    },
    /// `a <op> b`.
    Binary {
        /// Which one.
        op: BinOp,
        /// The left-hand side.
        left: Box<Scalar>,
        /// The right-hand side.
        right: Box<Scalar>,
    },
    /// A named call, with its arguments in the order written.
    Call {
        /// Which one.
        func: Func,
        /// Its arguments. The arity is checked when it is parsed - see [`Func::arity`].
        args: Vec<Scalar>,
    },
    /// `CASE WHEN <cond> THEN <value> ... [ELSE <value>] END`, and `if`/`multiIf`, which are
    /// the same thing under two spellings and are parsed into this.
    ///
    /// An absent `ELSE` answers null, which is what SQL says and what `Datum::Null` already
    /// means - so it needs nothing here to represent it.
    Case {
        /// The `WHEN`/`THEN` pairs, in the order written. Never empty.
        arms: Vec<(Scalar, Scalar)>,
        /// `ELSE`, when one was written.
        default: Option<Box<Scalar>>,
    },
}

impl Scalar {
    /// How many times this expression names the column - which has to be exactly one.
    ///
    /// A projection is one plan reading one field per column, so an expression naming two
    /// columns has no plan to be, and one naming none is a constant wearing a column's clothes.
    /// Both are refused where the item is parsed, and both get [`Refused::Shape`]: the sentence
    /// there is that a statement answers one question, which is what each of them breaks.
    pub fn leaves(&self) -> usize {
        match self {
            Self::Value => 1,
            Self::Literal(_) | Self::Now { .. } => 0,
            Self::Unary { arg, .. } => arg.leaves(),
            Self::Binary { left, right, .. } => left.leaves() + right.leaves(),
            Self::Call { args, .. } => args.iter().map(Self::leaves).sum(),
            Self::Case { arms, default } => {
                arms.iter().map(|(w, t)| w.leaves() + t.leaves()).sum::<usize>()
                    + default.as_ref().map_or(0, |d| d.leaves())
            }
        }
    }

    /// The expression written back, with `_` where the column's value goes.
    ///
    /// **For `EXPLAIN`, and it has to be complete.** A printer that showed
    /// `date_trunc('month', ts) AS ts` and a bare `ts` identically would let a lost expression
    /// through, and this printer is what a reader checks the answer's arithmetic against. Round
    /// brackets are put on every binary node rather than only where precedence needs them: this
    /// is read by somebody checking what runs, and an unambiguous line is worth more than a
    /// tidy one.
    pub fn print(&self) -> String {
        match self {
            Self::Value => "_".to_string(),
            Self::Literal(l) => crate::explain::literal(l),
            Self::Now { .. } => "now()".to_string(),
            Self::Unary { op, arg } => match op {
                UnOp::Neg => format!("-{}", arg.print()),
                UnOp::Not => format!("NOT {}", arg.print()),
            },
            Self::Binary { op, left, right } => {
                format!("({} {} {})", left.print(), op.symbol(), right.print())
            }
            Self::Call { func, args } => {
                let args: Vec<String> = args.iter().map(Self::print).collect();
                format!("{}({})", func.name(), args.join(", "))
            }
            Self::Case { arms, default } => {
                let mut out = "CASE".to_string();
                for (when, then) in arms {
                    out.push_str(&format!(" WHEN {} THEN {}", when.print(), then.print()));
                }
                if let Some(d) = default {
                    out.push_str(&format!(" ELSE {}", d.print()));
                }
                out.push_str(" END");
                out
            }
        }
    }

    /// Whether this is the identity - the column, untouched.
    ///
    /// A bare column parses through the same path as an expression, and an identity carried as
    /// an expression would make every projection look like one to the layers below. This is
    /// what lets the parser hand back a plain [`crate::ast::Proj::Column`] instead.
    pub fn is_identity(&self) -> bool {
        matches!(self, Self::Value)
    }
}

/// `-x` and `NOT x`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum UnOp {
    /// `-x`
    Neg,
    /// `NOT x`, which is `x = 0` written the other way round.
    Not,
}

/// Every infix operator, arithmetic and comparison alike.
///
/// Comparisons are here rather than in a separate boolean tree because they answer with a
/// number: this engine has no boolean cell, and `amount > 500` is a `1` or a `0` exactly as it
/// is in ClickHouse. That is also what lets one live inside a `CASE WHEN` without a second kind
/// of node to hold it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BinOp {
    /// `+`
    Add,
    /// `-`
    Sub,
    /// `*`
    Mul,
    /// `/`
    Div,
    /// `%`
    Mod,
    /// `=`
    Eq,
    /// `!=`
    Ne,
    /// `<`
    Lt,
    /// `<=`
    Le,
    /// `>`
    Gt,
    /// `>=`
    Ge,
    /// `AND`
    And,
    /// `OR`
    Or,
}

impl BinOp {
    /// How this operator is written, which is how [`Scalar::print`] writes it back.
    pub fn symbol(self) -> &'static str {
        match self {
            Self::Add => "+",
            Self::Sub => "-",
            Self::Mul => "*",
            Self::Div => "/",
            Self::Mod => "%",
            Self::Eq => "=",
            Self::Ne => "!=",
            Self::Lt => "<",
            Self::Le => "<=",
            Self::Gt => ">",
            Self::Ge => ">=",
            Self::And => "AND",
            Self::Or => "OR",
        }
    }

    /// How tightly this binds. Higher wins, which is what the climbing parser compares.
    ///
    /// The usual SQL table: `OR` loosest, then `AND`, then comparison, then `+ -`, then `* / %`.
    pub fn precedence(self) -> u8 {
        match self {
            Self::Or => 1,
            Self::And => 2,
            Self::Eq | Self::Ne | Self::Lt | Self::Le | Self::Gt | Self::Ge => 3,
            Self::Add | Self::Sub => 4,
            Self::Mul | Self::Div | Self::Mod => 5,
        }
    }

    /// The comparison this operator is, under the spelling the lexer normalises to.
    pub fn compare(op: &str) -> Option<Self> {
        Some(match op {
            "=" => Self::Eq,
            "!=" => Self::Ne,
            "<" => Self::Lt,
            "<=" => Self::Le,
            ">" => Self::Gt,
            ">=" => Self::Ge,
            _ => return None,
        })
    }

    /// The arithmetic this operator is, under the spelling [`crate::lex::Tok::Arith`] carries.
    /// `*` is not here: it arrives as [`crate::lex::Tok::Star`], which a select list also uses.
    pub fn arith(op: &str) -> Option<Self> {
        Some(match op {
            "+" => Self::Add,
            "-" => Self::Sub,
            "/" => Self::Div,
            "%" => Self::Mod,
            _ => return None,
        })
    }
}

/// Every scalar function this dialect has.
///
/// **A closed list, and deliberately so.** An unknown name still earns the refusal it earned
/// before - see `crate::parse::item::unsupported_call` - because "no such function" is a worse
/// sentence than the one naming what exists. Adding a name here is the whole of adding a
/// function, apart from its arm in the evaluator.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Func {
    // ---- arithmetic -------------------------------------------------------------------
    /// `abs(x)`
    Abs,
    /// `round(x)` or `round(x, digits)`
    Round,
    /// `floor(x)`
    Floor,
    /// `ceil(x)`
    Ceil,
    /// `sqrt(x)`
    Sqrt,
    /// `exp(x)`
    Exp,
    /// `ln(x)`
    Ln,
    /// `log10(x)`
    Log10,
    /// `log2(x)`
    Log2,
    /// `pow(x, y)`
    Pow,
    /// `sign(x)`
    Sign,

    // ---- bit --------------------------------------------------------------------------
    /// `bitAnd(a, b)`
    BitAnd,
    /// `bitOr(a, b)`
    BitOr,
    /// `bitXor(a, b)`
    BitXor,
    /// `bitNot(a)`
    BitNot,
    /// `bitShiftLeft(a, n)`
    Shl,
    /// `bitShiftRight(a, n)`
    Shr,

    // ---- string -----------------------------------------------------------------------
    /// `lower(s)`
    Lower,
    /// `upper(s)`
    Upper,
    /// `length(s)` - in characters, not bytes.
    Length,
    /// `substring(s, from)` or `substring(s, from, len)`. One-based, as SQL says.
    Substring,
    /// `position(s, needle)` - one-based, zero when absent.
    Position,
    /// `concat(a, b, ...)`
    Concat,
    /// `trim(s)`
    Trim,
    /// `ltrim(s)`
    LTrim,
    /// `rtrim(s)`
    RTrim,
    /// `reverse(s)`
    Reverse,
    /// `startsWith(s, prefix)`
    StartsWith,
    /// `endsWith(s, suffix)`
    EndsWith,
    /// `splitByChar(sep, s)` - a list in one cell, which `Datum::Keys` already carries.
    SplitByChar,

    // ---- conversion -------------------------------------------------------------------
    /// `toInt64`, `toUInt64`, `toInt32`, `toUInt32`, and `CAST(x AS <integer>)`.
    ///
    /// One variant for all of them: the width is a promise about storage, and nothing here
    /// stores anything. What the call actually asks for is "read this as a whole number", and
    /// that is one operation however wide the name says it is.
    ToInt,
    /// `toFloat64`, `toFloat32`, and `CAST(x AS <float>)`.
    ToFloat,
    /// `toString`, and `CAST(x AS <text>)`.
    ToStr,

    // ---- choice -----------------------------------------------------------------------
    /// `coalesce(a, b, ...)` - the first that is not null.
    Coalesce,
    /// `nullIf(a, b)` - null when they are equal.
    NullIf,
    /// `ifNull(a, b)` - `b` when `a` is null.
    IfNull,

    // ---- time -------------------------------------------------------------------------
    /// `toDate(ts)`
    ToDate,
    /// `date_trunc(unit, ts)`. The unit is the first argument, as a quoted string.
    DateTrunc,
    /// `date_diff(unit, a, b)`
    DateDiff,
    /// `date_add(unit, n, ts)`
    DateAdd,
    /// `date_sub(unit, n, ts)`
    DateSub,
    /// `formatDateTime(ts, format)`
    FormatDateTime,
    /// `toYear(ts)`
    ToYear,
    /// `toMonth(ts)`
    ToMonth,
    /// `toDayOfMonth(ts)`
    ToDayOfMonth,
    /// `toHour(ts)`
    ToHour,
    /// `toMinute(ts)`
    ToMinute,
    /// `toSecond(ts)`
    ToSecond,

    // ---- json -------------------------------------------------------------------------
    //
    // **These read a `TEXT` column, and there is no `JSON` type behind them.** A keyed column
    // holds a string; this family reads a key out of that string on its way into a cell, which
    // is what every other scalar here does with the value it is given. So no `Datum` variant is
    // added, no storage changes, and a document stays a document.
    //
    // One level of key, named as a plain string - not a path. A path language is a second
    // grammar to keep in step with somebody else's, and the argument against it is the one
    // `sql_no_regex` makes: the scanner underneath is a few hundred bytes, and a full pointer
    // syntax is a dependency and a class of pathological input to go with it.
    /// `JSONExtractString(json, key)` - the key's value as text, unquoted.
    JsonExtractString,
    /// `JSONExtractInt(json, key)` - the key's value as a whole number.
    JsonExtractInt,
    /// `JSONExtractFloat(json, key)` - the key's value as a real.
    JsonExtractFloat,
    /// `JSONExtractRaw(json, key)` - the key's value as it was written, brackets and all.
    ///
    /// The one that answers for a nested object or an array: it hands back the text, which the
    /// next call can read a key out of in turn. That is what one level of key buys without a
    /// path language to go with it.
    JsonExtractRaw,
    /// `JSONHas(json, key)` - 1 when the key is there, 0 when it is not.
    ///
    /// A number rather than a boolean because a cell here is a [`big_plan::Literal`]-shaped
    /// thing and there is no boolean among them, which is also what ClickHouse answers with.
    JsonHas,
}

impl Func {
    /// The function a name spells, or `None` when the name is not one of these.
    ///
    /// Case-insensitive throughout, like every other name in this dialect. The ClickHouse
    /// spellings and the standard SQL ones are both accepted where they differ, and both land
    /// on one variant - two spellings of a function are two ways to write it, not two
    /// functions to keep in step.
    pub fn of(name: &str) -> Option<Self> {
        const NAMES: &[(&str, Func)] = &[
            ("abs", Func::Abs),
            ("round", Func::Round),
            ("floor", Func::Floor),
            ("ceil", Func::Ceil),
            ("ceiling", Func::Ceil),
            ("sqrt", Func::Sqrt),
            ("exp", Func::Exp),
            ("ln", Func::Ln),
            ("log", Func::Ln),
            ("log10", Func::Log10),
            ("log2", Func::Log2),
            ("pow", Func::Pow),
            ("power", Func::Pow),
            ("sign", Func::Sign),
            ("bitAnd", Func::BitAnd),
            ("bitOr", Func::BitOr),
            ("bitXor", Func::BitXor),
            ("bitNot", Func::BitNot),
            ("bitShiftLeft", Func::Shl),
            ("bitShiftRight", Func::Shr),
            ("lower", Func::Lower),
            ("lcase", Func::Lower),
            ("upper", Func::Upper),
            ("ucase", Func::Upper),
            ("length", Func::Length),
            ("char_length", Func::Length),
            ("character_length", Func::Length),
            ("substring", Func::Substring),
            ("substr", Func::Substring),
            ("position", Func::Position),
            ("concat", Func::Concat),
            ("trim", Func::Trim),
            ("ltrim", Func::LTrim),
            ("rtrim", Func::RTrim),
            ("reverse", Func::Reverse),
            ("startsWith", Func::StartsWith),
            ("endsWith", Func::EndsWith),
            ("splitByChar", Func::SplitByChar),
            ("toInt64", Func::ToInt),
            ("toUInt64", Func::ToInt),
            ("toInt32", Func::ToInt),
            ("toUInt32", Func::ToInt),
            ("toFloat64", Func::ToFloat),
            ("toFloat32", Func::ToFloat),
            ("toString", Func::ToStr),
            ("coalesce", Func::Coalesce),
            ("nullIf", Func::NullIf),
            ("ifNull", Func::IfNull),
            ("toDate", Func::ToDate),
            ("date_trunc", Func::DateTrunc),
            ("dateTrunc", Func::DateTrunc),
            ("date_diff", Func::DateDiff),
            ("dateDiff", Func::DateDiff),
            ("date_add", Func::DateAdd),
            ("dateAdd", Func::DateAdd),
            ("date_sub", Func::DateSub),
            ("dateSub", Func::DateSub),
            ("formatDateTime", Func::FormatDateTime),
            ("toYear", Func::ToYear),
            ("toMonth", Func::ToMonth),
            ("toDayOfMonth", Func::ToDayOfMonth),
            ("toHour", Func::ToHour),
            ("toMinute", Func::ToMinute),
            ("toSecond", Func::ToSecond),
            // Both the ClickHouse spelling and the lower-case one somebody will type. The
            // `json_query` alias is the standard's name for what `JSONExtractRaw` does.
            ("JSONExtractString", Func::JsonExtractString),
            ("json_extract_string", Func::JsonExtractString),
            ("JSONExtractInt", Func::JsonExtractInt),
            ("json_extract_int", Func::JsonExtractInt),
            ("JSONExtractFloat", Func::JsonExtractFloat),
            ("json_extract_float", Func::JsonExtractFloat),
            ("JSONExtractRaw", Func::JsonExtractRaw),
            ("json_extract_raw", Func::JsonExtractRaw),
            ("json_query", Func::JsonExtractRaw),
            ("JSONHas", Func::JsonHas),
            ("json_has", Func::JsonHas),
        ];
        NAMES.iter().find(|(n, _)| name.eq_ignore_ascii_case(n)).map(|(_, f)| *f)
    }

    /// How many arguments this call takes: the fewest, and the most when there is a most.
    pub fn arity(self) -> (usize, Option<usize>) {
        match self {
            Self::Abs
            | Self::Floor
            | Self::Ceil
            | Self::Sqrt
            | Self::Exp
            | Self::Ln
            | Self::Log10
            | Self::Log2
            | Self::Sign
            | Self::BitNot
            | Self::Lower
            | Self::Upper
            | Self::Length
            | Self::Trim
            | Self::LTrim
            | Self::RTrim
            | Self::Reverse
            | Self::ToInt
            | Self::ToFloat
            | Self::ToStr
            | Self::ToDate
            | Self::ToYear
            | Self::ToMonth
            | Self::ToDayOfMonth
            | Self::ToHour
            | Self::ToMinute
            | Self::ToSecond => (1, Some(1)),

            Self::JsonExtractString
            | Self::JsonExtractInt
            | Self::JsonExtractFloat
            | Self::JsonExtractRaw
            | Self::JsonHas => (2, Some(2)),

            Self::Round => (1, Some(2)),

            Self::Pow
            | Self::BitAnd
            | Self::BitOr
            | Self::BitXor
            | Self::Shl
            | Self::Shr
            | Self::Position
            | Self::StartsWith
            | Self::EndsWith
            | Self::SplitByChar
            | Self::NullIf
            | Self::IfNull
            | Self::DateTrunc
            | Self::FormatDateTime => (2, Some(2)),

            Self::Substring => (2, Some(3)),
            Self::DateDiff => (3, Some(3)),
            Self::DateAdd | Self::DateSub => (3, Some(3)),

            // Variadic, and one argument is a `coalesce` that coalesces nothing.
            Self::Concat | Self::Coalesce => (2, None),
        }
    }

    /// The name this call is written back under, which is the default column name.
    pub fn name(self) -> &'static str {
        match self {
            Self::Abs => "abs",
            Self::Round => "round",
            Self::Floor => "floor",
            Self::Ceil => "ceil",
            Self::Sqrt => "sqrt",
            Self::Exp => "exp",
            Self::Ln => "ln",
            Self::Log10 => "log10",
            Self::Log2 => "log2",
            Self::Pow => "pow",
            Self::Sign => "sign",
            Self::BitAnd => "bitAnd",
            Self::BitOr => "bitOr",
            Self::BitXor => "bitXor",
            Self::BitNot => "bitNot",
            Self::Shl => "bitShiftLeft",
            Self::Shr => "bitShiftRight",
            Self::Lower => "lower",
            Self::Upper => "upper",
            Self::Length => "length",
            Self::Substring => "substring",
            Self::Position => "position",
            Self::Concat => "concat",
            Self::Trim => "trim",
            Self::LTrim => "ltrim",
            Self::RTrim => "rtrim",
            Self::Reverse => "reverse",
            Self::StartsWith => "startsWith",
            Self::EndsWith => "endsWith",
            Self::SplitByChar => "splitByChar",
            Self::ToInt => "toInt64",
            Self::ToFloat => "toFloat64",
            Self::ToStr => "toString",
            Self::Coalesce => "coalesce",
            Self::NullIf => "nullIf",
            Self::IfNull => "ifNull",
            Self::ToDate => "toDate",
            Self::DateTrunc => "date_trunc",
            Self::DateDiff => "date_diff",
            Self::DateAdd => "date_add",
            Self::DateSub => "date_sub",
            Self::FormatDateTime => "formatDateTime",
            Self::ToYear => "toYear",
            Self::ToMonth => "toMonth",
            Self::ToDayOfMonth => "toDayOfMonth",
            Self::ToHour => "toHour",
            Self::ToMinute => "toMinute",
            Self::ToSecond => "toSecond",
            Self::JsonExtractString => "JSONExtractString",
            Self::JsonExtractInt => "JSONExtractInt",
            Self::JsonExtractFloat => "JSONExtractFloat",
            Self::JsonExtractRaw => "JSONExtractRaw",
            Self::JsonHas => "JSONHas",
        }
    }

    /// Checks the argument count, naming the call rather than the position.
    pub fn check_arity(self, got: usize, at: usize) -> Result<()> {
        let (lo, hi) = self.arity();
        if got < lo || hi.is_some_and(|h| got > h) {
            return Err(SqlError::Refused { what: Refused::Shape, at });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every name maps to a function, and every function is reachable by at least one name -
    /// which is what catches a variant added to the enum and left out of the table.
    #[test]
    fn every_function_has_a_name_that_finds_it() {
        for f in [
            Func::Abs,
            Func::Round,
            Func::Concat,
            Func::ToInt,
            Func::DateDiff,
            Func::SplitByChar,
            Func::ToSecond,
        ] {
            assert_eq!(Func::of(f.name()), Some(f), "{f:?} does not answer to its own name");
        }
    }

    /// The one leaf rule, which is what the parser checks a finished expression against.
    #[test]
    fn leaves_counts_the_column_and_not_the_constants() {
        let one = Scalar::Binary {
            op: BinOp::Add,
            left: Box::new(Scalar::Value),
            right: Box::new(Scalar::Literal(Literal::Int(1))),
        };
        assert_eq!(one.leaves(), 1);

        let two = Scalar::Binary {
            op: BinOp::Add,
            left: Box::new(Scalar::Value),
            right: Box::new(Scalar::Value),
        };
        assert_eq!(two.leaves(), 2);

        assert_eq!(Scalar::Literal(Literal::Int(1)).leaves(), 0);
    }

    /// Precedence is the usual SQL table, and the climbing parser is only as right as this is.
    #[test]
    fn multiplication_binds_tighter_than_addition_and_comparison_loosest() {
        assert!(BinOp::Mul.precedence() > BinOp::Add.precedence());
        assert!(BinOp::Add.precedence() > BinOp::Gt.precedence());
        assert!(BinOp::Gt.precedence() > BinOp::And.precedence());
        assert!(BinOp::And.precedence() > BinOp::Or.precedence());
    }
}
