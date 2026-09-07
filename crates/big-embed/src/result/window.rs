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

//! Window columns, filled over the rows a projection already read.
//!
//! **This is the coordinator and it has to be**, for the reason the module above gives about
//! `HAVING` and `avg`: a partition is every row that shares a value, and a node holds a share of
//! them. `big_exec::merge_projected` puts the shares together; this runs once, afterwards, over
//! the whole answer.
//!
//! That is also why a window forces the full read. A function has to see every row of its
//! partition before it knows any one row's number, so the `LIMIT` cannot ride in the plan - see
//! `big_sql::Selection::Over`, which states the cost where it is written.
//!
//! **No frame.** Every function here is over the whole partition in the order given, which is
//! the default frame for the ranking and offset families and the only one this surface has. An
//! ordering under an aggregate window is refused at the text rather than answered as a running
//! total - see `sql_window_frame`.

use super::num::Num;
use super::{order_by_keys, Datum, Row};
use big_exec::Projected;
use big_sql::{Frame, Selected, Selection, Units, WinFunc};

/// Fills every window column of a projection, in place.
///
/// `table` is the plan's own answer - the raw values per record, in the order the plan read its
/// fields - rather than the rendered rows, because a window partitions and orders on the
/// **stored** value. Two output columns can read one field under two different expressions, and
/// a window that had to pick one of them would be picking silently.
pub(super) fn fill(out: &mut [Row], table: &[Projected], columns: &[Selected]) {
    for (at, column) in columns.iter().enumerate() {
        let Selection::Over { func, arg, offset, window } = &column.of else { continue };
        // The rows in the order this window sees them, which is its own and no other's: two
        // windows in one select list can partition and order differently, so each sorts its own
        // view rather than the answer.
        let order = ordered(table, window);
        for part in partitions(table, &order, &window.partition) {
            for (rank, filled) in values(func, *offset, *arg, table, part, column).into_iter() {
                out[part[rank]][at] = filled;
            }
        }
    }
}

/// The rows in the order one window sees them: by partition, then by its own ordering, then by
/// the order they arrived in.
///
/// **The last of those three is not a detail.** Record order is the only tie-break available
/// here and the only one a reader can predict, and it is the same one an `ORDER BY` over a
/// projection already promises - so a `row_number()` over rows this ordering cannot tell apart
/// is at least reproducible.
fn ordered(table: &[Projected], window: &Frame) -> Vec<usize> {
    let keys: Vec<Vec<Datum>> = table
        .iter()
        .map(|p| {
            window
                .partition
                .iter()
                .chain(window.order.iter().map(|(at, _)| at))
                .map(|at| stored(p, *at))
                .collect()
        })
        .collect();
    // The partition columns come first and always ascending - they group rather than order, so
    // which way they run changes nothing but has to be *some* way.
    let by: Vec<(usize, bool)> = (0..window.partition.len())
        .map(|i| (i, false))
        .chain(
            window
                .order
                .iter()
                .enumerate()
                .map(|(i, (_, desc))| (window.partition.len() + i, *desc)),
        )
        .collect();

    let mut out: Vec<usize> = (0..table.len()).collect();
    out.sort_by(|a, b| order_by_keys(&keys[*a], &keys[*b], &by).then(a.cmp(b)));
    out
}

/// The ordered rows, cut into the partitions `PARTITION BY` names.
///
/// Consecutive runs, because [`ordered`] sorted by the partition columns first. An empty
/// `PARTITION BY` is one partition of every row, which is what the clause means when it is
/// absent.
fn partitions<'a>(
    table: &[Projected],
    order: &'a [usize],
    partition: &[usize],
) -> Vec<&'a [usize]> {
    if partition.is_empty() {
        return match order.is_empty() {
            true => Vec::new(),
            false => vec![order],
        };
    }
    let same = |a: usize, b: usize| {
        partition.iter().all(|at| stored(&table[a], *at) == stored(&table[b], *at))
    };
    let mut out = Vec::new();
    let mut start = 0;
    for i in 1..order.len() {
        if !same(order[i - 1], order[i]) {
            out.push(&order[start..i]);
            start = i;
        }
    }
    if start < order.len() {
        out.push(&order[start..]);
    }
    out
}

/// One column's value for every row of one partition, as `(position in the partition, cell)`.
fn values(
    func: &WinFunc,
    offset: u32,
    arg: Option<usize>,
    table: &[Projected],
    part: &[usize],
    column: &Selected,
) -> Vec<(usize, Datum)> {
    let n = part.len();
    let step = offset as usize;
    // The value the function reads, rendered the way this column renders - so `lag(price)`
    // carries the point its field keeps, exactly as `price` itself would.
    let read = |i: usize| -> Datum {
        match arg {
            None => Datum::Null,
            Some(at) => table[part[i]]
                .values
                .get(at)
                .map(|v| Datum::projected(v, &column.units, column.apply.as_ref()))
                .unwrap_or(Datum::Null),
        }
    };
    // The stored numbers a fold runs over, absent values left out - the same rule every other
    // aggregate here follows, and the reason `avg` divides by what it counted rather than by the
    // partition's size.
    let folded: Vec<Num> = match arg {
        None => Vec::new(),
        Some(at) => part.iter().filter_map(|r| number(&table[*r], at)).collect(),
    };

    (0..n)
        .map(|i| {
            let cell = match func {
                WinFunc::RowNumber => Datum::Int((i + 1) as i128),
                WinFunc::Rank => Datum::Int(rank_at(table, part, i) as i128),
                WinFunc::DenseRank => Datum::Int(dense_rank_at(table, part, i) as i128),
                WinFunc::NTile => Datum::Int(ntile_at(i, n, step.max(1)) as i128),
                // `(rank - 1) / (rows - 1)`, and zero for a partition of one - which is the
                // standard's answer and avoids a division by nothing.
                WinFunc::PercentRank => Datum::Real(match n {
                    0 | 1 => 0.0,
                    _ => (rank_at(table, part, i) - 1) as f64 / (n - 1) as f64,
                }),
                // The share of the partition at or before this row in the ordering, so every row
                // of a tie carries the same number - which is what distinguishes it from
                // `row_number() / n`.
                WinFunc::CumeDist => {
                    Datum::Real(last_of_tie(table, part, i) as f64 / n.max(1) as f64)
                }
                WinFunc::Lag => match i.checked_sub(step) {
                    Some(j) => read(j),
                    None => Datum::Null,
                },
                WinFunc::Lead => match i + step < n {
                    true => read(i + step),
                    false => Datum::Null,
                },
                WinFunc::FirstValue => read(0),
                // **The partition's last row, which is the whole point of having no frame.**
                // Under the standard's default frame this would be the current row, which is a
                // number nobody wants and everybody is surprised by.
                WinFunc::LastValue => read(n - 1),
                WinFunc::NthValue => match step.checked_sub(1).filter(|k| *k < n) {
                    Some(k) => read(k),
                    None => Datum::Null,
                },
                // A count is of rows rather than of values: `count(*) OVER (...)` is the
                // partition's size, and `count(x)` is how many of them hold a value.
                WinFunc::Count => Datum::Int(match arg {
                    None => n as i128,
                    Some(_) => folded.len() as i128,
                }),
                WinFunc::Sum => fold(&folded, &column.units, |a, b| a + b),
                WinFunc::Min => fold(&folded, &column.units, f64::min),
                WinFunc::Max => fold(&folded, &column.units, f64::max),
                // No values is no average, which is `null` rather than a division by zero - the
                // same answer `min` gives over nothing, for the same reason.
                WinFunc::Avg => match folded.len() {
                    0 => Datum::Null,
                    k => Datum::Real(folded.iter().map(as_f64).sum::<f64>() / k as f64),
                },
            };
            (i, cell)
        })
        .collect()
}

/// A fold over a partition's stored numbers, rendered in this column's units.
///
/// Integers stay integers so a total is exact; a column holding floats folds as floats, which is
/// what it already is. Absent is absent rather than zero - a fold over no values is the `null`
/// an extreme over nothing already answers with.
fn fold(values: &[Num], units: &Units, f: impl Fn(f64, f64) -> f64) -> Datum {
    if values.is_empty() {
        return Datum::Null;
    }
    let exact: Option<Vec<i128>> = values
        .iter()
        .map(|n| match n {
            Num::Int(v) => Some(*v),
            Num::Real(_) => None,
        })
        .collect();
    match exact {
        Some(ints) => {
            let first = ints[0] as f64;
            let folded = ints.iter().skip(1).fold(first, |acc, v| f(acc, *v as f64));
            Datum::num(Some(Num::Int(folded as i128)), units)
        }
        None => {
            let first = as_f64(&values[0]);
            Datum::Real(values.iter().skip(1).fold(first, |acc, v| f(acc, as_f64(v))))
        }
    }
}

/// `rank()`: one more than the number of rows strictly before this one in the ordering, so ties
/// share a number and the next one skips.
fn rank_at(table: &[Projected], part: &[usize], i: usize) -> usize {
    let mut at = i;
    while at > 0 && same_order(table, part, at - 1, i) {
        at -= 1;
    }
    at + 1
}

/// `dense_rank()`: how many distinct orderings have been seen up to here, so the next one after a
/// tie does not skip.
fn dense_rank_at(table: &[Projected], part: &[usize], i: usize) -> usize {
    (1..=i).filter(|j| !same_order(table, part, *j - 1, *j)).count() + 1
}

/// The position of the last row tied with this one, counting from one - which is what
/// `cume_dist` is a share of.
fn last_of_tie(table: &[Projected], part: &[usize], i: usize) -> usize {
    let mut at = i;
    while at + 1 < part.len() && same_order(table, part, i, at + 1) {
        at += 1;
    }
    at + 1
}

/// Which of `buckets` as-equal-as-possible parts of a partition of `n` a row falls in.
///
/// The remainder goes to the earlier buckets, one each, which is what every dialect does and
/// what makes `ntile` over four rows and three buckets read `1, 1, 2, 3`.
fn ntile_at(i: usize, n: usize, buckets: usize) -> usize {
    let big = n % buckets;
    let small = n / buckets;
    let boundary = big * (small + 1);
    match i < boundary {
        true => i / (small + 1) + 1,
        false => big + (i - boundary) / small.max(1) + 1,
    }
}

/// Whether two rows of a partition are indistinguishable to the window's own ordering.
///
/// Compared on the **stored** values, for the reason [`stored`] gives.
fn same_order(table: &[Projected], part: &[usize], a: usize, b: usize) -> bool {
    table[part[a]].values == table[part[b]].values
}

/// One field of one record, as the cell it compares as.
///
/// **Plain units on purpose.** These values are only ever compared against other values of the
/// same field, and a scale is a constant multiplier - so the stored integer orders exactly as
/// the number it stands for does, and rendering the point first would buy nothing. Where the
/// value is *answered* rather than compared, `values` renders it in the column's own units.
fn stored(p: &Projected, at: usize) -> Datum {
    p.values.get(at).map(Datum::read_plain).unwrap_or(Datum::Null)
}

/// The stored number in one field, or `None` where the record holds nothing.
fn number(p: &Projected, at: usize) -> Option<Num> {
    match p.values.get(at)? {
        big_exec::Projection::Int(v) => Some(Num::Int(*v)),
        big_exec::Projection::Real(v) => Some(Num::Real(*v)),
        _ => None,
    }
}

fn as_f64(n: &Num) -> f64 {
    match n {
        Num::Int(v) => *v as f64,
        Num::Real(v) => *v,
    }
}
