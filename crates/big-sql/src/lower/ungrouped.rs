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

use super::measure::{having_of, measure_of, units_of, Measure};
use super::pql::{as_call, call_of, field_arg, named};
use super::{answer, rows_of, Ask, Calls, Probe, Statement};
use crate::ast::{Item, Name, Proj, Select};
use crate::error::{Refused, Result, SqlError};
use crate::shape::{Cell, Columns, Of, Selected, Shape, Units};
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
    // A `HAVING` names an aggregate, and the two shapes below answer with values rather than
    // with aggregates - so there is no number in either for one to be about. Refused here, once,
    // rather than in each of them: the aggregate case is the only one that can carry it, and it
    // does so at the bottom of this function.
    if let Some(h) = &select.having {
        if !columns.is_empty() || !stars.is_empty() {
            return Err(SqlError::Refused { what: Refused::Having, at: h.at });
        }
    }

    if let Some((first, _)) = columns.first() {
        // A projection: the stored values of some columns, for the records the `WHERE` selects.
        if !aggregates.is_empty() || !stars.is_empty() {
            // Values beside a number about the whole set is two answers of different heights.
            return Err(SqlError::Refused { what: Refused::Shape, at: first.at });
        }
        if let Some(order) = &select.order_by {
            // Sorting a projection means materialising every matching record's value and then
            // ordering it, which is a sort this surface has no operator behind - the values are
            // reconstructed a record at a time, in record order, and nothing holds them.
            return Err(SqlError::Refused { what: Refused::Order, at: order.at });
        }
        if select.offset.is_some() {
            return Err(SqlError::Refused { what: Refused::Offset, at: first.at });
        }
        // **The limit is optional, and its absence is a full scan.** A projection costs a point
        // read per record per column, so a statement without a `LIMIT` reads every matching
        // record at that price - which is what `SELECT <column> FROM t` asks for everywhere
        // else, and what somebody who writes it here means. The cut is still part of the plan
        // rather than a view of the answer, because it bounds the reads rather than trimming
        // them afterwards.
        let limit = select.limit;

        let mut args = vec![rows.clone()];
        args.extend(columns.iter().map(|(_, name)| field_arg(name)));
        if let Some(limit) = limit {
            args.push(named("n", Expr::Literal(Literal::Int(limit))));
        }
        return Ok(Statement {
            calls: vec![Ask { table: table.to_string(), call: call_of("Project", args) }],
            probes: Vec::new(),
            answer: answer(
                select,
                Shape::Table {
                    columns: Columns::Named(
                        columns
                            .iter()
                            .map(|(item, name)| Selected {
                                column: item.column(),
                                units: Units::Written {
                                    table: table.to_string(),
                                    field: name.column.clone(),
                                },
                            })
                            .collect(),
                    ),
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
            // The values that would order the rows are read back a record at a time, in record
            // order, and nothing holds them - the same sort a named projection is refused.
            return Err(SqlError::Refused { what: Refused::Order, at: order.at });
        }
        // `SELECT *` is every column the table declares, and the list is not written here
        // because nothing here knows the table. Both halves of the answer say so and both are
        // filled in against a schema: a `Project` with no `field=` for the plan, and
        // `Columns::All` for the header. A table with nothing a projection could read falls
        // back to record ids on both sides, which is what `*` answered before it expanded.
        let mut args = vec![rows.clone()];
        if let Some(limit) = select.limit {
            args.push(named("n", Expr::Literal(Literal::Int(limit))));
        }
        return Ok(Statement {
            calls: vec![Ask { table: table.to_string(), call: call_of("Project", args) }],
            probes: Vec::new(),
            answer: answer(
                select,
                Shape::Table {
                    columns: Columns::All {
                        table: table.to_string(),
                        limit: select.limit.map(|n| n as usize),
                    },
                },
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
    // What each entry measures, paired with where its number comes from - the same list a
    // grouping builds, and read by the same `having_of`. A `HAVING` may only name a number the
    // select list already asked for, and this is what decides that.
    let mut measures: Vec<(Measure, Of)> = Vec::new();
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
        measures.push((measure_of(item), of));
        cells.push(Cell { column: item.column(), of, units: units_of(table, &item.proj) });
    }

    let having = having_of(select, table, &measures)?;
    Ok(Statement { calls: calls.out, probes, answer: answer(select, Shape::Row { cells, having }) })
}
