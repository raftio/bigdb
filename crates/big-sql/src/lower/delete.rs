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

//! A delete's `WHERE`, lowered by exactly the function a `SELECT`'s is.
//!
//! Twenty lines, and that is the point: there is nothing about being a delete that changes what a
//! condition selects. `cond::rows` is total over every shape the parser accepts, so this cannot
//! fail for a reason about the predicate - only the table name is left to be wrong, and that is
//! the planner's to say.

use super::pql::as_call;
use crate::ast::Delete as Written;
use crate::delete::Delete;

/// The statement, as the call that selects what it clears.
pub fn lower(written: &Written) -> Delete {
    Delete {
        database: written.database.clone(),
        table: written.table.clone(),
        // The same call `SELECT * FROM t WHERE <the same predicate>` produces. A corpus case
        // writes both and expects one tree, because two lowerings of one predicate is two sets
        // waiting to differ - and the one that differed would be the one doing the deleting.
        rows: as_call(super::cond::rows(&written.filter)),
        reads: reads(&written.filter),
    }
}

/// The other tables a filter reads, in the order they are written and without repeats.
///
/// Only `IN (SELECT ... FROM b)` names one - it is the only construct in a `WHERE` that reaches
/// a second table - so this walk is short and stays short: a `Cond` that grew another way to
/// read one would fail to compile here rather than quietly go undemanded.
fn reads(cond: &crate::ast::Cond) -> Vec<String> {
    use crate::ast::Cond;
    let mut out = Vec::new();
    fn walk(cond: &Cond, out: &mut Vec<String>) {
        match cond {
            Cond::InRecords { table, filter, .. } => {
                let name = table.qualified();
                if !out.contains(&name) {
                    out.push(name);
                }
                // A filter on the inner set reads the same table, so there is nothing further to
                // collect - but it is walked anyway, because nesting is the parser's to bound
                // and not this function's to assume.
                if let Some(f) = filter {
                    walk(f, out);
                }
            }
            Cond::And(a, b) | Cond::Or(a, b) => {
                walk(a, out);
                walk(b, out);
            }
            Cond::Not(inner) => walk(inner, out),
            // Every other term is about this table's own columns, so none of them reads a
            // second one. Listed rather than wildcarded: a `Cond` that grew a way to reach
            // another table must fail to compile here instead of going quietly undemanded,
            // which is the whole value of this match being exhaustive.
            Cond::Cmp { .. }
            | Cond::In { .. }
            | Cond::Between { .. }
            | Cond::Like { .. }
            | Cond::Rounded { .. }
            | Cond::Segment { .. } => {}
        }
    }
    walk(cond, &mut out);
    out
}

/// The same, for an `UPDATE`.
///
/// Here rather than in a module of its own because it is the same lowering: an update's `WHERE`
/// selects the records to write, exactly as a delete's selects the records to clear, and both go
/// through `cond::rows`. Two functions in one file so that a change to one is written next to
/// the other rather than found later.
pub fn update(written: &crate::ast::Update) -> crate::update::Update {
    crate::update::Update {
        database: written.database.clone(),
        table: written.table.clone(),
        assignments: written.assignments.clone(),
        rows: as_call(super::cond::rows(&written.filter)),
        reads: reads(&written.filter),
    }
}
