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

//! `GROUP BY` one keyed column, with the aggregates of it the select list asked for.

use super::measure::{having_of, measure_of, names, units_of, Measure};
use super::pql::{as_expr, call_of, field_arg, named};
use super::{answer, count_plan, rows_of, Ask, Calls, Statement};
use crate::ast::{Agg, HavingAgg, Item, Name, OrderKey, Proj, Select};
use crate::error::{Refused, Result, SqlError};
use crate::shape::{Absent, Cell, Cut, GroupOrder, Of, OrderBy, Shape};
use big_plan::ast::{Expr, Literal};

/// `GROUP BY` one keyed column, with the aggregates of it the select list asked for.
///
/// Five steps, each of which can refuse: what the shape cannot express, what each aggregate
/// measures and which plan answers it, the columns, the `HAVING`, and where the cut ends up.
/// They are separate functions because they refuse for unrelated reasons - a column that is
/// neither grouped nor aggregated is a different mistake from a `HAVING` on an average - and
/// reading one of those reasons should not mean reading the other four.
pub(super) fn grouped(
    select: &Select,
    table: &str,
    rows: &Expr,
    group: &Name,
    stars: &[&Item],
    columns: &[(&Item, Name)],
    aggregates: &[&Item],
) -> Result<Statement> {
    refuse_what_a_grouping_cannot_hold(group, stars, columns, aggregates)?;

    let at = select.items.first().map_or(0, |i| i.at);
    let mut calls = Calls::new(at);
    let measures = measures_of(table, rows, group, aggregates, &mut calls)?;

    // Only the keys were asked for. `Distinct` is the plan that lists them; the counts it also
    // produces are what a `HAVING count(*)` reads.
    //
    // The same plan is added when any aggregate carries a `FILTER`, and there it is not about
    // counts at all - it is what says which groups exist. SQL groups the records the `WHERE`
    // selected and only then narrows each aggregate, so a group every record of which a filter
    // rejects is still a group, with `0` in the count cells and nothing in the rest. Without
    // this plan the answer would have no row for it, which is a missing group rather than an
    // empty one - a difference no client could see.
    if measures.is_empty() || aggregates.iter().any(|i| i.filter.is_some()) {
        calls.push(table, call_of("Distinct", vec![rows.clone(), field_arg(group)]))?;
    }

    let cells = cells_of(select, table, &measures);
    let having = having_of(select, table, &measures)?;

    let Ordering { ranked, order } = ordering(select, group, &measures)?;
    let cut = Cut {
        offset: select.offset.map(|n| n as usize),
        limit: select.limit.map(|n| n as usize),
        ties: select.with_ties,
    };

    // How many groups the plan itself may cut to.
    //
    // `TopN` ranks and truncates in one pass, which is the cheap path and the reason the
    // ranking is a plan variant at all. It can only carry the cut when nothing after the merge
    // can still change which rows survive: a `HAVING` drops groups the ranking counted, an
    // `OFFSET` means the rows wanted begin further down the ranking than `n` alone describes,
    // and `WITH TIES` means the answer is longer than `n`. The offset case still uses the plan,
    // asking it for `offset + limit` and taking the window out of it here.
    let plan_cut = if ranked && having.is_none() && !cut.ties {
        match (cut.limit, cut.offset) {
            (Some(l), Some(o)) => l.checked_add(o),
            (Some(l), None) => Some(l),
            (None, _) => None,
        }
    } else {
        None
    };
    // Truncating twice would truncate a ranking built to be exactly this long - harmless today
    // and wrong the moment the two disagree. The shape keeps the limit only when the plan's cut
    // is not already the final answer.
    let cut = Cut {
        limit: if plan_cut.is_some() && cut.is_plain_limit() { None } else { cut.limit },
        ..cut
    };

    // A ranking replaces the one plan the answer has, which is why `ordering` only returns one
    // over a single-plan answer: `TopN` cuts its own groups and knows nothing about the others.
    let mut calls = calls.out;
    let mut keys: Vec<usize> = (0..calls.len()).collect();
    if ranked {
        keys = vec![0];
        let mut args = vec![rows.clone(), field_arg(group)];
        if let Some(n) = plan_cut {
            args.push(named("n", Expr::Literal(Literal::Int(n as u64))));
        }
        calls = vec![Ask { table: table.to_string(), call: call_of("TopN", args) }];
    }

    Ok(Statement {
        calls,
        probes: Vec::new(),
        answer: answer(select, Shape::Groups { keys, cells, having, order, cut }),
    })
}

/// The three shapes a grouping cannot hold, refused before any plan is built.
///
/// Each is a different mistake. A `*` has no single value per group; a column that is neither
/// grouped nor aggregated is the classic SQL error and a real one here; and a distinct count or
/// a ranking *inside* a grouping is a grouping over a composite key this index never stored.
fn refuse_what_a_grouping_cannot_hold(
    group: &Name,
    stars: &[&Item],
    columns: &[(&Item, Name)],
    aggregates: &[&Item],
) -> Result<()> {
    if let Some(item) = stars.first() {
        return Err(SqlError::Refused { what: Refused::Shape, at: item.at });
    }
    for (item, name) in columns {
        if name.column != group.column {
            // The classic SQL error, and it is a real one here: a column that is neither
            // grouped nor aggregated has no single value per group.
            return Err(SqlError::Refused { what: Refused::Shape, at: item.at });
        }
    }
    for item in aggregates {
        match item.proj {
            // A distinct count inside a grouping is a grouping over a composite key this index
            // never stored.
            Proj::CountDistinct(_) => {
                return Err(SqlError::Refused { what: Refused::MultiDistinct, at: item.at })
            }
            // A ranking inside a grouping is the same thing: the keys of one column, per key of
            // another. Refused rather than reached as an unreachable arm below.
            Proj::TopKeys { .. } | Proj::Quantile { .. } => {
                return Err(SqlError::Refused { what: Refused::Shape, at: item.at })
            }
            _ => {}
        }
    }
    Ok(())
}

/// What each aggregate in the select list measures, and which plan answers it.
///
/// Worked out once, so that `HAVING` and `ORDER BY` name a number by matching against this
/// rather than by repeating the rules for what a number is.
fn measures_of(
    table: &str,
    rows: &Expr,
    group: &Name,
    aggregates: &[&Item],
    calls: &mut Calls,
) -> Result<Vec<(Measure, Of)>> {
    let mut measures: Vec<(Measure, Of)> = Vec::new();
    for item in aggregates {
        let rows = rows_of(rows, item);
        let of = match &item.proj {
            // The same instant in every row, which is what it is: a constant needs no plan and
            // no group, so it costs a grouping nothing to carry.
            Proj::Now { unix_seconds } => Of::Now { unix_seconds: *unix_seconds },
            // A rounding is applied to values read back per record, and a grouped or joined
            // answer holds none: what it carries per row is a key and the numbers folded under
            // it. Refused rather than silently rounding something else.
            Proj::TimeOf { .. } => {
                return Err(SqlError::Refused { what: Refused::Shape, at: item.at })
            }
            Proj::Count => {
                Of::Group { plan: count_plan(calls, table, &rows, group)?, absent: Absent::Zero }
            }
            Proj::Agg { func, field } => Of::Group {
                plan: calls.push(
                    table,
                    call_of(
                        "GroupBy",
                        vec![
                            rows,
                            field_arg(group),
                            named(
                                "aggregate",
                                as_expr(call_of(func.call(), vec![field_arg(field)])),
                            ),
                        ],
                    ),
                )?,
                // A sum over no records is `0` here and an extreme over none is absent, which
                // is what each already answers when a `WHERE` leaves it nothing.
                absent: match func {
                    Agg::Sum => Absent::Zero,
                    Agg::Min | Agg::Max => Absent::Null,
                },
            },
            // An average over a group is that group's sum over its count: two grouped plans,
            // divided after both have been merged. The division cannot happen earlier - a
            // ratio of one node's share is not a share of the ratio.
            Proj::Avg(field) => Of::Ratio {
                plan: calls.push(
                    table,
                    call_of(
                        "GroupBy",
                        vec![
                            rows.clone(),
                            field_arg(group),
                            named("aggregate", as_expr(call_of("Sum", vec![field_arg(field)]))),
                        ],
                    ),
                )?,
                over: count_plan(calls, table, &rows, group)?,
            },
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

/// The columns, in select-list order, which is the only order the caller asked for.
fn cells_of(select: &Select, table: &str, measures: &[(Measure, Of)]) -> Vec<Cell> {
    let mut next = measures.iter().map(|(_, of)| *of);
    select
        .items
        .iter()
        .map(|i| Cell {
            column: i.column(),
            of: match i.proj {
                Proj::Column(_) => Of::Key,
                _ => next.next().expect("one measure per aggregate, in select-list order"),
            },
            units: units_of(table, &i.proj),
        })
        .collect()
}

/// Where an `ORDER BY` ends up: in the plan, in the shape, or nowhere.
struct Ordering {
    /// The plan should be a `TopN`, which ranks by count and carries its own cut.
    ranked: bool,
    /// What the coordinator must do to the merged answer. `None` when it already arrives in
    /// the order asked for.
    order: Option<GroupOrder>,
}

/// Resolves the `ORDER BY` against the select list, refusing every ordering that names a
/// number the answer does not hold.
///
/// Four answers, not two. Ascending by the grouped column is the order groups already come back
/// in, so it is accepted and does nothing - a no-op is honest where a refusal would not be.
/// Descending by count is a ranking, and `TopN` does it in the plan. Everything else the engine
/// *can* answer is a sort of the merged list at the coordinator, which costs materialising every
/// group before the cut and is why it is not the default. What is left names a number that is
/// not there, and is refused.
fn ordering(select: &Select, group: &Name, measures: &[(Measure, Of)]) -> Result<Ordering> {
    let Some(order) = &select.order_by else {
        return Ok(Ordering { ranked: false, order: None });
    };
    let refuse = || SqlError::Refused { what: Refused::Order, at: order.at };

    // A name may be the grouped column, or an alias the select list gave to either half.
    let key_names: Vec<String> = select
        .items
        .iter()
        .filter(|i| matches!(i.proj, Proj::Column(_)))
        .map(Item::column)
        .chain(std::iter::once(group.column.clone()))
        .collect();
    let value_names: Vec<(String, Of)> = select
        .items
        .iter()
        .filter(|i| !matches!(i.proj, Proj::Column(_)))
        .map(Item::column)
        .zip(measures.iter().map(|(_, of)| *of))
        .collect();

    let by = match &order.key {
        OrderKey::Count => match names(&HavingAgg::Count, measures) {
            Some(of) => OrderBy::Value { of },
            None => return Err(refuse()),
        },
        OrderKey::Agg { func, field } => {
            let want = HavingAgg::Agg { func: *func, field: field.clone() };
            match names(&want, measures) {
                Some(of) => OrderBy::Value { of },
                None => return Err(refuse()),
            }
        }
        OrderKey::Avg(field) => match names(&HavingAgg::Avg(field.clone()), measures) {
            Some(of) => OrderBy::Value { of },
            None => return Err(refuse()),
        },
        OrderKey::Name(n) => match value_names.iter().find(|(c, _)| *c == n.column) {
            Some((_, of)) => OrderBy::Value { of: *of },
            None if key_names.contains(&n.column) => OrderBy::Key,
            // A name that is in neither list.
            None => return Err(refuse()),
        },
    };

    // The ranking replaces every plan with one `TopN`, so it is only available when there was
    // one plan to replace. A second aggregate has to be computed over the same groups, and a
    // ranking that cut its own would leave the others describing groups it had dropped.
    //
    // `WITH TIES` also rules it out, and for a different reason: the rows kept past the limit
    // are the ones the *ordering* cannot separate, so the coordinator has to be told what the
    // ordering was. A `TopN` answers in ranked order and says nothing about why.
    let single_count = !select.with_ties
        && measures.len() <= 1
        && matches!(by, OrderBy::Value { of: Of::Group { .. } })
        && measures.first().is_none_or(|(m, _)| *m == Measure::Count);

    Ok(match (by, order.desc) {
        // Groups arrive ordered by key already, so this asks for what it is going to get.
        (OrderBy::Key, false) => Ordering { ranked: false, order: None },
        // The ranking, and the only ordering the plan itself can carry.
        (OrderBy::Value { .. }, true) if single_count => Ordering { ranked: true, order: None },
        // A sort of the merged list: descending by key, ascending by count, or either
        // direction over an aggregate. Each is answerable, and each costs the whole answer
        // being held at the coordinator before it is cut.
        (by, desc) => Ordering { ranked: false, order: Some(GroupOrder { by, desc }) },
    })
}
