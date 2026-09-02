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

//! What each aggregate in a select list measures, and how a later clause names one.
//!
//! A grouped answer carries one number per group per aggregate, so `HAVING` and `ORDER BY` can
//! only name a number the select list already asked for. Comparing by value is what decides
//! that: the same aggregate over the same column is the same number, and anything else would
//! have to be computed.

use crate::ast::{self, Agg, HavingAgg, HavingOperand, Item, Name, Proj, Select};
use crate::error::{Refused, Result, SqlError};
use crate::shape::{Absent, Having, Of, Operand, Threshold, Units};
use big_plan::ast::Literal;

/// What the number one select-list entry produces is measured in.
///
/// **Whichever entries read a field's values are in that field's units**, and a decimal field
/// stores an integer - so a `sum` over one merges to 1250 where the values were 12.50. Naming
/// the field here is what lets [`crate::Shape::resolve`] scale it back on the way out, against
/// the same schema `WHERE price = 12.50` is converted against.
///
/// A count is in records and a `topK` answers with keys, so neither has a field to be measured
/// in. An average is a quotient of a sum in the field's units by a count, so it is in them too.
pub(super) fn units_of(table: &str, proj: &Proj) -> Units {
    // The units a scalar is handed are its leaf's; what it turns them into is decided in
    // [`crate::Shape::resolve`], where the expression is walked against the schema. Same
    // arrangement a rounded timestamp has always had, and for the same reason: only one of the
    // two ends knows the field, and only the other knows the call.
    if let Proj::Scalar { inner, .. } = proj {
        return units_of(table, inner);
    }
    // A moment, not a plain number. Without this the seconds it carries render as the count
    // they are, which is the same mistake a decimal read back unscaled makes.
    if let Proj::Now { .. } = proj {
        return Units::Seconds;
    }
    let Some(field) = field_measured(proj) else { return Units::PLAIN };
    // **The table, not the qualifier.** A qualifier may be an alias - `FROM tx AS t` makes
    // `t.amount` a column of `tx` - and what resolves a field is the name the catalog knows.
    // With one table in the statement there is nothing else it could belong to; a join has two,
    // and resolves the qualifier through its own scope before it gets here.
    Units::Written { table: table.to_string(), field: field.column.clone() }
}

/// The column an entry's number comes out of, or `None` when it does not come out of one.
pub(super) fn field_measured(proj: &Proj) -> Option<&Name> {
    match proj {
        Proj::Agg { field, .. } | Proj::Avg(field) | Proj::Quantile { field, .. } => Some(field),
        // The leaf's, because a scalar folds nothing: `round(sum(amount), 2)` is a sum of
        // `amount` with a rounding on the way out, and the column it measures is still `amount`.
        Proj::Scalar { inner, .. } => field_measured(inner),
        // A count is in records, a `topK` answers with keys, a key is not a number, and a star
        // is record ids.
        // `now()` measures nothing at all, and a rounded column is read back per record rather
        // than folded - neither is a number a `HAVING` could name.
        Proj::Count
        | Proj::CountDistinct(_)
        | Proj::TopKeys { .. }
        | Proj::Star
        | Proj::Column(_)
        | Proj::Now { .. } => None,
    }
}

/// What one aggregate in the select list measures.
///
/// Compared by value, which is how `HAVING` and `ORDER BY` decide whether they name a number
/// the answer actually holds: the same aggregate over the same column is the same number, and
/// anything else would have to be computed.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) enum Measure {
    /// The group's own count.
    Count,
    /// `sum(f)`, `min(f)`, `max(f)`.
    Agg {
        /// Which one.
        func: Agg,
        /// The column it measures.
        field: Name,
    },
    /// `avg(f)`.
    Avg(Name),
    /// An aggregate narrowed by a `FILTER`, which no bare `HAVING` or `ORDER BY` can name.
    ///
    /// Deliberately opaque: `HAVING count(*) > 5` over a select list holding only
    /// `count(*) FILTER (WHERE ...)` is naming a total the answer does not contain, and
    /// matching it to the filtered one would silently answer a different question. What names a
    /// filtered aggregate is its alias, which is the spelling that says which one is meant.
    Filtered,
}

pub(super) fn measure_of(item: &Item) -> Measure {
    if item.filter.is_some() {
        return Measure::Filtered;
    }
    match &item.proj {
        Proj::Agg { func, field } => Measure::Agg { func: *func, field: field.clone() },
        Proj::Avg(field) => Measure::Avg(field.clone()),
        _ => Measure::Count,
    }
}

/// The number a `HAVING` names, or `None` when the answer holds no such number.
pub(super) fn names(having: &HavingAgg, measures: &[(Measure, Of)]) -> Option<Of> {
    let want = match having {
        HavingAgg::Count => Measure::Count,
        HavingAgg::Agg { func, field } => Measure::Agg { func: *func, field: field.clone() },
        HavingAgg::Avg(field) => Measure::Avg(field.clone()),
    };
    // A grouping with no aggregate in its select list is a `Distinct`, whose counts exist and
    // are simply not rendered - so `HAVING count(*) >= 10` over it is answerable.
    if measures.is_empty() && want == Measure::Count {
        return Some(Of::Group { plan: 0, absent: Absent::Zero });
    }
    measures.iter().find(|(m, _)| *m == want).map(|(_, of)| *of)
}

/// A `HAVING` tree, resolved against the numbers this answer actually holds.
///
/// **One walk for every shape that has a `HAVING`.** Four of them do — a grouping, a pair
/// grouping, a join and an ungrouped aggregate — and they differ in exactly two ways, which is
/// why those two are the closures rather than four copies of the walk:
///
/// - `number` says which of this answer's numbers an aggregate is, and refuses the ones this
///   shape cannot compare. A grouping's average is an [`Of::Ratio`] and a join's is an
///   [`Of::PairedRatio`]; both are fractional where this comparison is not, and each shape
///   knows which of them is its own.
/// - `units` says which field a threshold beside that aggregate is written in the units of. A
///   grouping reads it off the one table; a join has two and has to resolve the qualifier.
///
/// The units of a literal come from **the aggregate on the other side of the comparison**,
/// which is the only thing they could come from: `sum(price) >= 10.00` is ten pounds because
/// `price` is in pounds, and the same `10.00` beside a count would be ten records.
pub(super) fn having_tree(
    h: &ast::Having,
    number: &impl Fn(&HavingAgg, usize) -> Result<Of>,
    units: &impl Fn(&HavingAgg, usize) -> Result<Option<(String, String)>>,
) -> Result<Having> {
    Ok(match h {
        ast::Having::And(a, b) => Having::And(
            Box::new(having_tree(a, number, units)?),
            Box::new(having_tree(b, number, units)?),
        ),
        ast::Having::Or(a, b) => Having::Or(
            Box::new(having_tree(a, number, units)?),
            Box::new(having_tree(b, number, units)?),
        ),
        ast::Having::Not(a) => Having::Not(Box::new(having_tree(a, number, units)?)),
        ast::Having::Cmp { left, op, right, at } => {
            // Both sides are read before either is turned into an operand, because a literal's
            // units are the *other* side's.
            let agg_of = |o: &HavingOperand| match o {
                HavingOperand::Agg(a) => Some(a.clone()),
                HavingOperand::Value(_) => None,
            };
            let (la, ra) = (agg_of(left), agg_of(right));
            // Two constants compared to each other is a statement about nothing the answer
            // holds - true or false for every group alike, and never what anybody meant.
            if la.is_none() && ra.is_none() {
                return Err(SqlError::Refused { what: Refused::Having, at: *at });
            }
            let side = |o: &HavingOperand, other: Option<&HavingAgg>| -> Result<Operand> {
                Ok(match o {
                    // The operand carries what it is measured in, for the same reason a cell
                    // does: only a schema knows a scale, and a comparison between two of these
                    // has to be told whether they are in the same one.
                    HavingOperand::Agg(a) => Operand::Of {
                        of: number(a, *at)?,
                        units: match units(a, *at)? {
                            Some((table, field)) => Units::Written { table, field },
                            // A count is in records, which is a unit no field defines.
                            None => Units::PLAIN,
                        },
                    },
                    HavingOperand::Value(v) => Operand::Value(match other {
                        // An aggregate's threshold is in the field's units, and only a schema
                        // knows what those are. `Shape::resolve` asks, with the planner's own
                        // conversion.
                        Some(a) => match units(a, *at)? {
                            Some((table, field)) => {
                                Threshold::Written { table, field, value: v.clone() }
                            }
                            // A count is in records, which is a unit no field defines. Anything
                            // but a whole number of them is not a count, and is refused here
                            // rather than rounded into one.
                            None => match v {
                                Literal::Int(n) => Threshold::Units(i128::from(*n)),
                                _ => {
                                    return Err(SqlError::Refused {
                                        what: Refused::Having,
                                        at: *at,
                                    })
                                }
                            },
                        },
                        // Unreachable: at least one side is an aggregate, checked above.
                        None => return Err(SqlError::Refused { what: Refused::Having, at: *at }),
                    }),
                })
            };
            Having::Cmp { left: side(left, ra.as_ref())?, op, right: side(right, la.as_ref())? }
        }
    })
}

/// The `HAVING` of a shape whose numbers are named by [`names`] and measured on one table.
///
/// The grouped and ungrouped cases, which differ in nothing a `HAVING` can see.
pub(super) fn having_of(
    select: &Select,
    table: &str,
    measures: &[(Measure, Of)],
) -> Result<Option<Having>> {
    let Some(h) = &select.having else { return Ok(None) };
    let number = |a: &HavingAgg, at: usize| {
        let of = names(a, measures).ok_or(SqlError::Refused { what: Refused::Having, at })?;
        // An average is fractional and this comparison is not. Rounding one into the other
        // would answer a question next to the one that was asked.
        if matches!(of, Of::Ratio { .. }) {
            return Err(SqlError::Refused { what: Refused::Having, at });
        }
        Ok(of)
    };
    let units = |a: &HavingAgg, _at: usize| Ok(field_of(a).map(|f| (table.to_string(), f)));
    Ok(Some(having_tree(h, &number, &units)?))
}

/// The column an aggregate's threshold is written in the units of, if any.
pub(super) fn field_of(having: &HavingAgg) -> Option<String> {
    match having {
        HavingAgg::Count => None,
        HavingAgg::Agg { field, .. } | HavingAgg::Avg(field) => Some(field.column.clone()),
    }
}
