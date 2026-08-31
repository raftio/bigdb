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

//! A `WHERE` clause as the set of records it selects.
//!
//! Total: every shape the parser accepts has a set operation behind it, which is why nothing
//! here returns a `Result`. Everything that does not was refused in the parser.

use super::pql::{call, row, window};
use crate::ast::{Cond, Name};
use big_plan::ast::Expr;
use big_plan::Literal;

/// A condition as the set of records it selects.
///
/// Total: every shape the parser accepts has a set operation behind it, which is why this
/// returns an [`Expr`] rather than a `Result`. Everything that does not was refused already.
pub(super) fn rows(cond: &Cond) -> Expr {
    match cond {
        Cond::Cmp { field, op, value } => row(field, op, value.clone()),

        // One value is the comparison it already was; `Union` of one would be a call the reader
        // has to see through.
        Cond::In { field, values } => match values.as_slice() {
            [one] => row(field, "=", one.clone()),
            many => call("Union", many.iter().map(|v| row(field, "=", v.clone())).collect()),
        },

        Cond::Between { field, low, high } => {
            call("Intersect", vec![row(field, ">=", low.clone()), row(field, "<=", high.clone())])
        }

        Cond::Not(inner) => call("Not", vec![rows(inner)]),
        Cond::Or(..) => call("Union", flatten(cond, Kind::Or).iter().map(|c| rows(c)).collect()),
        Cond::And(..) => conjunction(cond),
    }
}

/// `a AND b AND NOT c` as `Difference(Intersect(a, b), c)`.
///
/// The rewrite is worth doing rather than emitting `Intersect(a, b, Not(c))`: `Not` has to build
/// the whole table's exists row to complement against, and `Difference` does not. Both answer
/// the same set; one of them reads a fragment nobody asked about.
pub(super) fn conjunction(cond: &Cond) -> Expr {
    let parts = windows(flatten(cond, Kind::And));
    let (negated, plain): (Vec<Term<'_>>, Vec<Term<'_>>) =
        parts.into_iter().partition(|t| matches!(t, Term::Plain(Cond::Not(_))));

    let inner: Vec<Expr> = negated
        .iter()
        .map(|t| match t {
            Term::Plain(Cond::Not(i)) => rows(i),
            _ => unreachable!("partitioned on this"),
        })
        .collect();
    let positive: Vec<Expr> = plain.iter().map(Term::lower).collect();

    // The negated terms are *unioned* before being subtracted: `a AND NOT b AND NOT c` keeps
    // what is in `a` and in neither of the others, which is `a - (b OR c)`. Intersecting them
    // would subtract only what is in both, and every record in exactly one would survive - a
    // wrong answer that no shape of the result could reveal.
    match (all_of(positive), any_of(inner)) {
        (Some(keep), Some(drop)) => call("Difference", vec![keep, drop]),
        (Some(keep), None) => keep,
        // Nothing but negations: the complement of everything they select together.
        (None, Some(drop)) => call("Not", vec![drop]),
        (None, None) => unreachable!("a conjunction has at least two terms"),
    }
}

/// One term of a conjunction: an ordinary condition, or a time window fused out of several.
enum Term<'a> {
    Plain(&'a Cond),
    /// `f = "k" AND f BETWEEN lo AND hi`, which is one question rather than two.
    Window {
        field: &'a Name,
        key: &'a str,
        from: Option<u64>,
        to: Option<u64>,
    },
}

impl Term<'_> {
    fn lower(&self) -> Expr {
        match self {
            Self::Plain(c) => rows(c),
            Self::Window { field, key, from, to } => window(field, key, *from, *to),
        }
    }
}

/// Fuses a key equality and the bounds written against the same column into one window.
///
/// **A time quantum field carries a key and a time**, so `visit = 'home' AND visit >= <t>`
/// reads as "this visit, at or after then" - one question about one field, which is exactly
/// what `Row(visit="home", from=…)` answers by reading that field's day views instead of
/// everything it ever recorded.
///
/// Purely syntactic. Nothing here knows whether the field is a time quantum one, and it must
/// not: the class is the planner's to check, and it now refuses a window over a field with no
/// views rather than answering with the empty set those used to produce.
fn windows(parts: Vec<&Cond>) -> Vec<Term<'_>> {
    let mut out: Vec<Term<'_>> = Vec::new();
    let mut used = vec![false; parts.len()];

    for (i, part) in parts.iter().enumerate() {
        let Cond::Cmp { field, op, value: Literal::Str(key) } = part else { continue };
        if *op != "=" && *op != "==" {
            continue;
        }
        // Every bound written against the same column, whichever way round.
        let (mut from, mut to, mut any) = (None, None, false);
        for (j, other) in parts.iter().enumerate() {
            if j == i || used[j] {
                continue;
            }
            match other {
                Cond::Between { field: f, low: Literal::Int(lo), high: Literal::Int(hi) }
                    if f.column == field.column && f.qualifier == field.qualifier =>
                {
                    (from, to, any) = (Some(*lo), Some(*hi), true);
                    used[j] = true;
                }
                Cond::Cmp { field: f, op, value: Literal::Int(v) }
                    if f.column == field.column && f.qualifier == field.qualifier =>
                {
                    // An exclusive bound is a bound: the views are per day, so a second's
                    // difference at the edge is below what this field records anyway.
                    match *op {
                        ">=" | ">" => (from, any) = (Some(*v), true),
                        "<=" | "<" => (to, any) = (Some(*v), true),
                        _ => continue,
                    }
                    used[j] = true;
                }
                _ => {}
            }
        }
        if any {
            used[i] = true;
            out.push(Term::Window { field, key, from, to });
        }
    }

    for (i, part) in parts.iter().enumerate() {
        if !used[i] {
            out.push(Term::Plain(part));
        }
    }
    out
}

/// Which associative operator is being flattened.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    And,
    Or,
}

/// `a AND (b AND c)` as three terms rather than a tree of two.
///
/// `Intersect` and `Union` are variadic in the query language and the executor short-circuits an
/// intersection that has already emptied, so a flat call is not only shorter to read - it stops
/// sooner.
fn flatten(cond: &Cond, kind: Kind) -> Vec<&Cond> {
    let pair = match (cond, kind) {
        (Cond::And(a, b), Kind::And) => Some((a, b)),
        (Cond::Or(a, b), Kind::Or) => Some((a, b)),
        _ => None,
    };
    match pair {
        Some((a, b)) => {
            let mut out = flatten(a, kind);
            out.extend(flatten(b, kind));
            out
        }
        None => vec![cond],
    }
}

/// One term as itself, several as an `Intersect`, none as nothing.
fn all_of(parts: Vec<Expr>) -> Option<Expr> {
    combine("Intersect", parts)
}

/// The same, for a union.
fn any_of(parts: Vec<Expr>) -> Option<Expr> {
    combine("Union", parts)
}

fn combine(name: &str, mut parts: Vec<Expr>) -> Option<Expr> {
    match parts.len() {
        0 => None,
        1 => parts.pop(),
        _ => Some(call(name, parts)),
    }
}
