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

//! A resolved query, written back as a tree somebody can read.
//!
//! # Why this exists rather than `{:#?}`
//!
//! A derived `Debug` prints the struct, not the query: every field name, every `String`
//! wrapper, every `Box`. That is unreadable at the size a plan actually reaches, and - worse
//! for the use this printer was written for - it changes when a field is added that no plan
//! ever varies in. A test corpus whose expected output is a `Debug` dump is a corpus that
//! churns for reasons unrelated to what it claims.
//!
//! So this prints the *shape*: which records, and what is computed over them. Two plans that
//! answer the same question print the same lines, and a line that changes is a change in the
//! answer.
//!
//! # Totality
//!
//! Nothing here elides. The one place it comes close is the aggregate inside a grouping, which
//! the planner only ever builds over a placeholder bitmap ([`Rows::All`]) - so it folds onto
//! the head line as `-> sum(amount)` instead of costing three lines to say `all` in the middle
//! of. If that invariant ever stops holding, the fold does not apply and the aggregate prints
//! as a full child instead, which is the whole reason the fold is written as a match that can
//! fail rather than an assumption.

use crate::plan::{CmpOp, Plan, Rows};

/// The plan, as a tree of lines. No trailing newline.
///
/// ```text
/// GroupBy tx.country -> sum(amount)
/// └── intersect
///     ├── country = 'GB'
///     └── active = true
/// ```
pub fn explain(plan: &Plan) -> String {
    let mut out = String::new();
    out.push_str(&head(plan));
    let mut kids: Vec<Node> = vec![Node::Rows(rows_of(plan))];
    // Only when the fold on the head line did not take it - see the note on totality above.
    if let Some(agg) = unfolded_aggregate(plan) {
        kids.push(Node::Plan(agg));
    }
    write_children(&mut out, &kids, "");
    out
}

/// What a plan's children are, which is a bitmap and - in the case the fold declines - a plan.
enum Node<'a> {
    Rows(&'a Rows),
    Plan(&'a Plan),
}

/// The plan's own line: what it computes, over which table's which column.
fn head(plan: &Plan) -> String {
    match plan {
        Plan::Rows { table, .. } => format!("Rows {table}"),
        Plan::Count { table, .. } => format!("Count {table}"),
        Plan::Sum { table, field, .. } => format!("Sum {table}.{field}"),
        Plan::Min { table, field, .. } => format!("Min {table}.{field}"),
        Plan::Max { table, field, .. } => format!("Max {table}.{field}"),
        Plan::Distinct { table, field, .. } => format!("Distinct {table}.{field}"),
        // `usize::MAX` is the planner's "no cut", not a limit anybody wrote - a ranking under a
        // `HAVING` is built whole, because the groups that fail the predicate have to be dropped
        // before there is anything to truncate. Printed as the number it is, it would read as a
        // limit somebody chose, the same way a sentinel time bound would read as a date.
        Plan::TopN { table, field, n, .. } if *n == usize::MAX => {
            format!("TopN {table}.{field} n=unbounded")
        }
        Plan::TopN { table, field, n, .. } => format!("TopN {table}.{field} n={n}"),
        Plan::GroupBy { table, field, aggregate, .. } => {
            format!("GroupBy {table}.{field}{}", folded(aggregate))
        }
        Plan::GroupByPair { table, left, right, left_max, aggregate, .. } => format!(
            "GroupByPair {table}.({left}, {right}) left_max={left_max}{}",
            folded(aggregate)
        ),
        Plan::Project { table, fields, limit, .. } => {
            // No `limit=` at all where there is none, rather than `limit=none`: the line is
            // read as the shape of the work, and a full scan is the absence of a cut.
            let cut = limit.map_or(String::new(), |n| format!(" limit={n}"));
            format!("Project {table}.({}){cut}", fields.join(", "))
        }
    }
}

/// The bitmap every plan is computed over.
fn rows_of(plan: &Plan) -> &Rows {
    match plan {
        Plan::Rows { rows, .. }
        | Plan::Count { rows, .. }
        | Plan::Sum { rows, .. }
        | Plan::Min { rows, .. }
        | Plan::Max { rows, .. }
        | Plan::Distinct { rows, .. }
        | Plan::TopN { rows, .. }
        | Plan::GroupBy { rows, .. }
        | Plan::GroupByPair { rows, .. }
        | Plan::Project { rows, .. } => rows,
    }
}

/// A grouping's aggregate as a suffix on the head line, when it has the shape the planner
/// builds - an aggregate over the placeholder bitmap and nothing else.
///
/// The empty string when it does not, so that [`unfolded_aggregate`] prints it in full. The two
/// functions answer the same question and must stay opposites; they are next to each other so
/// that is visible.
fn folded(aggregate: &Plan) -> String {
    match aggregate {
        Plan::Count { rows: Rows::All, .. } => " -> count".to_string(),
        Plan::Sum { rows: Rows::All, field, .. } => format!(" -> sum({field})"),
        Plan::Min { rows: Rows::All, field, .. } => format!(" -> min({field})"),
        Plan::Max { rows: Rows::All, field, .. } => format!(" -> max({field})"),
        _ => String::new(),
    }
}

/// The aggregate a grouping carries, when the head line did not fold it.
fn unfolded_aggregate(plan: &Plan) -> Option<&Plan> {
    let aggregate = match plan {
        Plan::GroupBy { aggregate, .. } | Plan::GroupByPair { aggregate, .. } => aggregate,
        _ => return None,
    };
    folded(aggregate).is_empty().then_some(aggregate.as_ref())
}

/// One line for a bitmap node, without its children.
fn rows_head(rows: &Rows) -> String {
    match rows {
        Rows::All => "all".to_string(),
        Rows::Compare { field, op, value } => format!("{field} {} {value}", cmp(*op)),
        // The marker is not decoration. A signed field's bound is an `i64` all the way down and
        // a positive one prints identically to an unsigned bound, so without it two different
        // plans would print the same lines - which is the one thing this printer may not do.
        Rows::CompareSigned { field, op, value } => {
            format!("{field} {} {value} [signed]", cmp(*op))
        }
        Rows::Key { field, value } => format!("{field} = '{value}'"),
        Rows::KeyBetween { field, value, from, to } => {
            format!("{field} = '{value}' in [{}, {}]", bound(from), bound(to))
        }
        Rows::Bool { field, value } => format!("{field} = {value}"),
        Rows::Intersect(_) => "intersect".to_string(),
        Rows::Union(_) => "union".to_string(),
        Rows::Difference(..) => "minus".to_string(),
        Rows::Not(_) => "not".to_string(),
    }
}

/// A bitmap node's children, which only the set operations have.
fn rows_kids(rows: &Rows) -> Vec<&Rows> {
    match rows {
        Rows::Intersect(parts) | Rows::Union(parts) => parts.iter().collect(),
        Rows::Difference(a, b) => vec![a, b],
        Rows::Not(inner) => vec![inner],
        _ => Vec::new(),
    }
}

/// An absent bound printed as absent. A sentinel would read as a date somebody chose.
fn bound(b: &Option<i64>) -> String {
    match b {
        Some(v) => v.to_string(),
        None => "..".to_string(),
    }
}

fn cmp(op: CmpOp) -> &'static str {
    match op {
        CmpOp::Gt => ">",
        CmpOp::Ge => ">=",
        CmpOp::Lt => "<",
        CmpOp::Le => "<=",
        CmpOp::Eq => "=",
        CmpOp::Ne => "!=",
    }
}

/// Draws `kids` under a parent whose own line is already written, indenting each subtree by
/// `prefix` plus the elbow its position calls for.
fn write_children(out: &mut String, kids: &[Node], prefix: &str) {
    for (i, kid) in kids.iter().enumerate() {
        let last = i + 1 == kids.len();
        let (elbow, carry) = if last { ("└── ", "    ") } else { ("├── ", "│   ") };
        let inner = format!("{prefix}{carry}");
        out.push('\n');
        out.push_str(prefix);
        out.push_str(elbow);
        match kid {
            Node::Rows(rows) => {
                out.push_str(&rows_head(rows));
                let below: Vec<Node> = rows_kids(rows).into_iter().map(Node::Rows).collect();
                write_children(out, &below, &inner);
            }
            Node::Plan(plan) => {
                out.push_str(&head(plan));
                let mut below: Vec<Node> = vec![Node::Rows(rows_of(plan))];
                if let Some(agg) = unfolded_aggregate(plan) {
                    below.push(Node::Plan(agg));
                }
                write_children(out, &below, &inner);
            }
        }
    }
}
