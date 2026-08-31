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

//! What a cell's number is, and the three rules every caller shares about its absence.
//!
//! Absent is not zero, and the distinction is load-bearing in three places: a `HAVING` drops an
//! absent number, an ordering sorts it last, and a cell renders it `null`. Rolling it into zero
//! would make "no records" indistinguishable from "records that sum to nothing", which are two
//! different answers to every question here.

use super::Datum;
use crate::{Absent, Of, RowId, Value};

/// One cell of a single-row answer.
///
/// Split from [`number`] because one cell is not a number: `topK` holds the list of keys a
/// ranking produced, and a list is what ClickHouse's `topK` answers with too.
pub(super) fn scalar_cell(of: Of, values: &[Value], probes_at: usize) -> Datum {
    match of {
        // A search's answer sits after every call's, which is what `Answer::calls` records.
        Of::Probe { probe } => Datum::num(values.get(probes_at + probe).and_then(scalar_num)),
        Of::Keys { plan } => {
            Datum::Keys(
                values
                    .get(plan)
                    .and_then(Value::as_groups)
                    .unwrap_or(&[])
                    .iter()
                    // A group whose key this node has never been told is `null` in a row and is
                    // left out of a list, because a list of names is what was asked for.
                    .filter_map(|g| g.key.clone())
                    .collect(),
            )
        }
        of => Datum::num(number(of, values, None)),
    }
}

/// One number a shape asks for, out of the answers the plans produced.
///
/// `row` is the group being rendered, or `None` for an answer that is not grouped. `None` comes
/// back when there is no number: a `min` over records that hold no value, an average over no
/// records, a plan that answered nothing about this group. It is not zero, and every caller
/// keeps it apart from zero - a `HAVING` drops it, an ordering sorts it last, and a cell
/// renders `null`.
pub(super) fn number(of: Of, values: &[Value], row: Option<RowId>) -> Option<Num> {
    match of {
        // A key is a string, not a number. Reached only by a hand-built shape.
        Of::Key | Of::RightKey => None,
        // Reached only by a hand-built shape: `scalar_cell` reads a probe, because only it
        // knows where the searches' answers begin.
        Of::Probe { .. } => None,
        Of::Value { plan } => values.get(plan).and_then(scalar_num),
        // The counting step of `count(DISTINCT x)`, which happens here because here is after
        // the merge: a group that two nodes both hold is one group, and counting earlier would
        // count it twice.
        Of::Groups { plan } => {
            values.get(plan).and_then(Value::as_groups).map(|g| Num::Int(g.len() as i128))
        }
        // A group this plan said nothing about reads as the number it would have produced over
        // no records - see `Absent`. Only a `FILTER` can put a shape in that position.
        // A join's cells are read by `joined`, which pairs by key string rather than by row.
        // A list of keys is not a number and is rendered by `scalar_cell`.
        Of::Paired { .. } | Of::SharedKeys { .. } | Of::Keys { .. } => None,
        Of::Group { plan, absent } => match group_num(values.get(plan)?, row?) {
            Some(n) => Some(n),
            None => match absent {
                Absent::Zero => Some(Num::Int(0)),
                Absent::Null => None,
            },
        },
        Of::Ratio { plan, over } => {
            let top = match row {
                None => scalar_num(values.get(plan)?)?,
                Some(row) => group_num(values.get(plan)?, row)?,
            };
            let bottom = match row {
                None => scalar_num(values.get(over)?)?,
                Some(row) => group_num(values.get(over)?, row)?,
            };
            // No records is no average, which is `null` rather than a division by zero. The
            // same answer `min` gives over nothing, for the same reason.
            let bottom = as_f64(bottom);
            if bottom == 0.0 {
                return None;
            }
            Some(Num::Real(as_f64(top) / bottom))
        }
    }
}

/// One plan's whole answer, as a number.
pub(super) fn scalar_num(v: &Value) -> Option<Num> {
    Some(match v {
        Value::Count(n) => Num::Int(i128::from(*n)),
        // A total above `i128::MAX` needs 2^127 records each holding the largest value a `u64`
        // field can, so this saturates rather than growing every comparison a type.
        Value::Sum(n) => Num::Int(i128::try_from(*n).unwrap_or(i128::MAX)),
        Value::SignedSum(n) => Num::Int(*n),
        Value::Extreme(v) => Num::Int(i128::from((*v)?)),
        Value::SignedExtreme(v) => Num::Int(i128::from((*v)?)),
        Value::Groups(_) | Value::Rows(_) | Value::Table(_) | Value::Pairs(_) => return None,
    })
}

/// One group's number out of one plan's answer.
fn group_num(v: &Value, row: RowId) -> Option<Num> {
    v.as_groups()?.iter().find(|g| g.row == row).and_then(|g| scalar_num(&g.value))
}

/// A number a cell can hold: a count or a total, or the quotient an average is.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) enum Num {
    Int(i128),
    Real(f64),
}

fn as_f64(n: Num) -> f64 {
    match n {
        Num::Int(v) => v as f64,
        Num::Real(v) => v,
    }
}

/// The integer a `HAVING` compares against, or `None` for anything that is not one.
///
/// An average is deliberately not one: it is fractional and the comparison is not, and rounding
/// it into one would answer a question next to the one that was asked. The lowering refuses a
/// `HAVING` on an average before it reaches here; this is what makes that refusal safe rather
/// than merely tidy.
pub(super) fn int_of(n: Option<Num>) -> Option<i128> {
    match n {
        Some(Num::Int(v)) => Some(v),
        Some(Num::Real(_)) | None => None,
    }
}

/// Orders two numbers, with absent last in both directions.
///
/// Absent sorts last for the same reason it fails every `HAVING`: it is not a value, and
/// putting it at the top of a descending ranking would make "no value" outrank every value
/// there is.
pub(super) fn cmp_num(x: Option<Num>, y: Option<Num>, desc: bool) -> core::cmp::Ordering {
    match (x, y) {
        (Some(x), Some(y)) => {
            // `partial_cmp` is only `None` for a NaN, which no arithmetic here produces: the
            // one division is guarded against a zero denominator.
            let c = as_f64(x).partial_cmp(&as_f64(y)).unwrap_or(core::cmp::Ordering::Equal);
            // Integers are compared as integers, so two totals that differ by one still order
            // correctly past the range an `f64` counts exactly.
            let c = match (x, y) {
                (Num::Int(a), Num::Int(b)) => a.cmp(&b),
                _ => c,
            };
            if desc {
                c.reverse()
            } else {
                c
            }
        }
        (Some(_), None) => core::cmp::Ordering::Less,
        (None, Some(_)) => core::cmp::Ordering::Greater,
        (None, None) => core::cmp::Ordering::Equal,
    }
}
