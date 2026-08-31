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

//! `FROM a JOIN b ON a.k = b.k`, as arithmetic over what each side already counts.
//!
//! **Nothing here pairs records.** A record is a set of bits in one table and there is no
//! pointer to another; even the row ids behind a keyed column are per `(table, field)` and have
//! no reason to agree. What two tables share is the string a keyed column was interned from.
//!
//! For each key both sides hold, the join is the Cartesian product of the records holding it,
//! so every aggregate over it is arithmetic over two per-key numbers an ordinary single-table
//! grouping already produces. That is why a join is two ordinary plans and a `Shape::Join`, and
//! why it needed no `Plan` variant and no merge arm.

use super::cond::rows;
use super::measure::{measure_of, names, Measure};
use super::pql::{as_expr, call, call_of, field_arg, named};
use super::{answer, Calls, Statement};
use crate::ast::{Agg, Cond, HavingAgg, Item, Name, OrderKey, Proj, Select, Source};
use crate::error::{Refused, Result, SqlError};
use crate::shape::{Cell, Cut, GroupOrder, Having, Of, OrderBy, Pairing, Shape, Threshold};
use big_plan::ast::{Call, Expr, Literal};

/// Which of a join's two tables a column belongs to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Side {
    Left,
    Right,
}

impl Side {
    fn other(self) -> Self {
        match self {
            Self::Left => Self::Right,
            Self::Right => Self::Left,
        }
    }
}

/// The two tables a join has in scope, under the names the statement calls them by.
struct Scope<'a> {
    left: &'a Source,
    right: &'a Source,
}

impl Scope<'_> {
    /// Which table a column belongs to.
    ///
    /// **A qualifier is required.** The translation resolves names before it has ever seen a
    /// schema, so an unqualified column in a two-table statement is a name nothing here can
    /// decide - and picking the left one because it was written first would be guessing.
    fn side(&self, name: &Name, at: usize) -> Result<Side> {
        let refuse = || SqlError::Refused { what: Refused::Ambiguous, at };
        let q = name.qualifier.as_deref().ok_or_else(refuse)?;
        if q == self.left.label() {
            Ok(Side::Left)
        } else if q == self.right.label() {
            Ok(Side::Right)
        } else {
            Err(refuse())
        }
    }

    fn table(&self, side: Side) -> &str {
        match side {
            Side::Left => &self.left.table,
            Side::Right => &self.right.table,
        }
    }
}

/// `FROM a JOIN b ON a.k = b.k`, as arithmetic over what each side already answers.
///
/// **Nothing here is a join in the sense of pairing records.** For each key both sides hold, the
/// join is the Cartesian product of the records holding it, and every aggregate over that
/// product is a product of two per-key numbers: `count(*)` is `|A_s| · |B_s|`, `sum(a.x)` is
/// `sum_A(x) · |B_s|`, an extreme is one side's extreme over the keys the other holds. So the
/// two calls this makes are ordinary single-table groupings, each fanned out and merged exactly
/// as it would be alone, and the join itself is a [`Shape::Join`] applied to the merged answers.
pub(super) fn joined(
    select: &Select,
    join: &crate::ast::Join,
    stars: &[&Item],
    columns: &[(&Item, Name)],
    aggregates: &[&Item],
) -> Result<Statement> {
    let scope = Scope { left: &select.from, right: &join.source };
    if scope.left.label() == scope.right.label() {
        // `FROM tx JOIN tx ON ...`, where no qualifier can name one of them.
        return Err(SqlError::Refused { what: Refused::Ambiguous, at: join.at });
    }
    if let Some(item) = stars.first() {
        // A pair of records has no identity this engine stores, so there is nothing for a star
        // to answer with.
        return Err(SqlError::Refused { what: Refused::JoinShape, at: item.at });
    }

    let (key_left, key_right) = join_key(join, &scope)?;

    // Each table's own conditions. A term that names both is refused rather than applied to
    // one of them.
    let (cond_left, cond_right) = match &select.filter {
        None => (None, None),
        Some(cond) => split(cond, &scope, 0)?,
    };
    let rows_left = cond_left.as_ref().map_or_else(|| call("All", vec![]), rows);
    let rows_right = cond_right.as_ref().map_or_else(|| call("All", vec![]), rows);

    let at = select.items.first().map_or(0, |i| i.at);
    let mut calls = Calls::new(at);
    // The two plans whose keys are the join. Pushed first and unconditionally: they are what
    // says which keys are in it, and every cell is arithmetic against one of them.
    let count_left = calls.push(scope.table(Side::Left), grouped_count(&rows_left, key_left))?;
    let count_right =
        calls.push(scope.table(Side::Right), grouped_count(&rows_right, key_right))?;

    let sides = Sides {
        scope: &scope,
        key_left,
        key_right,
        count_left,
        count_right,
        rows_left,
        rows_right,
    };
    let per_key = sides.grouping(select, columns, at)?;

    let mut measures: Vec<(Measure, Of)> = Vec::new();
    for item in aggregates {
        measures.push((measure_of(item), sides.cell(item, per_key, &mut calls)?));
    }

    // Column order follows the select list, which is the only order the caller asked for.
    let mut next = measures.iter().map(|(_, of)| *of);
    let cells: Vec<Cell> = select
        .items
        .iter()
        .map(|i| Cell {
            column: i.column(),
            of: match i.proj {
                Proj::Column(_) => Of::Key,
                _ => next.next().expect("one measure per aggregate, in select-list order"),
            },
        })
        .collect();

    let having = join_having(select, &scope, &measures, per_key)?;

    let order = join_ordering(select, key_left, key_right, &scope, &measures, per_key)?;
    if !per_key && (select.limit.is_some() || select.offset.is_some()) {
        // A `LIMIT` on one row of numbers is a client that thinks it is paging something.
        return Err(SqlError::Refused { what: Refused::Shape, at });
    }

    Ok(Statement {
        calls: calls.out,
        probes: Vec::new(),
        answer: answer(
            select,
            Shape::Join {
                keys: (count_left, count_right),
                cells,
                per_key,
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

/// The join's own columns, normalised so `left` is the left table's whichever way round the
/// `ON` was written.
fn join_key<'a>(join: &'a crate::ast::Join, scope: &Scope<'_>) -> Result<(&'a Name, &'a Name)> {
    Ok(match scope.side(&join.left, join.at)? {
        Side::Left => {
            if scope.side(&join.right, join.at)? != Side::Right {
                return Err(SqlError::Refused { what: Refused::JoinOn, at: join.at });
            }
            (&join.left, &join.right)
        }
        Side::Right => {
            if scope.side(&join.right, join.at)? != Side::Left {
                return Err(SqlError::Refused { what: Refused::JoinOn, at: join.at });
            }
            (&join.right, &join.left)
        }
    })
}

/// The `HAVING`, which a join can only have when it has groups to keep or drop.
fn join_having(
    select: &Select,
    scope: &Scope<'_>,
    measures: &[(Measure, Of)],
    per_key: bool,
) -> Result<Option<Having>> {
    let Some(h) = &select.having else { return Ok(None) };
    if !per_key {
        // One row has no groups to keep or drop.
        return Err(SqlError::Refused { what: Refused::Having, at: h.at });
    }
    let of =
        names(&h.agg, measures).ok_or(SqlError::Refused { what: Refused::Having, at: h.at })?;
    let value = match having_field(&h.agg, scope, h.at)? {
        Some((table, field)) => Threshold::Written { table, field, value: h.value.clone() },
        None => match h.value {
            Literal::Int(n) => Threshold::Units(i128::from(n)),
            _ => return Err(SqlError::Refused { what: Refused::Having, at: h.at }),
        },
    };
    Ok(Some(Having { of, op: h.op, value }))
}

/// The four things a join needs to know per side, in one value.
///
/// These used to be three closures over locals, which meant the aggregate loop could only live
/// where they did. A side is left or right; what changes with it is which key pairs the records,
/// which plan counts them, and which half of the `WHERE` narrows them.
struct Sides<'a> {
    scope: &'a Scope<'a>,
    key_left: &'a Name,
    key_right: &'a Name,
    count_left: usize,
    count_right: usize,
    rows_left: Expr,
    rows_right: Expr,
}

impl<'a> Sides<'a> {
    /// The column this side is joined on.
    fn key(&self, side: Side) -> &'a Name {
        match side {
            Side::Left => self.key_left,
            Side::Right => self.key_right,
        }
    }

    /// The plan holding this side's per-key record counts.
    fn count(&self, side: Side) -> usize {
        match side {
            Side::Left => self.count_left,
            Side::Right => self.count_right,
        }
    }

    /// The records this side's half of the `WHERE` leaves.
    fn rows(&self, side: Side) -> Expr {
        match side {
            Side::Left => self.rows_left.clone(),
            Side::Right => self.rows_right.clone(),
        }
    }

    /// One aggregate over the join, as the pair of per-key numbers it is arithmetic over.
    ///
    /// **Nothing here pairs records.** For each key both sides hold the join is the Cartesian
    /// product of the records holding it, so every aggregate over that product is a product of
    /// two per-key numbers - which is why each of these is an [`Of::Paired`] naming two plans
    /// rather than a plan of its own.
    fn cell(&self, item: &Item, per_key: bool, calls: &mut Calls) -> Result<Of> {
        if item.filter.is_some() {
            // A `FILTER` narrows one aggregate's records. Over a join that would leave the two
            // sides disagreeing about which keys are in it, and a key one side dropped is a
            // pairing that never happened rather than a group with a zero in it.
            return Err(SqlError::Refused { what: Refused::JoinShape, at: item.at });
        }
        Ok(match &item.proj {
            // Every record on one side pairs with every record on the other, under each key.
            Proj::Count => {
                Of::Paired { left: self.count_left, right: self.count_right, how: Pairing::Product }
            }
            Proj::Agg { func, field } => {
                let side = self.scope.side(field, item.at)?;
                let plan = calls.push(
                    self.scope.table(side),
                    grouped_aggregate(&self.rows(side), self.key(side), *func, field),
                )?;
                Of::Paired {
                    left: plan,
                    right: self.count(side.other()),
                    // A total is scaled by how many times each record is repeated in the
                    // product; an extreme is not - repeating a value does not make it larger.
                    how: match func {
                        Agg::Sum => Pairing::Product,
                        Agg::Min => Pairing::Least,
                        Agg::Max => Pairing::Greatest,
                    },
                }
            }
            // The join key's own distinct count, which is how many keys are in the join. Any
            // other column would be a grouping over a pair of columns nothing stored.
            Proj::CountDistinct(field) => {
                let side = self.scope.side(field, item.at)?;
                if field.column != self.key(side).column || per_key {
                    return Err(SqlError::Refused { what: Refused::JoinShape, at: item.at });
                }
                Of::SharedKeys { left: self.count_left, right: self.count_right }
            }
            // An average over a join is a ratio of two of these, which is a cell holding two
            // cells. Refused with a sentence saying to write the two halves.
            Proj::Avg(_)
            | Proj::TopKeys { .. }
            | Proj::Quantile { .. }
            | Proj::Star
            | Proj::Column(_) => {
                return Err(SqlError::Refused { what: Refused::JoinShape, at: item.at })
            }
        })
    }

    /// Whether the answer is one row per key, and that the `GROUP BY` names the key it must.
    ///
    /// `GROUP BY` the join key is the only grouping a join has: the key is what pairs the
    /// records, so it is the only column both sides agree about.
    fn grouping(&self, select: &Select, columns: &[(&Item, Name)], at: usize) -> Result<bool> {
        let per_key = match select.group_by.as_slice() {
            [] => false,
            [g] => {
                let side = self.scope.side(g, at)?;
                if g.column != self.key(side).column {
                    return Err(SqlError::Refused { what: Refused::JoinShape, at });
                }
                true
            }
            // A join pairs records through one key, so that key is the only grouping it has.
            _ => return Err(SqlError::Refused { what: Refused::JoinShape, at }),
        };
        for (item, name) in columns {
            let side = self.scope.side(name, item.at)?;
            if !per_key || name.column != self.key(side).column {
                return Err(SqlError::Refused { what: Refused::JoinShape, at: item.at });
            }
        }
        Ok(per_key)
    }
}

/// One side's per-key record count, which is the plan a join is built out of.
fn grouped_count(rows: &Expr, key: &Name) -> Call {
    call_of("Distinct", vec![rows.clone(), field_arg(key)])
}

/// One side's per-key aggregate of one of its own columns.
fn grouped_aggregate(rows: &Expr, key: &Name, func: Agg, field: &Name) -> Call {
    call_of(
        "GroupBy",
        vec![
            rows.clone(),
            field_arg(key),
            named("aggregate", as_expr(call_of(func.call(), vec![field_arg(field)]))),
        ],
    )
}

/// The table and column a `HAVING` threshold is in the units of, if any.
fn having_field(
    having: &HavingAgg,
    scope: &Scope<'_>,
    at: usize,
) -> Result<Option<(String, String)>> {
    let name = match having {
        HavingAgg::Count => return Ok(None),
        HavingAgg::Agg { field, .. } | HavingAgg::Avg(field) => field,
    };
    let side = scope.side(name, at)?;
    Ok(Some((scope.table(side).to_string(), name.column.clone())))
}

/// The `ORDER BY` of a join, which orders the keys in it.
fn join_ordering(
    select: &Select,
    key_left: &Name,
    key_right: &Name,
    scope: &Scope<'_>,
    measures: &[(Measure, Of)],
    per_key: bool,
) -> Result<Option<GroupOrder>> {
    let Some(order) = &select.order_by else { return Ok(None) };
    let refuse = || SqlError::Refused { what: Refused::Order, at: order.at };
    if !per_key {
        // One row has nothing to order.
        return Err(refuse());
    }

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
            match names(&HavingAgg::Agg { func: *func, field: field.clone() }, measures) {
                Some(of) => OrderBy::Value { of },
                None => return Err(refuse()),
            }
        }
        OrderKey::Avg(_) => return Err(refuse()),
        OrderKey::Name(n) => match value_names.iter().find(|(c, _)| *c == n.column) {
            Some((_, of)) => OrderBy::Value { of: *of },
            // The join key, under either table's spelling or an alias of it.
            None if n.column == key_left.column || n.column == key_right.column => OrderBy::Key,
            None if n.qualifier.is_some() && scope.side(n, order.at).is_ok() => {
                return Err(refuse())
            }
            None => return Err(refuse()),
        },
    };

    // Ascending by key is the order the keys already come in, so it asks for what it gets.
    Ok(match (by, order.desc) {
        (OrderBy::Key, false) => None,
        (by, desc) => Some(GroupOrder { by, desc }),
    })
}

/// A `WHERE` split into each table's own half.
///
/// **Only an `AND` can be split.** `a.x = 1 OR b.y = 2` selects pairs where either side
/// matched, and neither side can be filtered to that on its own - so it is refused rather than
/// applied to one of them and quietly answering something narrower.
fn split(cond: &Cond, scope: &Scope<'_>, at: usize) -> Result<(Option<Cond>, Option<Cond>)> {
    if let Cond::And(a, b) = cond {
        let (la, ra) = split(a, scope, at)?;
        let (lb, rb) = split(b, scope, at)?;
        return Ok((both(la, lb), both(ra, rb)));
    }
    match touches(cond, scope, at)? {
        (true, false) => Ok((Some(cond.clone()), None)),
        (false, true) => Ok((None, Some(cond.clone()))),
        // A term naming both tables, or - impossibly, since a condition always names a
        // column - neither.
        _ => Err(SqlError::Refused { what: Refused::JoinFilter, at }),
    }
}

/// Which of a join's tables a condition mentions.
fn touches(cond: &Cond, scope: &Scope<'_>, at: usize) -> Result<(bool, bool)> {
    Ok(match cond {
        Cond::And(a, b) | Cond::Or(a, b) => {
            let (la, ra) = touches(a, scope, at)?;
            let (lb, rb) = touches(b, scope, at)?;
            (la || lb, ra || rb)
        }
        Cond::Not(inner) => touches(inner, scope, at)?,
        Cond::Cmp { field, .. } | Cond::In { field, .. } | Cond::Between { field, .. } => {
            match scope.side(field, at)? {
                Side::Left => (true, false),
                Side::Right => (false, true),
            }
        }
    })
}

/// Two halves of a conjunction, when there are two.
fn both(a: Option<Cond>, b: Option<Cond>) -> Option<Cond> {
    match (a, b) {
        (Some(a), Some(b)) => Some(Cond::And(Box::new(a), Box::new(b))),
        (Some(one), None) | (None, Some(one)) => Some(one),
        (None, None) => None,
    }
}
