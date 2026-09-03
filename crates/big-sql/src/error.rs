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
    /// A join this engine cannot pair records for: one that names no key at all - a comma
    /// between tables, `CROSS JOIN`, `NATURAL JOIN` - or one that would need a table grouped by
    /// two keys at once.
    Joins,
    /// An outer join, which names a key and then has to produce a row for a record with no
    /// partner under it.
    OuterJoin,
    /// A join condition that is not one equality between two columns.
    JoinOn,
    /// An aggregate, or a select list, a join cannot answer.
    JoinShape,
    /// A column that does not say which of a join's tables it belongs to.
    Ambiguous,
    /// A `WHERE` term that names two of a join's tables where they cannot be separated.
    JoinFilter,
    /// A semi-join whose inner set is larger than the union it would expand into.
    SetTooLarge,
    /// `EXPLAIN` over a statement containing a semi-join.
    ExplainSet,
    /// A `SEGMENT(...)` that reached the lowering, which means nothing expanded it.
    Segment,
    /// A `SEGMENT(...)` naming a view of a table this statement does not read.
    SegmentTable,
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
    /// An expression in the select list that names anything but exactly one column.
    ///
    /// Arithmetic and function calls **are** answered - see [`mod@crate::scalar`]. What is not
    /// is an expression over two columns at once, or over none: a projection is one plan
    /// reading one field per column, and there is no plan for either of those to be.
    Expression,
    /// `DISTINCT` over anything but one keyed column inside `count`.
    MultiDistinct,
    /// `IS NULL` or `IS NOT NULL`.
    Null,
    /// `UPDATE`, `TRUNCATE`, `MERGE`, or a schema change this engine has no operation behind.
    Write,
    /// An `INSERT` with no column list, which would be positional against a field order the
    /// statement does not carry.
    InsertColumns,
    /// An `id` that is not a whole number.
    InsertId,
    /// A field declared as `_record_id`, which is the name the record itself answers under.
    IdColumn,
    /// An `INSERT ... SELECT` whose query answers with something other than values.
    ///
    /// The form itself is answered - see [`crate::insert::Source::Select`] - and so is a
    /// *grouped* query, which is what a materialised rollup is here. What is refused is
    /// `SELECT *`, which answers with record ids: an id is the address a fact is written to
    /// rather than anything stored in a column, and writing one into a user's data would put
    /// this engine's own coordinates there.
    InsertSelect,
    /// `INSERT INTO t ... SELECT ... FROM t`: one table on both sides of one statement.
    InsertSelfRead,
    /// More rows in one `INSERT` than a statement may carry.
    InsertSize,
    /// `DELETE FROM`, which asks for a row this engine does not store.
    DeleteRows,
    /// `USE`, which asks a stateless surface to remember something between statements.
    ///
    /// Databases exist; a *session* does not. `POST /sql` answers one statement and keeps
    /// nothing, so `USE` is the client's to hold - `bigctl` does, and sends it as `?database=`.
    SessionUse,
    /// `sales.orders.amount`: a column qualified by more than an alias.
    ThreePartName,
    /// A **materialised** view. A plain one exists - see [`crate::Ddl::CreateView`].
    ///
    /// The difference is not a keyword: a plain view is a name for a statement, re-planned at
    /// every read and costing nothing until somebody reads it. A materialised one is a table
    /// plus a promise to keep it current, which is a write path.
    MaterializedView,
    /// A view body that is not a filter and a projection over one table.
    ///
    /// Raised at parse time, so it needs no schema and is reachable by the corpus.
    ViewBody,
    /// A statement naming a column its view does not expose.
    ///
    /// Raised where a view is expanded, in `big-embed`, because deciding it needs the stored
    /// statement. Not reachable by [`crate::translate`], which has no catalog.
    ViewColumn,
    /// Views nested past [`crate::MAX_VIEW_DEPTH`]. Also raised in `big-embed`.
    ViewDepth,
    /// `CASE <expr> WHEN <value> THEN ...`: the simple form of a case, which this dialect does
    /// not read.
    ///
    /// The searched form - `CASE WHEN <condition> THEN ...` - is answered, and so are `if`,
    /// `multiIf`, `coalesce`, `nullIf` and `ifNull`; all of them parse into one tree. What is
    /// refused is the *second spelling* of that tree, not the ability to choose.
    Case,
    /// `CAST`, `toInt64`, `toString`: a conversion between representations that do not convert.
    Cast,
    /// An aggregate this engine has no fold for — `argMin`, `stddev`, `corr`, `any`.
    Aggregate,
    /// `MODIFY`, `ALTER COLUMN` or `CHANGE`: a field's kind and depth are what its bit planes
    /// are, and there is no operation below that changes either.
    AlterKind,
    /// `RENAME`, on a table or a column. Names are what resolve a fact all the way down, and
    /// nothing below renames one.
    Rename,
    /// `ALTER TABLE ... ENGINE =`, which asks a table to store something else than it does.
    AlterEngine,
    /// A type name in a column list that names nothing this engine stores.
    ColumnType,
    /// `date_trunc` given a boundary the calendar does not have, or one finer than the column.
    TruncUnit,
    /// An interval spelling of a rounding this dialect writes as `date_trunc`.
    Interval,
    /// A scalar call in a `WHERE`, where there are no values yet to apply it to.
    ScalarFilter,
    /// A column constraint - `NOT NULL`, `PRIMARY KEY`, `DEFAULT` - which is a promise about
    /// rows, and there are no rows here to make it about.
    Constraint,
    /// `DECIMAL` or `NUMERIC` written without the precision and scale that say what it holds.
    DecimalScale,
    /// A bit depth outside the 1..=64 a bit-sliced value can occupy.
    BitDepth,
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
    /// `EXPLAIN PLAN` or `EXPLAIN SHAPE` over a statement that is not a query.
    ExplainHalf,
    /// `EXPLAIN`, asked of a surface that answers with plans rather than with rows.
    ///
    /// **Not a statement this engine refuses.** `Cluster::sql` answers one, and the corpus pins
    /// every character of what it says. What is refused is the *question*: an explanation has no
    /// plan to hand back, because the whole of what it asks for is that nothing runs.
    ///
    /// Raised in `big-embed`, next to the three statements that resolve to no plan either, and so
    /// not reachable by [`crate::translate`] - the same shape as [`Refused::ViewColumn`].
    ExplainRows,
}

impl Refused {
    /// Every refusal this dialect has, so a test can walk the list rather than remember it.
    ///
    /// **This exists to be checked against the corpus.** A refusal without a case is a sentence
    /// nobody has read since it was written, and the whole claim of this list is that each one
    /// tells somebody what exists instead - so `tests/gates.rs` insists every code here is
    /// reached by a statement in `tests/testdata`.
    ///
    /// Kept honest by [`Refused::rank`] below, whose exhaustive match will not compile until a
    /// new variant is named - and by a test asserting that every rank appears here exactly once,
    /// which is what catches naming one and forgetting to add it.
    pub const ALL: [Self; 53] = [
        Self::Joins,
        Self::OuterJoin,
        Self::JoinOn,
        Self::JoinShape,
        Self::Ambiguous,
        Self::JoinFilter,
        Self::Subquery,
        Self::Having,
        Self::Window,
        Self::Order,
        Self::Offset,
        Self::Expression,
        Self::MultiDistinct,
        Self::Null,
        Self::Write,
        Self::InsertColumns,
        Self::InsertId,
        Self::IdColumn,
        Self::InsertSelect,
        Self::InsertSelfRead,
        Self::InsertSize,
        Self::DeleteRows,
        Self::SessionUse,
        Self::MaterializedView,
        Self::Case,
        Self::Cast,
        Self::Aggregate,
        Self::AlterKind,
        Self::Rename,
        Self::AlterEngine,
        Self::ColumnType,
        Self::TruncUnit,
        Self::Interval,
        Self::ScalarFilter,
        Self::SetTooLarge,
        Self::ExplainSet,
        Self::Segment,
        Self::SegmentTable,
        Self::Constraint,
        Self::DecimalScale,
        Self::BitDepth,
        Self::Shape,
        Self::Predicate,
        Self::TooManyCalls,
        Self::Format,
        Self::Union,
        Self::Quantile,
        Self::ThreePartName,
        Self::ViewBody,
        Self::ViewColumn,
        Self::ViewDepth,
        Self::ExplainHalf,
        Self::ExplainRows,
    ];

    /// Where this refusal sits in [`Refused::ALL`], and the reason that list can be trusted.
    ///
    /// The match is exhaustive, so a variant added to the enum and not named here is a
    /// compile error rather than a hole in the coverage gate.
    ///
    /// Public because its only caller is the test below and one in `tests/gates.rs`, and a
    /// private one would be dead code in every build that is not a test build.
    pub fn rank(self) -> usize {
        match self {
            Self::Joins => 0,
            Self::OuterJoin => 1,
            Self::JoinOn => 2,
            Self::JoinShape => 3,
            Self::Ambiguous => 4,
            Self::JoinFilter => 5,
            Self::Subquery => 6,
            Self::Having => 7,
            Self::Window => 8,
            Self::Order => 9,
            Self::Offset => 10,
            Self::Expression => 11,
            Self::MultiDistinct => 12,
            Self::Null => 13,
            Self::Write => 14,
            Self::InsertColumns => 15,
            Self::InsertId => 16,
            Self::IdColumn => 17,
            Self::InsertSelect => 18,
            Self::InsertSelfRead => 48,
            Self::InsertSize => 19,
            Self::DeleteRows => 20,
            Self::SessionUse => 21,
            Self::MaterializedView => 22,
            Self::Case => 23,
            Self::Cast => 24,
            Self::Aggregate => 25,
            Self::AlterKind => 26,
            Self::Rename => 27,
            Self::AlterEngine => 28,
            Self::ColumnType => 29,
            Self::Constraint => 30,
            Self::DecimalScale => 31,
            Self::BitDepth => 32,
            Self::Shape => 33,
            Self::Predicate => 34,
            Self::TooManyCalls => 35,
            Self::Format => 36,
            Self::Union => 37,
            Self::Quantile => 38,
            Self::ThreePartName => 39,
            Self::ViewBody => 40,
            Self::ViewColumn => 41,
            Self::ViewDepth => 42,
            Self::ExplainHalf => 43,
            Self::ExplainRows => 44,
            Self::TruncUnit => 45,
            Self::Interval => 46,
            Self::ScalarFilter => 47,
            Self::SetTooLarge => 49,
            Self::ExplainSet => 50,
            Self::Segment => 51,
            Self::SegmentTable => 52,
        }
    }

    /// The stable identifier a client branches on.
    ///
    /// Several variants share `sql_unsupported`: they differ in which construct was written,
    /// which the message already says, and not in what the client should do about it — which is
    /// the same rule `PlanError` applies to its four parse variants.
    pub fn code(self) -> &'static str {
        match self {
            Self::SetTooLarge => "sql_set_too_large",
            Self::ExplainSet => "sql_explain_set",
            Self::Segment => "sql_segment_unexpanded",
            Self::SegmentTable => "sql_segment_table",
            Self::Joins => "sql_no_joins",
            Self::OuterJoin => "sql_no_outer_joins",
            Self::JoinOn => "sql_join_condition",
            Self::Ambiguous | Self::ThreePartName => "sql_ambiguous_column",
            Self::JoinFilter => "sql_join_filter",
            Self::Order => "sql_unsupported_order",
            Self::Null => "sql_no_nulls",
            // A `DELETE` shares `sql_read_only` with the other writes this surface does not
            // take: what the client does about it is the same in both cases, which is the rule
            // this list follows.
            Self::Write | Self::DeleteRows => "sql_read_only",
            Self::InsertColumns | Self::InsertId => "sql_insert_shape",
            Self::IdColumn => "sql_id_column",
            Self::InsertSize => "sql_insert_too_large",
            // Its own code rather than the shared one: what a client does about it is copy
            // through a second table, which is nothing like what the other shapes need.
            Self::InsertSelfRead => "sql_insert_self_read",
            Self::SessionUse => "sql_use_unsupported",
            Self::MaterializedView => "sql_no_materialized_views",
            Self::ViewBody => "sql_view_body",
            Self::ViewColumn => "sql_view_column",
            Self::ViewDepth => "sql_view_depth",
            Self::AlterKind => "sql_no_alter_column",
            Self::Rename => "sql_no_rename",
            Self::AlterEngine => "sql_no_alter_engine",
            Self::ColumnType => "sql_unknown_column_type",
            Self::TruncUnit => "sql_bad_trunc_unit",
            Self::Interval => "sql_unsupported",
            Self::ScalarFilter => "sql_scalar_in_filter",
            Self::Constraint => "sql_no_constraints",
            Self::DecimalScale => "sql_decimal_scale",
            Self::BitDepth => "sql_bit_depth",
            Self::TooManyCalls => "sql_too_many_aggregates",
            Self::Format => "sql_unknown_format",
            Self::Union => "sql_union",
            Self::Quantile => "sql_quantile_level",
            Self::ExplainHalf => "sql_explain_half",
            Self::ExplainRows => "sql_explain_rows",
            Self::Subquery
            | Self::Having
            | Self::Window
            | Self::Offset
            | Self::Expression
            | Self::MultiDistinct
            | Self::Shape
            | Self::JoinShape
            | Self::InsertSelect
            | Self::Case
            | Self::Cast
            | Self::Aggregate
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
                "a join here pairs records through a keyed column the tables share, written as \
                 `FROM a JOIN b ON a.k = b.k` and repeated for each further table - every one \
                 of them keyed on the same column. A comma between tables, `CROSS JOIN` and \
                 `NATURAL JOIN` name no key to pair on - the first two pair everything with \
                 everything, and the third asks the schema to choose - and a table joined on \
                 two different columns would have to be grouped by both at once, which is a \
                 pass over the second per value of the first"
            }
            Self::OuterJoin => {
                "`LEFT`, `RIGHT` and `FULL` are answered, but a `FULL JOIN` only where every \
                 join in the statement is one. `(a JOIN b) FULL JOIN c` pairs on the keys `a` \
                 and `b` share unioned with `c`'s, and what this engine carries is one flag per \
                 side saying whether it has to match - which cannot tell that nesting from the \
                 union of all three. Write the full join over one pair of tables"
            }
            Self::JoinOn => {
                "a join is one equality between two keyed columns, as `ON a.k = b.k`, \
                 naming the table being joined in on one side and a table already in `FROM` on \
                 the other. Two conditions would pair on a composite key this index never \
                 stored, and `USING` names one column for two tables that each keep their own \
                 dictionary"
            }
            Self::JoinShape => {
                "over a join this surface answers `count(*)`, `sum`, `min`, `max` and `avg` of \
                 one table's column, and `count(DISTINCT <the join key>)` - each of which is \
                 arithmetic over the per-key counts every side produces. What it has no answer \
                 for is a *row* of a join: a pair of records has no identity this engine stores"
            }
            Self::Ambiguous => {
                "with a join in the statement every column has to say which table it belongs \
                 to, as `a.amount`. The translation resolves names before it ever sees a \
                 schema, so there is nothing here to guess with"
            }
            Self::JoinFilter => {
                "a `WHERE` over a join is each table's own conditions, combined with `AND`. A \
                 term that names two of them under `OR` or `NOT` selects records neither side \
                 can be filtered to on its own"
            }
            Self::Segment => {
                "a segment is a named `WHERE` over one table, and expanding it needs the \
                 catalog the view is stored in - which this translation does not have. Reached \
                 through a server, `SEGMENT(...)` is substituted before anything is planned"
            }
            Self::SegmentTable => {
                "`SEGMENT(...)` names a view whose `WHERE` becomes a term of this one, so the \
                 view has to exist, has to be over exactly one table this statement reads, and \
                 has to select by something - a view with no `WHERE` is every record of its \
                 table and defines no set"
            }
            Self::ExplainSet => {
                "a statement with `IN (SELECT ...)` has no plan until it has run: the outer \
                 call is narrowed by the ids the inner set holds, so its tree is a fact about \
                 the other table's contents rather than about the statement. Explaining the \
                 inner set on its own says everything that is fixed about it"
            }
            Self::SetTooLarge => {
                "`IN (SELECT _record_id FROM ...)` narrows a bit-sliced column by the ids the \
                 inner set holds, and that is one equality per id - each of them a read per bit \
                 plane. A set this large is a scan wearing a `WHERE`, so it is refused with the \
                 number rather than answered slowly. Narrow the inner `WHERE`"
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
                "an expression in the select list is computed over one column - the one the \
                 projection reads, or the one the aggregate folds - and constants beside it: \
                 `round(amount / 100, 2)` and `substring(country, 1, 2)` are answered, and so \
                 is `date_diff('day', ts, now())`. Two columns in one cell is two plans, and a \
                 projection reads one field per column; an expression over no column at all is \
                 a constant, which needs no table to be true"
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
            Self::Write => {
                "the writes on this surface are `INSERT INTO t (...) VALUES (...)`, \
                 `CREATE TABLE`, `ALTER TABLE ... ADD`/`DROP COLUMN`, `DROP TABLE`, \
                 `CREATE DATABASE`/`DROP DATABASE` and `CREATE VIEW`/`DROP VIEW`. \
                 `UPDATE` has no row to change in place - a fact is a bit at `(row, record)`, \
                 so changing one means writing the new fact and clearing the old; `ALTER VIEW` \
                 is `CREATE OR REPLACE VIEW`, which says the whole statement rather than a \
                 change to one; and `TRUNCATE`, `MERGE`, `REPLACE` and `CREATE INDEX` are each \
                 a statement with no operation behind it here: a bitmap is already the index. \
                 Write facts in volume with `POST /table/{t}/import` and use the `/table` \
                 routes for the rest"
            }
            Self::InsertColumns => {
                "an `INSERT` names the columns it writes, as `INSERT INTO t (country, amount) \
                 VALUES ('GB', 100)`. This translation holds no schema - which is what keeps it a \
                 translation - so `INSERT INTO t VALUES (...)` would be positional against a \
                 field order that is not in the statement and that the next `ALTER TABLE ... \
                 ADD` moves"
            }
            Self::InsertId => {
                "an `id` is a whole number: a fact is a bit at `(row, record)`, so the id is \
                 the address it is written to rather than a name for what is written there - \
                 which is why `POST /table/{t}/import` names the record on every line, why \
                 `POST /table/{t}/delete` takes ids, and why `SELECT *` answers with nothing \
                 else. Write one, as `INSERT INTO t (_record_id, country) VALUES (7, 'GB')`, or leave \
                 the column out and the server allocates one"
            }
            Self::IdColumn => {
                "`_record_id` is what a record is called here, not a field it can hold: \
                 `SELECT *` answers with it, `INSERT INTO t (_record_id, ...)` writes about it, \
                 and `POST /table/{t}/delete` takes it. A field of that name could never be \
                 written through this surface - an `INSERT` would read the value as the record \
                 to write *about* - so a condition on it would answer nothing while the record \
                 sat there. It is spelled with the underscore precisely so that `id` is free \
                 for a field of yours"
            }
            Self::InsertSelect => {
                "`INSERT ... SELECT` reads any answer whose cells are **values**, naming as \
                 many columns as the `INSERT` does - a projection, and a grouping just as much: \
                 a key is a string and a count is a number, and writing a merged grouping into \
                 a table is what a materialised rollup is here. What it cannot read is \
                 `SELECT *`, which answers with record *ids* - the address a fact is written to \
                 rather than anything stored in a column. The ids are allocated, so the column \
                 list may not name `_record_id` either"
            }
            Self::InsertSelfRead => {
                "an `INSERT ... SELECT` reads one table and writes another. Reading and writing \
                 the same one in a single statement has no snapshot under it here: the records \
                 being written are visible to the read that is still running, so the statement \
                 would feed itself. Copy through a second table, or read it out and write it \
                 back with `POST /table/{t}/import`"
            }
            Self::InsertSize => {
                "an `INSERT` carries a bounded number of rows, because the whole statement is \
                 lexed and parsed into literals before the first fact is written - so the batch \
                 is resident twice over before any of it lands. A request is bounded by its \
                 bytes as well, and that is usually the one a statement meets first. \
                 `POST /table/{t}/import` is the route for volume: one fact per line, with no \
                 statement to hold"
            }
            Self::DeleteRows => {
                "there is no row here to delete: a record is the bits set for it across every \
                 field, and removing it means clearing each of them. `POST /table/{t}/delete` \
                 takes the record ids to clear, one per line - which is the `SELECT *` of the \
                 same `WHERE`, written back"
            }
            Self::SessionUse => {
                "`USE` asks this surface to remember a database between statements, and it \
                 remembers nothing: one statement is one request, answered and forgotten. The \
                 database is per request - send `?database=sales`, or qualify the name as \
                 `sales.orders`. `bigctl` accepts `USE` and does exactly that for you"
            }
            Self::ThreePartName => {
                "a column is qualified by an alias and nothing else, so `sales.orders.amount` \
                 has one name too many. A table is qualified - `FROM sales.orders` - and a \
                 column then reaches it through an alias: \
                 `FROM sales.orders o WHERE o.amount > 5`"
            }
            Self::MaterializedView => {
                "a materialised view is a table plus a promise to keep it current, and nothing \
                 here keeps that promise. A plain `CREATE VIEW` does exist - it is a name for a \
                 statement, re-planned at every read - and the materialised half is written out \
                 loud: create a table and write the answer into it"
            }
            Self::ViewBody => {
                "a view here is a filter and a projection over one table: \
                 `SELECT <columns> FROM <table> [WHERE ...]`. It is inlined into the statement \
                 that reads it, and there is no subquery below to nest one in - so a body that \
                 groups, aggregates, joins, orders or limits has no statement to become. `*` is \
                 refused too: a view is re-planned at every read, so a body written with one \
                 would start exposing whatever column the table gains next. A view names the \
                 columns it exposes"
            }
            Self::ViewColumn => {
                "the view does not expose that column, which is most of what a view is for. \
                 `SHOW CREATE VIEW` says what it does expose; reading the column means going to \
                 the table underneath, or a view that names it"
            }
            Self::ViewDepth => {
                "views nested too deep. A view over a view is expanded by substitution, so the \
                 depth is a bound on the statement this becomes rather than a taste in schemas"
            }
            Self::Case => {
                "a case here is written out in full - `CASE WHEN amount > 500 THEN 'big' ELSE \
                 'small' END` - and `if(cond, a, b)`, `multiIf(...)`, `coalesce`, `nullIf` and \
                 `ifNull` are answered too. The short form, `CASE amount WHEN 500 THEN ...`, is \
                 the same statement with the comparison factored out, and there is one spelling \
                 of it so that two trees cannot come to disagree about what they answer"
            }
            Self::Cast => {
                "a value's type is its field's, decided when the field was created: a keyed \
                 column is a string in a dictionary and an integer column is bit planes, and \
                 neither has a representation the other could be read as. A column that should \
                 be counted as a number is an `INT` field, and one that should be grouped is a \
                 `SET`"
            }
            Self::Aggregate => {
                "the aggregates here are `count`, `sum`, `min`, `max`, `avg`, `uniq` (which is \
                 `count(DISTINCT x)`), `topK` and `quantile`, each of which is a fold over bit \
                 planes or over a grouping - which is why every one of them is exact. `argMin`, \
                 `argMax`, `stddev`, `varPop` and `corr` need each record's value revisited \
                 against a running total, and this engine holds bits at `(row, record)` rather \
                 than values to revisit"
            }
            Self::AlterKind => {
                "a field's kind decides how every fact in it was routed and its bit depth is \
                 how many bitmaps hold a value, so neither can change without rewriting every \
                 fact ever written to it. Add a second field, copy into it, and drop the first \
                 - which is three statements because it is three changes, not one hidden \
                 inside a `MODIFY` that would look free"
            }
            Self::Rename => {
                "names are what resolve a fact from a client all the way to a bitmap, and \
                 nothing below this renames one. A new name means a new field or table, the \
                 facts copied into it, and the old one dropped"
            }
            Self::AlterEngine => {
                "the engine a table stores under is fixed when it is created: it decides what \
                 is written for every fact, and the facts already written were written under \
                 the old one. Create a second table under the engine you want and copy into it"
            }
            Self::TruncUnit => {
                "`date_trunc` rounds a moment back to a boundary, and the boundary has to be \
                 one the calendar has: year, quarter, month, week, day, hour, minute or second. \
                 A `DATE` counts whole days, so nothing below a day means anything about one"
            }
            Self::Interval => {
                "this dialect writes the rounding as `date_trunc('month', ts)` and not as an \
                 interval: there is one spelling so that two of them cannot come to disagree \
                 about what a month is"
            }
            Self::ScalarFilter => {
                "`toDate` and `date_trunc` round a value on the way out, where a decimal has \
                 its point put back - they read nothing the projection did not already read. A \
                 `WHERE` runs before there are any values to round, over bitmaps rather than \
                 over records, so a rounded column is not something it could test. Compare the \
                 column itself instead: `date_trunc('month', ts) = '2024-01-01'` is \
                 `ts >= '2024-01-01' AND ts < '2024-02-01'`, which is a range this engine \
                 answers off the bit planes"
            }
            Self::ColumnType => {
                "a column takes one of `SET`, `MUTEX`, `BOOL`, `TIMEQUANTUM`, `SIGNED`, \
                 `UINT(bits)`, `DECIMAL(precision, scale)`, `FLOAT32`, `FLOAT64`, `DATE`, \
                 `DATETIME`, or a SQL spelling of one of those: `TEXT`, `VARCHAR`, `CHAR` and \
                 `STRING` are a set, `TINYINT`, `SMALLINT`, `INT`, `INTEGER` and `BIGINT` are \
                 an unsigned integer of 8, 16, 32, 32 and 64 bits, `BOOLEAN` is a bool, \
                 `FLOAT` and `REAL` are a `FLOAT32`, `DOUBLE` is a `FLOAT64`, and `TIMESTAMP` \
                 is a `DATETIME`. None of them takes a width in brackets that its name does not \
                 already carry: `FLOAT(10, 2)` is a `DECIMAL(10, 2)`, which keeps those digits \
                 exactly where a float would not. There is nothing here a blob or a JSON \
                 document lands in"
            }
            Self::Constraint => {
                "a column list here declares fields and nothing else: there are no rows for \
                 `PRIMARY KEY` to be unique over, no nulls for `NOT NULL` to exclude, and no \
                 write path that would apply a `DEFAULT` to a record nobody wrote a fact for"
            }
            Self::DecimalScale => {
                "a decimal is written `DECIMAL(precision, scale)` - digits in total, then \
                 digits after the point. Neither is optional: `DECIMAL` alone is an integer \
                 wearing a different name, and `price > 5` means `> 500` on a field with two \
                 digits after the point, so the number a comparison is against depends on it"
            }
            Self::BitDepth => {
                "a value occupies between 1 and 64 bits here, one bitmap per bit. Wider than \
                 64 is wider than the integer a value is read back into; narrower than 1 is no \
                 value at all"
            }
            Self::Shape => {
                "a statement answers one question: an aggregate, or a grouped column and one \
                 aggregate of it"
            }
            Self::Predicate => {
                "a condition compares a column with a value using `=`, `!=`, `<`, `<=`, `>`, \
                 `>=`, `IN` or `BETWEEN`; there is no pattern matching and no arithmetic"
            }
            Self::ExplainHalf => {
                "`PLAN` and `SHAPE` name the two halves of a query's explanation - the plans its \
                 calls resolve to, and the columns and clauses its answer takes. A schema \
                 change, a write and a question about the catalog have one description and no \
                 halves, so `EXPLAIN` on its own is the whole of what there is to ask for"
            }
            Self::ExplainRows => {
                "`EXPLAIN` is answered as rows - one line of the description per row, under a \
                 column called `explain` - and this surface resolves a statement to plans \
                 instead, of which an explanation has none. Nothing is wrong with the \
                 statement: ask it of the surface that builds result sets"
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
                "a select list asks for one plan per aggregate and a join for one per table, \
                 and each is fanned out and merged on its own - so the number of them is what \
                 the statement costs in round trips. Ask for at most 16, counting `avg` as \
                 two: a sum and a count"
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The half [`Refused::rank`]'s exhaustive match cannot claim on its own: a variant can be
    /// given a rank and still be left out of [`Refused::ALL`], and the list is what the coverage
    /// gate walks.
    #[test]
    fn every_refusal_is_in_the_list_exactly_once() {
        let mut ranks: Vec<usize> = Refused::ALL.iter().map(|r| r.rank()).collect();
        ranks.sort_unstable();
        assert_eq!(ranks, (0..Refused::ALL.len()).collect::<Vec<_>>());
    }

    /// Every refusal says something, and the two halves are different jobs: the code is what a
    /// client branches on, and the sentence is what a person reads.
    #[test]
    fn every_refusal_carries_a_code_and_a_reason() {
        for refused in Refused::ALL {
            assert!(refused.code().starts_with("sql_"), "{refused:?}");
            assert!(refused.why().len() > 20, "{refused:?} says too little");
        }
    }
}
