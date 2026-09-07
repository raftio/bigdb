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
    /// A window where the answer is numbers about sets rather than rows.
    ///
    /// **Repurposed rather than retired.** This used to mean "window functions are not
    /// supported"; they are now, over a projection - so what is left is the shapes a window
    /// cannot be over, and the entries that name one without saying which rows it sees.
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
    /// `MERGE`, `REPLACE`, or a schema change this engine has no operation behind.
    ///
    /// `TRUNCATE`, `DELETE` and `UPDATE` were all here and are statements now. What stayed are
    /// the ones with nothing behind them at all: a merge needs a row to match on, and
    /// `CREATE INDEX` names a thing a bitmap already is.
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
    /// `DELETE FROM t` with no `WHERE`, which names every record.
    ///
    /// Not answered slowly: clearing every record one id at a time is the same answer
    /// `TRUNCATE TABLE` gives by freeing the fragments, at the worst price this engine can
    /// charge for it. Its own refusal rather than [`Self::DeleteTooLarge`] because it is decided
    /// at parse time and needs no count - and because what to write instead is a different
    /// statement rather than a narrower predicate. It also catches the `ORDER BY` and `LIMIT`
    /// MySQL takes on a delete: there is no order to take the first of here.
    DeleteAll,
    /// A delete whose `WHERE` selects more records than one transaction may clear.
    ///
    /// **The bound is on the half that cannot be interrupted.** The selection is a read and is
    /// bounded like every read - `SETTINGS max_execution_time` really stops it. The clearing is
    /// one transaction and nothing stops one, so what bounds it is the *count*, taken from the
    /// merged set before a bit is cleared. Raised where the count is known, which is the
    /// coordinator, so no statement in this crate's corpus reaches it.
    DeleteTooLarge,
    /// An `UPDATE` naming a column where writing a value adds one rather than replacing it.
    ///
    /// A `SET` column holds every value a record was ever given - that is what makes it a set -
    /// and a `TIMEQUANTUM` column writes a copy into the view for each moment. There is no
    /// per-value unset below this layer, so a new value would join the old rather than replace
    /// it. Raised in `big-cluster`, where the field kinds are, so no statement in this crate's
    /// corpus reaches it.
    UpdateColumn,
    /// `USE`, which asks a stateless surface to remember something between statements.
    ///
    /// Databases exist; a *session* does not. `POST /sql` answers one statement and keeps
    /// nothing, so `USE` is the client's to hold - `bigctl` does, and sends it as `?database=`.
    SessionUse,
    /// `SET`, `SET SESSION`, `SHOW VARIABLES`: a limit asked to outlive the statement it bounds.
    ///
    /// The same decision as [`Self::SessionUse`], reached from the other side - one statement is
    /// one request, answered and forgotten, so there is nothing for a `SET` to stick a limit to.
    /// What exists instead is the `SETTINGS` clause, which writes the limit on the statement,
    /// and which is therefore also the only spelling that survives a reconnect, a pooled proxy
    /// connection or a retry on another node.
    SessionSetting,
    /// A key in a `SETTINGS` clause that names no limit this engine reads.
    ///
    /// Refused rather than ignored, which is the opposite of what ClickHouse does. There are no
    /// bind parameters in this dialect, so the key was typed - and a dropped
    /// `max_execution_tim = 30` is a query running unbounded while its author believes it is
    /// bounded, which reads exactly like a query that was bounded and slow. See [`Self::Round`],
    /// which makes the same argument about a value.
    Setting,
    /// A `KILL` that names something other than one running query by its id.
    ///
    /// A predicate over users, tables or elapsed time kills a set nobody named, and which set it
    /// kills depends on what happened to be running when it arrived. `KILL MUTATION` names work
    /// this engine does not queue: a schema change is applied and finished.
    KillTarget,
    /// `TTL ts + INTERVAL 90 DAY`: retention as a standing promise.
    ///
    /// **Nothing here would keep it.** There is no scheduler in this workspace and `big-embed`
    /// publishes that it spawns no threads, so a `TTL` stored in the catalog would be a table
    /// plus a promise nothing keeps - which is the object [`Self::MaterializedView`] already
    /// refuses by name. What exists instead is the statement that does it now, and it is honest
    /// about what it does: it drops the *index* over those periods, not the records.
    DeclarativeTtl,
    /// A name under `system.` this build does not have.
    ///
    /// Its own refusal rather than the planner's `unknown_table`, because `system` is not a
    /// database anybody created: the views under it are this build's, so a wrong name there is a
    /// question about *this engine* and the answer is the list of the ones that exist.
    SystemTable,
    /// A clause a system view does not take.
    ///
    /// A `Shown` answer is a `ResultSet` built from the catalog, not a shape over plans - so
    /// there is nothing for a `GROUP BY` to group or an `ORDER BY` to order without a second
    /// filter engine over rows, which is the thing this variant exists in order not to need. The
    /// one exception is `WHERE database = '…'` (and `table = '…'` where it means something),
    /// because those two are already the parameters of the introspection underneath.
    SystemClause,
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
    /// `bitmap_and`, `bitmap_count`, `bsi_sum`, `to_bitmap`: a bitmap named as a value to pass
    /// between functions.
    ///
    /// **Its own refusal rather than a [`Self::Aggregate`], because nothing here is missing.**
    /// Every one of these has an answer on this surface already, under the spelling the rest of
    /// the dialect uses - `sum` is the bit-sliced sum, `count(DISTINCT x)` is the cardinality of
    /// a row, `AND` is the intersection. What the Doris and ClickHouse spellings assume is a
    /// `BITMAP` column to hand around, which is what [`Self::BitmapType`] is about; the message
    /// here names the local spelling for each family so that a query ported from either engine
    /// is told what to write, not that the feature is absent.
    BitmapFunction,
    /// `MODIFY`, `ALTER COLUMN` or `CHANGE`: a field's kind and depth are what its bit planes
    /// are, and there is no operation below that changes either.
    AlterKind,
    /// `RENAME COLUMN`. A field's name is what routes a fact to a bitmap, and nothing below
    /// renames one.
    ///
    /// **A table's name is not the same case, and this used to say it was.** A table is reached
    /// through an interned id - fragments, row keys, field ids and grants are all keyed by it -
    /// so its name is one catalog record and `ALTER TABLE t RENAME TO u` changes that record.
    /// This refusal narrowed to the half that is still true rather than disappearing, which is
    /// the widening `docs/versioning.md` allows.
    Rename,
    /// `ALTER TABLE ... ENGINE =`, which asks a table to store something else than it does.
    AlterEngine,
    /// A type name in a column list that names nothing this engine stores.
    ColumnType,
    /// `Nullable(T)`: a column declared to hold a value that may be absent.
    ///
    /// Separate from [`Self::ColumnType`] because it is not a name this engine failed to
    /// recognise - it is a name it recognises and declines. Absence is already how a column
    /// answers here: a record either has the bit set or it does not, and `SELECT *` says so by
    /// omission rather than by a null. Declaring it would add a second way to be absent that
    /// nothing below could tell from the first, and [`Self::Null`] is the other half of that
    /// same decision - it is why `IS NULL` has nothing to compare.
    NullableType,
    /// `BITMAP`, `AggregateFunction(groupBitmap, ...)`, `HLL`: a bitmap declared as a column's
    /// type.
    ///
    /// **A bitmap is not a value a column holds here; it is what every column already is.** A
    /// `SET` column is one bitmap per interned key and an integer column is one per bit plane,
    /// so a `BITMAP` column would be a bitmap inside a bitmap - and the operations it exists to
    /// carry are the ones this dialect already spells with `AND`, `count(DISTINCT)` and `sum`.
    /// See [`Self::BitmapFunction`] for the mapping.
    BitmapType,
    /// `date_trunc` given a boundary the calendar does not have, or one finer than the column.
    TruncUnit,
    /// An interval spelling of a rounding this dialect writes as `date_trunc`.
    Interval,
    /// A scalar call in a `WHERE`, where there are no values yet to apply it to.
    ScalarFilter,
    /// A `GROUP BY` term that is neither a column nor a calendar rounding of one.
    ///
    /// Its own refusal rather than a [`Self::Shape`], because the two say different things: a
    /// shape refusal is about the statement asking more than one question, and this is about a
    /// term the engine has no way to group *by*. What it names is also different - the two terms
    /// that do work - which is the whole reason the list is enumerated.
    GroupExpression,
    /// A rounding in a `WHERE` compared against a value it can never produce:
    /// `date_trunc('month', ts) = '2024-01-15'`, which no month begins on.
    ///
    /// Strictly this selects no records rather than being an error, and that is what another
    /// engine answers. It is refused here because there are no bind parameters in this dialect,
    /// so the value was typed rather than substituted - and an empty answer to a typo looks
    /// exactly like a table with no January in it.
    Round,
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

    // ---- who may do what -------------------------------------------------------------------
    /// `CREATE USER`, `ALTER USER`, `DROP USER`, `ALTER ROLE`.
    ///
    /// **The decision that people are not stored here, made visible.** A credential is a line in
    /// a file the server reads off its own disk, and a route that could write one would let an
    /// `admin` password rewrite the password file over the network. Roles are administered in
    /// SQL; who holds one is the users file's to say.
    CreateUser,
    /// A word in a privilege list that is not a privilege.
    AclPrivilege,
    /// A privilege that means nothing at the level it was granted on: `ROLES` inside a database,
    /// or `CREATE` on a table that would have to exist for the statement to parse.
    AclObject,
    /// `GRANT SELECT(a, b) ON t`. Grants here are per table.
    ///
    /// The other half of a published decision: this surface answers questions over whole tables,
    /// and a privilege finer than the answer is a fence somebody walks around by asking a
    /// slightly different question.
    AclColumns,
    /// `GRANT analyst TO senior`: a role holding another role.
    ///
    /// Also what a mistyped privilege becomes when the shape is otherwise a grant, which is why
    /// it is separate from [`Refused::AclPrivilege`] - the two call for different fixes.
    RoleGrant,
    /// `TO PUBLIC`, `TO ALL`: a role everybody holds without being given it.
    AclPublic,
    /// `WITH GRANT OPTION`, `WITH ADMIN OPTION`: the power to pass a grant on.
    GrantOption,
    /// `CREATE ROLE superuser`, or a grant aimed at it.
    ///
    /// It holds everything without being stored, which is what a database whose catalog is empty
    /// is recovered through - so it must not be creatable, droppable or narrowable.
    ReservedRole,
    /// `GROUP BY a WITH TOTALS`: a grand total beside the rows rather than among them.
    ///
    /// Refused because a result set here is columns and rows, one definition every format
    /// renders from - there is no second place to put a number. The same number *as a row* is
    /// `WITH ROLLUP`, whose last row is the empty grouping set.
    WithTotals,
    /// More sets than one statement may name - see `parse::select::MAX_GROUPING_SETS`.
    ///
    /// A bound on the fan-out rather than a taste in rollups: every set is its own grouping,
    /// planned and merged on its own, so sets multiplied by aggregates is what the statement
    /// costs in round trips.
    GroupingSets,
    /// `ORDER BY`, `LIMIT` or `OFFSET` over a `ROLLUP`, `CUBE` or `GROUPING SETS` answer.
    ///
    /// Those rows are one grouping per set rendered one after the other - the stacking
    /// `UNION ALL` is - so there is no single list to sort or to cut. Ordering each set on its
    /// own would look sorted and not be, and a `LIMIT 10` would answer ten rows *per set*.
    RollupOrder,
    /// A frame clause, or an ordering under an aggregate window - which means one.
    ///
    /// Every window this surface answers is over the whole partition in the order given. A
    /// frame that is silently the default is a different answer wearing the right syntax.
    WindowFrame,
    /// `WINDOW w AS (...)` and `OVER w`: the clause written somewhere other than where it is used.
    WindowName,
    /// `QUALIFY`: a `HAVING` over a window function's own output.
    ///
    /// A second filter, over numbers that exist only after every row has been read.
    Qualify,
    /// An approximate-distinct sketch, or a `-State`/`-Merge` combinator over one.
    ///
    /// Refused because there is nothing here for a sketch to approximate: a distinct count is a
    /// popcount, and the partial result that travels between nodes is the bitmap itself.
    Sketch,
    /// A regular expression, in the select list or in a `WHERE`.
    ///
    /// The pattern language here is `LIKE`'s, and it runs over a keyed column's dictionary
    /// rather than over records - see `big_db::like`, where the decision is written out.
    Regex,
    /// `ANALYZE TABLE`, and therefore `EXPLAIN ANALYZE`.
    ///
    /// There is no cost model for statistics to feed: the planner is purely syntactic.
    Analyze,
    /// `OPTIMIZE TABLE`, `ALTER TABLE ... COMPACT`, `FREEZE`.
    ///
    /// Compaction exists and is whole-file, because there are no parts to merge.
    Optimize,
    /// `s3(...)`, `url(...)`, `file(...)`, `generateRandom()` in a `FROM`.
    ///
    /// Reading an external source is a catalog this build has not got, and generating typed
    /// rows needs a generator it has not got either. `numbers(n)` is the one table function
    /// here, and it is named in the sentence so the refusal points at what does work.
    TableFunction,
    /// `numbers(n)` for an `n` past [`crate::MAX_NUMBERS`].
    ///
    /// **Refused with the number rather than clamped**, unlike a `SETTINGS` value. The clamp is
    /// allowed to be silent there because `EXPLAIN` prints the figure actually applied; a
    /// `Show` has no such line, so a quiet ceiling would be a different answer wearing the
    /// right shape.
    NumbersTooLarge,
    /// `neighbor`, `sequenceMatch`, `windowFunnel`, `retention`: the row-sequence family.
    ///
    /// One code with a mapping in the sentence, the way [`Self::BitmapFunction`] carries one:
    /// these are one family of questions, and what each needs is a different local spelling
    /// rather than a different kind of answer.
    SequenceFunction,
    /// A `SAMPLE` fraction that names no whole stride, or `SAMPLE BY` in a `CREATE TABLE`.
    ///
    /// **The clause itself is answered** - one record in every `n`, masked on the id's low bits.
    /// What is refused is a fraction that is not one over a whole number, because rounding it
    /// would answer a different question under the name of this one, and `SAMPLE BY`, which
    /// names the column a sample is drawn on: here the unit is the record id, which is not a
    /// column and cannot be chosen.
    Sample,
    /// `INSERT OVERWRITE ... PARTITION (...)`.
    ///
    /// **The statement itself is answered now**; what stays refused is the clause that aims it
    /// at a partition, because a shard here is a function of the record id and there is nothing
    /// for one to name. Narrowed rather than removed, which is the widening
    /// `docs/versioning.md` allows.
    Overwrite,
    /// `COPY INTO t FROM 's3://...'`.
    CopyInto,
    /// `CREATE TEMPORARY TABLE`.
    ///
    /// There is no session for one to be temporary *to*: `POST /sql` answers a statement and
    /// forgets everything, which is the decision [`Self::SessionUse`] and
    /// [`Self::SessionSetting`] both rest on.
    TemporaryTable,
    /// `PARTITION BY`, `DISTRIBUTED BY ... BUCKETS n`, `CLUSTER BY`.
    ///
    /// Distribution here is not declared, it is computed: a shard is `record_id >> 20`, so it
    /// falls out of the id a record was written under. A bucket count nothing reads would be the
    /// empty promise [`Self::Setting`] refuses a setting for.
    Distribution,
    /// A column declared as `MATERIALIZED expr` or `ALIAS expr`.
    ///
    /// Both are computed columns, and they are computed at opposite ends: one at write time,
    /// which is the materialised view this build has not got, and one at read time, which is
    /// what a `VIEW` here already is.
    ComputedColumn,
    /// `Array(T)`, `Map(K, V)`, `Tuple(...)`, `Nested(...)`.
    ///
    /// Every stored value here is a `u64` from the write path down to the codec. But a keyed
    /// column already holds *many* values for one record, which is the thing an array of strings
    /// is for - so the sentence points there rather than reporting an absence.
    CompositeType,
    /// An `ENUM` declared in a shape this dialect does not keep.
    ///
    /// **The type itself is answered now.** What is refused is the `'a' = 1` form, whose number
    /// says which integer the value is stored as - here the dictionary assigns that, so keeping
    /// the number would be a promise nothing honours - and a duplicate member, which would be
    /// unreachable and would make `SHOW CREATE TABLE` disagree with what was typed.
    EnumType,
    /// `ARRAY JOIN`, `LATERAL VIEW explode(...)`, `UNNEST(...)`.
    ///
    /// The row-per-element shape it produces is what `GROUP BY <keyed column>` already produces,
    /// and by walking a dictionary rather than multiplying rows.
    ArrayJoin,
    /// `arrayMap`, `has`, `arrayExists`, and the lambdas that go inside them.
    ///
    /// One code with a mapping in the sentence, the way [`Self::BitmapFunction`] carries one:
    /// each of these names a question a keyed column answers under a different word.
    ArrayFunction,
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
    pub const ALL: [Self; 97] = [
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
        Self::DeleteAll,
        Self::DeleteTooLarge,
        Self::UpdateColumn,
        Self::DeclarativeTtl,
        Self::KillTarget,
        Self::SessionUse,
        Self::SessionSetting,
        Self::Setting,
        Self::MaterializedView,
        Self::Case,
        Self::Cast,
        Self::Aggregate,
        Self::BitmapFunction,
        Self::AlterKind,
        Self::Rename,
        Self::AlterEngine,
        Self::ColumnType,
        Self::NullableType,
        Self::BitmapType,
        Self::TruncUnit,
        Self::Interval,
        Self::ScalarFilter,
        Self::Round,
        Self::GroupExpression,
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
        Self::SystemTable,
        Self::SystemClause,
        Self::ThreePartName,
        Self::ViewBody,
        Self::ViewColumn,
        Self::ViewDepth,
        Self::ExplainHalf,
        Self::ExplainRows,
        Self::CreateUser,
        Self::AclPrivilege,
        Self::AclObject,
        Self::AclColumns,
        Self::RoleGrant,
        Self::AclPublic,
        Self::GrantOption,
        Self::ReservedRole,
        Self::WithTotals,
        Self::GroupingSets,
        Self::RollupOrder,
        Self::WindowFrame,
        Self::WindowName,
        Self::Qualify,
        Self::Sketch,
        Self::Regex,
        Self::Analyze,
        Self::Optimize,
        Self::TableFunction,
        Self::NumbersTooLarge,
        Self::SequenceFunction,
        Self::Sample,
        Self::Overwrite,
        Self::CopyInto,
        Self::TemporaryTable,
        Self::Distribution,
        Self::ComputedColumn,
        Self::CompositeType,
        Self::EnumType,
        Self::ArrayJoin,
        Self::ArrayFunction,
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
            Self::DeleteAll => 20,
            Self::DeleteTooLarge => 70,
            Self::UpdateColumn => 71,
            Self::DeclarativeTtl => 72,
            Self::KillTarget => 73,
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
            Self::CreateUser => 53,
            Self::AclPrivilege => 54,
            Self::AclObject => 55,
            Self::AclColumns => 56,
            Self::RoleGrant => 57,
            Self::AclPublic => 58,
            Self::GrantOption => 59,
            Self::ReservedRole => 60,
            Self::Round => 61,
            Self::GroupExpression => 62,
            Self::BitmapFunction => 63,
            Self::NullableType => 64,
            Self::BitmapType => 65,
            Self::SessionSetting => 66,
            Self::Setting => 67,
            Self::SystemTable => 68,
            Self::SystemClause => 69,
            Self::WithTotals => 74,
            Self::GroupingSets => 75,
            Self::RollupOrder => 76,
            Self::WindowFrame => 77,
            Self::WindowName => 78,
            Self::Qualify => 79,
            Self::Sketch => 80,
            Self::Regex => 81,
            Self::Analyze => 82,
            Self::Optimize => 83,
            Self::TableFunction => 84,
            Self::NumbersTooLarge => 85,
            Self::SequenceFunction => 86,
            Self::Sample => 87,
            Self::Overwrite => 88,
            Self::CopyInto => 89,
            Self::TemporaryTable => 90,
            Self::Distribution => 91,
            Self::ComputedColumn => 92,
            Self::CompositeType => 93,
            Self::EnumType => 94,
            Self::ArrayJoin => 95,
            Self::ArrayFunction => 96,
        }
    }

    /// The stable identifier a client branches on.
    ///
    /// Several variants share `sql_unsupported`: they differ in which construct was written,
    /// which the message already says, and not in what the client should do about it — which is
    /// the same rule `PlanError` applies to its four parse variants.
    pub fn code(self) -> &'static str {
        match self {
            Self::WithTotals => "sql_with_totals",
            Self::GroupingSets => "sql_too_many_grouping_sets",
            Self::RollupOrder => "sql_rollup_order",
            Self::WindowFrame => "sql_window_frame",
            Self::WindowName => "sql_window_named",
            Self::Qualify => "sql_qualify",
            Self::Sketch => "sql_sketch",
            Self::Regex => "sql_no_regex",
            Self::Analyze => "sql_no_analyze",
            Self::Optimize => "sql_no_optimize",
            Self::TableFunction => "sql_no_table_function",
            Self::NumbersTooLarge => "sql_numbers_too_large",
            Self::SequenceFunction => "sql_no_sequence_function",
            Self::Sample => "sql_no_sample",
            Self::Overwrite => "sql_no_overwrite",
            Self::CopyInto => "sql_no_copy_into",
            Self::TemporaryTable => "sql_no_temporary_table",
            Self::Distribution => "sql_no_distribution",
            Self::ComputedColumn => "sql_computed_column",
            Self::CompositeType => "sql_composite_type",
            Self::EnumType => "sql_enum_type",
            Self::ArrayJoin => "sql_no_array_join",
            Self::ArrayFunction => "sql_array_function",
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
            Self::Write => "sql_read_only",
            // Their own codes, and not `sql_read_only`: a delete *is* taken now, so a client
            // reading "this surface does not write" would be told something untrue about a
            // statement that is fine. What each needs is different too - one says write a
            // different statement, the other says narrow the one you wrote.
            Self::DeleteAll => "sql_delete_all",
            Self::DeleteTooLarge => "sql_delete_too_large",
            Self::UpdateColumn => "sql_update_column",
            Self::DeclarativeTtl => "sql_declarative_ttl",
            Self::KillTarget => "sql_kill_target",
            Self::InsertColumns | Self::InsertId => "sql_insert_shape",
            Self::IdColumn => "sql_id_column",
            Self::InsertSize => "sql_insert_too_large",
            // Its own code rather than the shared one: what a client does about it is copy
            // through a second table, which is nothing like what the other shapes need.
            Self::InsertSelfRead => "sql_insert_self_read",
            Self::SessionUse => "sql_use_unsupported",
            Self::SessionSetting => "sql_no_session_settings",
            Self::Setting => "sql_unknown_setting",
            Self::SystemTable => "sql_system_table",
            Self::SystemClause => "sql_system_clause",
            Self::MaterializedView => "sql_no_materialized_views",
            Self::ViewBody => "sql_view_body",
            Self::ViewColumn => "sql_view_column",
            Self::ViewDepth => "sql_view_depth",
            Self::AlterKind => "sql_no_alter_column",
            Self::Rename => "sql_no_rename",
            Self::AlterEngine => "sql_no_alter_engine",
            Self::ColumnType => "sql_unknown_column_type",
            // Three codes rather than one shared with `sql_unknown_column_type` or
            // `sql_unsupported`: each names a construct the client has to *rewrite*, and the
            // rewrite is different in all three - drop the wrapper, declare `SET`, or spell the
            // operation the way this dialect spells it.
            Self::NullableType => "sql_nullable_type",
            Self::BitmapType => "sql_bitmap_type",
            Self::BitmapFunction => "sql_bitmap_function",
            Self::TruncUnit => "sql_bad_trunc_unit",
            Self::Interval => "sql_unsupported",
            Self::ScalarFilter => "sql_scalar_in_filter",
            Self::Round => "sql_rounded_value",
            Self::GroupExpression => "sql_group_expression",
            Self::Constraint => "sql_no_constraints",
            Self::DecimalScale => "sql_decimal_scale",
            Self::BitDepth => "sql_bit_depth",
            Self::TooManyCalls => "sql_too_many_aggregates",
            Self::Format => "sql_unknown_format",
            Self::Union => "sql_union",
            Self::Quantile => "sql_quantile_level",
            Self::ExplainHalf => "sql_explain_half",
            Self::ExplainRows => "sql_explain_rows",
            Self::CreateUser => "sql_no_users",
            Self::AclPrivilege => "sql_unknown_privilege",
            Self::AclObject => "sql_acl_object",
            Self::AclColumns => "sql_acl_columns",
            Self::RoleGrant => "sql_no_role_hierarchy",
            Self::AclPublic => "sql_no_public",
            Self::GrantOption => "sql_no_grant_option",
            Self::ReservedRole => "sql_reserved_role",
            Self::Window => "sql_window_shape",
            Self::Subquery
            | Self::Having
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
            Self::Window => {
                "a window is computed over the rows a **projection** read - `SELECT country, \
                 row_number() OVER (PARTITION BY country ORDER BY amount DESC) FROM t` - \
                 because that is the one answer this surface materialises as rows. A grouped \
                 answer, a tuple grouping and a join are numbers about sets rather than rows, \
                 so there is nothing for a partition to be a partition of: `ORDER BY count(*) \
                 DESC LIMIT 10` is the ranking those already carry, and `topK(n)(x)` is the \
                 same ranking as a list in one cell. A window function written without `OVER` \
                 earns this too - it is a window missing the clause that says which rows it \
                 ranks"
            }
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
                 `ALTER VIEW` \
                 is `CREATE OR REPLACE VIEW`, which says the whole statement rather than a \
                 change to one; and `MERGE`, `REPLACE` and `CREATE INDEX` are each \
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
            Self::KillTarget => {
                "`KILL QUERY` names one running query by its id, as `KILL QUERY '<node>/<n>'` \
                 or `KILL QUERY WHERE query_id = '<node>/<n>'`. `SHOW PROCESSLIST` says what the \
                 ids are. A predicate over users, tables or elapsed time would kill a set nobody \
                 named - and a different set each time it ran - and `KILL MUTATION` names work \
                 this engine does not queue: a schema change is applied and finished, with no \
                 background rewrite left to cancel"
            }
            Self::DeclarativeTtl => {
                "`TTL` names a promise to expire data on its own, and there is nothing here to \
                 keep one: this database has no scheduler and the library under it starts no \
                 threads. What exists is the statement that does it when you run it - \
                 `ALTER TABLE t DROP DAYS BEFORE '2026-01-01' ON ts` - which travels to every \
                 node the way a schema change does, so no two of them disagree about what a \
                 window holds. Note what it removes: the per-period *index* over those days, not \
                 the records. A `BETWEEN` stops finding them and `count(*)` does not change; \
                 `DELETE FROM t WHERE ts < '2026-01-01'` is the one that removes them. Run it \
                 from cron"
            }
            Self::UpdateColumn => {
                "that column holds every value a record was ever given, so writing a new one \
                 adds it rather than replacing it - and there is no per-value unset below this \
                 surface to remove the old. What updates in place are the columns where one \
                 record holds one value: the integer, decimal and float columns, `MUTEX` and \
                 `BOOL`. For a `SET` or a `TIMEQUANTUM`, delete the record and write it again, \
                 which is two statements because it is two operations"
            }
            Self::DeleteAll => {
                "a `DELETE` here names the records to clear, as \
                 `DELETE FROM t WHERE ts < '2026-01-01'`. Without a `WHERE` it names every one of \
                 them, and clearing them id by id is the most expensive way to say what \
                 `TRUNCATE TABLE t` says by freeing the fragments - which keeps the table, its \
                 fields and its row keys. `ORDER BY` and `LIMIT` are refused for a different \
                 reason: a record id is an address rather than a position, so taking the first \
                 ten of a set would clear an arbitrary ten while reading as though it had chosen \
                 them"
            }
            Self::DeleteTooLarge => {
                "a delete is one transaction and nothing interrupts one: the `WHERE` is bounded \
                 by `max_execution_time`, and the clearing that follows it is not. This one \
                 selects more records than that half may carry. Narrow the `WHERE` - a window on \
                 a time column is the usual way, deleting a day or a month at a time - or lower \
                 the ceiling deliberately with `SETTINGS max_delete_records`, which may only \
                 tighten it. `TRUNCATE TABLE t` is the whole-table spelling and costs fragments \
                 rather than records"
            }
            Self::SessionUse => {
                "`USE` asks this surface to remember a database between statements, and it \
                 remembers nothing: one statement is one request, answered and forgotten. The \
                 database is per request - send `?database=sales`, or qualify the name as \
                 `sales.orders`. `bigctl` accepts `USE` and does exactly that for you"
            }
            Self::SessionSetting => {
                "`SET` asks this surface to remember a limit between statements, and it remembers \
                 nothing: one statement is one request, answered and forgotten - the same reason \
                 `USE` is refused. A limit is written on the statement it bounds, as \
                 `SELECT ... SETTINGS max_execution_time = 30`, which is also the only spelling \
                 that survives a reconnect, a pooled proxy connection or a retry on another \
                 node. The keys are `max_execution_time` (seconds), `max_memory_usage` (bytes), \
                 `max_result_rows` and `max_delete_records`, and each may only lower what the \
                 server allows"
            }
            Self::Setting => {
                "that key names no limit this engine reads. The three it reads are \
                 `max_execution_time` (seconds a read may run), `max_memory_usage` (bytes of \
                 bitmap it may hold) and `max_result_rows` (records it may read back). \
                 `max_threads`, `max_block_size` and `join_algorithm` are refused rather than \
                 accepted and ignored: there are no bind parameters here, so a key was typed, \
                 and a dropped one is a statement running without the bound its author believes \
                 it has"
            }
            Self::SystemTable => {
                "there is no such view under `system.`. This build has `system.tables` (every \
                 table and view, in every database), `system.columns` (every column of every \
                 table), `system.databases` and `system.parts` (every fragment: table, field, \
                 view and shard, which is the one thing no other statement shows). `system` is \
                 not a database anybody creates, so a name here is a question about this engine \
                 rather than about your schema - `system.query_log` and `system.processlist` \
                 would each need state this build does not keep"
            }
            Self::WithTotals => {
                "`WITH TOTALS` puts the grand total outside the rows, in a `totals` field beside them - \
                 and a result set here is columns and rows, one definition all five formats render \
                 from, so there is nowhere for it to go. The same number as a *row* is `GROUP BY a WITH \
                 ROLLUP`, whose last row is the empty grouping set; `grouping(a)` is what tells that row \
                 from a group whose key this node was never told"
            }
            Self::GroupingSets => {
                "`ROLLUP`, `CUBE` and `GROUPING SETS` are one grouping per set - a call planned, fanned \
                 out to every owner and merged on its own - so the number of sets multiplied by the \
                 aggregates in the select list is what the statement costs in round trips, against the \
                 same budget every select list answers to. `CUBE` over four columns is sixteen sets, \
                 which is that whole budget before a second aggregate is named. `WITH ROLLUP` is one set \
                 per prefix - five for four columns - and `GROUPING SETS ((a,b),(a),())` names exactly \
                 the ones you want, up to eight"
            }
            Self::RollupOrder => {
                "a `ROLLUP`, `CUBE` or `GROUPING SETS` answer is one grouping per set, rendered one \
                 after the other - the same stacking `UNION ALL` is - so there is no single list for an \
                 `ORDER BY` to sort or a `LIMIT` to cut. Ordering each set on its own would look sorted \
                 and not be, and a `LIMIT 10` would answer with ten rows per set. The rows do arrive in \
                 a fixed order - the sets longest first, each in its own key order - so order and cut \
                 them in the client, or write the one set you want ordered as its own `GROUP BY`"
            }
            Self::WindowFrame => {
                "a window here is computed over the whole partition in the order given, which is the \
                 default frame for `row_number`, `rank`, `lag`, `first_value` and the rest of those two \
                 families - so `ROWS`, `RANGE`, `GROUPS` and `EXCLUDE` have nothing to narrow and are \
                 refused rather than accepted and ignored. An `ORDER BY` under `sum`, `avg`, `count`, \
                 `min` or `max` says the same thing: the running total, whose frame is `RANGE UNBOUNDED \
                 PRECEDING` and whose answer is a different number from the partition total. Write \
                 `sum(x) OVER (PARTITION BY c)` for the partition total, and order the answer itself"
            }
            Self::WindowName => {
                "a window is written where it is used - `row_number() OVER (PARTITION BY country ORDER \
                 BY amount DESC)` - and there is no `WINDOW w AS (...)` to name one somewhere else. A name \
                 would be a second place for the clause to live and a second thing to keep in step with the \
                 first; writing it out at each entry is longer and says what it does"
            }
            Self::Qualify => {
                "`QUALIFY` filters on a window function's own output, which exists only once every row of \
                 every partition has been read - so it is a second pass over the finished answer rather than \
                 anything a plan could carry. `WHERE` narrows the records the window then ranks, which is the \
                 cheap half and usually the one that was meant; filter on the ranking itself in the client"
            }
            Self::Sketch => {
                "there is no sketch here because there is nothing for one to approximate: `count(DISTINCT \
                 x)` is the cardinality of a bitmap, which is a popcount - exact, and paid per container \
                 rather than per record. `uniq`, `uniqExact`, `uniqCombined`, `uniqCombined64`, `uniqHLL12`, \
                 `uniqTheta` and `approx_count_distinct` are all accepted exactly as written and all answered \
                 exactly, and `quantile`, `quantileExact` and `median` are exact too. A `-State`/`-Merge` pair \
                 exists elsewhere to carry a partial sketch between queries; the partial result that travels \
                 between nodes here **is** the bitmap, and the engine merges it"
            }
            Self::Regex => {
                "the pattern language here is `LIKE`'s: `%` for any run of characters, `_` for exactly one, \
                 and `ILIKE` to fold case. It runs over a keyed column's **dictionary** rather than over \
                 records - each distinct string is stored once with the bitmap of the records holding it - so \
                 `country LIKE 'G%'` costs the field's cardinality once where a row engine pays it per record. \
                 A regular expression engine in that loop would be a dependency, a compile step and a class of \
                 pathological pattern, to implement two characters. Write `LIKE` or `ILIKE`, or `IN (...)` for \
                 a fixed set of values"
            }
            Self::Analyze => {
                "there are no statistics to gather. The planner here is purely syntactic - a call resolves \
                 against the schema, and nothing about the data changes which plan it becomes - so there is no \
                 cost model for an `ANALYZE` to feed. What this engine keeps instead is a zone map per \
                 fragment, `min`, `max`, `bit_depth` and whether it holds values, written as facts are and \
                 therefore never stale; it is already readable as `SELECT * FROM system.parts`. `EXPLAIN \
                 <statement>` is the whole of what this surface says about a statement, and it says it without \
                 running one. And `EXPLAIN ANALYZE` in the other sense - run it, then report \
                 what it cost - is absent for a second reason worth separating from the first: \
                 the numbers exist per node, but nothing carries them back beside the answer \
                 and nothing says how two nodes' would merge"
            }
            Self::Optimize => {
                "compaction here is whole-file rather than per part, because there are no parts to merge: a \
                 fragment is a range of records inside one page store, and what accumulates is free pages \
                 rather than small files. Rewriting the store into a fresh one and swapping it in is a server \
                 operation - `POST /admin/backup` - rather than a statement, because it needs the file's \
                 exclusive lock, which a statement running inside a read does not hold. `SELECT * FROM \
                 system.parts` is what says whether it is worth doing"
            }
            Self::TableFunction => {
                "the one table function here is `numbers(n)`, which counts. Reading an external \
                 source needs a catalog this build has not got - `s3`, `url` and `file` are the \
                 external-catalog milestone - and loading data is `POST /table/{t}/import`, \
                 which streams it in without a statement having to name a path"
            }
            Self::NumbersTooLarge => {
                "`numbers(n)` builds every row at the coordinator before any of them is written \
                 out, so the count is the only bound there is: `SETTINGS max_result_rows` \
                 bounds a query, and this is answered from the catalog side without ever \
                 reaching a read. Ask for fewer, or write the rows into a table and select from \
                 that"
            }
            Self::SequenceFunction => {
                "`neighbor(x, n)` is `lag(x, n)` backwards and `lead(x, n)` forwards, and this \
                 dialect writes it as whichever one was meant - one spelling per operation, so \
                 two cannot come to disagree about which direction a sign means. \
                 `runningDifference(x)` is here and is `x - lag(x, 1)`. `sequenceMatch`, \
                 `windowFunnel` and `retention` are not: each is a pattern over a *sliding \
                 window* of rows, which is the frame `sql_window_frame` refuses, plus a pattern \
                 language over them. Counting by period is `GROUP BY date_trunc(...)`"
            }
            Self::Sample => {
                "`SAMPLE` takes one record in every `n`, written as `SAMPLE 10`, `SAMPLE 1/10` \
                 or `SAMPLE 0.1` - three spellings of one stride. A fraction that is not one \
                 over a whole number has no stride: `0.3` would be one in 3.33, and answering it \
                 as one in three is a tenth more data than was asked for under the name that was \
                 asked for. `SAMPLE BY` is a different thing again - it names the column a \
                 sample is drawn on, and here the unit is the record id, which is not a column"
            }
            Self::Overwrite => {
                "`INSERT OVERWRITE` is answered, but not at a partition: a shard here is \
                 `record_id >> 20`, computed from the id a record was written under, so there is \
                 nothing for `PARTITION (...)` to name. Write the statement without it. Note \
                 what the plain form does and does not promise - it empties and then writes, and \
                 nothing wraps those two in one transaction, so a crash between them leaves the \
                 table empty. For a replacement that is genuinely atomic, write into a second \
                 table and `EXCHANGE TABLES` them"
            }
            Self::CopyInto => {
                "loading data is `POST /table/{t}/import`, which streams rows in without a \
                 statement having to name a path the server can reach. Reading object storage \
                 from inside a query is an external catalog, and that is a milestone of its own \
                 rather than a clause"
            }
            Self::TemporaryTable => {
                "there is no session for a table to be temporary to: `POST /sql` answers one \
                 statement and forgets everything about the caller, which is the same decision \
                 that refuses `USE` and `SET`. An ordinary `CREATE TABLE` and a `DROP TABLE` \
                 when you are done is the pair that survives a reconnect, a proxied connection \
                 and a retry against another node"
            }
            Self::Distribution => {
                "distribution here is computed rather than declared: a shard is `record_id >> \
                 20`, so which one a record lands in follows from the id it was written under \
                 and there is nothing for a clause to choose. A bucket count nothing reads would \
                 be a promise this engine does not keep - the objection `sql_unknown_setting` \
                 makes to `max_threads`. `SELECT * FROM system.parts` is where the distribution \
                 that actually happened can be seen"
            }
            Self::ComputedColumn => {
                "a computed column is computed at one of two moments, and each has its own \
                 answer here. `ALIAS expr` is computed when the column is read, which is what a \
                 view already is: `CREATE VIEW v AS SELECT <expr> AS <name>, ... FROM t`. \
                 `MATERIALIZED expr` is computed when the row is written, which is a \
                 materialised view - and that one this build has not got (`sql_no_materialized \
                 _views`)"
            }
            Self::CompositeType => {
                "every value stored here is one number wide, from the fact that is written to \
                 the codec that packs it, so there is nowhere to put a list or a pair. But a \
                 keyed column already holds *many* values for one record - that is what `SET` \
                 is - so `Array(String)` is `SET` under another name, and it arrives indexed: \
                 `has(arr, x)` is `WHERE c = 'x'`, and one row per element is `GROUP BY c`. A \
                 `Map` or a `Tuple` is two columns"
            }
            Self::EnumType => {
                "an enum is written `ENUM('a', 'b')` - the values, and nothing else. The \
                 `'a' = 1` form says which integer each value is stored as, and here a mutex \
                 interns each one into the dictionary and the *dictionary* decides that number, \
                 so a written `= 1` would be a promise about storage that nothing keeps. A \
                 repeated value is refused for a nearer reason: the second one is unreachable, \
                 and a list holding fewer values than it names would make `SHOW CREATE TABLE` \
                 disagree with what was typed"
            }
            Self::ArrayJoin => {
                "one row per element of a repeated column is `GROUP BY <the column>`, which is \
                 what a keyed column already gives: the elements are its keys, and walking them \
                 is a walk of the dictionary rather than a multiplication of rows. There is no \
                 array-valued column here for the clause to expand - see `sql_composite_type`"
            }
            Self::ArrayFunction => {
                "a repeated column here is a `SET`, and each of these has a spelling over one: \
                 `has(c, x)` is `c = 'x'` in a `WHERE`, `arrayExists` is the same, \
                 `arrayDistinct`/`arrayUniq` is `count(DISTINCT c)`, `arraySum` is `sum(c)`, \
                 and one row per element is `GROUP BY c`. A lambda has no collection to walk \
                 for the same reason - there is no array-valued column (`sql_composite_type`)"
            }
            Self::SystemClause => {
                "a system view answers in full, and the clause it takes is \
                 `WHERE database = '<name>'` - on `system.columns` and `system.parts` also \
                 `WHERE table = '<name>'`, joined by `AND`. Those two are the only ones because \
                 they are already what `SHOW TABLES FROM d` and `DESCRIBE t` pass underneath; \
                 the answer is a set of rows read out of the catalog rather than a plan over \
                 bitmaps, so there is nothing here for a join, a grouping, an ordering, a limit \
                 or a `SETTINGS` budget to act on. Order it and cut it in the client, or select \
                 the columns you want by name"
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
            Self::BitmapFunction => {
                "every one of these already has an answer here, spelled the way the rest of the \
                 dialect is spelled - the bitmaps are the storage, not a value to pass between \
                 functions. `bitmap_count` and `bitmapCardinality` are `count(DISTINCT x)`, and \
                 exact, because the cardinality of a bitmap is a popcount - which is also why \
                 `uniq`, `uniqHLL12` and `approx_count_distinct` are read as written and answered \
                 exactly rather than refused at all; `bsi_sum` is `sum(x)` and \
                 `bsi_range` is `x BETWEEN a AND b`, both folds over bit planes; `bitmap_and` is \
                 `AND` and `bitmap_andnot` is `NOT IN (SELECT _record_id FROM ...)`; `to_bitmap` \
                 and `groupBitmapState` name what a `SET` column already did when the fact was \
                 written. What has no spelling here is a bitmap held *in* a column, which is what \
                 a `BITMAP` type would be"
            }
            Self::NullableType => {
                "absence is already how a column answers here, so there is no wrapper to declare: \
                 a record either has the bit set for a value or it does not, and `SELECT *` says \
                 so by leaving the cell out. `Nullable(String)` would add a second way to be \
                 absent that nothing below could tell from the first. Declare the type itself - \
                 `TEXT`, `INT`, `DECIMAL(10, 2)` - and read the absence off the answer"
            }
            Self::BitmapType => {
                "a bitmap is not a value a column holds here; it is what every column already is. \
                 A `SET` column is one bitmap per interned key and an integer column is one per \
                 bit plane, so a `BITMAP` column would be a bitmap inside a bitmap. Declare `SET` \
                 for the column you would have built one from: `count(DISTINCT x)` over it is the \
                 cardinality, and it is exact rather than a sketch"
            }
            Self::AlterKind => {
                "a field's kind decides how every fact in it was routed and its bit depth is \
                 how many bitmaps hold a value, so neither can change without rewriting every \
                 fact ever written to it. Add a second field, copy into it, and drop the first \
                 - which is three statements because it is three changes, not one hidden \
                 inside a `MODIFY` that would look free. To rebuild the whole table instead, \
                 create it beside this one and `EXCHANGE TABLES` them"
            }
            Self::Rename => {
                "a field's name is what routes a fact to a bitmap, and nothing below this \
                 renames one: a new name means a new field, the facts copied into it, and the \
                 old one dropped. A *table* is not reached by name below the catalog - it is \
                 reached by an interned id, which is why `ALTER TABLE t RENAME TO u` costs one \
                 record and is a statement this surface has"
            }
            Self::AlterEngine => {
                "the engine a table stores under is fixed when it is created: it decides what \
                 is written for every fact, and the facts already written were written under \
                 the old one. Create a second table under the engine you want, copy into it, \
                 then `EXCHANGE TABLES old AND new` - which puts it in place in one change and \
                 leaves the old one under the other name"
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
                "a scalar runs on a value that has been read, and a `WHERE` runs before any \
                 has been: it chooses a set out of bitmaps, so there is nothing here for one to \
                 apply to. The roundings are the exception, because they can be turned around \
                 rather than run - `date_trunc('month', ts) = '2024-01-01'` is every value in \
                 `[2024-01-01, 2024-02-01)`, which is a range off the bit planes - and \
                 `date_trunc`, `toDate` and `toYear` are answered here for that reason. This \
                 one cannot be turned around: `lower(country) = 'gb'` and `abs(balance) > 5` \
                 each have answers scattered across the column rather than gathered into a \
                 range. Write the comparison against the column itself"
            }
            Self::GroupExpression => {
                "a `GROUP BY` term is a column, or `date_trunc(<boundary>, <column>)` over a \
                 DATE or DATETIME column - those are the two the engine has a plan for. Any \
                 other expression over a column relabels its values without merging them, which \
                 answers one row per stored value with all of them printed under one name"
            }
            Self::Round => {
                "no rounding ever produces this value, so nothing could match it: \
                 `date_trunc('month', ts)` answers the first day of a month, and \
                 `'2024-01-15'` is not one. Compare against the boundary that contains it - \
                 `= '2024-01-01'` - or write the range itself, `ts >= '2024-01-15'`. Another \
                 engine answers this with no rows; it is refused here because there are no bind \
                 parameters in this dialect, so the value was typed rather than substituted, \
                 and an empty answer to a typo reads as a table with nothing in that month"
            }
            Self::ColumnType => {
                "a column takes one of `SET`, `MUTEX`, `BOOL`, `TIMEQUANTUM`, `SIGNED`, \
                 `UINT(bits)`, `DECIMAL(precision, scale)`, `FLOAT32`, `FLOAT64`, `DATE`, \
                 `DATETIME`, or a SQL spelling of one of those: `TEXT`, `VARCHAR`, `CHAR` and \
                 `STRING` are a set, `TINYINT`, `SMALLINT`, `INT`, `INTEGER` and `BIGINT` are \
                 an unsigned integer of 8, 16, 32, 32 and 64 bits, `BOOLEAN` is a bool, \
                 `FLOAT` and `REAL` are a `FLOAT32`, `DOUBLE` is a `FLOAT64`, and `TIMESTAMP` \
                 is a `DATETIME`. `LowCardinality(String)` is a set too - that is what a set \
                 already is here, one bitmap per interned key - but only over a string: a \
                 number is bit planes, with no dictionary to be low cardinality of. None of \
                 them takes a width in brackets that its name does not \
                 already carry: `FLOAT(10, 2)` is a `DECIMAL(10, 2)`, which keeps those digits \
                 exactly where a float would not. There is nothing here a blob or a JSON \
                 document lands in - a document is a `TEXT` column, and `JSONExtractString(c, \
                 'k')` reads a key out of one. `IPv4` is not on this list at all: it is read as \
                 the `UINT(32)` an address is. `UUID` and `IPv6` are, and the difference is what \
                 would be lost - both are 128 bits where a bit-sliced value stops at 64, so the \
                 only column they could land in is a keyed one, which answers `=` but not `<`. \
                 Declare them `TEXT` if equality is all you need, and know that is what you have"
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
            Self::CreateUser => {
                "people are not stored in this database. A credential is a line in the users \
                 file the server reads off its own disk, written with `big passwd`, and a route \
                 that could write one would let an `admin` password rewrite the password file \
                 over the network. What SQL administers is roles - `CREATE ROLE`, `GRANT`, \
                 `REVOKE` - and the users file says who holds one"
            }
            Self::AclPrivilege => {
                "the privileges are SELECT, INSERT, DELETE, CREATE, DROP, ALTER and ROLES, or \
                 ALL for every one grantable on the object named"
            }
            Self::AclObject => {
                "a privilege has to mean something where it is granted: ROLES is held on `*.*` \
                 or nowhere, because a role that could hand out privileges inside one database \
                 would still be handing out the power to hand them out; and CREATE is held on \
                 `*.*` or `db.*`, because a table that exists is not one there is anything left \
                 to create"
            }
            Self::AclColumns => {
                "grants here are per table. This surface answers questions over whole tables, \
                 so a privilege on some columns of one would be a fence that a slightly \
                 different question walks around - grant on the table, or keep the columns \
                 somebody may not read in a table of their own"
            }
            Self::RoleGrant => {
                "a role does not hold another role. What was written names a role where a \
                 privilege belongs: grant the privileges themselves, or give the person the \
                 other role in the users file. One name resolving to a set of others is a graph \
                 whose answer depends on how far it is walked"
            }
            Self::AclPublic => {
                "there is no role everybody holds. A privilege nobody was given and everybody \
                 has is one that no listing explains and no revoke reaches - make a role, grant \
                 it what it needs, and name it in the users file for whoever should hold it"
            }
            Self::GrantOption => {
                "a grant cannot be passed on. Delegation here is one privilege, ROLES on `*.*`, \
                 which is the power to administer every role - held or not, with nothing in \
                 between, because the levels between are what nobody can audit"
            }
            Self::ReservedRole => {
                "`superuser` holds everything and exists without being created. It is what a \
                 database whose catalog is empty is recovered through, so it cannot be made, \
                 dropped, or narrowed by a grant - name any other role"
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
