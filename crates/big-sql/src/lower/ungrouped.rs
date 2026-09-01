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

//! `SELECT *`, a projection of stored values, or aggregates over the whole filtered set.

use super::measure::units_of;
use super::pql::{as_call, call_of, field_arg, named};
use super::{answer, rows_of, Ask, Calls, Probe, Statement, MAX_PROJECTION};
use crate::ast::{Item, Name, Proj, Select};
use crate::error::{Refused, Result, SqlError};
use crate::shape::{Cell, Of, Selected, Shape, Units};
use big_plan::ast::{Expr, Literal};

/// `SELECT *`, or aggregates over the whole filtered set.
pub(super) fn ungrouped(
    select: &Select,
    table: &str,
    rows: &Expr,
    stars: &[&Item],
    columns: &[(&Item, Name)],
    aggregates: &[&Item],
) -> Result<Statement> {
    if let Some(h) = &select.having {
        // No `GROUP BY`, so there are no groups for a `HAVING` to keep or drop. SQL allows one
        // over the implicit single group; here that answer is a scalar and filtering it would
        // mean returning zero rows or one, which is a question nobody asks in this dialect.
        return Err(SqlError::Refused { what: Refused::Having, at: h.at });
    }

    if let Some((first, _)) = columns.first() {
        // A projection: the stored values of some columns, for the records the `WHERE` selects.
        if !aggregates.is_empty() || !stars.is_empty() {
            // Values beside a number about the whole set is two answers of different heights.
            return Err(SqlError::Refused { what: Refused::Shape, at: first.at });
        }
        if let Some(order) = &select.order_by {
            // Sorting a projection means reading every matching record's value before the cut
            // rather than after it, which is the cost the mandatory limit exists to bound.
            return Err(SqlError::Refused { what: Refused::Order, at: order.at });
        }
        if select.offset.is_some() {
            return Err(SqlError::Refused { what: Refused::Offset, at: first.at });
        }
        // **The limit is required, and it is required here rather than defaulted.** A
        // projection costs a point read per record per column; without a cut that is the whole
        // table, and a default would be this surface choosing how much of it to pay for.
        let Some(limit) = select.limit.filter(|n| *n as usize <= MAX_PROJECTION && *n > 0) else {
            return Err(SqlError::Refused { what: Refused::Projection, at: first.at });
        };

        let mut args = vec![rows.clone()];
        args.extend(columns.iter().map(|(_, name)| field_arg(name)));
        args.push(named("n", Expr::Literal(Literal::Int(limit))));
        return Ok(Statement {
            calls: vec![Ask { table: table.to_string(), call: call_of("Project", args) }],
            probes: Vec::new(),
            answer: answer(
                select,
                Shape::Table {
                    columns: columns
                        .iter()
                        .map(|(item, name)| Selected {
                            column: item.column(),
                            units: Units::Written {
                                table: table.to_string(),
                                field: name.column.clone(),
                            },
                        })
                        .collect(),
                },
            ),
        });
    }

    if let Some(item) = stars.first() {
        if !aggregates.is_empty() || stars.len() > 1 {
            return Err(SqlError::Refused { what: Refused::Shape, at: item.at });
        }
        if select.offset.is_some() {
            // A record listing is paged with a cursor, which is stable under concurrent
            // inserts where a skip count is not. See `Refused::Offset`.
            return Err(SqlError::Refused { what: Refused::Offset, at: item.at });
        }
        if let Some(order) = &select.order_by {
            // Records come back in id order and there is nothing to sort them by: the values
            // that would order them are exactly the ones this surface does not materialise.
            return Err(SqlError::Refused { what: Refused::Order, at: order.at });
        }
        return Ok(Statement {
            calls: vec![Ask { table: table.to_string(), call: as_call(rows.clone()) }],
            probes: Vec::new(),
            answer: answer(
                select,
                Shape::Records { column: item.column(), limit: select.limit.map(|n| n as usize) },
            ),
        });
    }

    // Zero aggregates is impossible - the parser needs at least one item, and the other two
    // buckets have been dealt with.
    let at = aggregates.first().map_or(0, |i| i.at);
    if let Some(order) = &select.order_by {
        // One row of numbers has nothing to order.
        return Err(SqlError::Refused { what: Refused::Order, at: order.at });
    }
    if select.limit.is_some() || select.offset.is_some() {
        // A `LIMIT` or an `OFFSET` on one row of numbers is a client that thinks it is paging
        // something.
        return Err(SqlError::Refused { what: Refused::Shape, at });
    }

    let mut calls = Calls::new(at);
    let mut probes: Vec<Probe> = Vec::new();
    let mut cells = Vec::new();
    for item in aggregates {
        let rows = rows_of(rows, item);
        let of = match &item.proj {
            Proj::Count => Of::Value { plan: calls.push(table, call_of("Count", vec![rows]))? },
            Proj::Agg { func, field } => Of::Value {
                plan: calls.push(table, call_of(func.call(), vec![rows, field_arg(field)]))?,
            },
            // A distinct count is a `Distinct` whose groups are counted after every node has
            // contributed to them. Doing it any earlier counts one node's groups.
            Proj::CountDistinct(field) => Of::Groups {
                plan: calls.push(table, call_of("Distinct", vec![rows, field_arg(field)]))?,
            },
            Proj::Avg(field) => Of::Ratio {
                plan: calls.push(table, call_of("Sum", vec![rows.clone(), field_arg(field)]))?,
                over: calls.push(table, call_of("Count", vec![rows]))?,
            },
            // The ranking a `TopN` already answers, rendered as a list rather than as rows.
            Proj::TopKeys { n, field } => Of::Keys {
                plan: calls.push(
                    table,
                    call_of(
                        "TopN",
                        vec![rows, field_arg(field), named("n", Expr::Literal(Literal::Int(*n)))],
                    ),
                )?,
            },
            // Not a call at all: a search, run after every call has been answered.
            Proj::Quantile { per_mille, field } => {
                probes.push(Probe {
                    table: table.to_string(),
                    rows: as_call(rows),
                    field: field.column.clone(),
                    per_mille: *per_mille,
                });
                Of::Probe { probe: probes.len() - 1 }
            }
            Proj::Star | Proj::Column(_) => unreachable!("sorted into the other two buckets"),
        };
        cells.push(Cell { column: item.column(), of, units: units_of(table, &item.proj) });
    }

    Ok(Statement { calls: calls.out, probes, answer: answer(select, Shape::Row { cells }) })
}
