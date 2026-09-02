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

//! Evaluating a [`Scalar`] over a cell that has already been built.
//!
//! # What this operates on, and why it matters
//!
//! **A [`Datum`], not a stored integer.** Everything up to here works in the units a field
//! stores, which is what keeps it exact: a decimal of scale two merges as `1250` where the
//! values were `12.50`. By the time an expression runs, that has already been turned into a
//! `Datum::Dec { units: 1250, scale: 2 }` - a number that knows what it stands for.
//!
//! That is the whole reason there is no unit arithmetic in this file. `amount / 100` means what
//! the reader wrote it to mean, because the left-hand side is `12.50` rather than `1250`, and
//! the shape does not have to carry an output unit for a renderer to interpret. It is also why
//! [`big_sql::Shape::resolve`] only *type-checks* an expression rather than resolving one.
//!
//! # Exactness
//!
//! Addition, subtraction and multiplication of two decimals stay decimal: the scales are
//! aligned, the integers are operated on, and the result carries the scale it earned. Division
//! is the one that leaves - a quotient of two decimals is not generally a decimal - and it
//! answers [`Datum::Real`], which is what every engine does with it. Nothing here routes an
//! exact value through a float on its way to an exact answer.
//!
//! # Absent values
//!
//! A record that holds no value in a field arrives as [`Datum::Null`], and almost everything
//! here answers `Null` for a `Null` argument. The exceptions are the ones whose whole job is to
//! be about absence - `coalesce`, `ifNull`, and a `CASE` whose condition is false.

use super::{fixed, Datum};
use big_civil::Unit;
use big_plan::Literal;
use big_sql::scalar::{BinOp, Func, Scalar, UnOp};

/// The value one expression produces, given the value its leaf read.
///
/// `value` is what the plan answered for this column - the projected value, or the merged
/// aggregate - and is what [`Scalar::Value`] stands for.
pub fn eval(expr: &Scalar, value: &Datum) -> Datum {
    match expr {
        Scalar::Value => value.clone(),
        Scalar::Literal(l) => literal(l),
        Scalar::Now { unix_seconds } => Datum::Timestamp(*unix_seconds),
        Scalar::Unary { op, arg } => unary(*op, &eval(arg, value)),
        Scalar::Binary { op, left, right } => binary(*op, &eval(left, value), &eval(right, value)),
        Scalar::Call { func, args } => {
            let args: Vec<Datum> = args.iter().map(|a| eval(a, value)).collect();
            call(*func, &args)
        }
        // The first arm whose condition is true. An absent `ELSE` answers null, which is what
        // SQL says and what a missing value already is here.
        Scalar::Case { arms, default } => {
            for (when, then) in arms {
                if truthy(&eval(when, value)) {
                    return eval(then, value);
                }
            }
            match default {
                Some(d) => eval(d, value),
                None => Datum::Null,
            }
        }
    }
}

/// A constant, as the cell it stands for.
fn literal(l: &Literal) -> Datum {
    match l {
        Literal::Int(v) => Datum::Int(i128::from(*v)),
        Literal::Sint(v) => Datum::Int(i128::from(*v)),
        Literal::Dec { units, scale } => Datum::Dec { units: i128::from(*units), scale: *scale },
        Literal::Sdec { units, scale } => Datum::Dec { units: i128::from(*units), scale: *scale },
        Literal::Str(s) => Datum::Text(s.clone()),
        // A boolean is a `1` or a `0` here, which is the same thing a comparison answers - this
        // engine has no boolean cell, and inventing one for a written `TRUE` would give
        // `active` and `TRUE` two different types.
        Literal::Bool(b) => Datum::Int(i128::from(*b)),
    }
}

/// Whether a value counts as true, which is what a `CASE` arm and `AND`/`OR` ask.
///
/// **Absent is false, not unknown.** The same two-valued reading `Having::holds` takes, and for
/// the same reason: there is no null to propagate here, a bit is set or it is not.
fn truthy(d: &Datum) -> bool {
    match d {
        Datum::Null => false,
        Datum::Int(v) => *v != 0,
        Datum::Dec { units, .. } => *units != 0,
        Datum::Real(v) => *v != 0.0,
        Datum::Date(v) | Datum::Timestamp(v) => *v != 0,
        Datum::Text(s) => !s.is_empty(),
        Datum::Keys(k) => !k.is_empty(),
    }
}

fn unary(op: UnOp, arg: &Datum) -> Datum {
    match op {
        UnOp::Not => match arg {
            Datum::Null => Datum::Null,
            other => Datum::Int(i128::from(!truthy(other))),
        },
        UnOp::Neg => match arg {
            Datum::Int(v) => Datum::Int(-v),
            Datum::Dec { units, scale } => Datum::Dec { units: -units, scale: *scale },
            Datum::Real(v) => Datum::Real(-v),
            _ => Datum::Null,
        },
    }
}

/// A number as a pair of (integer, scale), which is the form the exact arithmetic works in.
///
/// A date and a timestamp are counts, so they arrive here as the numbers they are - which is
/// what makes `seen + 3600` an hour later rather than a null.
fn as_fixed(d: &Datum) -> Option<(i128, u8)> {
    match d {
        Datum::Int(v) => Some((*v, 0)),
        Datum::Dec { units, scale } => Some((*units, *scale)),
        Datum::Date(v) | Datum::Timestamp(v) => Some((i128::from(*v), 0)),
        _ => None,
    }
}

/// Both numbers at the same scale, so that they can be added or compared as integers.
///
/// Widening rather than narrowing: `1.5` and `1.25` meet at two digits as `150` and `125`, and
/// nothing is rounded away to get there.
fn align(a: (i128, u8), b: (i128, u8)) -> Option<(i128, i128, u8)> {
    let scale = a.1.max(b.1);
    let lift = |(v, s): (i128, u8)| -> Option<i128> {
        v.checked_mul(10i128.checked_pow(u32::from(scale - s))?)
    };
    Some((lift(a)?, lift(b)?, scale))
}

/// A number as an `f64`, for the calls that have no exact answer.
fn as_real(d: &Datum) -> Option<f64> {
    match d {
        Datum::Real(v) => Some(*v),
        other => as_fixed(other).map(|(v, s)| v as f64 / 10f64.powi(i32::from(s))),
    }
}

/// Two numeric cells in order, or `None` when at least one is not a number.
///
/// Shared with the `ORDER BY` a projection carries, so that a comparison inside a `CASE` and a
/// comparison that decides which row comes first are the same comparison - including how a
/// decimal and an integer meet, which is at the wider scale rather than through a float.
pub(super) fn compare_numbers(l: &Datum, r: &Datum) -> Option<core::cmp::Ordering> {
    if let (Some(a), Some(b)) = (as_fixed(l), as_fixed(r)) {
        let (x, y, _) = align(a, b)?;
        return Some(x.cmp(&y));
    }
    as_real(l).zip(as_real(r)).and_then(|(a, b)| a.partial_cmp(&b))
}

fn binary(op: BinOp, l: &Datum, r: &Datum) -> Datum {
    match op {
        // Short-circuiting is not observable here - both sides are already evaluated, and
        // neither can fail or cost anything - so these are written as the boolean they are.
        BinOp::And => return Datum::Int(i128::from(truthy(l) && truthy(r))),
        BinOp::Or => return Datum::Int(i128::from(truthy(l) || truthy(r))),
        _ => {}
    }
    if matches!(l, Datum::Null) || matches!(r, Datum::Null) {
        return Datum::Null;
    }

    // Strings compare, and nothing else about them is arithmetic. Ordered by the bytes, which
    // is how a key is ordered everywhere else in this engine.
    if let (Datum::Text(a), Datum::Text(b)) = (l, r) {
        return match compare_op(op, a.cmp(b)) {
            Some(v) => Datum::Int(i128::from(v)),
            None => Datum::Null,
        };
    }

    // Exact where both sides are exact, which is every case but a float on one of them.
    if let (Some(a), Some(b)) = (as_fixed(l), as_fixed(r)) {
        if let Some(d) = exact(op, a, b, l, r) {
            return d;
        }
    }
    let (Some(a), Some(b)) = (as_real(l), as_real(r)) else { return Datum::Null };
    match op {
        BinOp::Add => Datum::Real(a + b),
        BinOp::Sub => Datum::Real(a - b),
        BinOp::Mul => Datum::Real(a * b),
        BinOp::Div if b == 0.0 => Datum::Null,
        BinOp::Div => Datum::Real(a / b),
        BinOp::Mod if b == 0.0 => Datum::Null,
        BinOp::Mod => Datum::Real(a % b),
        // A NaN on either side compares to nothing, which is what `partial_cmp` answering
        // `None` means and what every engine renders as an absent answer.
        other => match a.partial_cmp(&b).and_then(|ord| compare_op(other, ord)) {
            Some(v) => Datum::Int(i128::from(v)),
            None => Datum::Null,
        },
    }
}

/// Arithmetic on two exact numbers, or `None` where there is no exact answer to give.
///
/// The temporal cases are the reason `l` and `r` are still here: a date plus a number of days is
/// a date, and losing that would render the answer as the count it is stored as - the same
/// mistake reading a decimal back unscaled makes.
fn exact(op: BinOp, a: (i128, u8), b: (i128, u8), l: &Datum, r: &Datum) -> Option<Datum> {
    let temporal = |v: i128| match (l, r) {
        // Only when exactly one side is the moment: the difference of two dates is a number of
        // days rather than a date, which is what `date_diff` is for.
        (Datum::Date(_), Datum::Date(_)) | (Datum::Timestamp(_), Datum::Timestamp(_)) => {
            Datum::Int(v)
        }
        (Datum::Date(_), _) | (_, Datum::Date(_)) => Datum::Date(v as i64),
        (Datum::Timestamp(_), _) | (_, Datum::Timestamp(_)) => Datum::Timestamp(v as i64),
        _ => Datum::Int(v),
    };
    let fixed = |v: i128, scale: u8| match scale {
        0 => temporal(v),
        s => Datum::Dec { units: v, scale: s },
    };

    match op {
        BinOp::Add => {
            let (x, y, s) = align(a, b)?;
            Some(fixed(x.checked_add(y)?, s))
        }
        BinOp::Sub => {
            let (x, y, s) = align(a, b)?;
            Some(fixed(x.checked_sub(y)?, s))
        }
        // Scales add, which is what makes a product of two two-digit numbers a four-digit one -
        // exact, and the reason this is not routed through a float.
        BinOp::Mul => {
            let scale = a.1.checked_add(b.1)?;
            Some(fixed(a.0.checked_mul(b.0)?, scale))
        }
        // A quotient of two exact numbers is not generally exact, so division leaves the exact
        // world **for every row, including the ones that happen to come out whole**.
        //
        // Keeping the whole ones exact was the first thing this did, and it was wrong: over
        // `amount / 100` on 250, 1000 and 75 it would answer `Real`, `Int`, `Real` - one column
        // holding two types, decided by the data. That is the thing [`Datum`] says out loud it
        // exists to prevent, and a client would have to sniff each cell to read the column.
        //
        // Division by zero is the exception, and it is not a type: nothing divided by nothing
        // is absent, which is what every SQL engine answers and what `Null` already means.
        BinOp::Div => {
            let (_, y, _) = align(a, b)?;
            match y {
                0 => Some(Datum::Null),
                _ => None,
            }
        }
        BinOp::Mod => {
            let (x, y, s) = align(a, b)?;
            match y {
                0 => Some(Datum::Null),
                _ => Some(fixed(x % y, s)),
            }
        }
        cmp => {
            let (x, y, _) = align(a, b)?;
            compare_op(cmp, x.cmp(&y)).map(|v| Datum::Int(i128::from(v)))
        }
    }
}

/// Whether an ordering satisfies a comparison, or `None` when the operator is not one.
fn compare_op(op: BinOp, ord: core::cmp::Ordering) -> Option<bool> {
    use core::cmp::Ordering::*;
    Some(match op {
        BinOp::Eq => ord == Equal,
        BinOp::Ne => ord != Equal,
        BinOp::Lt => ord == Less,
        BinOp::Le => ord != Greater,
        BinOp::Gt => ord == Greater,
        BinOp::Ge => ord != Less,
        _ => return None,
    })
}

/// The text a value reads as, which is what every string call works on.
///
/// A date and a timestamp are written the way this engine writes them everywhere, rather than
/// as the counts they are stored as: `toString(day)` answering `19723` would be answering with
/// the storage rather than with the value.
fn text_of(d: &Datum) -> Option<String> {
    Some(match d {
        Datum::Null => return None,
        Datum::Text(s) => s.clone(),
        Datum::Int(v) => v.to_string(),
        Datum::Dec { units, scale } => fixed(*units, *scale),
        Datum::Real(v) => v.to_string(),
        Datum::Date(v) => super::date_text(*v),
        Datum::Timestamp(v) => super::timestamp_text(*v),
        Datum::Keys(k) => k.join(","),
    })
}

/// The count of seconds or days a temporal value carries, with which of the two it is.
///
/// **A written date counts as one.** `date_diff('month', d, '2024-03-31')` compares a column
/// against a moment somebody typed, and there is nowhere else for that string to become one:
/// unlike a `WHERE`, which converts its literal against the field it is compared to, an
/// expression is evaluated on values and only sees the text. Read the same two spellings
/// `big_civil` reads everywhere else, so a date means here exactly what it means in a `WHERE`.
fn moment(d: &Datum) -> Option<(i64, bool)> {
    match d {
        Datum::Date(v) => Some((*v, true)),
        Datum::Timestamp(v) => Some((*v, false)),
        Datum::Text(s) => match big_civil::parse_datetime(s) {
            Some(secs) => Some((secs, false)),
            None => big_civil::parse_date(s).map(|days| (days, true)),
        },
        _ => None,
    }
}

fn call(func: Func, args: &[Datum]) -> Datum {
    // Every call but these three answers null for a null argument, and saying so once here is
    // what keeps each arm below about the thing it computes.
    let absent = args.iter().any(|a| matches!(a, Datum::Null));
    if absent && !matches!(func, Func::Coalesce | Func::IfNull | Func::NullIf) {
        return Datum::Null;
    }
    match func {
        Func::Coalesce => {
            args.iter().find(|a| !matches!(a, Datum::Null)).cloned().unwrap_or(Datum::Null)
        }
        Func::IfNull => match args {
            [Datum::Null, fallback] => fallback.clone(),
            [v, _] => v.clone(),
            _ => Datum::Null,
        },
        Func::NullIf => match args {
            [a, b] if binary(BinOp::Eq, a, b) == Datum::Int(1) => Datum::Null,
            [a, _] => a.clone(),
            _ => Datum::Null,
        },
        _ => computed(func, args),
    }
}

/// The calls that are arithmetic or text, once absence has been dealt with above.
fn computed(func: Func, args: &[Datum]) -> Datum {
    let real = |i: usize| args.get(i).and_then(as_real);
    let int = |i: usize| args.get(i).and_then(as_fixed).map(|(v, s)| v / 10i128.pow(u32::from(s)));
    let text = |i: usize| args.get(i).and_then(text_of);
    let some = |o: Option<Datum>| o.unwrap_or(Datum::Null);

    match func {
        // ---- arithmetic ---------------------------------------------------------------
        Func::Abs => match args.first() {
            Some(Datum::Int(v)) => Datum::Int(v.abs()),
            Some(Datum::Dec { units, scale }) => Datum::Dec { units: units.abs(), scale: *scale },
            Some(Datum::Real(v)) => Datum::Real(v.abs()),
            _ => Datum::Null,
        },
        // `round(x)` and `round(x, digits)`. Exact on a decimal, because rounding one is
        // moving its point rather than approximating it.
        Func::Round => some(round(args)),
        Func::Floor => some(real(0).map(|v| Datum::Real(v.floor())).map(whole)),
        Func::Ceil => some(real(0).map(|v| Datum::Real(v.ceil())).map(whole)),
        Func::Sqrt => some(real(0).map(|v| Datum::Real(v.sqrt()))),
        Func::Exp => some(real(0).map(|v| Datum::Real(v.exp()))),
        Func::Ln => some(real(0).map(|v| Datum::Real(v.ln()))),
        Func::Log10 => some(real(0).map(|v| Datum::Real(v.log10()))),
        Func::Log2 => some(real(0).map(|v| Datum::Real(v.log2()))),
        Func::Pow => some(real(0).zip(real(1)).map(|(a, b)| Datum::Real(a.powf(b)))),
        Func::Sign => {
            some(real(0).map(|v| Datum::Int(i128::from(v.partial_cmp(&0.0).map_or(0, sign_of)))))
        }

        // ---- bit ----------------------------------------------------------------------
        Func::BitAnd => some(int(0).zip(int(1)).map(|(a, b)| Datum::Int(a & b))),
        Func::BitOr => some(int(0).zip(int(1)).map(|(a, b)| Datum::Int(a | b))),
        Func::BitXor => some(int(0).zip(int(1)).map(|(a, b)| Datum::Int(a ^ b))),
        Func::BitNot => some(int(0).map(|a| Datum::Int(!a))),
        Func::Shl => some(int(0).zip(int(1)).and_then(|(a, b)| shift(a, b, true))),
        Func::Shr => some(int(0).zip(int(1)).and_then(|(a, b)| shift(a, b, false))),

        // ---- string -------------------------------------------------------------------
        Func::Lower => some(text(0).map(|s| Datum::Text(s.to_lowercase()))),
        Func::Upper => some(text(0).map(|s| Datum::Text(s.to_uppercase()))),
        // In characters rather than bytes, which is what a reader of a keyed column means.
        Func::Length => some(text(0).map(|s| Datum::Int(s.chars().count() as i128))),
        Func::Trim => some(text(0).map(|s| Datum::Text(s.trim().to_string()))),
        Func::LTrim => some(text(0).map(|s| Datum::Text(s.trim_start().to_string()))),
        Func::RTrim => some(text(0).map(|s| Datum::Text(s.trim_end().to_string()))),
        Func::Reverse => some(text(0).map(|s| Datum::Text(s.chars().rev().collect()))),
        Func::StartsWith => {
            some(text(0).zip(text(1)).map(|(s, p)| Datum::Int(i128::from(s.starts_with(&p)))))
        }
        Func::EndsWith => {
            some(text(0).zip(text(1)).map(|(s, p)| Datum::Int(i128::from(s.ends_with(&p)))))
        }
        Func::Substring => some(substring(args)),
        // One-based, and zero for absent - which is SQL's convention and ClickHouse's both.
        Func::Position => some(text(0).zip(text(1)).map(|(s, n)| {
            Datum::Int(s.find(&n).map_or(0, |b| s[..b].chars().count() as i128 + 1))
        })),
        Func::Concat => {
            let parts: Option<Vec<String>> = args.iter().map(text_of).collect();
            some(parts.map(|p| Datum::Text(p.concat())))
        }
        // The separator first, as ClickHouse writes it.
        Func::SplitByChar => some(text(0).zip(text(1)).map(|(sep, s)| match sep.chars().next() {
            Some(c) => Datum::Keys(s.split(c).map(str::to_string).collect()),
            None => Datum::Keys(vec![s]),
        })),

        // ---- conversion ---------------------------------------------------------------
        Func::ToInt => some(real(0).map(|v| Datum::Int(v.trunc() as i128))),
        Func::ToFloat => some(real(0).map(Datum::Real)),
        Func::ToStr => some(text(0).map(Datum::Text)),

        // ---- time ---------------------------------------------------------------------
        // A day count in, a day count out. Over a `DATE` there is nothing to do, which is why
        // the shape no longer has to rewrite this into an identity truncation - the value says
        // which of the two it is.
        Func::ToDate => match args.first().and_then(moment) {
            Some((v, true)) => Datum::Date(v),
            Some((v, false)) => Datum::Date(big_civil::to_days(v)),
            None => Datum::Null,
        },
        Func::DateTrunc => some(unit_arg(args).zip(args.get(1).and_then(moment)).map(
            |(u, (v, days))| match days {
                true => Datum::Date(big_civil::truncate_days(v, u)),
                false => Datum::Timestamp(big_civil::truncate(v, u)),
            },
        )),
        Func::DateDiff => some(date_diff(args)),
        Func::DateAdd => some(date_shift(args, 1)),
        Func::DateSub => some(date_shift(args, -1)),
        Func::FormatDateTime => some(
            args.first()
                .and_then(seconds_of)
                .zip(text(1))
                .map(|(secs, f)| Datum::Text(format_moment(secs, &f))),
        ),
        Func::ToYear
        | Func::ToMonth
        | Func::ToDayOfMonth
        | Func::ToHour
        | Func::ToMinute
        | Func::ToSecond => some(
            args.first().and_then(seconds_of).map(|s| Datum::Int(i128::from(part_of(func, s)))),
        ),

        // Handled in `call`, which is where absence decides the answer.
        Func::Coalesce | Func::NullIf | Func::IfNull => Datum::Null,
    }
}

/// `-1`, `0` or `1`, from an ordering against zero.
fn sign_of(ord: core::cmp::Ordering) -> i8 {
    match ord {
        core::cmp::Ordering::Less => -1,
        core::cmp::Ordering::Equal => 0,
        core::cmp::Ordering::Greater => 1,
    }
}

/// A whole `Real` read back as an `Int`, so that `floor(x)` answers a number rather than a
/// number with a `.0` on it.
fn whole(d: Datum) -> Datum {
    match d {
        Datum::Real(v) if v.fract() == 0.0 && v.abs() < 9e18 => Datum::Int(v as i128),
        other => other,
    }
}

/// A shift by a count that has to fit in the width being shifted.
///
/// A shift by more than the width is undefined in Rust and zero in most SQL engines; answering
/// null instead says the question had no answer rather than inventing one.
fn shift(a: i128, b: i128, left: bool) -> Option<Datum> {
    let n = u32::try_from(b).ok()?;
    if n >= 127 {
        return Some(Datum::Null);
    }
    Some(Datum::Int(if left { a.checked_shl(n)? } else { a.checked_shr(n)? }))
}

/// `round(x)` and `round(x, digits)`.
///
/// Exact on a decimal: rounding one is moving its point and carrying, rather than approximating
/// it through a float and hoping the digits survive.
fn round(args: &[Datum]) -> Option<Datum> {
    let digits = match args.get(1) {
        None => 0,
        Some(d) => as_fixed(d).map(|(v, s)| v / 10i128.pow(u32::from(s)))?,
    };
    let digits = i32::try_from(digits).ok()?;
    match args.first()? {
        // Already whole, and rounding a whole number to more digits leaves it whole.
        Datum::Int(v) if digits >= 0 => Some(Datum::Int(*v)),
        Datum::Dec { units, scale } => {
            let want = u8::try_from(digits.max(0)).ok()?;
            match want >= *scale {
                // Widening keeps every digit, so there is nothing to round away.
                true => Some(Datum::Dec { units: *units, scale: *scale }),
                false => {
                    let drop = 10i128.checked_pow(u32::from(scale - want))?;
                    let rounded = round_div(*units, drop);
                    Some(match want {
                        0 => Datum::Int(rounded),
                        w => Datum::Dec { units: rounded, scale: w },
                    })
                }
            }
        }
        other => {
            let v = as_real(other)?;
            let f = 10f64.powi(digits);
            Some(whole(Datum::Real((v * f).round() / f)))
        }
    }
}

/// Integer division that rounds half away from zero, which is what SQL's `round` does and what
/// a reader of `round(12.345, 2)` expects to see.
fn round_div(v: i128, by: i128) -> i128 {
    let half = by / 2;
    match v >= 0 {
        true => (v + half) / by,
        false => (v - half) / by,
    }
}

/// `substring(s, from)` and `substring(s, from, len)`, one-based over characters.
fn substring(args: &[Datum]) -> Option<Datum> {
    let s = text_of(args.first()?)?;
    let from = as_fixed(args.get(1)?).map(|(v, sc)| v / 10i128.pow(u32::from(sc)))?;
    // One-based, and anything at or below zero starts at the beginning - which is what every
    // engine does with it rather than refusing.
    let skip = usize::try_from(from.max(1) - 1).ok()?;
    let taken: Box<dyn Iterator<Item = char>> = match args.get(2) {
        None => Box::new(s.chars().skip(skip)),
        Some(n) => {
            let n = as_fixed(n).map(|(v, sc)| v / 10i128.pow(u32::from(sc)))?;
            let n = usize::try_from(n.max(0)).ok()?;
            Box::new(s.chars().skip(skip).take(n))
        }
    };
    Some(Datum::Text(taken.collect()))
}

/// The calendar boundary a time call's first argument names.
fn unit_arg(args: &[Datum]) -> Option<Unit> {
    match args.first() {
        Some(Datum::Text(u)) => Unit::parse(u),
        _ => None,
    }
}

/// `date_diff(unit, a, b)` - how many whole units from `a` to `b`.
///
/// Calendar units are counted on the calendar rather than by dividing a second count: a month
/// is not a fixed number of seconds, and `date_diff('month', '2024-01-31', '2024-03-01')` has
/// to answer 1 rather than whatever 30 days happens to divide to. The clock units below a day
/// are the division, because there they are exact.
fn date_diff(args: &[Datum]) -> Option<Datum> {
    let unit = unit_arg(args)?;
    let a = seconds_of(args.get(1)?)?;
    let b = seconds_of(args.get(2)?)?;
    let n = match unit {
        Unit::Second => b.checked_sub(a)?,
        Unit::Minute => b.div_euclid(60) - a.div_euclid(60),
        Unit::Hour => b.div_euclid(3_600) - a.div_euclid(3_600),
        Unit::Day => b.div_euclid(SECS_PER_DAY) - a.div_euclid(SECS_PER_DAY),
        Unit::Week => (big_civil::truncate(b, Unit::Week) - big_civil::truncate(a, Unit::Week))
            .div_euclid(SECS_PER_DAY * 7),
        // Whole calendar steps, which is the count of month boundaries crossed.
        Unit::Month | Unit::Quarter | Unit::Year => {
            let (x, y) = (big_civil::decompose(a), big_civil::decompose(b));
            let months = (y.year - x.year) * 12 + i64::from(y.month) - i64::from(x.month);
            match unit {
                Unit::Month => months,
                Unit::Quarter => months.div_euclid(3),
                _ => months.div_euclid(12),
            }
        }
    };
    Some(Datum::Int(i128::from(n)))
}

/// `date_add(unit, n, ts)` and `date_sub`, which is the same call with the count negated.
fn date_shift(args: &[Datum], sign: i64) -> Option<Datum> {
    let unit = unit_arg(args)?;
    let n = whole_of(args.get(1)?)?;
    let n = i64::try_from(n).ok()?.checked_mul(sign)?;
    let (v, days) = moment(args.get(2)?)?;
    let secs = if days { v.checked_mul(SECS_PER_DAY)? } else { v };

    let moved = match unit {
        Unit::Second => secs.checked_add(n)?,
        Unit::Minute => secs.checked_add(n.checked_mul(60)?)?,
        Unit::Hour => secs.checked_add(n.checked_mul(3_600)?)?,
        Unit::Day => secs.checked_add(n.checked_mul(SECS_PER_DAY)?)?,
        Unit::Week => secs.checked_add(n.checked_mul(SECS_PER_DAY * 7)?)?,
        // On the calendar, and clamped to the length of the month landed in - which is what
        // makes adding a month to the 31st answer the 30th rather than sliding into the next
        // month. Every engine does this and the ones that do not are the ones with the bug.
        Unit::Month | Unit::Quarter | Unit::Year => {
            let step = match unit {
                Unit::Month => n,
                Unit::Quarter => n.checked_mul(3)?,
                _ => n.checked_mul(12)?,
            };
            let c = big_civil::decompose(secs);
            let total = (c.year * 12 + i64::from(c.month) - 1).checked_add(step)?;
            let (year, month) = (total.div_euclid(12), total.rem_euclid(12) as u32 + 1);
            let day = c.day.min(big_civil::days_in_month(year, month));
            big_civil::days_from_civil(year, month, day).checked_mul(SECS_PER_DAY)?
                + secs.rem_euclid(SECS_PER_DAY)
        }
    };
    Some(match days {
        true => Datum::Date(moved.div_euclid(SECS_PER_DAY)),
        false => Datum::Timestamp(moved),
    })
}

/// One field out of a moment.
fn part_of(func: Func, secs: i64) -> i64 {
    let c = big_civil::decompose(secs);
    match func {
        Func::ToYear => c.year,
        Func::ToMonth => i64::from(c.month),
        Func::ToDayOfMonth => i64::from(c.day),
        Func::ToHour => i64::from(c.hour),
        Func::ToMinute => i64::from(c.minute),
        Func::ToSecond => i64::from(c.second),
        _ => 0,
    }
}

/// A moment as a second count, whichever of the two temporal kinds it is.
fn seconds_of(d: &Datum) -> Option<i64> {
    moment(d).and_then(seconds_of_parts)
}

/// The same, from the pair [`moment`] already handed back.
fn seconds_of_parts((v, days): (i64, bool)) -> Option<i64> {
    match days {
        true => v.checked_mul(SECS_PER_DAY),
        false => Some(v),
    }
}

/// A number with its scale divided out, for the arguments that are counts rather than values.
fn whole_of(d: &Datum) -> Option<i128> {
    as_fixed(d).map(|(v, s)| v / 10i128.pow(u32::from(s)))
}

/// A moment under a `strftime`-style format, with the handful of specifiers a client actually
/// writes. An unknown one is left as it was written rather than eaten, so a format with a typo
/// in it shows the typo.
fn format_moment(secs: i64, format: &str) -> String {
    let c = big_civil::decompose(secs);
    let mut out = String::new();
    let mut chars = format.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '%' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('Y') => out.push_str(&format!("{:04}", c.year)),
            Some('m') => out.push_str(&format!("{:02}", c.month)),
            Some('d') => out.push_str(&format!("{:02}", c.day)),
            Some('H') => out.push_str(&format!("{:02}", c.hour)),
            Some('M') => out.push_str(&format!("{:02}", c.minute)),
            Some('S') => out.push_str(&format!("{:02}", c.second)),
            Some('F') => out.push_str(&super::date_text(secs.div_euclid(SECS_PER_DAY))),
            Some('%') => out.push('%'),
            Some(other) => {
                out.push('%');
                out.push(other);
            }
            None => out.push('%'),
        }
    }
    out
}

/// Seconds in a day. The same constant `big_civil` keeps privately, and it is here rather than
/// borrowed because these calls are about SQL's calendar rather than about the storage's.
const SECS_PER_DAY: i64 = 86_400;
