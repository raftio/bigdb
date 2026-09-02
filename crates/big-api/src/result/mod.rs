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
use num::{int_of, number, scalar_cell, Num};

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
    /// A number out of a decimal field, with the digits that field keeps.
    ///
    /// **Not an `f64`.** A decimal field stores an integer precisely so that no value passes
    /// through a float on its way in - see `big_plan::Literal::Dec` - and rendering one through
    /// a float on the way out would put back exactly the error the storage layer went to
    /// trouble to avoid. The point is placed when the cell is written instead.
    Dec {
        /// The stored number, in the units the field keeps.
        units: i128,
        /// How many of its digits are after the point.
        scale: u8,
    },
    /// The quotient an average is.
    Real(f64),
    /// A row key, as the string it was interned from.
    Text(String),
    /// The list of keys a `topK` produced, which is what that function answers with.
    Keys(Vec<String>),
}

impl Datum {
    /// One cell of a projected row, in the units its column is in.
    fn projected(p: &big_exec::Projection, digits: u8) -> Self {
        match p {
            big_exec::Projection::Absent => Self::Null,
            big_exec::Projection::Int(v) => Self::num(Some(Num::Int(*v)), digits),
            big_exec::Projection::Text(s) => Self::Text(s.clone()),
            big_exec::Projection::Texts(v) => Self::Keys(v.clone()),
        }
    }

    /// A number that may not be there, in the units the cell says it is in.
    ///
    /// **Where a decimal stops being an integer.** Everything up to here - the plan, the
    /// fan-out, the merge, the `HAVING` - works in the units the field stores, which is what
    /// keeps all of it exact. This is the last step, and the only one that has to know the
    /// scale.
    ///
    /// An average is already a quotient and already a float, so it is divided rather than
    /// pointed: `sum(price) / count(*)` over units is the answer in units, and the value it
    /// stands for is that over ten to the scale.
    fn num(n: Option<Num>, digits: u8) -> Self {
        match (n, digits) {
            (None, _) => Datum::Null,
            (Some(Num::Int(v)), 0) => Datum::Int(v),
            (Some(Num::Int(units)), scale) => Datum::Dec { units, scale },
            (Some(Num::Real(v)), 0) => Datum::Real(v),
            (Some(Num::Real(v)), scale) => Datum::Real(v / 10f64.powi(i32::from(scale))),
        }
    }

    /// A key that is there. The absent case is [`Datum::Null`] and every caller spells it out.
    fn text(s: &str) -> Self {
        Datum::Text(s.to_string())
    }
}

/// A number in a field's units, with the point put back where the scale says.
///
/// Exact by construction: the digits are the stored integer's, and the point is placed among
/// them rather than computed. Nothing here goes through a float, which is the whole reason a
/// decimal field stores an integer in the first place.
pub fn fixed(units: i128, scale: u8) -> String {
    if scale == 0 {
        return units.to_string();
    }
    let scale = usize::from(scale);
    // Padded to at least one digit before the point, so five hundredths reads `0.05` rather
    // than `.05` - which is a number some readers take and some refuse.
    let digits = format!("{:0>width$}", units.unsigned_abs(), width = scale + 1);
    let point = digits.len() - scale;
    let sign = if units < 0 { "-" } else { "" };
    format!("{sign}{}.{}", &digits[..point], &digits[point..])
}

/// A result set: the columns a statement named, and the rows its answers came to.
#[derive(Clone, PartialEq, Debug)]
pub struct ResultSet {
    /// The column names, in the order the statement named them.
    pub columns: Vec<String>,
    /// The rows. Each is as wide as `columns`.
    pub rows: Vec<Row>,
}

/// A result set of one row and one cell, which is what a statement that changed something
/// answers with.
///
/// **Not a `Shape`.** A `Shape` describes how the answers *plans* produced become cells, and
/// every one of its cells names a plan by index; a schema change makes no plan, so building one
/// here would mean naming a plan that does not exist and then reading a value out of a list
/// that was never filled. The number is already the answer, so this says so.
pub fn one_cell(column: &str, value: Datum) -> ResultSet {
    ResultSet { columns: vec![column.to_string()], rows: vec![vec![value]] }
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
        // The one shape whose `HAVING` decides *how many* rows there are rather than which: the
        // whole filtered set is one group, so failing the test leaves no row at all. Applied
        // here, after the merge, for the same reason a grouping's is - a total under the
        // threshold on one node can be over it once every node has contributed.
        Shape::Row { cells, having } => {
            let kept = match having {
                None => true,
                Some(h) => h.holds(&|of| int_of(number(of, values, None))),
            };
            if !kept {
                return Vec::new();
            }
            vec![cells.iter().map(|c| scalar_cell(c, values, probes_at)).collect()]
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
        Shape::Table { columns } => values
            .first()
            .and_then(Value::as_table)
            .unwrap_or(&[])
            .iter()
            // Every shape a projected cell can be already has a `Datum`: the result set was
            // built to carry keys and lists of keys because a grouping answers with them, and a
            // projected keyed column is the same string arriving by a different route.
            .map(|p| {
                p.values
                    .iter()
                    .zip(columns.named())
                    .map(|(v, c)| Datum::projected(v, c.units.digits()))
                    .collect()
            })
            .collect(),

        Shape::Groups { keys, cells, having, order, cut } => {
            grouped(keys, cells, having.as_ref(), *order, *cut, values)
        }

        Shape::Pairs { keys, cells, having, order, cut } => {
            paired(keys, cells, having.as_ref(), *order, *cut, values)
        }

        Shape::Join { sides, cells, per_key, having, order, cut, .. } => {
            joined(sides, cells, *per_key, having.as_ref(), *order, *cut, values)
        }

        // `UNION ALL`: each branch's rows, one after the other. The branches read the same flat
        // list of answers, because their cells were rebased onto it when they were joined.
        Shape::Union { branches } => {
            branches.iter().flat_map(|b| rows_of(b, values, probes_at)).collect()
        }
    }
}
