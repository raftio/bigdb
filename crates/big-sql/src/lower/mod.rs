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

//! [`Select`] to a query-language [`Call`].
//!
//! **The output is PQL, not a plan.** That is the whole architecture of this crate in one
//! sentence: the planner resolves what comes out of here exactly as it resolves text a user
//! typed, so there is one place that knows what `country = 'GB'` means against a keyed field,
//! one place that turns `12.50` into the units a decimal stores, and one set of error codes for
//! both surfaces. Lowering to a `Plan` directly would have made a second one of each, and the
//! second copy is the one that drifts.
//!
//! Nothing here consults a schema, so nothing here can fail for a reason about the database.
//! Every failure in this module is about the *statement*: a shape with no plan behind it, or a
//! construct the engine refuses.

use crate::ast::{Item, Name, Proj, Query, Select};
use crate::error::{Refused, Result, SqlError};
use crate::shape::{Answer, Shape};
use big_plan::ast::{Call, Expr};

/// How many plans one statement may ask for.
///
/// **A bound on the fan-out, not a taste in select lists.** Each call is planned, sent to every
/// owner and merged on its own, so a statement's cost in round trips is the length of this
/// list. Sixteen is past any dashboard row and short of a statement that would quietly become a
/// denial of service; `avg` costs two, which is the case worth knowing about.
pub const MAX_CALLS: usize = 16;

/// How many records one projection may read.
///
/// **A bound on the point reads, not a taste in page sizes.** A projection reconstructs a value
/// per record per column, and the number of records is the whole of what it costs. Ten thousand
/// is a page a client can hold and a cost a node can absorb; past that the answer wants a
/// cursor, which is what `GET /table/{t}/records` is for.
pub const MAX_PROJECTION: usize = 10_000;

mod cond;
mod grouped;
mod join;
mod measure;
mod pairs;
mod pql;
mod ungrouped;

use cond::rows;
use pql::{call, call_of, field_arg};

/// One query-language call, and the table it is asked of.
///
/// The table travels with the call rather than beside the statement, because a join asks each
/// of its two tables a question of its own: the left side's calls name the left table and the
/// right side's name the right. A single-table statement is the case where every ask names the
/// same one.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Ask {
    /// The table. A query is asked *of* an index, and the planner takes the table as a
    /// parameter rather than finding it in the text.
    pub table: String,
    /// The call.
    pub call: Call,
}

/// A number found by asking the same question with moving bounds until it converges.
///
/// **The one thing here that is a search rather than a question.** A quantile is the value at a
/// rank, and no plan answers that: what a plan answers is how many records hold a value at or
/// below a bound. So the bound moves until the count lands on the rank, and each step is an
/// ordinary `Count` the engine already answers - fanned out and merged like any other, which is
/// why an exact quantile needed no `Plan` variant and no merge arm either.
///
/// It costs one round trip per step, about the bit depth of the field. That is the honest price
/// of holding no values in memory: ClickHouse's exact quantile keeps every value it saw, and
/// this keeps none.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Probe {
    /// The table.
    pub table: String,
    /// The records the quantile is over, as the call that selects them.
    pub rows: Call,
    /// The column.
    pub field: String,
    /// Which quantile, in parts per thousand. 500 is the median.
    pub per_mille: u32,
}

/// A statement, translated.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Statement {
    /// The query-language calls this statement means, in the order the shape names them.
    ///
    /// Usually one. Several when the select list asks several questions of the same records:
    /// `SELECT count(*), sum(amount)` is two calls, each planned, fanned out and merged exactly
    /// as if it had been written alone. Nothing below this line learns that they arrived
    /// together, which is what keeps [`crate::Shape`]'s promise — no new `Plan` variant, no new
    /// merge arm — while the surface grows.
    pub calls: Vec<Ask>,
    /// The searches this statement makes, run after the calls and appended to their answers.
    ///
    /// Separate from `calls` because they are a different shape of work: a call is asked once
    /// and a probe is asked until it converges. A caller that could not tell them apart would
    /// have to guess how many round trips a statement costs.
    pub probes: Vec<Probe>,
    /// What the caller asked to see of the answers, and how to write it.
    pub answer: Answer,
}

impl Statement {
    /// The tables this statement reads, in the order they were written, without repeats.
    pub fn tables(&self) -> Vec<&str> {
        let mut out: Vec<&str> = Vec::new();
        for ask in &self.calls {
            if !out.contains(&ask.table.as_str()) {
                out.push(&ask.table);
            }
        }
        out
    }
}

/// The calls a statement makes, indexed the way a [`Cell`] names them.
///
/// Deduplicating is not an optimisation here so much as the obvious reading of the statement:
/// `SELECT country, count(*), avg(amount) GROUP BY country` needs the group counts twice and
/// there is one question behind them, so it is asked once.
struct Calls {
    out: Vec<Ask>,
    at: usize,
}

impl Calls {
    fn new(at: usize) -> Self {
        Self { out: Vec::new(), at }
    }

    fn push(&mut self, table: &str, call: Call) -> Result<usize> {
        let ask = Ask { table: table.to_string(), call };
        if let Some(i) = self.out.iter().position(|c| *c == ask) {
            return Ok(i);
        }
        if self.out.len() == MAX_CALLS {
            return Err(SqlError::Refused { what: Refused::TooManyCalls, at: self.at });
        }
        self.out.push(ask);
        Ok(self.out.len() - 1)
    }
}

/// Translates one parsed statement, which may be several stacked by `UNION ALL`.
///
/// Each branch is lowered on its own and its plans appended to one flat list, with its shape
/// rebased onto that list. Everything downstream therefore sees a statement with some calls and
/// a shape, exactly as it did before unions existed.
pub fn lower(query: &Query) -> Result<Statement> {
    let [only] = query.branches.as_slice() else { return union(query) };
    let mut out = lower_one(only)?;
    out.answer.calls = out.calls.len();
    Ok(out)
}

/// Several branches, stacked.
fn union(query: &Query) -> Result<Statement> {
    let at = query.branches.first().and_then(|b| b.items.first()).map_or(0, |i| i.at);
    let mut calls: Vec<Ask> = Vec::new();
    let mut probes: Vec<Probe> = Vec::new();
    let mut branches: Vec<Shape> = Vec::new();
    let mut width = None;

    for branch in &query.branches {
        let one = lower_one(branch)?;
        // The branches have to line up. SQL names the columns after the first, so a second
        // branch of a different width is a statement with no answer rather than a wider one.
        let here = one.answer.shape.columns().len();
        match width {
            None => width = Some(here),
            Some(w) if w == here => {}
            Some(_) => {
                return Err(SqlError::Refused { what: Refused::Union, at: branch_at(branch) })
            }
        }
        if calls.len() + one.calls.len() > MAX_CALLS {
            return Err(SqlError::Refused { what: Refused::TooManyCalls, at });
        }
        branches.push(one.answer.shape.rebase(calls.len()).rebase_probes(probes.len()));
        calls.extend(one.calls);
        probes.extend(one.probes);
    }

    let total = calls.len();
    Ok(Statement {
        calls,
        probes,
        answer: Answer {
            calls: total,
            shape: Shape::Union { branches },
            // The last branch's, which is where ClickHouse writes it.
            format: query.branches.last().map(|b| b.format).unwrap_or_default(),
        },
    })
}

fn branch_at(select: &Select) -> usize {
    select.items.first().map_or(0, |i| i.at)
}

/// Translates one `SELECT`.
fn lower_one(select: &Select) -> Result<Statement> {
    let rows = match &select.filter {
        Some(cond) => rows(cond),
        None => call("All", vec![]),
    };

    // Three buckets, because the select list decides the plans and every legal list is one of a
    // small number of combinations of them.
    let mut stars = Vec::new();
    let mut columns = Vec::new();
    let mut aggregates = Vec::new();
    for item in &select.items {
        match &item.proj {
            Proj::Star => stars.push(item),
            Proj::Column(name) => columns.push((item, name.clone())),
            _ => aggregates.push(item),
        }
    }

    if let Some(join) = &select.join {
        return join::joined(select, join, &stars, &columns, &aggregates);
    }

    let table = &select.from.table;
    match select.group_by.as_slice() {
        [] => ungrouped::ungrouped(select, table, &rows, &stars, &columns, &aggregates),
        [one] => grouped::grouped(select, table, &rows, one, &stars, &columns, &aggregates),
        // Two columns: one pass over the second per value of the first. See `pairs`.
        [left, right] => {
            pairs::pairs(select, table, &rows, (left, right), &stars, &columns, &aggregates)
        }
        // The parser allows at most two, so this is unreachable rather than a third refusal.
        _ => unreachable!("at most two grouped columns"),
    }
}

/// A shape, under the format the statement asked for it in.
///
/// One function so that every path through the lowering carries the `FORMAT` clause, rather
/// than four that each have to remember to.
fn answer(select: &Select, shape: Shape) -> Answer {
    // Filled in by `lower`, which is where the call list is final.
    Answer { shape, format: select.format, calls: 0 }
}

/// The records one select-list entry measures: the statement's `WHERE`, narrowed by this
/// entry's own `FILTER (WHERE ...)`.
///
/// An intersection rather than anything cleverer, because that is exactly what the clause
/// means. `Intersect` is variadic and the executor short-circuits one that has already emptied,
/// so a filtered aggregate is not a second pass over the records - it is a second bitmap
/// intersected into the first.
fn rows_of(rows: &Expr, item: &Item) -> Expr {
    match &item.filter {
        None => rows.clone(),
        Some(cond) => call("Intersect", vec![rows.clone(), self::rows(cond)]),
    }
}

/// The plan behind a written `count(*)` over a grouping, which several cells can share.
///
/// `GroupBy` with no aggregate rather than `Distinct`, which the planner resolves to a grouping
/// whose measure is a count. The two answer identically and `Distinct` is the cheaper of them -
/// it counts rows instead of materialising each group's records and then counting - so this is
/// worth revisiting. It is left alone here because the analytical benchmark's `big-sql` column
/// is measured against a PQL `GroupBy`, and a phase that adds clauses should not quietly change
/// what is being measured. `SELECT c FROM t GROUP BY c`, which nobody has benchmarked, already
/// takes the cheaper one.
fn count_plan(calls: &mut Calls, table: &str, rows: &Expr, group: &Name) -> Result<usize> {
    calls.push(table, call_of("GroupBy", vec![rows.clone(), field_arg(group)]))
}
