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

use crate::ast::{Agg, HavingAgg, Item, Name, Proj};
use crate::shape::{Absent, Of, Units};

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
        // A count is in records, a `topK` answers with keys, a key is not a number, and a star
        // is record ids.
        Proj::Count
        | Proj::CountDistinct(_)
        | Proj::TopKeys { .. }
        | Proj::Star
        | Proj::Column(_) => None,
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

/// The column an aggregate's threshold is written in the units of, if any.
pub(super) fn field_of(having: &HavingAgg) -> Option<String> {
    match having {
        HavingAgg::Count => None,
        HavingAgg::Agg { field, .. } | HavingAgg::Avg(field) => Some(field.column.clone()),
    }
}
