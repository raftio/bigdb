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

use crate::shape::Format;
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
    /// The table being joined in.
    pub source: Source,
    /// The column on the left-hand table.
    pub left: Name,
    /// The column on the right-hand table.
    pub right: Name,
    /// Byte offset, for the refusal.
    pub at: usize,
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
    /// `WHERE`, absent when every record is in play.
    pub filter: Option<Cond>,
    /// `GROUP BY`, at most one column.
    ///
    /// Also carries `SELECT DISTINCT c`, which standard SQL defines as `SELECT c GROUP BY c`
    /// and which this engine answers with the same plan. The parser normalises one into the
    /// other so the lowering has a single path to `Distinct`.
    pub group_by: Vec<Name>,
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
    /// The column name this entry produces.
    pub fn column(&self) -> String {
        match &self.alias {
            Some(a) => a.clone(),
            None => match &self.proj {
                // The record id, under the name an `INSERT` writes it by - they are the
                // same number, and `id` is left free for a field of that name.
                Proj::Star => crate::insert::RECORD_COLUMN.to_string(),
                Proj::Column(c) => c.column.clone(),
                Proj::Count | Proj::CountDistinct(_) => "count".to_string(),
                Proj::Agg { func, .. } => func.name().to_string(),
                Proj::Avg(_) => "avg".to_string(),
                Proj::TopKeys { .. } => "topK".to_string(),
                Proj::Quantile { .. } => "quantile".to_string(),
            },
        }
    }
}

/// What one select-list entry asks for.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Proj {
    /// `*`, which here means the record id and nothing else — see [`crate::Shape::Records`].
    Star,
    /// A bare column: the grouped column, or one of a projection's.
    Column(Name),
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

/// `HAVING <aggregate> <op> <value>`.
///
/// A predicate on the one number each group carries, and nothing else. It is not part of the
/// plan: groups are filtered where `count(DISTINCT x)` is counted, at the coordinator after
/// every node has contributed, because a group under the threshold on one node can be over it
/// once the rest have been added. Filtering per node would answer a different question,
/// quietly.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Having {
    /// Which number is being compared. It has to be the one the select list asked for: a
    /// grouped answer carries exactly one value per group, so a `HAVING` on any other
    /// aggregate would filter on a number the answer does not hold.
    pub agg: HavingAgg,
    /// One of `=`, `!=`, `<`, `<=`, `>`, `>=`, normalised by the lexer.
    pub op: &'static str,
    /// The value to compare against, exactly as written. A threshold on a decimal field is
    /// still in written units here; [`crate::Shape::resolve`] turns it into stored units with
    /// the same conversion a `WHERE` comparison gets.
    pub value: Literal,
    /// Byte offset, for the refusal.
    pub at: usize,
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
    /// `<column> BETWEEN <low> AND <high>`, inclusive at both ends.
    Between {
        /// The column.
        field: Name,
        /// The lower bound.
        low: Literal,
        /// The upper bound.
        high: Literal,
    },
}
