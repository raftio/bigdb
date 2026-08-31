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

//! How an answer becomes columns and rows.
//!
//! **This is where SQL keeps everything the plan does not carry**, and it is the reason no new
//! `Plan` variant is needed to add a SQL surface. A plan says what to compute; a shape says what
//! the caller asked to see of it — which columns, in which order, filtered how, cut to which
//! length.
//!
//! It matters that the split falls here rather than one layer down. `big-cluster` merges by the
//! plan, so a plan variant it has not been taught is a distributed answer that is *wrong* rather
//! than absent. A shape is applied to the merged answer, at the coordinator, once — so
//! `count(DISTINCT x)` counts the groups after every node has contributed to them, a `HAVING`
//! sees each group's whole count, and `ORDER BY sum(x) DESC` ranks totals rather than one node's
//! share of them. Every one of those is the only place the answer is right.
//!
//! The cost of that is stated rather than hidden: an ordering the plan cannot carry means every
//! group is materialised at the coordinator before the cut. `TopN` still carries its own ranking
//! when it can — see [`Shape::Groups`]'s `order`.

use big_plan::{FieldClass, Literal, PlanError, Schema};

/// A predicate on one of the numbers each group carries, applied to the merged answer.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Having {
    /// Which number is being compared, named the way a cell names one.
    ///
    /// An [`Of`] rather than a plan index because a grouped answer can carry several numbers,
    /// and because the one function that reads a number out of a merged answer should be the
    /// one every clause uses. A `HAVING` on an [`Of::Ratio`] is refused before it gets here:
    /// an average is fractional and this comparison is not.
    pub of: Of,
    /// One of `=`, `!=`, `<`, `<=`, `>`, `>=`.
    pub op: &'static str,
    /// The number to compare against.
    pub value: Threshold,
}

/// A `HAVING` threshold, before and after it has met a schema.
///
/// Two variants rather than one number, because the conversion is not optional and a shape
/// that skipped it would compare a written `100.00` against a stored `10000` — off by a factor
/// of a hundred, with both numbers valid and nothing downstream able to notice. Making the
/// unconverted form its own variant is what turns "remember to call [`Shape::resolve`]" into
/// something the type says.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Threshold {
    /// As written, and not yet against a schema.
    ///
    /// `field` is the column the aggregate measures, which is what decides the scale. A
    /// `count(*)` threshold never takes this form: a count is in records, so it is already in
    /// the units the answer is in and the lowering writes it as [`Threshold::Units`].
    Written {
        /// The table the column belongs to. Carried rather than taken from the statement,
        /// because a join has two and only the lowering knows which side an aggregate measured.
        table: String,
        /// The aggregated column.
        field: String,
        /// The value exactly as the statement wrote it.
        value: Literal,
    },
    /// In the units the field stores, which is the unit the answer is in.
    Units(i128),
}

impl Having {
    /// Whether a group carrying `n` survives.
    ///
    /// `None` is a group with no number — a `min` or `max` over records that hold no value in
    /// the field. It fails every comparison rather than defaulting to zero: absent is not the
    /// same answer as zero, and this surface has no null for it to propagate through.
    pub fn keeps(&self, n: Option<i128>) -> bool {
        let Threshold::Units(want) = self.value else {
            // Unreachable through `translate` + `resolve`, and a shape can also be built by
            // hand. Dropping every group is the safe direction: it is visibly wrong, where
            // keeping every group would look like a `HAVING` that simply matched a lot.
            return false;
        };
        let Some(n) = n else { return false };
        match self.op {
            "=" => n == want,
            "!=" => n != want,
            "<" => n < want,
            "<=" => n <= want,
            ">" => n > want,
            ">=" => n >= want,
            // The lexer produces no other comparison, so this is unreachable rather than a
            // silent "keep everything".
            other => unreachable!("unexpected comparison in HAVING: {other}"),
        }
    }
}

/// How a result set is written out.
///
/// **Not part of the shape**, and the distinction is the point: a shape says what the answer
/// *is* - which columns, in which order, cut to what length - and this says how those bytes are
/// spelled. A client that asks for the same question in TSV and in JSON asked one question.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Format {
    /// `{"columns": [...], "rows": [[...]]}`, which is what this surface has always answered.
    #[default]
    Json,
    /// One row per line, cells separated by tabs, no header. ClickHouse's `TabSeparated`.
    Tsv,
    /// The same with a header line of column names. ClickHouse's `TabSeparatedWithNames`.
    TsvWithNames,
    /// Comma separated, quoted the way a spreadsheet expects.
    Csv,
    /// The same with a header line.
    CsvWithNames,
}

impl Format {
    /// The name a `FORMAT` clause writes, or `None` for one this surface does not have.
    pub fn of(name: &str) -> Option<Self> {
        Some(match () {
            _ if name.eq_ignore_ascii_case("JSON") => Self::Json,
            _ if name.eq_ignore_ascii_case("JSONCompact") => Self::Json,
            _ if name.eq_ignore_ascii_case("TSV") || name.eq_ignore_ascii_case("TabSeparated") => {
                Self::Tsv
            }
            _ if name.eq_ignore_ascii_case("TSVWithNames")
                || name.eq_ignore_ascii_case("TabSeparatedWithNames") =>
            {
                Self::TsvWithNames
            }
            _ if name.eq_ignore_ascii_case("CSV") => Self::Csv,
            _ if name.eq_ignore_ascii_case("CSVWithNames") => Self::CsvWithNames,
            _ => return None,
        })
    }

    /// The `Content-Type` a response carrying this format should declare.
    pub fn content_type(self) -> &'static str {
        match self {
            Self::Json => "application/json",
            Self::Tsv | Self::TsvWithNames => "text/tab-separated-values",
            Self::Csv | Self::CsvWithNames => "text/csv",
        }
    }
}

/// A statement's answer: what it is, and how it is written out.
///
/// The two are carried together because a caller needs both and neither belongs inside the
/// other - a `Shape` that knew about tab separators would be a shape that has an opinion about
/// HTTP.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Answer {
    /// What the answer is.
    pub shape: Shape,
    /// How to write it.
    pub format: Format,
    /// How many of the answers a caller collects are the statement's calls'.
    ///
    /// The searches' answers follow them, so this is where [`Of::Probe`]'s index starts. Here
    /// rather than folded into that index because the two lists are built separately and only
    /// meet when a caller runs them - a shape that had guessed the boundary would be a shape
    /// that knew how its caller collects answers.
    pub calls: usize,
}

/// How a list of rows is cut down, after the merge and after the ordering.
///
/// The three travel together because they are one decision applied in one order — skip, take,
/// then keep whatever ties with the last one taken — and because every shape that can hold a
/// list holds all three. Three loose fields per shape would be three chances to apply them in
/// a different order in each.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Cut {
    /// `OFFSET`, applied after the ordering.
    pub offset: Option<usize>,
    /// `LIMIT`. Absent for a `TopN` that carries its own cut — truncating twice would be
    /// truncating a ranking built to be exactly this long.
    pub limit: Option<usize>,
    /// `WITH TIES`: after the limit, keep every further row whose ordering value equals the
    /// last kept one's.
    ///
    /// Only meaningful under an ordering, which is what the parser checks: without one there is
    /// no value for a row to tie on.
    pub ties: bool,
}

impl Cut {
    /// Whether the plan may carry the cut itself.
    ///
    /// A `TopN` ranks and truncates in one pass, which is the cheap path — but only when
    /// nothing after the merge can still change which rows survive. An offset means the rows
    /// wanted begin further down than the limit describes, and `WITH TIES` means the answer is
    /// longer than the limit says.
    pub fn is_plain_limit(&self) -> bool {
        self.offset.is_none() && !self.ties
    }
}

/// Which half of a group an ordering sorts on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OrderBy {
    /// The group's key.
    Key,
    /// One of the numbers the group carries — its count, or an aggregate that replaced it.
    Value {
        /// Which number, named the way a cell names one.
        of: Of,
    },
}

/// `ORDER BY` over a list of groups, applied after the merge.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct GroupOrder {
    /// Which half.
    pub by: OrderBy,
    /// `DESC` was written.
    pub desc: bool,
}

/// One column of an answer, and where its number comes from.
///
/// **A cell names a plan by index**, into the statement's [`crate::Statement::calls`]. That
/// indirection is what lets one statement ask several questions: `SELECT count(*), sum(amount)`
/// is two plans, fanned out and merged independently, and re-joined here into one row. Nothing
/// below this line knows they were written together.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Cell {
    /// The column name, which is the alias when one was written.
    pub column: String,
    /// Which number goes in it.
    pub of: Of,
}

/// What a cell reads.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Of {
    /// The whole answer of one plan: a count, a sum, an extreme.
    Value {
        /// Index into the statement's calls.
        plan: usize,
    },
    /// How many groups one plan produced.
    ///
    /// `count(DISTINCT x)`. The plan behind it is a `Distinct`, so this is the counting step,
    /// and it happens after the merge for the reason in this module's header.
    Groups {
        /// Index into the statement's calls.
        plan: usize,
    },
    /// One plan's answer divided by another's.
    ///
    /// `avg(f)`, which is `sum(f) / count(*)` and is not a plan this engine has. Two plans and
    /// a division is what every engine does with an average; doing it here rather than in the
    /// executor means the division happens after the merge, which is the only place both
    /// halves are whole.
    Ratio {
        /// The numerator's plan.
        plan: usize,
        /// The denominator's plan.
        over: usize,
    },
    /// The group's key, or a pair's left half.
    Key,
    /// A pair's right half. Only meaningful inside [`Shape::Pairs`].
    RightKey,
    /// The answer of one of the statement's searches, which is not a plan at all.
    ///
    /// Indexed into [`crate::Statement::probes`] rather than into its calls, because the two
    /// are different work: a call is asked once and a probe is asked until it converges.
    Probe {
        /// Index into the statement's probes.
        probe: usize,
    },
    /// The group's own number, from one of the statement's grouped plans.
    ///
    /// Several of these in one shape is a grouped answer with more than one aggregate, joined
    /// on the group's row id after every plan has been merged.
    Group {
        /// Index into the statement's calls.
        plan: usize,
        /// What this cell holds for a group that plan said nothing about.
        absent: Absent,
    },
    /// Two per-key numbers, one from each side of a join, made into one.
    ///
    /// Which side is which does not matter for a product and does for an extreme, so `left` is
    /// always the plan carrying the number and `right` the one that only says whether the other
    /// side holds the key at all.
    Paired {
        /// The plan whose per-key number this cell is about.
        left: usize,
        /// The plan the other side of the join is counted by.
        right: usize,
        /// How the two become one.
        how: Pairing,
    },
    /// The keys one plan produced, as a list in a single cell.
    ///
    /// `topK(n)(x)`. The plan is the `TopN` a ranking already had, so what makes this a
    /// different answer is only how it is rendered: a list in one cell rather than one row per
    /// key. ClickHouse's `topK` returns an array and this matches it - and is exact where that
    /// one is approximate, because the ranking here is a ranking rather than a sketch.
    Keys {
        /// Index into the statement's calls.
        plan: usize,
    },
    /// How many keys both sides of a join hold — `count(DISTINCT <the join key>)`.
    SharedKeys {
        /// The left side's grouped count.
        left: usize,
        /// The right side's.
        right: usize,
    },
}

/// What a cell holds when its plan produced no number for the group.
///
/// **This only arises under a `FILTER`**, which is the one clause that leaves the statement's
/// plans describing different groups: a group every record of which the filter rejects is
/// missing from that plan's answer entirely, while the others still know it exists.
///
/// The two answers are chosen so that a group the filter emptied reads exactly as it would if
/// the plan had run over its records and found none. That is `0` for a count and `0` for a sum
/// — this engine has no absent total, and `SELECT sum(x) FROM t WHERE <nothing matches>` has
/// always answered `0` — and absent for a `min` or a `max`, which is the `null` those already
/// answer over nothing. Standard SQL would make the sum `null` too; matching the rest of this
/// engine is worth more than matching that, because a client reading two spellings of the same
/// nothing should not get two answers.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Absent {
    /// Zero: a count, or a sum.
    Zero,
    /// Nothing: an extreme over no values.
    Null,
}

/// How a join turns one key's two numbers into the cell's number.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Pairing {
    /// The two multiplied, and summed over every shared key when the answer is one row.
    ///
    /// This is what a row count of an equi-join *is*: every record on one side pairs with every
    /// record on the other under the same key. `sum(a.x)` is the same arithmetic with the sum
    /// standing in for the count on its side.
    Product,
    /// The left number alone, for keys the right side holds at all; the smallest of them when
    /// the answer is one row.
    ///
    /// A `min` over a join is not scaled by the other side: repeating a value does not make it
    /// smaller. What the other side decides is only whether the key is in the join.
    Least,
    /// The same, largest.
    Greatest,
}

impl Of {
    /// Every plan this cell reads.
    ///
    /// **A grouped answer is driven by the union of the groups its plans produced**, and with
    /// `UNION ALL` a statement's flat list holds other branches' plans too - so which plans a
    /// shape may look at has to come from the shape rather than from the length of the list.
    pub fn plans(self) -> Vec<usize> {
        match self {
            Self::Key | Self::RightKey | Self::Probe { .. } => Vec::new(),
            Self::Value { plan }
            | Self::Groups { plan }
            | Self::Keys { plan }
            | Self::Group { plan, .. } => vec![plan],
            Self::Ratio { plan, over } => vec![plan, over],
            Self::Paired { left, right, .. } | Self::SharedKeys { left, right } => {
                vec![left, right]
            }
        }
    }

    /// The plan a cell reads first, for a caller checking a shape against a set of plans.
    pub fn plan(self) -> Option<usize> {
        match self {
            Self::Value { plan }
            | Self::Groups { plan }
            | Self::Keys { plan }
            | Self::Group { plan, .. } => Some(plan),
            Self::Ratio { plan, .. } | Self::Paired { left: plan, .. } => Some(plan),
            Self::SharedKeys { left, .. } => Some(left),
            Self::Key | Self::RightKey | Self::Probe { .. } => None,
        }
    }
}

/// What to make of the value a plan produced.
///
/// Not parameterised by the executor's `Value` type, and it could not be: this crate links no
/// storage. A shape describes the answer without naming it, and the layer that holds both
/// applies one to the other.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Shape {
    /// One row, one cell per select-list entry.
    ///
    /// A single count is the one-cell case, and `SELECT count(*), sum(amount), avg(amount)` is
    /// the reason this is a list: each cell reads its own plan, and the row is assembled once
    /// every one of them has been merged.
    Row {
        /// The cells, in the order the select list wrote them.
        cells: Vec<Cell>,
    },
    /// Record ids, one per row.
    ///
    /// The one shape that is not what SQL would give elsewhere, and it is deliberate: this
    /// engine stores facts as bits at `(row, record)` and has no row of values to hand back, so
    /// `SELECT *` answers with the identity of each match and nothing else. Better a column
    /// visibly called `id` than a table of values reconstructed at a point read apiece.
    Records {
        /// The column name.
        column: String,
        /// `LIMIT`, applied after the merge.
        limit: Option<usize>,
    },
    /// The stored values of some columns, one row per record.
    ///
    /// The shape a projection answers with, and the only one whose cells are values a record
    /// holds rather than numbers about a set of them. There is nothing for a shape to do to it:
    /// the plan carried the columns and the cut, because a projection's cost is a point read
    /// per record per column and a cut applied afterwards would be a cut applied after paying
    /// for it. The names are here so that the header can be written without the plan.
    Table {
        /// The columns, in the order the select list wrote them, which is the order the cells
        /// come in.
        columns: Vec<String>,
    },
    /// Several answers, one after the other.
    ///
    /// `UNION ALL`. Nothing here is a set operation over records: each branch is a whole
    /// statement with its own plans, and stacking is done to the rendered rows. That is why it
    /// needed no engine change - and why every branch's plan indices point into the one flat
    /// list the statement carries, rebased when they were joined.
    Union {
        /// The branches, in the order written. The first names the columns.
        branches: Vec<Shape>,
    },
    /// One row per combination of two keyed columns that any record holds both of.
    ///
    /// `GROUP BY a, b`. **Not a composite key**, which this index never stored: for each value
    /// of the left column the records holding it are a set, and grouping *those* by the right
    /// column is the ordinary grouping the engine already does. The plan pays one pass over the
    /// right column per value of the left, which is why it carries that bound itself.
    Pairs {
        /// The plans whose pairs make the rows, in plan order.
        keys: Vec<usize>,
        /// The columns, in the order written. One is [`Of::Key`] and one [`Of::RightKey`].
        cells: Vec<Cell>,
        /// `HAVING`, applied per pair after the merge.
        having: Option<Having>,
        /// `ORDER BY`, applied to the pairs.
        order: Option<GroupOrder>,
        /// `OFFSET`, `LIMIT` and `WITH TIES`, applied last.
        cut: Cut,
    },
    /// An equi-join between two tables on a keyed column.
    ///
    /// **What is joined is the key's string, and it is joined after both sides have been
    /// merged.** A record is a set of bits in one table and there is no pointer to another, so
    /// what two tables share is the string a keyed column was interned from. The row ids behind
    /// those strings are per `(table, field)` and have no reason to agree, which is why the
    /// join cannot happen in a plan and is not one.
    ///
    /// For each key `s` the join is the Cartesian product of the records holding `s` on each
    /// side, so `count(*)` is `|A_s| · |B_s|` summed over every shared key, `sum(a.x)` is
    /// `sum_A(x) · |B_s|` summed the same way, and a `min` is the smallest of one side's
    /// minima over the keys the other side holds at all. Every one of those is arithmetic over
    /// per-key numbers that an ordinary single-table grouping already produces — which is why a
    /// join adds no `Plan` variant and no merge arm.
    Join {
        /// The two plans whose keys *are* the join: one grouped count per side, left then
        /// right. A key both of them hold is a row of the join; a key only one holds is not.
        keys: (usize, usize),
        /// The columns, in the order written.
        cells: Vec<Cell>,
        /// `GROUP BY` the join key: one row per shared key. Without it the answer is one row,
        /// folding every shared key — summed for a product, and the extreme for an extreme.
        per_key: bool,
        /// `HAVING`, applied per key before the ordering and the cut.
        having: Option<Having>,
        /// `ORDER BY`, applied to the keys.
        order: Option<GroupOrder>,
        /// `OFFSET`, `LIMIT` and `WITH TIES`, applied last and in that order.
        cut: Cut,
    },
    /// One row per group, in the columns the select list asked for.
    ///
    /// With more than one aggregate the cells read different plans, and the rows are joined on
    /// the group's row id after each plan has been merged on its own. Joining on the row rather
    /// than on the key is what makes the join independent of which node answered first: a row
    /// id is assigned once, cluster-wide, and a key a node has not been told is `null`.
    Groups {
        /// The plans whose groups make the rows, in plan order.
        ///
        /// **Explicit rather than "every plan the statement made".** A keys-only grouping names
        /// no plan from any cell - `SELECT DISTINCT c` renders the key and nothing else - and a
        /// `UNION ALL` puts other branches' plans in the same list. Which plans define the rows
        /// is a fact about this shape, so it is recorded here rather than guessed at from the
        /// length of the answers.
        keys: Vec<usize>,
        /// The columns, in the order written. Exactly one is [`Of::Key`].
        cells: Vec<Cell>,
        /// `HAVING`, applied after the merge and **before** the ordering and the limit, which
        /// is the order SQL specifies and the only order that is right here: a group under the
        /// threshold on one node can be over it once every node has contributed, so filtering
        /// earlier would answer a different question.
        having: Option<Having>,
        /// `ORDER BY`, applied after the merge.
        ///
        /// Absent when the answer already arrives in the order asked for — ascending by key,
        /// which is how the executor labels groups, and the descending ranking a `TopN` plan
        /// carries itself. Present only when the coordinator has to sort, which is also when
        /// every group has to be materialised before the cut. That cost is the reason this
        /// stays `None` wherever the plan can do the work instead.
        order: Option<GroupOrder>,
        /// `OFFSET`, `LIMIT` and `WITH TIES`, applied last and in that order. A `TopN` under a
        /// `HAVING` keeps its limit here rather than in the plan, because the plan cannot cut
        /// to `n` before the rows that fail the predicate have been dropped.
        cut: Cut,
    },
}

impl Shape {
    /// The column names, in order, for a caller rendering a header.
    pub fn columns(&self) -> Vec<&str> {
        match self {
            // SQL names a union's columns after its first branch, whatever the others called
            // theirs.
            Self::Union { branches } => branches.first().map(Shape::columns).unwrap_or_default(),
            Self::Pairs { cells, .. } => cells.iter().map(|c| c.column.as_str()).collect(),
            Self::Records { column, .. } => vec![column.as_str()],
            Self::Table { columns } => columns.iter().map(String::as_str).collect(),
            Self::Row { cells } | Self::Groups { cells, .. } | Self::Join { cells, .. } => {
                cells.iter().map(|c| c.column.as_str()).collect()
            }
        }
    }

    /// Turns every written threshold into the units the answer is in.
    ///
    /// **The one part of a shape that needs a schema**, and it is split out for the reason the
    /// rest of this crate is schema-free: [`crate::translate`] stays a pure function of the
    /// text, testable at parser speed, and the single step that has to ask what a field is
    /// happens in the same place and at the same time as the planning that already does.
    ///
    /// The conversion itself is [`big_plan::to_units`] — the planner's own, not a copy. A
    /// `HAVING sum(price) >= 100.00` means what `WHERE price >= 100.00` means, because it is
    /// the same code deciding.
    pub fn resolve(self, schema: &impl Schema) -> Result<Self, PlanError> {
        let one = |h: Option<Having>| -> Result<Option<Having>, PlanError> {
            match h {
                None => Ok(None),
                Some(h) => Ok(Some(Having {
                    of: h.of,
                    op: h.op,
                    value: resolve_threshold(schema, h.value)?,
                })),
            }
        };
        Ok(match self {
            Self::Groups { keys, cells, having, order, cut } => {
                Self::Groups { keys, cells, having: one(having)?, order, cut }
            }
            Self::Pairs { keys, cells, having, order, cut } => {
                Self::Pairs { keys, cells, having: one(having)?, order, cut }
            }
            Self::Join { keys, cells, per_key, having, order, cut } => {
                Self::Join { keys, cells, per_key, having: one(having)?, order, cut }
            }
            Self::Union { branches } => Self::Union {
                branches: branches
                    .into_iter()
                    .map(|b| b.resolve(schema))
                    .collect::<Result<Vec<_>, _>>()?,
            },
            other => other,
        })
    }
}

impl Shape {
    /// The same shape, with every plan index moved along by `by`.
    ///
    /// **What makes a union cost no engine change.** Each branch is lowered on its own, so its
    /// cells count plans from zero; the statement carries one flat list, so the branches after
    /// the first have to be told where theirs begin. Done once, here, rather than by every
    /// reader having to know which branch a cell came from.
    /// The same shape, with every probe index moved along by `by`.
    ///
    /// Its own walk rather than a second argument to [`Shape::rebase`], because probes and
    /// calls are separate lists and a shape rebased onto one has not been rebased onto the
    /// other. Two calls that each say what they move keeps that visible.
    pub fn rebase_probes(self, by: usize) -> Self {
        if by == 0 {
            return self;
        }
        let cells = |cells: Vec<Cell>| -> Vec<Cell> {
            cells
                .into_iter()
                .map(|c| Cell {
                    column: c.column,
                    of: match c.of {
                        Of::Probe { probe } => Of::Probe { probe: probe + by },
                        other => other,
                    },
                })
                .collect()
        };
        match self {
            Self::Row { cells: c } => Self::Row { cells: cells(c) },
            Self::Union { branches } => Self::Union {
                branches: branches.into_iter().map(|b| b.rebase_probes(by)).collect(),
            },
            // No other shape can hold a probe: a quantile is one number over the whole
            // filtered set, and the lowering refuses one per group.
            other => other,
        }
    }

    pub fn rebase(self, by: usize) -> Self {
        if by == 0 {
            return self;
        }
        let cells = |cells: Vec<Cell>| -> Vec<Cell> {
            cells.into_iter().map(|c| Cell { column: c.column, of: c.of.rebase(by) }).collect()
        };
        let having =
            |h: Option<Having>| h.map(|h| Having { of: h.of.rebase(by), op: h.op, value: h.value });
        let order = |o: Option<GroupOrder>| {
            o.map(|o| GroupOrder {
                by: match o.by {
                    OrderBy::Key => OrderBy::Key,
                    OrderBy::Value { of } => OrderBy::Value { of: of.rebase(by) },
                },
                desc: o.desc,
            })
        };
        match self {
            Self::Row { cells: c } => Self::Row { cells: cells(c) },
            Self::Pairs { keys, cells: c, having: h, order: o, cut } => Self::Pairs {
                keys: keys.into_iter().map(|k| k + by).collect(),
                cells: cells(c),
                having: having(h),
                order: order(o),
                cut,
            },
            Self::Groups { keys, cells: c, having: h, order: o, cut } => Self::Groups {
                keys: keys.into_iter().map(|k| k + by).collect(),
                cells: cells(c),
                having: having(h),
                order: order(o),
                cut,
            },
            Self::Join { keys, cells: c, per_key, having: h, order: o, cut } => Self::Join {
                keys: (keys.0 + by, keys.1 + by),
                cells: cells(c),
                per_key,
                having: having(h),
                order: order(o),
                cut,
            },
            Self::Union { branches } => {
                Self::Union { branches: branches.into_iter().map(|b| b.rebase(by)).collect() }
            }
            // Neither names a plan: both read the statement's first, which a branch of a union
            // never is unless it is the first branch, where `by` is zero.
            other @ (Self::Records { .. } | Self::Table { .. }) => other,
        }
    }
}

impl Of {
    /// The same cell, reading a plan `by` further along the statement's list.
    fn rebase(self, by: usize) -> Self {
        match self {
            Self::Value { plan } => Self::Value { plan: plan + by },
            Self::Groups { plan } => Self::Groups { plan: plan + by },
            Self::Keys { plan } => Self::Keys { plan: plan + by },
            Self::Group { plan, absent } => Self::Group { plan: plan + by, absent },
            Self::Ratio { plan, over } => Self::Ratio { plan: plan + by, over: over + by },
            Self::Paired { left, right, how } => {
                Self::Paired { left: left + by, right: right + by, how }
            }
            Self::SharedKeys { left, right } => {
                Self::SharedKeys { left: left + by, right: right + by }
            }
            Self::Key => Self::Key,
            Self::RightKey => Self::RightKey,
            // A probe is not a plan, and its index moves with the probes rather than the calls.
            Self::Probe { probe } => Self::Probe { probe },
        }
    }
}

/// A written threshold in the units its field stores.
///
/// The field's class is what turns `100.00` into `10000` on a decimal with two places, and
/// what refuses a bound a field cannot hold.
fn resolve_threshold(schema: &impl Schema, t: Threshold) -> Result<Threshold, PlanError> {
    let Threshold::Written { table, field, value } = t else { return Ok(t) };

    let class = schema
        .field_class(&table, &field)
        .ok_or_else(|| PlanError::UnknownField { table: table.clone(), field: field.clone() })?;

    match (class, &value) {
        (FieldClass::Integer { scale }, Literal::Int(_) | Literal::Dec { .. }) => {
            Ok(Threshold::Units(i128::from(big_plan::to_units(&field, &value, scale)?)))
        }
        (FieldClass::Signed, Literal::Int(n)) => Ok(Threshold::Units(i128::from(*n))),
        (FieldClass::Signed, Literal::Sint(n)) => Ok(Threshold::Units(i128::from(*n))),
        // A `HAVING` on a keyed or boolean field never reaches here: only `sum`, `min` and
        // `max` carry a field, and the planner refuses all three on those classes before this
        // runs. The arm exists so a hand-built shape gets an error rather than a wrong answer.
        (class, _) => Err(PlanError::OperatorNotAllowed {
            field,
            op: "HAVING".to_string(),
            class: match class {
                FieldClass::Integer { .. } => "an integer field",
                FieldClass::Signed => "a signed integer field",
                FieldClass::Keyed(_) => "a keyed field",
                FieldClass::Boolean => "a boolean field",
            },
        }),
    }
}
