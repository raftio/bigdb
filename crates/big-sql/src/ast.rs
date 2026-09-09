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

//! The shape of an accepted statement, before it is asked whether it means anything.
//!
//! Nothing here has met a schema, and nothing here has been checked for whether the engine can
//! answer it. Both happen in [`mod@crate::lower`], which keeps three failures apart that would
//! otherwise arrive as one: the text is not a statement, the statement is not one this engine
//! answers, and the statement names something that does not exist.

use crate::shape::{Format, WinFunc};
use big_plan::Literal;

/// A column, and the table it was written against.
///
/// The qualifier is kept rather than dropped, because with a join in the statement it is the
/// only thing that says which table a column belongs to. It is **not** resolved here: the
/// select list is parsed before `FROM`, so the aliases it refers to are not known yet. Which
/// table a name means is decided in [`mod@crate::lower`], where both are.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Name {
    /// `a` in `a.amount`, which is a table name or an alias for one.
    pub qualifier: Option<String>,
    /// The column.
    pub column: String,
}

impl Name {
    /// An unqualified name, which is every name in a statement with one table.
    pub fn bare(column: impl Into<String>) -> Self {
        Self { qualifier: None, column: column.into() }
    }
}

impl core::fmt::Display for Name {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match &self.qualifier {
            Some(q) => write!(f, "{q}.{}", self.column),
            None => write!(f, "{}", self.column),
        }
    }
}

/// One table in `FROM`, under the name the rest of the statement calls it by.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Source {
    /// `sales` in `FROM sales.orders`. `None` means the request's default database.
    ///
    /// Two tables in one statement may name different databases: a join here pairs records
    /// through the *string* a keyed column was interned from, and a string is the same string
    /// whichever namespace the table holding it lives in. So a cross-database join costs
    /// exactly what a same-database one costs, and there is nothing to forbid.
    pub database: Option<String>,
    /// The table.
    pub table: String,
    /// `AS x`, when one was written.
    pub alias: Option<String>,
}

impl Source {
    /// What a qualifier has to say to mean this table: its alias if it has one, its name if
    /// not. An alias **replaces** the name, which is what SQL says and what keeps
    /// `FROM tx a JOIN tx b` from being ambiguous.
    pub fn label(&self) -> &str {
        self.alias.as_deref().unwrap_or(&self.table)
    }

    /// The name the plan carries: `database.table`, or the bare table when the statement did
    /// not qualify it.
    ///
    /// A bare name is left bare rather than filled in with `default`, because which database it
    /// means is not this crate's to know - it is the request's, and
    /// [`crate::qualify`] applies it one layer up where that is in scope.
    pub fn qualified(&self) -> String {
        match &self.database {
            Some(d) => format!("{d}.{}", self.table),
            None => self.table.clone(),
        }
    }
}

/// `JOIN <table> ON <left> = <right>`.
///
/// One equality between two keyed columns, and nothing else. **That is not a subset of joins
/// chosen for convenience — it is the join this engine can answer exactly.** A record is a set
/// of bits in one table and there is no pointer to another, so what two tables share is the
/// *string* a keyed column was interned from. For each such string the join is the Cartesian
/// product of the records holding it on each side, and every aggregate over that product is a
/// product of per-key numbers both sides already produce. See [`crate::Shape::Join`].
///
/// Several of these are a **star**: every table grouped by the one key they all share. A table
/// that would need a second key column is a chain, and is refused - grouping one table by two
/// columns at once is a pass over the second per value of the first.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Join {
    /// Which sides a row of this join has to have a partner on. See [`JoinKind`].
    pub kind: JoinKind,
    /// The table being joined in.
    pub source: Source,
    /// The column on the left-hand table.
    pub left: Name,
    /// The column on the right-hand table.
    pub right: Name,
    /// Byte offset, for the refusal.
    pub at: usize,
}

/// Which sides of a join a key has to be held by for it to be a row.
///
/// **An outer join here is not a row with half of it nulled out - it is a side that stops
/// removing keys from the space.** For each key, the join is the Cartesian product of the
/// records each side holds under it; a side that need not match contributes exactly one
/// null-filled partner where it holds none, and one is the identity of that product. So
/// `count(*)` over a `LEFT JOIN` is `Σ_s |A_s| · max(|B_s|, 1)` over the keys `A` holds, which
/// is the same arithmetic the inner join already was.
///
/// That is why this is one word per `JOIN` and not a second code path: it decides
/// [`crate::JoinSide::required`], and nothing else.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum JoinKind {
    /// Every side has to hold the key. The intersection.
    Inner,
    /// The table being joined in need not hold it.
    Left,
    /// Only the table being joined in has to hold it.
    Right,
    /// No side has to hold it: the space is every key any side holds.
    Full,
}

/// One accepted statement: a `SELECT`, or several stacked by `UNION ALL`.
///
/// **Only `UNION ALL`.** A plain `UNION` removes duplicate rows, and a row here is a rendered
/// answer rather than a stored tuple - there is nothing to compare two of them by that would
/// not be comparing strings. Refused by name rather than answered as `ALL`, which would return
/// rows the statement asked to have removed.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Query {
    /// The branches, in the order written. One for an ordinary statement.
    pub branches: Vec<Select>,
}

/// Which half of an explanation `EXPLAIN` was asked for.
///
/// There are two printers because a statement means two things that move independently - the
/// plans its calls resolve to, and the shape its answer takes - and [`mod@crate::explain`] says
/// at length why keeping them apart is worth more than one combined dump. The default prints
/// both; the two halves are named for a reader who wants one of them to stop churning.
///
/// **Only a query has a shape.** A schema change, a write and a question about the catalog each
/// have exactly one printer, so naming a half of one is refused rather than ignored - see
/// [`crate::Refused::ExplainHalf`].
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum ExplainMode {
    /// `EXPLAIN <statement>`: everything there is to say about it.
    #[default]
    All,
    /// `EXPLAIN PLAN <select>`: the plan tree per call, and nothing about the answer.
    Plan,
    /// `EXPLAIN SHAPE <select>`: the answer's columns and clauses, and no plans.
    Shape,
}

/// One accepted `SELECT`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Select {
    /// The select list, in the order written — which is the order the columns come out in.
    pub items: Vec<Item>,
    /// The table named in `FROM`.
    pub from: Source,
    /// `JOIN`, one per table past the first, in the order written. Empty for a statement about
    /// one table.
    ///
    /// Every one of them has to key its table on the same column the others do: what makes
    /// several joins answerable is that they are one star around one shared key.
    pub joins: Vec<Join>,
    /// `SAMPLE`: one record in every `n`, absent when the statement asked for all of them.
    ///
    /// **Held as the stride rather than as the fraction that was written**, because the stride
    /// is what the engine does: `SAMPLE 0.1` and `SAMPLE 1/10` are one number by the time
    /// anything reads this, so nothing downstream has to know which was typed.
    pub sample: Option<u32>,
    /// `WHERE`, absent when every record is in play.
    pub filter: Option<Cond>,
    /// `GROUP BY`: the columns, and the boundary each is rounded to.
    ///
    /// Also carries `SELECT DISTINCT c`, which standard SQL defines as `SELECT c GROUP BY c`
    /// and which this engine answers with the same plan. The parser normalises one into the
    /// other so the lowering has a single path to `Distinct`.
    pub group_by: Vec<Grouping>,
    /// Which combinations of [`Select::group_by`] the answer has a row for.
    ///
    /// **Normalised where it is written**, into positions in `group_by`: `WITH ROLLUP`, `WITH
    /// CUBE` and `GROUPING SETS ((a,b),(a),())` are three spellings of a list somebody could
    /// have written out by hand, and the lowering should not have to know which one was typed.
    /// The same decision `SELECT DISTINCT` is normalised by, for the same reason - one path to
    /// the plans instead of three that have to agree.
    ///
    /// `None` for a plain `GROUP BY`, which is the single set naming every column and is left
    /// unwritten so that nothing existing changes shape.
    pub grouping_sets: Option<GroupingSets>,
    /// `HAVING count(*) <op> <n>`, absent when every group is kept.
    pub having: Option<Having>,
    /// `ORDER BY`, at most one key.
    pub order_by: Option<Order>,
    /// `LIMIT`.
    pub limit: Option<u64>,
    /// `WITH TIES` after `LIMIT`: keep every row whose ordering value equals the last one's.
    ///
    /// Only means anything under an `ORDER BY` - without one there is no value for a row to tie
    /// on, and the parser refuses the combination rather than treating it as a plain limit.
    pub with_ties: bool,
    /// `OFFSET`, legal only where the answer is a list of groups.
    ///
    /// A record listing is paged with the `after` cursor, which is stable while a skip count
    /// is not: records inserted under the cursor do not shift the page, and records inserted
    /// under an offset do. Groups have no cursor, so an offset is the only paging they can
    /// have, and it is applied to a list this surface has already materialised in full.
    pub offset: Option<u64>,
    /// `FORMAT`, which says how the answer is written rather than what it is.
    pub format: Format,
}

/// A window function and the rows it sees: `row_number() OVER (PARTITION BY a ORDER BY b)`.
///
/// **Pre-schema, so the columns are names.** The lowering turns each into a position in the list
/// of fields the projection reads, which is what makes "a window may only read a column the
/// projection reads" a fact about the type rather than a check somebody has to remember - see
/// [`crate::Selection::Over`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Over {
    /// Which function.
    pub func: WinFunc,
    /// The column it reads. `None` for the ranking family, and for `count(*)`.
    pub arg: Option<Name>,
    /// `lag(x, 2)`, `nth_value(x, 2)`, `ntile(4)`. One where none was written.
    pub offset: u32,
    /// `PARTITION BY`, empty for one partition of every row.
    pub partition: Vec<Name>,
    /// The window's own `ORDER BY`: the column and whether it descends, in the order written.
    pub order: Vec<(Name, bool)>,
    /// Byte offset of the entry, for the refusals it earns.
    pub at: usize,
}

/// One entry in the select list, with the name its column will carry.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Item {
    /// What is being selected.
    pub proj: Proj,
    /// `FILTER (WHERE ...)`, which narrows this aggregate and no other.
    ///
    /// Standard SQL, and the clause that lets one statement answer several segments at once:
    /// `count(*), count(*) FILTER (WHERE amount >= 500)` is a total and a share of it, over one
    /// pass of the same `WHERE`. Here it is simply a second condition intersected into this
    /// entry's own row set, which is why it costs a plan rather than an evaluator.
    pub filter: Option<Cond>,
    /// `AS <name>`, which overrides the default column name and can be used by `ORDER BY`.
    pub alias: Option<String>,
    /// Byte offset, so a refusal can point at the entry that caused it.
    pub at: usize,
}

impl Item {
    /// What this entry *plans*, with any expression around it seen through.
    ///
    /// **Every site that decides a plan reads this rather than `proj`.** A scalar reads nothing
    /// its leaf did not read, so `round(sum(amount), 2)` and `sum(amount)` plan identically -
    /// and a lowering that matched on `proj` would have to repeat itself once per arm to say
    /// so. The expression is picked up separately by [`Item::apply`], which is the only other
    /// half there is.
    pub fn leaf(&self) -> &Proj {
        match &self.proj {
            Proj::Scalar { inner, .. } => inner,
            other => other,
        }
    }

    /// The expression applied to this entry's number on the way out, if there is one.
    pub fn apply(&self) -> Option<&crate::Scalar> {
        match &self.proj {
            Proj::Scalar { expr, .. } => Some(expr),
            _ => None,
        }
    }

    /// The column name this entry produces.
    pub fn column(&self) -> String {
        match &self.alias {
            Some(a) => a.clone(),
            None => match &self.proj {
                // The record id, under the name an `INSERT` writes it by - they are the
                // same number, and `id` is left free for a field of that name.
                Proj::Star => crate::insert::RECORD_COLUMN.to_string(),
                Proj::Column(c) => c.column.clone(),
                // The expression as written, which is what ClickHouse and Postgres both name
                // such a column. Sliced from the statement rather than printed back from the
                // tree, so that there is no second dialect to keep in step with this one.
                Proj::Scalar { written, .. } => written.clone(),
                Proj::Count | Proj::CountDistinct(_) => "count".to_string(),
                Proj::Agg { func, .. } => func.name().to_string(),
                Proj::Avg(_) => "avg".to_string(),
                Proj::TopKeys { .. } => "topK".to_string(),
                Proj::Quantile { .. } => "quantile".to_string(),
                Proj::Now { .. } => "now()".to_string(),
                // The function's name, which is how ClickHouse and Postgres both name an
                // unaliased window column.
                Proj::Window(w) => w.func.name().to_string(),
            },
        }
    }
}

/// What one select-list entry asks for.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Proj {
    /// A window function, which is a number about this row rather than about the whole set.
    ///
    /// Its own variant rather than an aggregate wearing a clause, because it lowers nowhere near
    /// one: an aggregate is a plan and this is arithmetic over the rows a projection already
    /// read. `lower_one` buckets it on its own for exactly that reason, and the grouped paths
    /// refuse it by name.
    Window(Box<Over>),
    /// `*`: every column the table declares — see [`crate::Columns::All`], which is where the
    /// list is filled in, because nothing at this level knows the table.
    Star,
    /// A bare column: the grouped column, or one of a projection's.
    Column(Name),
    /// `now()` - the instant the statement was read.
    ///
    /// Carries the moment rather than a marker, because **one statement has one now**. Reading
    /// the clock again at the coordinator would let `SELECT now() FROM t WHERE ts < now()`
    /// compare against two different instants, and reading it per node would let two shards
    /// disagree about which records match. It is taken once, where the statement is parsed.
    Now {
        /// Seconds since the Unix epoch.
        unix_seconds: i64,
    },
    /// Arithmetic or a function call over the one number this entry is about.
    ///
    /// **The plan is `inner`'s, unchanged.** A scalar reads nothing the projection or the
    /// aggregate underneath it did not already read - it is applied to the number on its way
    /// into a cell, where a decimal has its point put back. So `round(avg(amount), 2)` is the
    /// `avg` plan and a rounding, and `round(amount, 2)` is the projection and the same
    /// rounding. See [`mod@crate::scalar`], which says at length why that is the only place an
    /// expression can live in an engine with no rows.
    Scalar {
        /// What the leaf was: a column, an aggregate, a count. Planned exactly as it would be
        /// on its own.
        inner: Box<Proj>,
        /// The expression, with [`crate::Scalar::Value`] standing for the leaf's number.
        expr: crate::Scalar,
        /// The entry as it was written, which is the column name when no `AS` was given.
        written: String,
    },
    /// `count(*)`.
    Count,
    /// `count(DISTINCT <column>)`.
    CountDistinct(Name),
    /// `sum(<column>)`, `min(<column>)`, `max(<column>)`.
    Agg {
        /// Which one.
        func: Agg,
        /// The column it measures.
        field: Name,
    },
    /// `avg(<column>)`.
    ///
    /// Its own variant rather than a fourth [`Agg`], because it is the one aggregate that is
    /// not a plan: this engine has no average to compute, so it is a sum over a count, taken
    /// after both have been merged. Folding it into `Agg` would give it a `call()` that names
    /// a call the query language does not have.
    Avg(Name),
    /// `quantile(p)(<column>)` — the value `p` of the way through the sorted values.
    ///
    /// **Exact, where ClickHouse's is a sketch.** There is no plan for it: it is found by asking
    /// how many records hold a value at or below a bound and moving the bound until the count
    /// lands on the rank asked for. That is a search rather than a question, which is why it is
    /// a [`crate::Probe`] and not a call.
    Quantile {
        /// Which quantile, in parts per thousand. 500 is the median.
        per_mille: u32,
        /// The column.
        field: Name,
    },
    /// `topK(n)(<column>)` — the `n` most common values of a keyed column, as one cell holding
    /// a list of them.
    ///
    /// ClickHouse's `topK` is approximate and this one is exact: the plan behind it is the
    /// `TopN` the ranking already had, and what differs is only that the answer is rendered as
    /// a list in one cell rather than as one row per key.
    TopKeys {
        /// How many keys to rank.
        n: u64,
        /// The column ranked.
        field: Name,
    },
}

/// The three aggregates that are not a count.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Agg {
    /// `sum`
    Sum,
    /// `min`
    Min,
    /// `max`
    Max,
}

impl Agg {
    /// The call this becomes in the query language, which is also the default column name.
    pub fn name(self) -> &'static str {
        match self {
            Self::Sum => "sum",
            Self::Min => "min",
            Self::Max => "max",
        }
    }

    /// The same, capitalised the way the query language spells it.
    pub fn call(self) -> &'static str {
        match self {
            Self::Sum => "Sum",
            Self::Min => "Min",
            Self::Max => "Max",
        }
    }
}

/// A `HAVING` clause, as a tree of comparisons over the numbers a group carries.
///
/// A predicate on what each group holds, and not part of the plan: groups are filtered where
/// `count(DISTINCT x)` is counted, at the coordinator after every node has contributed, because
/// a group under the threshold on one node can be over it once the rest have been added.
/// Filtering per node would answer a different question, quietly.
///
/// # Why this is a tree and [`Cond`] is a different one
///
/// The same shape, deliberately, so the two read alike — and two types rather than one because
/// they select different things. A `Cond` names *columns* and becomes bitmap set operations
/// before anything runs; this names *aggregates* and is evaluated per group on numbers that
/// already exist. `WHERE` runs on the way in and `HAVING` on the way out, and giving them one
/// type would invite a term to be moved between them, which changes the answer.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Having {
    /// `a AND b`
    And(Box<Having>, Box<Having>),
    /// `a OR b`
    Or(Box<Having>, Box<Having>),
    /// `NOT a`
    Not(Box<Having>),
    /// `<operand> <op> <operand>`
    Cmp {
        /// The left-hand side.
        left: HavingOperand,
        /// One of `=`, `!=`, `<`, `<=`, `>`, `>=`, normalised by the lexer.
        op: &'static str,
        /// The right-hand side. An operand rather than a literal, which is what lets
        /// `HAVING sum(paid) > sum(due)` be written: both sides are numbers a group carries,
        /// and nothing about the comparison cares which side an aggregate is on.
        right: HavingOperand,
        /// Byte offset, for the refusal.
        at: usize,
    },
}

impl Having {
    /// Where this clause begins, for a refusal that has no more specific place to point.
    pub fn at(&self) -> usize {
        match self {
            Self::And(a, _) | Self::Or(a, _) => a.at(),
            Self::Not(a) => a.at(),
            Self::Cmp { at, .. } => *at,
        }
    }

    /// Every aggregate this clause names, in the order written.
    ///
    /// The list the lowering checks against what the answer carries — and, once a `HAVING` may
    /// name an aggregate the select list did not ask for, the list of numbers that still have
    /// to be computed.
    pub fn aggregates(&self) -> Vec<&HavingAgg> {
        let mut out = Vec::new();
        self.walk(&mut out);
        out
    }

    fn walk<'a>(&'a self, out: &mut Vec<&'a HavingAgg>) {
        match self {
            Self::And(a, b) | Self::Or(a, b) => {
                a.walk(out);
                b.walk(out);
            }
            Self::Not(a) => a.walk(out),
            Self::Cmp { left, right, .. } => {
                for side in [left, right] {
                    if let HavingOperand::Agg(a) = side {
                        out.push(a);
                    }
                }
            }
        }
    }
}

/// One side of a `HAVING` comparison.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum HavingOperand {
    /// A number the group carries.
    Agg(HavingAgg),
    /// A value to compare against, exactly as written. A threshold on a decimal field is still
    /// in written units here; [`crate::Shape::resolve`] turns it into stored units with the
    /// same conversion a `WHERE` comparison gets.
    Value(Literal),
}

/// Which of a grouped answer's numbers a `HAVING` names.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum HavingAgg {
    /// `count(*)`.
    Count,
    /// `sum(f)`, `min(f)`, `max(f)`.
    Agg {
        /// Which one.
        func: Agg,
        /// The column it measures.
        field: Name,
    },
    /// `avg(f)`, which the lowering refuses: an average is fractional and this surface's
    /// comparison is not. Carried so the refusal can say that rather than say "not an
    /// aggregate".
    Avg(Name),
}

/// `ORDER BY <key> [ASC|DESC]`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Order {
    /// What is being ordered by.
    pub key: OrderKey,
    /// `DESC` was written.
    pub desc: bool,
    /// Byte offset, for the refusal.
    pub at: usize,
}

/// What an `ORDER BY` names.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum OrderKey {
    /// `count(*)` written out.
    Count,
    /// `sum(f)`, `min(f)` or `max(f)` written out.
    ///
    /// Legal only when it is one of the aggregates the select list already asked for: a group
    /// carries the numbers that were selected, and ordering by another would mean computing it.
    Agg {
        /// Which one.
        func: Agg,
        /// The column it measures.
        field: Name,
    },
    /// `avg(f)` written out, under the same rule.
    Avg(Name),
    /// A name, which may be a column or an alias from the select list. Which one it is depends
    /// on the select list, so it is resolved in the lowering rather than here.
    Name(Name),
}

/// A `WHERE` clause, as a tree of set operations over one table.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Cond {
    /// `a AND b`
    And(Box<Cond>, Box<Cond>),
    /// `a OR b`
    Or(Box<Cond>, Box<Cond>),
    /// `NOT a`
    Not(Box<Cond>),
    /// `<column> <op> <value>`
    Cmp {
        /// The column.
        field: Name,
        /// One of `=`, `!=`, `<`, `<=`, `>`, `>=`, already normalised by the lexer.
        op: &'static str,
        /// The value, exactly as written.
        value: Literal,
    },
    /// `<column> IN (<value>, ...)`, and `NOT IN` as a [`Cond::Not`] around one.
    In {
        /// The column.
        field: Name,
        /// At least one value; an empty list is a syntax error rather than an empty set,
        /// because SQL does not accept one either.
        values: Vec<Literal>,
    },
    /// `SEGMENT(<view>)`: a named set of records of *this* table, used as a term.
    ///
    /// **A segment is a `WHERE` with a name, and composing two of them is one bitmap
    /// operation.** The view it names is over the same table, so nothing crosses between
    /// tables and no ids travel: the term is replaced by that view's own condition, and
    /// `SEGMENT(a) AND NOT SEGMENT(b)` becomes the `Difference` the lowering already emits for
    /// any other conjunction. Which is the whole point - a set that costs nothing to name, and
    /// whose intersection with another set is the operation this engine is built out of.
    ///
    /// Distinct from reading the view as a table. `FROM v` answers *the view's* statement, one
    /// per statement, and two of them cannot be combined; `SEGMENT(v)` takes only the records
    /// and leaves the question to the reader, which is what makes it composable.
    ///
    /// **Expanded before the lowering, in `big_embed::views`**, because it needs the catalog -
    /// the same place and the same depth bound a view read through `FROM` gets. Nothing below
    /// that layer ever sees one.
    Segment {
        /// The view named, as a source so it carries a database the way every other name does.
        view: Source,
        /// Byte offset, for the refusal.
        at: usize,
    },
    /// `<column> IN (SELECT _record_id FROM <table> [WHERE ...])`: the semi-join.
    ///
    /// **This is the one join shape a bitmap engine is actually built for.** The column holds
    /// record ids of the other table, so the inner statement is an ordinary set of records -
    /// the same `Rows` call any `WHERE` already produces - and the outer term is the union of
    /// the bitmaps whose value is one of them. Nothing pairs records and nothing is grouped:
    /// what crosses between the tables is a set of ids, and a set is what this engine merges.
    ///
    /// The inner statement is deliberately not a whole [`Select`]. `_record_id` is the only
    /// column it may name, because a record id is the only thing one table can hold about
    /// another's records, and there is nothing for a `GROUP BY` or a `LIMIT` in here to mean.
    ///
    /// `NOT IN` is a [`Cond::Not`] around one, exactly as it is for a list of values - which is
    /// the anti-join, and comes free.
    InRecords {
        /// The column holding the other table's record ids. A bit-sliced integer field.
        field: Name,
        /// The table the ids are records of.
        table: Source,
        /// The inner `WHERE`, or `None` for every record the table holds.
        filter: Option<Box<Cond>>,
        /// Byte offset, for the refusal.
        at: usize,
    },
    /// `<column> LIKE '<pattern>'`, and `ILIKE` for the folded one. `NOT LIKE` is a
    /// [`Cond::Not`] around one, exactly as `NOT IN` is.
    ///
    /// **A set operation like every other term here.** A keyed column interns each distinct
    /// string once, so this is the union of the bitmaps whose key matches - which is why it
    /// belongs beside `=` and `IN` rather than among the things a `WHERE` cannot do. See
    /// [`big_plan::Rows::KeyLike`].
    Like {
        /// The column.
        field: Name,
        /// The pattern, exactly as written: `%` for any run, `_` for one, `\` to escape either.
        pattern: String,
        /// `ILIKE` was written.
        fold: bool,
    },
    /// `<column> BETWEEN <low> AND <high>`, inclusive at both ends.
    Between {
        /// The column.
        field: Name,
        /// The lower bound.
        low: Literal,
        /// The upper bound.
        high: Literal,
    },
    /// `round(<column>, <digits>) <op> <value>`, and `floor`/`ceil`, in a `WHERE`.
    ///
    /// **Carried rather than rewritten, which is the opposite of what a rounded *date* does.**
    /// A date's bounds are two written dates, and a written date means the same thing to a
    /// column counting days and to one counting seconds - so the parser can compute them and
    /// this dialect keeps one rewrite for both. A number's bounds are not like that:
    /// `round(x, 2) > 5` is `x >= 5.01` on a `DECIMAL(10,2)`, `x >= 5.005` on a `DECIMAL(10,4)`,
    /// `x >= 6` on an `INT`, and plain `x > 5` on a `SIGNED`, where rounding to two digits is
    /// the identity. Every one of those is a different question, and which one it is depends on
    /// the field's scale - so the term travels as written and the arithmetic happens in
    /// `big_plan`, in the field's own units, where every bound comes out an exact integer.
    Rounded {
        /// The column.
        field: Name,
        /// Which rounding was written.
        round: Rounding,
        /// One of `=`, `!=`, `<`, `<=`, `>`, `>=`, already normalised by the lexer.
        op: &'static str,
        /// The value the rounded column is compared against, exactly as written.
        value: Literal,
    },
}

/// The sets a `ROLLUP`, `CUBE` or `GROUPING SETS` clause names.
///
/// Each entry is a subset of [`Select::group_by`], and a row of the answer belongs to exactly one
/// of them - so a statement carrying this is several groupings whose rows are stacked, which is
/// what lets it need no plan the engine did not already have.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct GroupingSets {
    /// One entry per set: positions in [`Select::group_by`], ascending and without repeats.
    ///
    /// **Longest first**, and lexicographic within a length. That is the answer's row order, so
    /// it is fixed where the clause is read rather than left to whatever order a set builder
    /// happened to produce - and it puts the branch where every key column is a real key first,
    /// which is the branch [`crate::Shape::columns`] names the answer after.
    pub of: Vec<Vec<usize>>,
    /// Byte offset of the clause, for the refusals it earns.
    pub at: usize,
}

/// One `GROUP BY` term.
///
/// **Not an expression, and the type is the refusal.** The engine has a plan for exactly two
/// terms - a column, and a calendar rounding of one - so those are the two this holds. Letting a
/// general expression in would push "which of these has a plan" down into the lowering, where it
/// would be a list somebody maintains rather than a shape the compiler already knows.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Grouping {
    /// The column being grouped.
    pub name: Name,
    /// The boundary its values are rounded to before they are grouped.
    ///
    /// `None` is the bare column, which is every grouping that existed before buckets did - and
    /// is why `GROUP BY a, b` needs no rewriting to keep working.
    pub bucket: Option<big_civil::Unit>,
    /// Byte offset, so a refusal can point at the term.
    pub at: usize,
}

/// A rounding of a number that a `WHERE` can be answered through.
///
/// All three are non-decreasing, which is the property that matters: the values that round into
/// one answer are contiguous, so the records behind it are a range rather than a scatter. `abs`
/// is not on this list for exactly that reason.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Rounding {
    /// `round(x)` and `round(x, digits)`: half away from zero, which is what a `DECIMAL` does
    /// everywhere else in this engine.
    Round {
        /// Digits kept after the point. `round(x)` is `round(x, 0)`.
        digits: u8,
    },
    /// `floor(x)`: the whole number at or below.
    Floor,
    /// `ceil(x)`: the whole number at or above.
    Ceil,
}

/// `DELETE FROM t WHERE ...` as it was written.
///
/// The lowered half is [`crate::delete::Delete`]. Here as an AST node rather than there for the
/// reason [`Select`] is here: it carries a `Cond` and byte offsets, neither of which survives
/// lowering, and both of which a refusal needs in order to point at the thing that was written.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Delete {
    pub database: Option<String>,
    pub table: String,
    /// **Never `None`.** `DELETE FROM t` with no `WHERE` is refused at parse time and pointed at
    /// `TRUNCATE TABLE`, so the type does not have to carry a case the parser cannot produce -
    /// which is also what stops a later edit from reading a missing filter as "every record".
    pub filter: Cond,
    /// Where the table name is, for the refusals raised once the set has been counted.
    pub at: usize,
}

/// `UPDATE t SET c = v WHERE ...` as it was written.
///
/// The lowered half is [`crate::update::Update`], and the split is [`Delete`]'s.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Update {
    pub database: Option<String>,
    pub table: String,
    /// The columns to write and the value each takes, in the order written.
    pub assignments: Vec<(String, big_plan::Literal)>,
    /// **Never `None`**, for the reason [`Delete::filter`] is never `None`.
    pub filter: Cond,
    /// Where the table name is, for the refusals raised once the set has been counted.
    pub at: usize,
}
