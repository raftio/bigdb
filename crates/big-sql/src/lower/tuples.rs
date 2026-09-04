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

//! `GROUP BY a, b` — one row per combination of two keyed columns.
//!
//! **Not a composite key, which this index never stored.** For each value of the left column
//! the records holding it are a set, and grouping *those* by the right column is the ordinary
//! grouping the engine already does. So a pair grouping is one grouping per value of the left
//! column — and the number of those is what it costs, which is why [`MAX_LEFT`] travels in the
//! plan rather than being applied to the answer.

use super::measure::{field_of, having_tree, measure_of, names, units_of, Measure};
use super::pql::{as_expr, call_of, field_arg, named};
use super::{answer, rows_of, Calls, Statement};
use crate::ast::{Grouping, HavingAgg, Item, Name, OrderKey, Proj, Select};
use crate::error::{Refused, Result, SqlError};
use crate::shape::{Cell, Cut, GroupOrder, Of, OrderBy, Shape};
use big_plan::ast::{Expr, Literal};

/// How many passes over an inner column one grouping may make.
///
/// **A bound on the passes, not a taste in groupings.** Grouping by several columns is one pass
/// over the next per combination of the ones before it, so this number is what the statement
/// costs. Checked at the top of every level, which is what makes one budget enough for any
/// arity: the frontier after the first column *is* the pass count for the second, so the product
/// is bounded without anybody multiplying cardinalities nobody knows in advance.
///
/// A thousand is past any dashboard's dimensions and short of a grouping that would quietly
/// become a scan; past it the executor refuses by name rather than truncating, because an answer
/// with fewer groups than exist is one nothing could reveal.
pub const MAX_PASSES: u64 = 1_000;

pub(super) fn tuples(
    select: &Select,
    table: &str,
    rows: &Expr,
    by: &[Grouping],
    stars: &[&Item],
    columns: &[(&Item, Name)],
    aggregates: &[&Item],
) -> Result<Statement> {
    let at = select.items.first().map_or(0, |i| i.at);
    if let Some(item) = stars.first() {
        return Err(SqlError::Refused { what: Refused::Shape, at: item.at });
    }
    // `GROUP BY c, c` is `GROUP BY c` written twice, and the combination of a column with itself
    // is every record paired with itself. Refused rather than answered as either.
    if by
        .iter()
        .enumerate()
        .any(|(i, g)| by[..i].iter().any(|earlier| earlier.name.column == g.name.column))
    {
        return Err(SqlError::Refused { what: Refused::Shape, at });
    }
    // Every bare column must be one of the grouped ones - the classic SQL rule, and a real one
    // here: a column that is neither grouped nor aggregated has no single value per combination
    // - and must describe the same values that grouping term does. See `super::agrees`.
    for (item, name) in columns {
        match by.iter().find(|g| g.name.column == name.column) {
            Some(g) if super::agrees(item, g) => {}
            _ => return Err(SqlError::Refused { what: Refused::Shape, at: item.at }),
        }
    }

    let mut calls = Calls::new(at);
    let mut measures: Vec<(Measure, Of)> = Vec::new();
    for item in aggregates {
        let rows = rows_of(rows, item);
        let mut args = tuple_args(by, rows);
        match &item.proj {
            // A count needs no aggregate argument: a grouping counts its records anyway.
            Proj::Count => {}
            Proj::Agg { func, field } => {
                args.push(named("aggregate", as_expr(call_of(func.call(), vec![field_arg(field)]))))
            }
            // Everything else is a second question about the combination rather than a measure
            // of it: an average is a ratio of two, a distinct count is another column to group
            // by, and a ranking is one more.
            _ => return Err(SqlError::Refused { what: Refused::Shape, at: item.at }),
        }
        let plan = calls.push(table, call_of("GroupByTuple", args))?;
        measures.push((measure_of(item), Of::Group { plan, absent: absent_of(item) }));
    }
    // Only the keys were asked for: the counts the grouping produces anyway are what a
    // `HAVING count(*)` reads.
    if measures.is_empty() {
        calls.push(table, call_of("GroupByTuple", tuple_args(by, rows.clone())))?;
    }

    // Column order follows the select list. Which axis of the key a column is comes from which of
    // the grouped ones it names, so `SELECT b, a ... GROUP BY a, b` renders them as asked.
    let mut next = measures.iter().map(|(_, of)| *of);
    let cells: Vec<Cell> = select
        .items
        .iter()
        .map(|i| {
            let axis = match i.leaf() {
                Proj::Column(n) => by.iter().position(|g| g.name.column == n.column),
                _ => None,
            };
            Cell {
                column: i.column(),
                of: match axis {
                    Some(axis) => Of::KeyAt { axis: axis as u8 },
                    None => next.next().expect("one measure per aggregate, in select-list order"),
                },
                units: units_of(table, &i.proj),
                // The plan already rounded a bucket level, exactly as it does for a grouping of
                // one column - see `grouped::cells_of`.
                apply: match (axis.and_then(|a| by.get(a)), i.apply()) {
                    (Some(g), _) if g.bucket.is_some() => None,
                    (_, apply) => apply.cloned(),
                },
            }
        })
        .collect();

    let having = match &select.having {
        None => None,
        Some(h) => {
            let number = |a: &_, at: usize| {
                names(a, &measures).ok_or(SqlError::Refused { what: Refused::Having, at })
            };
            let units = |a: &_, _at: usize| Ok(field_of(a).map(|f| (table.to_string(), f)));
            Some(having_tree(h, &number, &units)?)
        }
    };

    let order = ordering(select, by, &measures)?;
    Ok(Statement {
        calls: calls.out.clone(),
        probes: Vec::new(),
        answer: answer(
            select,
            Shape::Tuples {
                axes: by.len() as u8,
                keys: (0..calls.out.len()).collect(),
                cells,
                having,
                order,
                cut: Cut {
                    offset: select.offset.map(|n| n as usize),
                    limit: select.limit.map(|n| n as usize),
                    ties: select.with_ties,
                },
            },
        ),
    })
}

/// The bitmap, the levels and the budget: everything a `GroupByTuple` takes but its aggregate.
///
/// A repeating `by=`, so the arity is a number rather than a shape. A bucket level is a nested
/// `Bucket(...)` because it carries a boundary and a budget of its own, and a bare name cannot
/// say either.
fn tuple_args(by: &[Grouping], rows: Expr) -> Vec<Expr> {
    let mut args = vec![rows];
    for g in by {
        args.push(named(
            "by",
            match g.bucket {
                None => Expr::Ident(g.name.column.clone()),
                Some(unit) => as_expr(call_of(
                    "Bucket",
                    vec![
                        field_arg(&g.name),
                        named("unit", Expr::Literal(Literal::Str(unit.name().to_string()))),
                        named("n", Expr::Literal(Literal::Int(super::MAX_BUCKETS))),
                    ],
                )),
            },
        ));
    }
    args.push(named("n", Expr::Literal(Literal::Int(MAX_PASSES))));
    args
}

/// What a cell holds for a combination its plan said nothing about, which only a `FILTER` can
/// produce.
fn absent_of(item: &Item) -> crate::shape::Absent {
    use crate::ast::Agg;
    use crate::shape::Absent;
    match &item.proj {
        Proj::Agg { func: Agg::Min | Agg::Max, .. } => Absent::Null,
        _ => Absent::Zero,
    }
}

/// The `ORDER BY` of a pair grouping, which orders the pairs.
///
/// There is no ranking to fold into the plan here: `TopN` ranks one column's groups, and what is
/// being ordered is a combination of two. So every ordering is a sort of the merged answer, and
/// the cost of that - every pair held at the coordinator before the cut - is what `MAX_LEFT`
/// already bounds.
fn ordering(
    select: &Select,
    by: &[Grouping],
    measures: &[(Measure, Of)],
) -> Result<Option<GroupOrder>> {
    let Some(order) = &select.order_by else { return Ok(None) };
    let refuse = || SqlError::Refused { what: Refused::Order, at: order.at };

    let value_names: Vec<(String, Of)> = select
        .items
        .iter()
        .filter(|i| !matches!(i.proj, Proj::Column(_)))
        .map(Item::column)
        .zip(measures.iter().map(|(_, of)| *of))
        .collect();

    let by_value = match &order.key {
        OrderKey::Count => names(&HavingAgg::Count, measures),
        OrderKey::Agg { func, field } => {
            names(&HavingAgg::Agg { func: *func, field: field.clone() }, measures)
        }
        OrderKey::Avg(_) => return Err(refuse()),
        OrderKey::Name(n) => match value_names.iter().find(|(c, _)| *c == n.column) {
            Some((_, of)) => Some(*of),
            // Either grouped column, or an alias the select list gave one.
            None if by.iter().any(|g| g.name.column == n.column) => {
                return Ok(match order.desc {
                    // Combinations arrive in key order, outermost column first, which is what
                    // ascending asks for.
                    false => None,
                    true => Some(GroupOrder { by: OrderBy::Key, desc: true }),
                });
            }
            None => return Err(refuse()),
        },
    };
    let of = by_value.ok_or_else(refuse)?;
    Ok(Some(GroupOrder { by: OrderBy::Value { of }, desc: order.desc }))
}
