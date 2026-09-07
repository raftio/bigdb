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

//! `WITH ROLLUP`, `WITH CUBE` and `GROUP BY GROUPING SETS`: one grouping per set, stacked.
//!
//! **No `Plan` variant and no merge arm.** Each set is the ordinary grouping the engine already
//! answers for that arity - no columns is a whole-set aggregate, one is a `GroupBy`, two or more
//! is a `GroupByTuple` - and what makes them one answer is that the rows are written one after
//! the other. That is the same stacking `UNION ALL` is, through the same [`Shape::Union`], which
//! is why this costs nothing below `big-sql`.
//!
//! What it *does* cost is the fan-out: one grouping per set, each planned, sent to every owner
//! and merged on its own. The intermediate results are deliberately **not** shared - folding a
//! `GroupBy(country)` out of an already-computed `GroupByTuple(country, city)` would be a per-set
//! rollup in the merge, which is the one thing this surface does not add. `MAX_GROUPING_SETS` is
//! where that price is named instead of hidden.
//!
//! The three clauses are one path because the parser normalised them into one: by the time this
//! runs, all it has is a list of subsets of the written `GROUP BY` list.

use super::grouped::measures_of;
use super::measure::{measure_of, units_of, Measure};
use super::pql::{call_of, field_arg};
use super::tuples::{tuple_args, tuple_measures};
use super::{answer, group_call, rows_of, Calls, Statement};
use crate::ast::{Grouping, GroupingSets, Item, Name, Proj, Select};
use crate::error::{Refused, Result, SqlError};
use crate::shape::{Cell, Cut, Of, Shape};
use big_plan::ast::Expr;

/// `sets` are the subsets; the columns they are subsets *of* are `select.group_by`, read here
/// rather than passed alongside so the two cannot arrive describing different lists.
pub(super) fn sets(
    select: &Select,
    table: &str,
    rows: &Expr,
    sets: &GroupingSets,
    stars: &[&Item],
    columns: &[(&Item, Name)],
    aggregates: &[&Item],
) -> Result<Statement> {
    let by = select.group_by.as_slice();
    refuse_what_no_set_can_hold(select, by, stars, columns, aggregates)?;

    let at = select.items.first().map_or(0, |i| i.at);
    // **One `Calls` for every set**, rather than `super::union`'s one per branch. Three things
    // follow, and all three are wanted: one `MAX_CALLS` budget across the whole statement, so
    // the round-trip count is bounded once; the dedupe applies across sets, so two that do ask
    // the same question ask it once; and no rebasing at all, because every index a branch
    // writes is already an index into the one list.
    let mut calls = Calls::new(at);
    let mut branches: Vec<Shape> = Vec::new();
    for set in &sets.of {
        branches.push(branch(select, table, rows, by, set, aggregates, &mut calls)?);
    }

    Ok(Statement {
        calls: calls.out,
        probes: Vec::new(),
        answer: answer(select, Shape::Union { branches }),
    })
}

/// What no grouping set can hold, refused once against the written `GROUP BY` list.
///
/// **Checked against the written list rather than per set, and that is the whole trick.** A bare
/// `b` in the select list of `GROUP BY a, b WITH ROLLUP` is legal, because the sets that name `b`
/// have a value for it and the sets that do not render it as `null`. Asking each set on its own
/// would refuse the `(a)` branch over exactly the column the subtotal exists to blank out.
///
/// The rest are the refusals a single grouping already earns, and they are the same here for the
/// same reasons - see `grouped::refuse_what_a_grouping_cannot_hold`, whose text this mirrors
/// because the mistakes are identical and only the list they are checked against differs.
fn refuse_what_no_set_can_hold(
    select: &Select,
    by: &[Grouping],
    stars: &[&Item],
    columns: &[(&Item, Name)],
    aggregates: &[&Item],
) -> Result<()> {
    if let Some(item) = stars.first() {
        return Err(SqlError::Refused { what: Refused::Shape, at: item.at });
    }
    // A repeated grouping column, which is one column written twice - the same argument
    // `GROUP BY c, c` gets. `GROUPING SETS` catches its own repeats while interning; this is
    // what catches `GROUP BY c, c WITH ROLLUP`.
    if by
        .iter()
        .enumerate()
        .any(|(i, g)| by[..i].iter().any(|earlier| earlier.name.column == g.name.column))
    {
        return Err(SqlError::Refused { what: Refused::Shape, at: by[0].at });
    }
    for (item, name) in columns {
        match by.iter().find(|g| g.name.column == name.column) {
            Some(g) if super::agrees(item, g) => {}
            _ => return Err(SqlError::Refused { what: Refused::Shape, at: item.at }),
        }
    }
    for item in aggregates {
        match item.leaf() {
            Proj::CountDistinct(_) => {
                return Err(SqlError::Refused { what: Refused::MultiDistinct, at: item.at })
            }
            Proj::TopKeys { .. } | Proj::Quantile { .. } => {
                return Err(SqlError::Refused { what: Refused::Shape, at: item.at })
            }
            _ => {}
        }
    }
    // **The ordering and the cut, refused together because they fail for one reason.** These
    // rows are several groupings rendered one after the other, so there is no single list for an
    // `ORDER BY` to sort or a `LIMIT` to cut - and applying either per set would look like an
    // ordered, cut answer while being neither. See `Refused::RollupOrder`, which says what to do
    // instead.
    if let Some(order) = &select.order_by {
        return Err(SqlError::Refused { what: Refused::RollupOrder, at: order.at });
    }
    if select.limit.is_some() || select.offset.is_some() || select.with_ties {
        let at = select.items.first().map_or(0, |i| i.at);
        return Err(SqlError::Refused { what: Refused::RollupOrder, at });
    }
    Ok(())
}

/// One set, as the shape that answers it.
///
/// The arity picks the machinery, and each of the three is the one an ordinary statement naming
/// those columns would have used: no columns is the whole-set aggregate `SELECT count(*) FROM t`
/// makes, one is `grouped`'s, and two or more are `tuples`'.
fn branch(
    select: &Select,
    table: &str,
    rows: &Expr,
    by: &[Grouping],
    set: &[usize],
    aggregates: &[&Item],
    calls: &mut Calls,
) -> Result<Shape> {
    let held: Vec<Grouping> = set.iter().map(|i| by[*i].clone()).collect();
    let mut keys: Vec<usize> = Vec::new();

    let measures = match held.as_slice() {
        [] => whole_set(table, rows, aggregates, calls)?,
        [one] => {
            let measures = measures_of(table, rows, one, aggregates, calls)?;
            // The plan that says which groups exist, for the same two cases `grouped` adds it
            // in: nothing was asked but the keys, or a `FILTER` means some plan is silent about
            // a group that nonetheless exists.
            if measures.is_empty() || aggregates.iter().any(|i| i.filter.is_some()) {
                let call = match one.bucket {
                    None => call_of("Distinct", vec![rows.clone(), field_arg(&one.name)]),
                    Some(_) => group_call(one, rows.clone(), None),
                };
                keys.push(calls.push(table, call)?);
            }
            measures
        }
        many => {
            let measures = tuple_measures(table, rows, many, aggregates, calls)?;
            if measures.is_empty() {
                keys.push(
                    calls.push(table, call_of("GroupByTuple", tuple_args(many, rows.clone())))?,
                );
            }
            measures
        }
    };

    // Every plan this branch reads, which is what `keys` means: the rows of a grouped answer are
    // the union of the groups its plans produced. Collected from the branch's own cells rather
    // than taken as `0..calls.len()`, because the list is now shared - a branch that named the
    // whole list would claim the plans of every set before it.
    for (_, of) in &measures {
        for plan in of.plans() {
            if !keys.contains(&plan) {
                keys.push(plan);
            }
        }
    }
    keys.sort_unstable();

    let cells = cells_for_set(select, table, by, set, &measures);
    let having = super::measure::having_of(select, table, &measures)?;

    Ok(match held.len() {
        0 => Shape::Row { cells, having },
        1 => Shape::Groups { keys, cells, having, order: None, cut: Cut::default() },
        n => Shape::Tuples { axes: n as u8, keys, cells, having, order: None, cut: Cut::default() },
    })
}

/// The aggregates of the empty grouping set: the whole filtered set, as one row.
///
/// The grand total, and it is an ordinary ungrouped answer - `count(*)`, `sum(x)`, an average as
/// a ratio of two. Written here rather than reached through `ungrouped`, which sorts bare columns
/// into a projection and would answer this branch with a `Shape::Table` of records.
///
/// Only four entries can arrive: `refuse_what_no_set_can_hold` has already turned away the
/// distinct counts, rankings and quantiles a grouping cannot hold, which is what makes the last
/// arm unreachable rather than a hole.
fn whole_set(
    table: &str,
    rows: &Expr,
    aggregates: &[&Item],
    calls: &mut Calls,
) -> Result<Vec<(Measure, Of)>> {
    let mut measures: Vec<(Measure, Of)> = Vec::new();
    for item in aggregates {
        let rows = rows_of(rows, item);
        let of = match item.leaf() {
            Proj::Count => Of::Value { plan: calls.push(table, call_of("Count", vec![rows]))? },
            Proj::Agg { func, field } => Of::Value {
                plan: calls.push(table, call_of(func.call(), vec![rows, field_arg(field)]))?,
            },
            Proj::Avg(field) => Of::Ratio {
                plan: calls.push(table, call_of("Sum", vec![rows.clone(), field_arg(field)]))?,
                over: calls.push(table, call_of("Count", vec![rows]))?,
            },
            Proj::Now { unix_seconds } => Of::Now { unix_seconds: *unix_seconds },
            Proj::Scalar { .. } => unreachable!("leaf() sees through the expression"),
            Proj::Star
            | Proj::Column(_)
            | Proj::CountDistinct(_)
            | Proj::TopKeys { .. }
            | Proj::Quantile { .. } => unreachable!("refused above"),
        };
        measures.push((measure_of(item), of));
    }
    Ok(measures)
}

/// One set's columns, in select-list order.
///
/// A key cell for each grouped column the set names, a `null` for each it does not, and the
/// measures in the order the select list wrote them.
///
/// **The null is what a subtotal row is.** A set that does not name `country` merges every
/// country into one row, so there is no value for that cell to hold - and [`Of::Const`] is how a
/// shape says a branch carries a constant without naming a plan that never ran. Its units stay
/// `PLAIN`: a null renders as a null whatever the column would have been, and giving it the
/// grouped field's units would send `Shape::resolve` looking up a field this branch never read.
fn cells_for_set(
    select: &Select,
    table: &str,
    by: &[Grouping],
    set: &[usize],
    measures: &[(Measure, Of)],
) -> Vec<Cell> {
    let mut next = measures.iter().map(|(_, of)| *of);
    select
        .items
        .iter()
        .map(|i| {
            // Which written grouping term this entry names, if it names one at all.
            let term = match i.leaf() {
                Proj::Column(n) => by.iter().position(|g| g.name.column == n.column),
                _ => None,
            };
            // ...and where that term sits in *this* set, which is the axis of the key.
            let axis = term.and_then(|t| set.iter().position(|held| *held == t));
            Cell {
                column: i.column(),
                of: match (term, axis) {
                    // A one-column set is a `Shape::Groups`, whose single key is `Of::Key`;
                    // anything wider is a `Shape::Tuples`, which counts its axes.
                    (Some(_), Some(_)) if set.len() == 1 => Of::Key,
                    (Some(_), Some(axis)) => Of::KeyAt { axis: axis as u8 },
                    // A grouped column this set does not name: the subtotal's blank.
                    (Some(_), None) => Of::Const { value: None },
                    (None, _) => {
                        next.next().expect("one measure per aggregate, in select-list order")
                    }
                },
                units: units_of(table, &i.proj),
                // The plan already rounded a bucket level, exactly as it does for a grouping of
                // one column - see `grouped::cells_of`. A blanked column has nothing to apply an
                // expression to either, and `null` is what it renders as regardless.
                apply: match (term.and_then(|t| by.get(t)), axis) {
                    (Some(_), None) => None,
                    (Some(g), Some(_)) if g.bucket.is_some() => None,
                    _ => i.apply().cloned(),
                },
            }
        })
        .collect()
}
