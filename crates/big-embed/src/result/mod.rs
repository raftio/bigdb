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
mod json;
mod num;
mod scalar;
mod window;

use crate::{Answer, Cell, Shape, Units, Value};
use big_sql::{Scalar, Selection};
use group::{grouped, joined, tupled};
use num::{int_of, number, scalar_cell, Num};
pub use scalar::eval;

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
    /// A value out of a float field, or the quotient an average is.
    Real(f64),
    /// A civil date, as days since the Unix epoch.
    ///
    /// A value rather than a rendered string, for the reason given above: the three output
    /// formats disagree about how a date is spelled - JSON quotes it, CSV does not - and agree
    /// completely about what it is. Rendering it here would also make a date indistinguishable
    /// from a row key that happens to look like one.
    Date(i64),
    /// An instant, as seconds since the Unix epoch. UTC, like every date in this engine.
    Timestamp(i64),
    /// A row key, as the string it was interned from.
    Text(String),
    /// The list of keys a `topK` produced, which is what that function answers with.
    Keys(Vec<String>),
}

impl Datum {
    /// One cell of a projected row, in the units its column is in.
    ///
    /// **The expression is applied last, to the finished cell.** Building the `Datum` first is
    /// what lets the evaluator work on `12.50` rather than on the `1250` a decimal field
    /// stores, and what lets `toDate` tell a day count from a second count without being told
    /// which it was handed. See [`mod@scalar`].
    pub(super) fn projected(
        p: &big_exec::Projection,
        units: &Units,
        apply: Option<&Scalar>,
    ) -> Self {
        let value = Self::read(p, units);
        match apply {
            None => value,
            Some(expr) => scalar::eval(expr, &value),
        }
    }

    /// A projected value in plain units, for comparing two values of one field against each
    /// other rather than answering with either.
    pub(super) fn read_plain(p: &big_exec::Projection) -> Self {
        Self::read(p, &Units::PLAIN)
    }

    /// The cell a projected value is before any expression has been applied to it.
    fn read(p: &big_exec::Projection, units: &Units) -> Self {
        match p {
            big_exec::Projection::Absent => Self::Null,
            big_exec::Projection::Int(v) => Self::num(Some(Num::Int(*v)), units),
            // Already decoded, because undoing the float transform needed the field's width and
            // the executor was the last layer holding a catalog.
            big_exec::Projection::Real(v) => Self::Real(*v),
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
    fn num(n: Option<Num>, units: &Units) -> Self {
        let Some(n) = n else { return Datum::Null };
        match (n, units) {
            // A count from the epoch, read back as the date it stands for. An `avg` over one is
            // not a date and stays the number it is - which is also why `sum` over a date is
            // refused at plan time rather than rendered into something here.
            (Num::Int(v), Units::Date) => Datum::Date(v as i64),
            (Num::Int(v), Units::Seconds) => Datum::Timestamp(v as i64),
            (Num::Real(v), Units::Date | Units::Seconds) => Datum::Real(v),
            (n, units) => match (n, units.digits()) {
                (Num::Int(v), 0) => Datum::Int(v),
                (Num::Int(units), scale) => Datum::Dec { units, scale },
                (Num::Real(v), 0) => Datum::Real(v),
                (Num::Real(v), scale) => Datum::Real(v / 10f64.powi(i32::from(scale))),
            },
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

/// Two cells in the order an `ORDER BY` puts them.
///
/// **Absent sorts last, whichever direction was asked for**, which is why `desc` is an argument
/// here rather than a `.reverse()` at the call site. Reversing the whole comparison would carry
/// the absent rows to the front under `DESC`, and a row with no value in the column is not a
/// large one any more than it is a small one. The standard leaves the choice to the
/// implementation and engines disagree, so it is written down: burying them at the end is what
/// a reader of `ORDER BY amount DESC LIMIT 10` means.
///
/// Numbers compare as numbers across the kinds - a `Dec` against an `Int` meets at the scale,
/// the same alignment the evaluator does - and text compares by bytes, which is how a key is
/// ordered everywhere else in this engine. Two cells of kinds that do not compare keep the order
/// they arrived in, because a projection column holds one kind and mixing them is not a shape
/// this produces.
fn order_cells(a: &Datum, b: &Datum, desc: bool) -> core::cmp::Ordering {
    use core::cmp::Ordering;
    // Decided before the direction is applied, and therefore unaffected by it.
    match (a, b) {
        (Datum::Null, Datum::Null) => return Ordering::Equal,
        (Datum::Null, _) => return Ordering::Greater,
        (_, Datum::Null) => return Ordering::Less,
        _ => {}
    }
    let ord = match (a, b) {
        (Datum::Text(x), Datum::Text(y)) => x.cmp(y),
        (Datum::Keys(x), Datum::Keys(y)) => x.cmp(y),
        _ => scalar::compare_numbers(a, b).unwrap_or(Ordering::Equal),
    };
    match desc {
        true => ord.reverse(),
        false => ord,
    }
}

/// Two rows in the order a list of keys puts them, each key with its own direction.
///
/// **The one ordering this crate did not have.** An `ORDER BY` over a projection is a single
/// column by name - the parser refuses a comma outright - and that was enough while the only
/// ordering was the answer's own. A window's is a list: `row_number() OVER (PARTITION BY country
/// ORDER BY ts, amount)` is a numbering nobody can predict without the second key.
///
/// Absent sorts last in every key and in both directions, which is [`order_cells`]' rule and has
/// to be the same rule: a window that buried its nulls differently from the ordering underneath
/// it would number rows in an order the answer is not in.
pub(super) fn order_by_keys(
    a: &[Datum],
    b: &[Datum],
    keys: &[(usize, bool)],
) -> core::cmp::Ordering {
    keys.iter()
        .map(|(at, desc)| match (a.get(*at), b.get(*at)) {
            (Some(x), Some(y)) => order_cells(x, y, *desc),
            _ => core::cmp::Ordering::Equal,
        })
        .find(|o| o.is_ne())
        .unwrap_or(core::cmp::Ordering::Equal)
}

/// A cell as the literal that would have written it, for `INSERT ... SELECT`.
///
/// `Ok(None)` is a cell with no value - the source record held nothing in that field - and the
/// caller writes no fact for it, which is what "no value" already means here. `Err` is a cell
/// this language has no literal for at all.
///
/// **The one of those is a float**, and the reason is worth stating: a [`big_plan::Literal`] is
/// exact by construction - an integer, or an integer and a scale - because every value on the
/// write path has to mean the same thing it meant when it was typed. There is no spelling of an
/// arbitrary `f64` in that set, and the nearest decimal is a *different number*. So a float
/// column is refused by name rather than written as something close to what was read.
///
/// A date and a timestamp go back as the strings they render to, which is not a lossy step: it
/// is the exact text an `INSERT` would have carried, read back by the same `to_count` the
/// literal form uses.
pub fn literal_of(cell: &Datum) -> Result<Option<big_plan::Literal>, &'static str> {
    use big_plan::Literal;
    Ok(match cell {
        Datum::Null => None,
        Datum::Int(v) => Some(match u64::try_from(*v) {
            Ok(n) => Literal::Int(n),
            Err(_) => Literal::Sint(i64::try_from(*v).map_err(|_| "a number this wide")?),
        }),
        Datum::Dec { units, scale } => Some(match u64::try_from(*units) {
            Ok(n) => Literal::Dec { units: n, scale: *scale },
            Err(_) => Literal::Sdec {
                units: i64::try_from(*units).map_err(|_| "a number this wide")?,
                scale: *scale,
            },
        }),
        Datum::Date(days) => Some(Literal::Str(date_text(*days))),
        Datum::Timestamp(secs) => Some(Literal::Str(timestamp_text(*secs))),
        Datum::Text(s) => Some(Literal::Str(s.clone())),
        Datum::Real(_) => return Err("a FLOAT or DOUBLE column"),
        // A list of keys is what `topK` answers with, and a projection never produces one - so
        // this is unreachable rather than a shape somebody can write.
        Datum::Keys(_) => return Err("a list of keys"),
    })
}

/// A cell's number with the expression its entry asked for applied to it.
///
/// **One function for all four shapes.** A `Row`, a grouping, a pair grouping and a join each
/// build their cells differently and each has to apply the expression the same way, so the
/// alternative is four copies of two lines and the day one of them is forgotten - which would
/// be an expression that silently does not run.
pub(super) fn applied(cell: &Cell, value: Datum) -> Datum {
    match &cell.apply {
        None => value,
        Some(expr) => scalar::eval(expr, &value),
    }
}

/// A day count, written the way a date is written. UTC, like every date in this engine.
///
/// Beside [`fixed`] because it is the same kind of thing: the last step, and the only one that
/// knows what the number it is handed stands for.
pub fn date_text(days: i64) -> String {
    big_civil::format_date(days)
}

/// A second count, written the way a timestamp is written.
pub fn timestamp_text(unix_seconds: i64) -> String {
    big_civil::format_datetime(unix_seconds)
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
            vec![cells.iter().map(|c| applied(c, scalar_cell(c, values, probes_at))).collect()]
        }

        // Record ids, which already come in ascending order - so ascending costs nothing and
        // descending is the same list read backwards. The cut is applied after the reversal,
        // which is the only order that answers `ORDER BY _record_id DESC LIMIT 10` with the ten
        // highest rather than the ten lowest read backwards.
        Shape::Records { limit, descending, .. } => match values.first().and_then(Value::as_rows) {
            None => Vec::new(),
            Some(m) => {
                let ids: Box<dyn Iterator<Item = _>> = match descending {
                    false => Box::new(m.records_from(0)),
                    true => Box::new(m.records_from(0).collect::<Vec<_>>().into_iter().rev()),
                };
                ids.take(limit.unwrap_or(usize::MAX))
                    .map(|r| vec![Datum::Int(i128::from(r))])
                    .collect()
            }
        },

        // Nothing to do but render: the plan carried the columns and the cut, because a
        // projection's cost is a point read per record per column and a cut applied here would
        // be one applied after paying for it.
        Shape::Table { columns, order, cut } => {
            let table = values.first().and_then(Value::as_table).unwrap_or(&[]);
            // **A column names the field it reads, rather than sitting opposite it.** The two
            // lists were the same one until windows: a window reads a column to partition,
            // order or fold by and that column need not be in the header, so the plan reads
            // more fields than the answer shows and the positions no longer line up.
            //
            // Every shape a projected cell can be already has a `Datum`: the result set was
            // built to carry keys and lists of keys because a grouping answers with them, and a
            // projected keyed column is the same string arriving by a different route.
            let mut out: Vec<Row> = table
                .iter()
                .map(|p| {
                    columns
                        .named()
                        .iter()
                        .map(|c| match c.of {
                            Selection::Read { at } => p
                                .values
                                .get(at)
                                .map(|v| Datum::projected(v, &c.units, c.apply.as_ref()))
                                .unwrap_or(Datum::Null),
                            // Filled by `window::fill`, which needs every row before it can
                            // answer for any one of them.
                            Selection::Over { .. } => Datum::Null,
                        })
                        .collect()
                })
                .collect();

            window::fill(&mut out, table, columns.named());

            // **Sorted here, after every value has been read, because there is nowhere else.**
            // A projection's rows are values reconstructed a record at a time, in record order,
            // and nothing below this holds them - so the plan cannot rank them and the cut
            // cannot ride in it either. See `big_sql::RowOrder`, which states what that costs.
            if let Some(o) = order {
                if let Some(at) = columns.named().iter().position(|c| c.column == o.column) {
                    // A stable sort, so rows that tie stay in record order - which is the only
                    // tie-break available here and the one a reader can predict.
                    out.sort_by(|a, b| order_cells(&a[at], &b[at], o.desc));
                }
            }
            if let Some(n) = cut {
                out.truncate(*n);
            }
            out
        }

        Shape::Groups { keys, cells, having, order, cut } => {
            grouped(keys, cells, having.as_ref(), *order, *cut, values)
        }

        Shape::Tuples { keys, cells, having, order, cut, .. } => {
            tupled(keys, cells, having.as_ref(), *order, *cut, values)
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
