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

//! `FROM a JOIN b ON a.k = b.k`, and every further table on the same key, as arithmetic over
//! what each side already counts.
//!
//! **Nothing here pairs records.** A record is a set of bits in one table and there is no
//! pointer to another; even the row ids behind a keyed column are per `(table, field)` and have
//! no reason to agree. What the tables share is the string a keyed column was interned from.
//!
//! For each key every side holds, the join is the Cartesian product of the records holding it,
//! so every aggregate over it is arithmetic over per-key numbers an ordinary single-table
//! grouping already produces: `count(*)` is `Σ_s Π_i |X_i,s|`. That is why a join is one
//! ordinary plan per table and a `Shape::Join`, and why it needed no `Plan` variant and no merge
//! arm - at two tables or at ten.
//!
//! **What the width costs is the shape of the `FROM`, not the arithmetic.** Every table has to
//! be grouped by exactly one column, which makes the joins a *star* around one shared key. A
//! table joined on two different columns is a chain, and would have to be grouped by both at
//! once - a pass over the second column per value of the first, which is the cost
//! `GROUP BY a, b, c` is already refused for.

use super::cond::rows;
use super::measure::{field_measured, having_tree, measure_of, names, Measure};
use super::pql::{as_expr, call, call_of, field_arg, named};
use super::{answer, Calls, Statement, MAX_CALLS};
use crate::ast::{Agg, Cond, HavingAgg, Item, Join, Name, OrderKey, Proj, Select, Source};
use crate::error::{Refused, Result, SqlError};
use crate::shape::{
    Cell, Cut, GroupOrder, Having, JoinSide, Keying, Of, OrderBy, Pairing, Shape, Units,
};
use big_plan::ast::{Call, Expr};

/// Which of a join's tables a column belongs to: a position in `FROM` order.
///
/// The same number a cell's `side` is and a `Shape::Join`'s `keys` are indexed by, which is why
/// it is a position rather than a name.
type Side = usize;

/// The tables a join has in scope, under the names the statement calls them by.
///
/// `FROM` first, then each `JOIN`'s table in the order written - so a join's new table is always
/// the one at its own position plus one, which is what lets `star_keys` tell a table being
/// brought in from one already here.
struct Scope<'a> {
    sources: Vec<&'a Source>,
    /// `database.table` per source, in the same order. See [`Source::qualified`].
    qualified: Vec<String>,
}

impl<'a> Scope<'a> {
    fn of(select: &'a Select) -> Self {
        let sources: Vec<&Source> =
            std::iter::once(&select.from).chain(select.joins.iter().map(|j| &j.source)).collect();
        // Held rather than built per call: `table` hands out a borrow, and a qualified name
        // formatted on demand would have nothing to borrow from.
        let qualified = sources.iter().map(|s| s.qualified()).collect();
        Self { sources, qualified }
    }

    /// Which table a column belongs to.
    ///
    /// **A qualifier is required.** The translation resolves names before it has ever seen a
    /// schema, so an unqualified column in a joined statement is a name nothing here can
    /// decide - and picking the first table because it was written first would be guessing.
    fn side(&self, name: &Name, at: usize) -> Result<Side> {
        let refuse = || SqlError::Refused { what: Refused::Ambiguous, at };
        let q = name.qualifier.as_deref().ok_or_else(refuse)?;
        self.sources.iter().position(|s| s.label() == q).ok_or_else(refuse)
    }

    fn table(&self, side: Side) -> &str {
        &self.qualified[side]
    }

    /// Refuses two tables in scope under one name, which no qualifier could tell apart.
    fn distinct_labels(&self, joins: &[Join]) -> Result<()> {
        for (i, s) in self.sources.iter().enumerate() {
            if self.sources[..i].iter().any(|e| e.label() == s.label()) {
                // `FROM tx JOIN tx ON ...`, where no qualifier can name one of them.
                let at = joins.get(i.saturating_sub(1)).map_or(0, |j| j.at);
                return Err(SqlError::Refused { what: Refused::Ambiguous, at });
            }
        }
        Ok(())
    }
}

/// One key column per table, which is what makes several joins a star rather than a chain.
///
/// A `JOIN` names the table it brings in on one side of its `ON` and a table already in `FROM`
/// on the other, and the column it names on each is that table's key. A table named twice has
/// to be named on the same column both times: a second key column would mean grouping it by
/// both at once, which is a pass over one column per value of the other.
fn star_keys<'a>(joins: &'a [Join], scope: &Scope<'_>) -> Result<Vec<&'a Name>> {
    let mut keys: Vec<Option<&Name>> = vec![None; scope.sources.len()];
    for (n, join) in joins.iter().enumerate() {
        let new = n + 1;
        let refuse = |what| SqlError::Refused { what, at: join.at };
        let (l, r) = (scope.side(&join.left, join.at)?, scope.side(&join.right, join.at)?);
        // One side is the table being joined in, the other one already in scope. Two already in
        // scope is a condition about neither of the tables this `JOIN` is about; a table not
        // joined in yet is a forward reference to a name that is not in scope where it stands.
        let known = match (l == new, r == new) {
            (true, false) => r,
            (false, true) => l,
            _ => return Err(refuse(Refused::JoinOn)),
        };
        if known >= new {
            return Err(refuse(Refused::JoinOn));
        }
        for (side, name) in [(l, &join.left), (r, &join.right)] {
            match keys[side] {
                None => keys[side] = Some(name),
                // The same column under either table's spelling: still one key, still a star.
                Some(k) if k.column == name.column => {}
                // A second key column on one table: the chain join.
                Some(_) => return Err(refuse(Refused::Joins)),
            }
        }
    }
    Ok(keys
        .into_iter()
        .map(|k| k.expect("every table is keyed by the join that brought it in"))
        .collect())
}

/// `FROM a JOIN b ON a.k = b.k`, and every further table on that key, as arithmetic over what
/// each side already answers.
///
/// **Nothing here is a join in the sense of pairing records.** For each key every side holds,
/// the join is the Cartesian product of the records holding it, and every aggregate over that
/// product is a product of per-key numbers: `count(*)` is `Π_i |X_i,s|`, `sum(a.x)` is
/// `sum_A(x) · Π_{i≠A} |X_i,s|`, an extreme is one side's extreme over the keys every other side
/// holds. So the calls this makes are ordinary single-table groupings, one per table, each fanned
/// out and merged exactly as it would be alone; the join itself is a [`Shape::Join`] applied to
/// the merged answers.
pub(super) fn joined(
    select: &Select,
    stars: &[&Item],
    columns: &[(&Item, Name)],
    aggregates: &[&Item],
) -> Result<Statement> {
    let scope = Scope::of(select);
    scope.distinct_labels(&select.joins)?;
    if let Some(item) = stars.first() {
        // A pair of records has no identity this engine stores, so there is nothing for a star
        // to answer with.
        return Err(SqlError::Refused { what: Refused::JoinShape, at: item.at });
    }
    let at = select.items.first().map_or(0, |i| i.at);
    // **One grouped count per table, and the calls cannot say so themselves**: `Calls::push`
    // dedupes on `(table, call)`, so a star of aliases of one table collapses to a single call
    // and a hundred of them would slip past the cap. Checked before the `WHERE` is split, which
    // carries one bit per side and would run out of word before it ran out of tables.
    if scope.sources.len() > MAX_CALLS {
        return Err(SqlError::Refused { what: Refused::TooManyCalls, at });
    }

    let keys = star_keys(&select.joins, &scope)?;

    // Each table's own conditions. A term that names two of them is refused rather than applied
    // to one.
    let conds = match &select.filter {
        None => vec![None; scope.sources.len()],
        Some(cond) => split(cond, &scope, 0, scope.sources.len())?,
    };
    let rows_of: Vec<Expr> =
        conds.iter().map(|c| c.as_ref().map_or_else(|| call("All", vec![]), rows)).collect();

    let mut calls = Calls::new(at);
    // The plans whose keys are the join. Pushed first and unconditionally, in `FROM` order: they
    // are what says which keys are in it, and every cell is arithmetic against them.
    let counts = keys
        .iter()
        .enumerate()
        .map(|(i, key)| calls.push(scope.table(i), grouped_count(&rows_of[i], key)))
        .collect::<Result<Vec<_>>>()?;

    let sides = Sides { scope: &scope, keys, counts, rows: rows_of };
    let per_key = sides.grouping(select, columns, at)?;

    let mut measures: Vec<(Measure, Of)> = Vec::new();
    for item in aggregates {
        measures.push((measure_of(item), sides.cell(item, per_key, &mut calls)?));
    }

    // Which table each entry's number comes out of, resolved through the scope because a
    // qualifier here may be any side's name or any side's alias. Done before the cells so that
    // a name naming none of them is the refusal `Scope::side` gives rather than a guess.
    let units = select
        .items
        .iter()
        .map(|i| match field_measured(&i.proj) {
            None => Ok(Units::PLAIN),
            Some(field) => Ok(Units::Written {
                table: scope.table(scope.side(field, i.at)?).to_string(),
                field: field.column.clone(),
            }),
        })
        .collect::<Result<Vec<_>>>()?;

    // Column order follows the select list, which is the only order the caller asked for.
    let mut next = measures.iter().map(|(_, of)| *of);
    let cells: Vec<Cell> = select
        .items
        .iter()
        .zip(units)
        .map(|(i, units)| Cell {
            column: i.column(),
            units,
            of: match i.proj {
                Proj::Column(_) => Of::Key,
                _ => next.next().expect("one measure per aggregate, in select-list order"),
            },
        })
        .collect();

    let having = join_having(select, &scope, &measures, per_key)?;

    let order = join_ordering(select, &sides, &measures, per_key)?;
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
                // One axis, and every side keyed on it and required: the star, which is the
                // only join this surface lowers.
                axes: 1,
                sides: sides
                    .counts
                    .iter()
                    .map(|plan| JoinSide {
                        keyed: Keying::By { plan: *plan, axis: 0 },
                        required: true,
                    })
                    .collect(),
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
        return Err(SqlError::Refused { what: Refused::Having, at: h.at() });
    }
    let number = |a: &_, at: usize| {
        let of = names(a, measures).ok_or(SqlError::Refused { what: Refused::Having, at })?;
        // An average is fractional and this comparison is not. Rounding one into the other
        // would answer a question next to the one that was asked - the same refusal a grouping
        // gives, over the variant a join's average takes.
        if matches!(of, Of::PairedRatio { .. }) {
            return Err(SqlError::Refused { what: Refused::Having, at });
        }
        Ok(of)
    };
    // A join has two tables, so which one a threshold is in the units of is a question about
    // the qualifier rather than about the statement.
    let units = |a: &_, at: usize| having_field(a, scope, at);
    Ok(Some(having_tree(h, &number, &units)?))
}

/// The three things a join needs to know per side, in one value.
///
/// These used to be closures over locals, which meant the aggregate loop could only live where
/// they did. A side is a position in `FROM` order; what changes with it is which column keys the
/// records, which plan counts them, and which part of the `WHERE` narrows them. All three lists
/// are as long as the join is wide, and indexed by the same number a cell's `side` is.
struct Sides<'a> {
    scope: &'a Scope<'a>,
    keys: Vec<&'a Name>,
    counts: Vec<usize>,
    rows: Vec<Expr>,
}

impl<'a> Sides<'a> {
    /// The column this side is joined on.
    fn key(&self, side: Side) -> &'a Name {
        self.keys[side]
    }

    /// The records this side's part of the `WHERE` leaves.
    fn rows(&self, side: Side) -> Expr {
        self.rows[side].clone()
    }

    /// One aggregate over the join, as the per-key numbers it is arithmetic over.
    ///
    /// **Nothing here pairs records.** For each key every side holds the join is the Cartesian
    /// product of the records holding it, so every aggregate over that product is a product of
    /// per-key numbers - which is why each of these is an [`Of::Paired`] naming one plan and the
    /// position it sits at rather than a plan of its own.
    fn cell(&self, item: &Item, per_key: bool, calls: &mut Calls) -> Result<Of> {
        if item.filter.is_some() {
            // A `FILTER` narrows one aggregate's records. Over a join that would leave the
            // sides disagreeing about which keys are in it, and a key one side dropped is a
            // pairing that never happened rather than a group with a zero in it.
            return Err(SqlError::Refused { what: Refused::JoinShape, at: item.at });
        }
        Ok(match &item.proj {
            Proj::Now { unix_seconds } => Of::Now { unix_seconds: *unix_seconds },
            // A rounding is applied to values read back per record, and a grouped or joined
            // answer holds none: what it carries per row is a key and the numbers folded under
            // it. Refused rather than silently rounding something else.
            Proj::TimeOf { .. } => {
                return Err(SqlError::Refused { what: Refused::Shape, at: item.at })
            }
            // Every record on one side pairs with every record on every other, under each key.
            // Counted from the first side, which is as good as any: the product is over all of
            // them and the shape names them in `keys`.
            Proj::Count => Of::Paired { plan: self.counts[0], side: 0, how: Pairing::Product },
            Proj::Agg { func, field } => {
                let side = self.scope.side(field, item.at)?;
                let plan = calls.push(
                    self.scope.table(side),
                    grouped_aggregate(&self.rows(side), self.key(side), *func, field),
                )?;
                Of::Paired {
                    plan,
                    side,
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
                Of::SharedKeys
            }
            // An average over a join is that side's total over that side's record count, and
            // **both are folded before they are divided**: each is scaled by the same per-key
            // product, which is inside both sums rather than outside the fraction, so the mean
            // of the per-key means is a different number and not the one asked for.
            Proj::Avg(field) => {
                let side = self.scope.side(field, item.at)?;
                let top = calls.push(
                    self.scope.table(side),
                    grouped_aggregate(&self.rows(side), self.key(side), Agg::Sum, field),
                )?;
                // This side's own per-key record count, which is the plan the join is built out
                // of - so an average costs one call rather than two.
                Of::PairedRatio { top, bottom: self.counts[side], side }
            }
            Proj::TopKeys { .. } | Proj::Quantile { .. } | Proj::Star | Proj::Column(_) => {
                return Err(SqlError::Refused { what: Refused::JoinShape, at: item.at })
            }
        })
    }

    /// Whether the answer is one row per key, and that the `GROUP BY` names the key it must.
    ///
    /// `GROUP BY` the join key is the only grouping a join has: the key is what pairs the
    /// records, so it is the only column every side agrees about.
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
    sides: &Sides<'_>,
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
        // Answered now that a join has an average to order by, and by the same lookup every
        // other aggregate uses: it has to be one the select list produced.
        OrderKey::Avg(field) => match names(&HavingAgg::Avg(field.clone()), measures) {
            Some(of) => OrderBy::Value { of },
            None => return Err(refuse()),
        },
        OrderKey::Name(n) => match value_names.iter().find(|(c, _)| *c == n.column) {
            Some((_, of)) => OrderBy::Value { of: *of },
            // The join key, under any table's spelling or an alias of it.
            None if sides.keys.iter().any(|k| k.column == n.column) => OrderBy::Key,
            None if n.qualifier.is_some() && sides.scope.side(n, order.at).is_ok() => {
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

/// A `WHERE` split into each table's own conditions, one entry per side.
///
/// **Only an `AND` can be split.** `a.x = 1 OR b.y = 2` selects pairs where either side
/// matched, and neither side can be filtered to that on its own - so it is refused rather than
/// applied to one of them and quietly answering something narrower.
fn split(cond: &Cond, scope: &Scope<'_>, at: usize, n: usize) -> Result<Vec<Option<Cond>>> {
    if let Cond::And(a, b) = cond {
        let (a, b) = (split(a, scope, at, n)?, split(b, scope, at, n)?);
        return Ok(a.into_iter().zip(b).map(|(x, y)| both(x, y)).collect());
    }
    let mask = touches(cond, scope, at)?;
    // A term naming two tables, or - impossibly, since a condition always names a column - none.
    if mask.count_ones() != 1 {
        return Err(SqlError::Refused { what: Refused::JoinFilter, at });
    }
    let mut out = vec![None; n];
    out[mask.trailing_zeros() as usize] = Some(cond.clone());
    Ok(out)
}

/// Which of a join's tables a condition mentions, as one bit per side.
///
/// A bitmask rather than a list because the recursion unions two of these at every `AND`, and
/// the width it has to hold is the fan-out cap - which `joined` checks before calling this.
fn touches(cond: &Cond, scope: &Scope<'_>, at: usize) -> Result<u32> {
    Ok(match cond {
        Cond::And(a, b) | Cond::Or(a, b) => touches(a, scope, at)? | touches(b, scope, at)?,
        Cond::Not(inner) => touches(inner, scope, at)?,
        Cond::Cmp { field, .. } | Cond::In { field, .. } | Cond::Between { field, .. } => {
            1 << scope.side(field, at)?
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
