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

/// A predicate on the numbers each group carries, applied to the merged answer.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Having {
    /// `a AND b`
    And(Box<Having>, Box<Having>),
    /// `a OR b`
    Or(Box<Having>, Box<Having>),
    /// `NOT a`
    Not(Box<Having>),
    /// One comparison between two operands.
    Cmp {
        /// The left-hand side.
        left: Operand,
        /// One of `=`, `!=`, `<`, `<=`, `>`, `>=`.
        op: &'static str,
        /// The right-hand side.
        right: Operand,
    },
}

/// One side of a comparison in a [`Having`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Operand {
    /// A number the answer carries, named the way a cell names one.
    ///
    /// An [`Of`] rather than a plan index because an answer can carry several numbers, and
    /// because the one function that reads a number out of a merged answer should be the one
    /// every clause uses.
    Of {
        /// Which number.
        of: Of,
        /// What it is measured in — the same [`Units`] the cell reading it carries.
        ///
        /// **Carried because two aggregates can be compared to each other**, and a comparison
        /// between numbers in different units is not a comparison. A `sum` over a field of
        /// scale two merges to 500 where the values were 5.00, so `sum(amount) > sum(price)`
        /// over the stored numbers would answer 10 > 500 where it was asked 10 > 5 — with both
        /// numbers valid and nothing in the result able to show it. Only a schema knows a
        /// scale, so the check is [`Shape::resolve`]'s.
        units: Units,
    },
    /// A number to compare against.
    Value(Threshold),
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
    /// One number against one threshold, which is what every `HAVING` was before there was a
    /// tree and what most of them still are.
    ///
    /// The companion to [`Cell::plain`] and [`Shape::row`], for the same reason: a caller with
    /// nothing to say about the tree should not have to build one.
    pub fn cmp(of: Of, units: Units, op: &'static str, value: Threshold) -> Self {
        Self::Cmp { left: Operand::Of { of, units }, op, right: Operand::Value(value) }
    }

    /// Whether a group survives, given a way to read the numbers it carries.
    ///
    /// `read` answers `None` for a number the group has none of — a `min` or `max` over records
    /// that hold no value in the field, or a plan that said nothing about this group.
    ///
    /// **A comparison with an absent operand is false, not unknown.** Absent is not zero and it
    /// is not a value, so nothing is true of it; and the boolean operators above are ordinary
    /// two-valued ones rather than SQL's three-valued kind. That is a deliberate divergence and
    /// the reason it is safe here is that this surface has no null to propagate: a bit is set
    /// or it is not. Note what it means for `NOT`, which is the case three-valued logic exists
    /// to argue about — `NOT (min(x) > 5)` **keeps** a group with no `x` at all, because the
    /// comparison inside it is false rather than unknown.
    pub fn holds(&self, read: &impl Fn(Of) -> Option<i128>) -> bool {
        match self {
            Self::And(a, b) => a.holds(read) && b.holds(read),
            Self::Or(a, b) => a.holds(read) || b.holds(read),
            Self::Not(a) => !a.holds(read),
            Self::Cmp { left, op, right } => {
                let (Some(l), Some(r)) = (left.value(read), right.value(read)) else {
                    return false;
                };
                match *op {
                    "=" => l == r,
                    "!=" => l != r,
                    "<" => l < r,
                    "<=" => l <= r,
                    ">" => l > r,
                    ">=" => l >= r,
                    // The lexer produces no other comparison, so this is unreachable rather
                    // than a silent "keep everything".
                    other => unreachable!("unexpected comparison in HAVING: {other}"),
                }
            }
        }
    }

    /// The same clause against a branch's plans, moved by `by`. See [`Shape::rebase`].
    pub fn rebase(self, by: usize) -> Self {
        match self {
            Self::And(a, b) => Self::And(Box::new(a.rebase(by)), Box::new(b.rebase(by))),
            Self::Or(a, b) => Self::Or(Box::new(a.rebase(by)), Box::new(b.rebase(by))),
            Self::Not(a) => Self::Not(Box::new(a.rebase(by))),
            Self::Cmp { left, op, right } => {
                Self::Cmp { left: left.rebase(by), op, right: right.rebase(by) }
            }
        }
    }

    /// Every number this clause reads, so a shape can say which plans it names.
    pub fn plans(&self) -> Vec<usize> {
        let mut out = Vec::new();
        self.collect(&mut out);
        out
    }

    fn collect(&self, out: &mut Vec<usize>) {
        match self {
            Self::And(a, b) | Self::Or(a, b) => {
                a.collect(out);
                b.collect(out);
            }
            Self::Not(a) => a.collect(out),
            Self::Cmp { left, right, .. } => {
                for side in [left, right] {
                    if let Operand::Of { of, .. } = side {
                        out.extend(of.plans());
                    }
                }
            }
        }
    }
}

impl Operand {
    /// The same operand against a branch's plans, moved by `by`.
    fn rebase(self, by: usize) -> Self {
        match self {
            Self::Of { of, units } => Self::Of { of: of.rebase(by), units },
            Self::Value(t) => Self::Value(t),
        }
    }

    /// The number this side stands for, or `None` when there is none.
    fn value(&self, read: &impl Fn(Of) -> Option<i128>) -> Option<i128> {
        match self {
            Self::Of { of, .. } => read(*of),
            // An unresolved threshold is unreachable through `translate` + `resolve`, and a
            // shape can also be built by hand. Answering `None` drops the group, which is the
            // safe direction: it is visibly wrong, where keeping every group would look like a
            // `HAVING` that simply matched a lot.
            Self::Value(Threshold::Written { .. }) => None,
            Self::Value(Threshold::Units(n)) => Some(*n),
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

/// What a number is measured in, before and after it has met a schema.
///
/// **A decimal field stores an integer**: `12.50` in a field of scale two is 1250 bit planes
/// deep, and 1250 is what a `sum` over it merges to. So an answer that handed that number back
/// unscaled would be off by a factor of a hundred - with both numbers valid, and nothing in the
/// answer able to show which one it was. `WHERE price = 12.50` already converts; this is the
/// same conversion on the way out, so that a value written by a statement reads back as the
/// value that was written.
///
/// Two variants for the reason [`Threshold`] has two: the conversion is not optional, and
/// making the unconverted form its own variant is what turns "remember to call
/// [`Shape::resolve`]" into something the type says.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Units {
    /// Digits after the point. Zero for a count, for a key, and for a field that stores whole
    /// numbers - which is every field but a decimal.
    Digits(u8),
    /// The field the number came out of, until a schema has said what that field keeps.
    ///
    /// `table` is carried rather than taken from the statement for the reason
    /// [`Threshold::Written`] carries it: a join has two, and only the lowering knows which
    /// side an aggregate measured.
    Written { table: String, field: String },
}

impl Default for Units {
    fn default() -> Self {
        Self::PLAIN
    }
}

impl Units {
    /// How many digits after the point, once resolved.
    ///
    /// An unresolved form answers zero, which is what it would have rendered as before there
    /// was anything to resolve - reachable only through a shape built by hand, since
    /// [`Shape::resolve`] runs on the one path a statement takes.
    pub fn digits(&self) -> u8 {
        match self {
            Self::Digits(n) => *n,
            Self::Written { .. } => 0,
        }
    }

    /// The units of a number that is a count, a key, or anything else a field's scale does not
    /// describe.
    pub const PLAIN: Self = Self::Digits(0);
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
    /// What that number is measured in - see [`Units`]. `Digits(0)` for everything but a
    /// number that came out of a decimal field.
    pub units: Units,
}

/// The columns of a projection, before and after a schema has named them.
///
/// Two variants for the reason [`Threshold`] has two: `SELECT *` cannot be turned into a list
/// of columns where it is written, because nothing there knows the table. Making the unexpanded
/// form its own variant is what turns "remember to call [`Shape::resolve`]" into something the
/// type says.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Columns {
    /// The select list, as written, in the order it wrote them.
    Named(Vec<Selected>),
    /// `SELECT *`: every column the table declares, filled in by [`Shape::resolve`].
    ///
    /// Expanded through [`big_plan::expanded_columns`], which is the same function the planner
    /// expands the matching `Project` with - a header of four columns over a plan that read
    /// three is an answer that is wrong rather than absent.
    All {
        table: String,
        /// The statement's `LIMIT`, carried only for the fall back to [`Shape::Records`]: where
        /// the expansion has columns the plan holds the cut, because a projection pays for
        /// every record it reads and a cut applied here would be one applied afterwards.
        limit: Option<usize>,
    },
}

impl Columns {
    /// The columns, once a schema has named them.
    ///
    /// Empty for an unexpanded `SELECT *`, which is reachable only through a shape built by
    /// hand: [`Shape::resolve`] runs on the one path a statement takes, and it leaves no `All`
    /// behind.
    pub fn named(&self) -> &[Selected] {
        match self {
            Self::Named(columns) => columns,
            Self::All { .. } => &[],
        }
    }
}

/// One column of a projection: the values of a field, read back per record.
///
/// Not a [`Cell`], because a projected column names no plan - a projection is one plan whose
/// answer is already a table of rows, and the columns are the fields it read. It still needs
/// [`Units`] for the same reason a cell does: a decimal read back unscaled is off by a factor
/// of its scale.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Selected {
    /// The column name, which is the alias when one was written.
    pub column: String,
    /// What the values in it are measured in.
    pub units: Units,
}

impl Cell {
    /// A cell whose number needs no scaling: a count, a key, or a field that stores whole
    /// numbers.
    ///
    /// The units of a cell that reads a field are the field's, and only the lowering knows
    /// which field that was - see [`Units::Written`].
    pub fn plain(column: impl Into<String>, of: Of) -> Self {
        Self { column: column.into(), of, units: Units::PLAIN }
    }
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
    /// One side's per-key number, made one with every other side's, over a join.
    ///
    /// The cell names only the plan carrying the number; the sides it is scaled against are
    /// [`Shape::Join::keys`], which is what says how many tables the join has. A cell cannot
    /// carry them itself and stay [`Copy`], and would be saying twice what the shape already
    /// says once.
    Paired {
        /// The plan whose per-key number this cell is about.
        plan: usize,
        /// Which of [`Shape::Join::keys`] that plan belongs to.
        ///
        /// **An index into the join's keys, not into the statement's calls** - which is why
        /// [`Of::rebase`] moves `plan` along and leaves this alone, and why [`Of::plans`] does
        /// not report it.
        side: usize,
        /// How this side's number and the others' become one.
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
    /// How many keys every side of a join holds — `count(DISTINCT <the join key>)`.
    ///
    /// Names no plan: the sides are [`Shape::Join::sides`], and this is the size of their
    /// intersection.
    SharedKeys,
    /// One paired number over another, over a join — `avg` of one table's column.
    ///
    /// **The two halves are folded before they are divided**, which is the whole reason this is
    /// a variant rather than an [`Of::Ratio`] of two cells. Each side of the fraction is scaled
    /// by the same per-key product `Π_{i≠side}`, and that product is *inside both sums*: it
    /// does not cancel, so `Σ_s (top_s / bottom_s)` is a different number from
    /// `(Σ_s top_s) / (Σ_s bottom_s)` and only the second is the average.
    ///
    /// Flat rather than two nested [`Of`]s because a cell has to stay [`Copy`]. Both plans move
    /// under [`Of::rebase`]; `side` is a position among the join's sides and does not.
    PairedRatio {
        /// The plan holding this side's total.
        top: usize,
        /// The plan holding this side's record count, which is what the total is an average of.
        bottom: usize,
        /// Which of [`Shape::Join::sides`] both plans belong to.
        side: usize,
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

/// One table of a join: how it is keyed, and whether a row of the join needs it.
///
/// Not part of a [`Cell`], which names only the plan carrying its own number. Which sides exist
/// and how they are keyed is a fact about the *join*, said once here rather than once per cell -
/// and a cell could not carry it and stay [`Copy`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct JoinSide {
    /// The plan that answers for this side, and what it is keyed by.
    pub keyed: Keying,
    /// Whether this side must hold a point for that point to be a row of the join.
    ///
    /// **The whole of inner and outer, in one flag.** The rows of a join are the points every
    /// required side holds; with no side required they are the points *any* side holds. An
    /// inner join requires every side, which is the only thing this surface lowers today; a
    /// `LEFT JOIN` would require the left one, and a `FULL` one none.
    pub required: bool,
}

/// How a side of a join is keyed, and the plan that says so.
///
/// One enum rather than a plan and a separate arity, because the arity *is* which plan variant
/// answered: no axis is a `Count`, one is a `Distinct`, and two would be a `GroupByPair`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Keying {
    /// One axis of the join's key space: a `Distinct` over that column.
    By {
        /// Index into the statement's calls.
        plan: usize,
        /// Which of the join's axes this side's keys are values of.
        axis: u8,
    },
}

impl Keying {
    /// Every plan this side reads, by index into the statement's calls.
    ///
    /// Exhaustive with no fallback arm on purpose: a side that gains a second plan and is not
    /// named here is a plan [`Shape::plans`] does not report, which is a number nobody reads and
    /// an answer that is quietly short.
    pub fn plans(self) -> Vec<usize> {
        match self {
            Self::By { plan, .. } => vec![plan],
        }
    }

    /// The plan this side is answered by, for a caller reading one number out of it.
    pub fn plan(self) -> usize {
        match self {
            Self::By { plan, .. } => plan,
        }
    }

    /// The same side, reading a plan `by` further along the statement's list.
    ///
    /// The axis is a position in the join's own key space and does not move; only the plan does.
    fn rebase(self, by: usize) -> Self {
        match self {
            Self::By { plan, axis } => Self::By { plan: plan + by, axis },
        }
    }
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
    /// Every plan this cell reads **by index into the statement's calls**.
    ///
    /// **A grouped answer is driven by the union of the groups its plans produced**, and with
    /// `UNION ALL` a statement's flat list holds other branches' plans too - so which plans a
    /// shape may look at has to come from the shape rather than from the length of the list.
    ///
    /// A join's other sides are not here: [`Of::Paired`] names them by position in
    /// [`Shape::Join::keys`] and [`Of::SharedKeys`] names them not at all, so a caller after
    /// every plan a *shape* reads wants [`Shape::plans`] rather than this.
    pub fn plans(self) -> Vec<usize> {
        match self {
            Self::Key | Self::RightKey | Self::Probe { .. } | Self::SharedKeys => Vec::new(),
            Self::Value { plan }
            | Self::Groups { plan }
            | Self::Keys { plan }
            | Self::Group { plan, .. }
            | Self::Paired { plan, .. } => vec![plan],
            Self::Ratio { plan, over } => vec![plan, over],
            Self::PairedRatio { top, bottom, .. } => vec![top, bottom],
        }
    }

    /// The plan a cell reads first, for a caller checking a shape against a set of plans.
    pub fn plan(self) -> Option<usize> {
        match self {
            Self::Value { plan }
            | Self::Groups { plan }
            | Self::Keys { plan }
            | Self::Group { plan, .. }
            | Self::Paired { plan, .. }
            | Self::Ratio { plan, .. }
            | Self::PairedRatio { top: plan, .. } => Some(plan),
            Self::Key | Self::RightKey | Self::Probe { .. } | Self::SharedKeys => None,
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
        /// `HAVING` over the implicit single group, absent when the row is always kept.
        ///
        /// **The one shape whose `HAVING` decides how many rows there are rather than which.**
        /// A grouping drops the groups that fail; here there is exactly one row, so failing the
        /// test empties the answer. That is what standard SQL says an ungrouped `HAVING` means -
        /// the whole filtered set is one group - and it is a question worth being able to ask:
        /// `SELECT count(*) FROM t WHERE ... HAVING count(*) > 1000` is a threshold alarm that
        /// answers with nothing until it fires.
        having: Option<Having>,
    },
    /// Record ids, one per row.
    ///
    /// The one shape that is not what SQL would give elsewhere, and what `SELECT *` falls back
    /// to where there is nothing a projection could read - a table with no fields, or one whose
    /// engine keeps no values and declares only keyed columns. This engine stores facts as bits
    /// at `(row, record)`, so a record with no readable column still has an identity, and a
    /// column visibly called by the record's own name is worth more than an empty header.
    ///
    /// It is also the only way a record id reaches a SQL answer at all: `_record_id` names a
    /// record on the way in, for an `INSERT`, and is not a column a select list can ask for.
    /// `GET /table/{t}/records` is the route that lists them.
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
        columns: Columns,
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
    /// An equi-join between two or more tables on a keyed column they all share.
    ///
    /// **What is joined is the key's string, and it is joined after every side has been
    /// merged.** A record is a set of bits in one table and there is no pointer to another, so
    /// what the tables share is the string a keyed column was interned from. The row ids behind
    /// those strings are per `(table, field)` and have no reason to agree, which is why the
    /// join cannot happen in a plan and is not one.
    ///
    /// For each key `s` the join is the Cartesian product of the records holding `s` on every
    /// side, so `count(*)` is `Π_i |X_i,s|` summed over every shared key, `sum(a.x)` is
    /// `sum_A(x) · Π_{i≠A} |X_i,s|` summed the same way, and a `min` is the smallest of one
    /// side's minima over the keys every other side holds at all. Every one of those is
    /// arithmetic over per-key numbers that an ordinary single-table grouping already produces
    /// — which is why a join of any width adds no `Plan` variant and no merge arm.
    ///
    /// Three tables are a *star*: every one of them grouped by the one key they share. A table
    /// that would need a second key column is a chain, and is refused before it gets here -
    /// grouping one table by two columns at once is a pass over the second per value of the
    /// first, which is a cost this surface does not take.
    Join {
        /// How many key coordinates a row of this join has.
        ///
        /// One for a star, which is every join this surface lowers. Kept as a number rather
        /// than read off the sides because it is the join's own width: it says how wide a point
        /// of the key space is, and every side's axis has to name one of them.
        axes: u8,
        /// One per table, in the order `FROM` wrote them.
        ///
        /// A point every *required* side holds is a row of the join; a point any required side
        /// is missing is not.
        sides: Vec<JoinSide>,
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
    /// One row of cells, kept whatever it holds.
    ///
    /// The companion to [`Cell::plain`], and it exists for the same reason: a caller that has
    /// nothing to say about a field should not have to name it. Only the lowering ever has a
    /// `HAVING` to put here, so every other construction of a `Shape::Row` — every test
    /// fixture, every example — was spelling out `having: None` to say nothing.
    ///
    /// That is not tidiness. A struct-like variant is constructed by naming every field, so a
    /// field added to `Row` churns each of those sites without any of them meaning anything
    /// different afterwards; the diff then hides the two or three places where the new field
    /// genuinely had to be decided. Going through here keeps that from happening again.
    pub fn row(cells: Vec<Cell>) -> Self {
        Self::Row { cells, having: None }
    }

    /// The column names, in order, for a caller rendering a header.
    pub fn columns(&self) -> Vec<&str> {
        match self {
            // SQL names a union's columns after its first branch, whatever the others called
            // theirs.
            Self::Union { branches } => branches.first().map(Shape::columns).unwrap_or_default(),
            Self::Pairs { cells, .. } => cells.iter().map(|c| c.column.as_str()).collect(),
            Self::Records { column, .. } => vec![column.as_str()],
            Self::Table { columns } => columns.named().iter().map(|c| c.column.as_str()).collect(),
            Self::Row { cells, .. } | Self::Groups { cells, .. } | Self::Join { cells, .. } => {
                cells.iter().map(|c| c.column.as_str()).collect()
            }
        }
    }

    /// Every cell in this shape, a union's branches included.
    ///
    /// Companion to [`Shape::columns`], which answers the same walk with only the names. This
    /// one is for a caller that has to check what each cell *reads* - which plan, and whether
    /// the statement made it. A shape naming a plan that does not exist is a panic waiting for
    /// whoever assembles the rows, a long way from the text that caused it.
    pub fn cells(&self) -> Vec<&Cell> {
        match self {
            Self::Records { .. } | Self::Table { .. } => Vec::new(),
            Self::Union { branches } => branches.iter().flat_map(Shape::cells).collect(),
            Self::Row { cells, .. }
            | Self::Groups { cells, .. }
            | Self::Pairs { cells, .. }
            | Self::Join { cells, .. } => cells.iter().collect(),
        }
    }

    /// Every plan this shape reads, a union's branches included.
    ///
    /// Companion to [`Shape::cells`], and the one a caller checking a shape against a
    /// statement's calls wants: a shape names plans its cells do not. The rows of a grouping
    /// are driven by `keys`, and a join's cells are scaled against sides they name only by
    /// position - so walking the cells alone would miss every plan that decides which rows
    /// there are.
    pub fn plans(&self) -> Vec<usize> {
        let driving = match self {
            Self::Groups { keys, .. } | Self::Pairs { keys, .. } => keys.clone(),
            Self::Join { sides, .. } => sides.iter().flat_map(|s| s.keyed.plans()).collect(),
            Self::Row { .. } | Self::Records { .. } | Self::Table { .. } | Self::Union { .. } => {
                Vec::new()
            }
        };
        driving.into_iter().chain(self.cells().iter().flat_map(|c| c.of.plans())).collect()
    }

    /// Puts every written threshold and every cell's units against the schema.
    ///
    /// **The one part of a shape that needs a schema**, and it is split out for the reason the
    /// rest of this crate is schema-free: [`crate::translate`] stays a pure function of the
    /// text, testable at parser speed, and the single step that has to ask what a field is
    /// happens in the same place and at the same time as the planning that already does.
    ///
    /// Two conversions, and they are the two ends of one: a threshold is written as a value and
    /// compared against a stored integer, so it converts on the way in; a cell holds that stored
    /// integer and is read as a value, so it converts on the way out. Skipping either is off by
    /// a factor of the scale, with both numbers valid and nothing in the answer able to show it.
    ///
    /// The inward conversion is [`big_plan::to_units`] — the planner's own, not a copy. A
    /// `HAVING sum(price) >= 100.00` means what `WHERE price >= 100.00` means, because it is
    /// the same code deciding.
    pub fn resolve(self, schema: &impl Schema) -> Result<Self, PlanError> {
        let one = |h: Option<Having>| -> Result<Option<Having>, PlanError> {
            match h {
                None => Ok(None),
                Some(h) => Ok(Some(resolve_having(schema, h)?)),
            }
        };
        let all = |cells: Vec<Cell>| -> Result<Vec<Cell>, PlanError> {
            cells
                .into_iter()
                .map(|c| Ok(Cell { units: resolve_units(schema, c.units)?, ..c }))
                .collect()
        };
        Ok(match self {
            Self::Row { cells, having } => Self::Row { cells: all(cells)?, having: one(having)? },
            Self::Groups { keys, cells, having, order, cut } => {
                Self::Groups { keys, cells: all(cells)?, having: one(having)?, order, cut }
            }
            Self::Pairs { keys, cells, having, order, cut } => {
                Self::Pairs { keys, cells: all(cells)?, having: one(having)?, order, cut }
            }
            Self::Join { axes, sides, cells, per_key, having, order, cut } => Self::Join {
                axes,
                sides,
                cells: all(cells)?,
                per_key,
                having: one(having)?,
                order,
                cut,
            },
            Self::Table { columns } => match columns {
                Columns::Named(columns) => Self::Table {
                    columns: Columns::Named(
                        columns
                            .into_iter()
                            .map(|c| Ok(Selected { units: resolve_units(schema, c.units)?, ..c }))
                            .collect::<Result<Vec<_>, PlanError>>()?,
                    ),
                },
                // `SELECT *`. An empty expansion is a table with nothing a projection could
                // read, and the planner turned the same call into a `Rows`; the header follows
                // it back to record ids rather than naming no columns at all.
                Columns::All { table, limit } => {
                    let names = big_plan::expanded_columns(schema, &table);
                    if names.is_empty() {
                        return Ok(Self::Records {
                            column: crate::RECORD_COLUMN.to_string(),
                            limit,
                        });
                    }
                    Self::Table {
                        columns: Columns::Named(
                            names
                                .into_iter()
                                .map(|field| {
                                    let units = Units::Written {
                                        table: table.clone(),
                                        field: field.clone(),
                                    };
                                    Ok(Selected {
                                        column: field,
                                        units: resolve_units(schema, units)?,
                                    })
                                })
                                .collect::<Result<Vec<_>, PlanError>>()?,
                        ),
                    }
                }
            },
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
                    units: c.units,
                })
                .collect()
        };
        match self {
            // The `HAVING` names one of the cells' own numbers, and a `HavingAgg` is never a
            // quantile - so there is nothing there to move. Carried through unchanged rather
            // than dropped.
            Self::Row { cells: c, having } => Self::Row { cells: cells(c), having },
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
            cells
                .into_iter()
                .map(|c| Cell { column: c.column, of: c.of.rebase(by), units: c.units })
                .collect()
        };
        let having = |h: Option<Having>| h.map(|h| h.rebase(by));
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
            Self::Row { cells: c, having: h } => Self::Row { cells: cells(c), having: having(h) },
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
            Self::Join { axes, sides, cells: c, per_key, having: h, order: o, cut } => Self::Join {
                axes,
                sides: sides
                    .into_iter()
                    .map(|s| JoinSide { keyed: s.keyed.rebase(by), required: s.required })
                    .collect(),
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
            // `side` is a position in the join's own keys rather than in the statement's
            // calls, so it stays where it is while the plan it points past moves along.
            Self::Paired { plan, side, how } => Self::Paired { plan: plan + by, side, how },
            // Both halves are plans and both move; `side` is a position among the join's own
            // sides, so it stays where it is.
            Self::PairedRatio { top, bottom, side } => {
                Self::PairedRatio { top: top + by, bottom: bottom + by, side }
            }
            // Names no plan of its own: the sides are the join's keys, which the shape rebased.
            Self::SharedKeys => Self::SharedKeys,
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
/// How many digits after the point a field keeps, for a number that came out of it.
///
/// A field that is not a decimal keeps none, which is what every other class answers - and a
/// field nobody has is the planner's error, the same one a condition on it would raise.
fn resolve_units(schema: &impl Schema, u: Units) -> Result<Units, PlanError> {
    let Units::Written { table, field } = u else { return Ok(u) };

    let class = schema
        .field_class(&table, &field)
        .ok_or_else(|| PlanError::UnknownField { table: table.clone(), field: field.clone() })?;
    Ok(match class {
        FieldClass::Integer { scale } => Units::Digits(scale),
        _ => Units::PLAIN,
    })
}

/// Puts every threshold in a `HAVING` against the schema, wherever in the tree it sits.
///
/// A walk rather than a single conversion, because a tree can hold several — and every one of
/// them has to be converted, or a clause would compare a written `100.00` against a stored
/// `10000` in one branch and correctly in another.
fn resolve_having(schema: &impl Schema, h: Having) -> Result<Having, PlanError> {
    let pair = |schema: &_, a: Box<Having>, b: Box<Having>| -> Result<_, PlanError> {
        Ok((Box::new(resolve_having(schema, *a)?), Box::new(resolve_having(schema, *b)?)))
    };
    Ok(match h {
        Having::And(a, b) => {
            let (a, b) = pair(schema, a, b)?;
            Having::And(a, b)
        }
        Having::Or(a, b) => {
            let (a, b) = pair(schema, a, b)?;
            Having::Or(a, b)
        }
        Having::Not(a) => Having::Not(Box::new(resolve_having(schema, *a)?)),
        Having::Cmp { left, op, right } => {
            // The field names, captured before resolution turns them into bare digits, so the
            // refusal can say which two columns disagreed.
            let name = |o: &Operand| match o {
                Operand::Of { units: Units::Written { field, .. }, .. } => field.clone(),
                _ => String::new(),
            };
            let (ln, rn) = (name(&left), name(&right));
            let (left, right) = (resolve_operand(schema, left)?, resolve_operand(schema, right)?);
            // **Two numbers are comparable only in the same units.** A threshold was converted
            // into the units of the aggregate beside it, so a comparison against one always
            // agrees; two aggregates were each converted into their own field's, and those can
            // differ. Refused rather than compared, because comparing them is off by a factor
            // of the scale with both numbers valid.
            if let (Operand::Of { units: l, .. }, Operand::Of { units: r, .. }) = (&left, &right) {
                if l.digits() != r.digits() {
                    return Err(PlanError::OperatorNotAllowed {
                        field: rn,
                        op: format!("a comparison against a total of `{ln}`"),
                        class: "a decimal of a different scale",
                    });
                }
            }
            Having::Cmp { left, op, right }
        }
    })
}

fn resolve_operand(schema: &impl Schema, o: Operand) -> Result<Operand, PlanError> {
    Ok(match o {
        Operand::Of { of, units } => Operand::Of { of, units: resolve_units(schema, units)? },
        Operand::Value(t) => Operand::Value(resolve_threshold(schema, t)?),
    })
}

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
