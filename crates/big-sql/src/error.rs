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

//! Every way a statement can be refused, and the sentence each refusal gives.
//!
//! Two kinds live here and the difference matters. A [`SqlError::Syntax`] says the text is not a
//! statement. A [`SqlError::Refused`] says it *is* one, and this engine will not answer it — the
//! `JOIN` case, and the reason this crate exists in the shape it does. A client that cannot tell
//! those apart will retry the second one forever.

use big_plan::PlanError;

/// A construct this dialect understands and will not answer.
///
/// Enumerated rather than left as free text so that the refusal list is one `match` that a test
/// can walk, instead of a set of strings scattered through the parser. Every variant carries a
/// stable code and a sentence naming what to do instead, when there is something to do.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Refused {
    /// A join this engine cannot pair records for: a comma between tables, or a third table.
    Joins,
    /// An outer join, which has to produce a row for a record with no partner.
    OuterJoin,
    /// A join condition that is not one equality between two columns.
    JoinOn,
    /// An aggregate, or a select list, a join cannot answer.
    JoinShape,
    /// A column that does not say which of a join's two tables it belongs to.
    Ambiguous,
    /// A `WHERE` term that mixes both tables of a join where they cannot be separated.
    JoinFilter,
    /// A subquery, a CTE, or `UNION` between two selects.
    Subquery,
    /// A `HAVING` that names an aggregate the answer does not carry.
    Having,
    /// A window function, or `OVER`.
    Window,
    /// `ORDER BY` something the answer holds no number or key for.
    Order,
    /// `OFFSET` where the answer is not a list of groups.
    Offset,
    /// Arithmetic or a function call in the select list.
    Expression,
    /// `DISTINCT` over anything but one keyed column inside `count`.
    MultiDistinct,
    /// `IS NULL` or `IS NOT NULL`.
    Null,
    /// Selecting stored values without the cut that bounds what it costs.
    Projection,
    /// A window over a time quantum field.
    TimeWindow,
    /// `INSERT`, `UPDATE`, `DELETE`, or a schema change other than `CREATE TABLE`.
    Write,
    /// A column list on `CREATE TABLE`.
    ColumnList,
    /// A select list this dialect cannot turn into one plan.
    Shape,
    /// A `WHERE` term that is not a comparison — `LIKE`, and anything else with no set
    /// operation behind it.
    Predicate,
    /// A select list asking for more plans than one statement may fan out.
    TooManyCalls,
    /// A `FORMAT` this surface cannot write.
    Format,
    /// A `UNION` that is not `UNION ALL`, or branches that do not line up.
    Union,
    /// A quantile level that is not a fraction this surface resolves.
    Quantile,
}

impl Refused {
    /// The stable identifier a client branches on.
    ///
    /// Several variants share `sql_unsupported`: they differ in which construct was written,
    /// which the message already says, and not in what the client should do about it — which is
    /// the same rule `PlanError` applies to its four parse variants.
    pub fn code(self) -> &'static str {
        match self {
            Self::Joins => "sql_no_joins",
            Self::OuterJoin => "sql_no_outer_joins",
            Self::JoinOn => "sql_join_condition",
            Self::Ambiguous => "sql_ambiguous_column",
            Self::JoinFilter => "sql_join_filter",
            Self::Order => "sql_unsupported_order",
            Self::Null => "sql_no_nulls",
            Self::Projection => "sql_projection_unsupported",
            Self::TimeWindow => "sql_no_time_window",
            Self::Write => "sql_read_only",
            Self::ColumnList => "sql_no_column_list",
            Self::TooManyCalls => "sql_too_many_aggregates",
            Self::Format => "sql_unknown_format",
            Self::Union => "sql_union",
            Self::Quantile => "sql_quantile_level",
            Self::Subquery
            | Self::Having
            | Self::Window
            | Self::Offset
            | Self::Expression
            | Self::MultiDistinct
            | Self::Shape
            | Self::JoinShape
            | Self::Predicate => "sql_unsupported",
        }
    }

    /// What the refusal is, and what exists instead.
    ///
    /// The second half is the part that earns its keep. Someone who writes a join has a
    /// question, and "syntax error" does not tell them the engine has no joins to give.
    pub fn why(self) -> &'static str {
        match self {
            Self::Joins => {
                "a join here pairs records through a keyed column two tables share, written as \
                 `FROM a JOIN b ON a.k = b.k`. A comma between tables is a cross join, which \
                 has no key to pair on, and a third table would need a key all three share"
            }
            Self::OuterJoin => {
                "only an inner join is answered. An outer join has to produce a row for a \
                 record with no partner, and what this engine computes about a join is \
                 arithmetic over the records each key holds on both sides - there is no row to \
                 null out half of"
            }
            Self::JoinOn => {
                "a join is one equality between two keyed columns, as `ON a.k = b.k`. Two \
                 conditions would pair on a composite key this index never stored, and `USING` \
                 names one column for two tables that each keep their own dictionary"
            }
            Self::JoinShape => {
                "over a join this surface answers `count(*)`, `sum`, `min` and `max` of one \
                 table's column, and `count(DISTINCT <the join key>)` - each of which is \
                 arithmetic over the per-key counts both sides produce. `avg` is not among \
                 them: write the sum and the count and divide. There are no joined rows of \
                 values to select, because a pair of records has no identity this engine stores"
            }
            Self::Ambiguous => {
                "with a join in the statement every column has to say which table it belongs \
                 to, as `a.amount`. The translation resolves names before it ever sees a \
                 schema, so there is nothing here to guess with"
            }
            Self::JoinFilter => {
                "a `WHERE` over a join is each table's own conditions, combined with `AND`. A \
                 term that mixes both tables under `OR` or `NOT` selects records neither side \
                 can be filtered to on its own"
            }
            Self::Subquery => {
                "subqueries, CTEs and `UNION` between selects are not supported; a set of \
                 records is combined inside `WHERE` with `AND`, `OR` and `NOT`"
            }
            Self::Having => {
                "`HAVING` compares the aggregate the select list already asked for, over a \
                 `GROUP BY` - `count(*)` when the answer carries counts, or the same `sum`, \
                 `min` or `max` it selects. A grouped answer holds one number per group, so a \
                 `HAVING` on a second one would filter on a number that is not there, and one \
                 without a `GROUP BY` has no groups to keep"
            }
            Self::Window => "window functions are not supported",
            Self::Order => {
                "a grouped answer is ordered by the grouped column or by the one aggregate the \
                 select list asked for, in either direction; `ORDER BY count(*) DESC` is the \
                 ranking the plan itself carries. An ordering that names a second aggregate \
                 names a number the answer does not hold, and a record listing has no values \
                 to order by at all"
            }
            Self::Offset => {
                "`OFFSET` pages a list of groups, which this surface materialises in full \
                 before it cuts. A record listing is paged with the `after` cursor on \
                 `GET /table/{t}/records` instead: a cursor does not shift when records are \
                 inserted under it, and a skip count does"
            }
            Self::Expression => {
                "the select list takes a column or an aggregate of one, and nothing computed \
                 from them: there is no expression evaluator here"
            }
            Self::MultiDistinct => {
                "`DISTINCT` takes one keyed column - as `SELECT DISTINCT <column>`, which is \
                 `GROUP BY <column>`, or as `count(DISTINCT <column>)`. Two of them is a \
                 grouping over a composite key this index never stored"
            }
            Self::Null => {
                "there are no nulls here: a record either carries a value or the bit is not set, \
                 and neither is a null that comparisons propagate"
            }
            Self::Projection => {
                "selecting stored values needs a `LIMIT` of between 1 and 10000, because \
                 reconstructing one costs a point read per record per column and the number of \
                 records is the whole of what it costs. A keyed column cannot be selected at \
                 all - it has no read back from a record to its string - so count it, \
                 aggregate it, or group by it"
            }
            Self::TimeWindow => {
                "a window is a key and its bounds written against the same time quantum \
                 column - `visit = 'home' AND visit BETWEEN <seconds> AND <seconds>`, or one \
                 bound alone with `>=` or `<=`. A field with no views by time has no window to \
                 answer, only every record it ever held"
            }
            Self::Write => {
                "this surface writes no rows and changes no schema but a table's; write with \
                 `POST /table/{t}/import` and use the `/table` routes for the rest"
            }
            Self::ColumnList => {
                "`CREATE TABLE` here takes no column list: a field kind may be a set, a mutex \
                 or a time quantum, none of which a SQL type names. Declare fields with \
                 `POST /table/{t}/field/{f}?kind=...`"
            }
            Self::Shape => {
                "a statement answers one question: an aggregate, or a grouped column and one \
                 aggregate of it"
            }
            Self::Predicate => {
                "a condition compares a column with a value using `=`, `!=`, `<`, `<=`, `>`, \
                 `>=`, `IN` or `BETWEEN`; there is no pattern matching and no arithmetic"
            }
            Self::Quantile => {
                "a quantile takes a level between 0 and 1 with at most three digits after the \
                 point, as `quantile(0.95)(amount)`; `median(amount)` is `quantile(0.5)`. A \
                 finer level would name a place in the distribution no number of records this \
                 engine holds could resolve"
            }
            Self::Union => {
                "`UNION ALL` stacks two answers with the same number of columns, named by the \
                 first. A plain `UNION` removes duplicate rows, and a row here is a rendered \
                 answer rather than a stored tuple - there is nothing to compare two of them by \
                 that is not comparing strings"
            }
            Self::Format => {
                "the formats this surface writes are `JSON`, `JSONCompact`, `TSV`, \
                 `TabSeparated`, `TSVWithNames`, `TabSeparatedWithNames`, `CSV` and \
                 `CSVWithNames`. Anything else would be a spelling of bytes nobody here \
                 produces"
            }
            Self::TooManyCalls => {
                "a select list asks for one plan per aggregate, and each is fanned out and \
                 merged on its own - so the number of them is what the statement costs in \
                 round trips. Ask for at most 16, counting `avg` as two: a sum and a count"
            }
        }
    }
}

/// Everything that can go wrong between the text and a [`big_plan::ast::Call`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum SqlError {
    /// The text is not a statement.
    Syntax {
        /// Byte offset into the statement.
        at: usize,
        /// What was there.
        found: String,
        /// What would have been accepted.
        want: &'static str,
    },
    /// A string literal that never closed.
    UnterminatedString {
        /// Byte offset of the opening quote.
        at: usize,
    },
    /// A number too wide for the engine's widest value.
    NumberTooLarge {
        /// Byte offset of the number.
        at: usize,
    },
    /// A decimal written with a leading `-`. Decimal fields are unsigned.
    NegativeDecimal {
        /// Byte offset of the number.
        at: usize,
    },
    /// The `WHERE` clause nested deeper than [`crate::parse::MAX_DEPTH`].
    TooDeep {
        /// Where the limit was reached.
        at: usize,
        /// The limit itself.
        limit: usize,
    },
    /// A well-formed statement this engine will not answer.
    Refused {
        /// Which construct.
        what: Refused,
        /// Where it was written.
        at: usize,
    },
    /// Resolution failed for a reason the query language has already named — an unknown table,
    /// an unknown field, an operator the field's class does not take.
    ///
    /// Carried rather than restated so that a client sees `unknown_field` whether the field was
    /// misspelled in SQL or in PQL. A code that changed with the surface would make every
    /// client's error handling surface-specific for no gain.
    Plan(PlanError),
}

impl From<PlanError> for SqlError {
    fn from(e: PlanError) -> Self {
        Self::Plan(e)
    }
}

impl core::fmt::Display for SqlError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Syntax { at, found, want } => {
                write!(f, "at byte {at}: expected {want}, found `{found}`")
            }
            Self::UnterminatedString { at } => write!(f, "at byte {at}: unterminated string"),
            Self::NumberTooLarge { at } => write!(f, "at byte {at}: number does not fit in u64"),
            Self::NegativeDecimal { at } => write!(
                f,
                "at byte {at}: decimal fields are unsigned, so a negative decimal cannot be \
                 compared against one"
            ),
            Self::TooDeep { at, limit } => {
                write!(f, "at byte {at}: the condition nests deeper than the limit of {limit}")
            }
            Self::Refused { what, at } => write!(f, "at byte {at}: {}", what.why()),
            Self::Plan(e) => write!(f, "{e}"),
        }
    }
}

impl SqlError {
    /// A stable, machine-readable name for the failure.
    ///
    /// The five syntactic variants share `parse_error` with PQL's parser, because a client that
    /// sent unparseable text has the same thing to do about it in either language.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Syntax { .. }
            | Self::UnterminatedString { .. }
            | Self::NumberTooLarge { .. }
            | Self::NegativeDecimal { .. } => "parse_error",
            Self::TooDeep { .. } => "query_too_deep",
            Self::Refused { what, .. } => what.code(),
            Self::Plan(e) => e.code(),
        }
    }
}

impl core::error::Error for SqlError {}

/// This crate's `Result`.
pub type Result<T> = core::result::Result<T, SqlError>;
