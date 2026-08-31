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

//! Turning a statement's answers into the rows it asked for.
//!
//! **This is the coordinator's last step, and it has to be.** A `Shape` says what a `SELECT`
//! wanted that no single plan could carry - a `HAVING`, an `OFFSET`, `WITH TIES`, the division
//! an `avg` is, the arithmetic a join is - and every one of those is only right once every
//! owner's answer is in. `big-sql` decides the shape, the planner and the fan-out answer the
//! plans inside it, and this applies the shape to what came back.
//!
//! It used to live in `big-http`, inside the JSON writer, which made two things true that
//! should not have been: the arithmetic could only be tested through a socket, and the
//! separated formats got their cells by *parsing back* the JSON that had just been written. A
//! [`ResultSet`] is the single definition of what a cell holds, and every format renders from
//! it directly.

mod group;
mod num;

use crate::{Answer, Shape, Value};
use group::{grouped, joined, paired};
use num::{scalar_cell, Num};

/// One row of a result set.
pub type Row = Vec<Datum>;

/// One cell of a result set.
///
/// A type rather than a rendered string, because the three output formats disagree about how a
/// cell is spelled and agree completely about what it is. `Real` is kept apart from `Int` for
/// the same reason: an average has a fractional part and a count does not, and a column that is
/// sometimes one and sometimes the other is a column a client has to sniff.
#[derive(Clone, PartialEq, Debug)]
pub enum Datum {
    /// No value. Not zero: a `min` over no records, an average over none, a group a plan said
    /// nothing about, or a key this node was never told.
    Null,
    /// A count, a total, or an extreme, in the units the field stores.
    Int(i128),
    /// The quotient an average is.
    Real(f64),
    /// A row key, as the string it was interned from.
    Text(String),
    /// The list of keys a `topK` produced, which is what that function answers with.
    Keys(Vec<String>),
}

impl Datum {
    /// One cell of a projected row.
    fn projected(p: &big_exec::Projection) -> Self {
        match p {
            big_exec::Projection::Absent => Self::Null,
            big_exec::Projection::Int(v) => Self::Int(*v),
            big_exec::Projection::Text(s) => Self::Text(s.clone()),
            big_exec::Projection::Texts(v) => Self::Keys(v.clone()),
        }
    }

    /// A number that may not be there.
    fn num(n: Option<Num>) -> Self {
        match n {
            None => Datum::Null,
            Some(Num::Int(v)) => Datum::Int(v),
            Some(Num::Real(v)) => Datum::Real(v),
        }
    }

    /// A key that is there. The absent case is [`Datum::Null`] and every caller spells it out.
    fn text(s: &str) -> Self {
        Datum::Text(s.to_string())
    }
}

/// A result set: the columns a statement named, and the rows its answers came to.
#[derive(Clone, PartialEq, Debug)]
pub struct ResultSet {
    /// The column names, in the order the statement named them.
    pub columns: Vec<String>,
    /// The rows. Each is as wide as `columns`.
    pub rows: Vec<Row>,
}

/// Assembles a statement's answers into the rows its shape asks for.
///
/// `values` holds one answer per plan the statement made, in the order the shape names them,
/// followed by the searches' answers - which is what [`Answer::calls`] records the boundary of.
pub fn result_set(answer: &Answer, values: &[Value]) -> ResultSet {
    ResultSet {
        columns: answer.shape.columns().into_iter().map(str::to_string).collect(),
        rows: rows_of(&answer.shape, values, answer.calls),
    }
}

/// The rows one shape reads out of the answers its plans produced.
///
/// Recursive for exactly one reason: a `UNION ALL` is branches of this.
fn rows_of(shape: &Shape, values: &[Value], probes_at: usize) -> Vec<Row> {
    match shape {
        Shape::Row { cells } => {
            vec![cells.iter().map(|c| scalar_cell(c.of, values, probes_at)).collect()]
        }

        Shape::Records { limit, .. } => match values.first().and_then(Value::as_rows) {
            None => Vec::new(),
            Some(m) => m
                .records_from(0)
                .take(limit.unwrap_or(usize::MAX))
                .map(|r| vec![Datum::Int(i128::from(r))])
                .collect(),
        },

        // Nothing to do but render: the plan carried the columns and the cut, because a
        // projection's cost is a point read per record per column and a cut applied here would
        // be one applied after paying for it.
        Shape::Table { .. } => values
            .first()
            .and_then(Value::as_table)
            .unwrap_or(&[])
            .iter()
            // Every shape a projected cell can be already has a `Datum`: the result set was
            // built to carry keys and lists of keys because a grouping answers with them, and a
            // projected keyed column is the same string arriving by a different route.
            .map(|p| p.values.iter().map(Datum::projected).collect())
            .collect(),

        Shape::Groups { keys, cells, having, order, cut } => {
            grouped(keys, cells, having.as_ref(), *order, *cut, values)
        }

        Shape::Pairs { keys, cells, having, order, cut } => {
            paired(keys, cells, having.as_ref(), *order, *cut, values)
        }

        Shape::Join { keys, cells, per_key, having, order, cut } => {
            joined(*keys, cells, *per_key, having.as_ref(), *order, *cut, values)
        }

        // `UNION ALL`: each branch's rows, one after the other. The branches read the same flat
        // list of answers, because their cells were rebased onto it when they were joined.
        Shape::Union { branches } => {
            branches.iter().flat_map(|b| rows_of(b, values, probes_at)).collect()
        }
    }
}
