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

//! Running a plan.
//!
//! The only crate that knows both halves. `big-db` never hears about a query language and
//! `big-plan` never links a pager; the adapter that lets them meet lives here, which is why
//! [`CatalogSchema`] is a newtype rather than an `impl` on either side.
//!
//! There is almost nothing to this file, and that is the point: planning already rejected
//! every query that could fail for a reason the storage layer would have to explain, and
//! [`Matches`] already provides the algebra, so execution is a fold.

#![deny(unsafe_code)]

use std::collections::BTreeMap;

use big_db::catalog::{Catalog, FieldKind};
use big_db::{DbRead, Matches, RangeOp, RecordId, RowId};
use big_pager::Pager;
use big_plan::{CmpOp, FieldClass, Keyed, Level, Plan, PlanError, Rows, Schema, TimeUnit};

pub mod error;
pub use error::{ExecError, Result};

/// What a group is one of.
///
/// **The identity, and only the identity.** The label sits beside it in [`Group`] rather than
/// inside it, because two nodes' contributions to one group are folded on this - and a node that
/// was never told a key must not thereby become a different group.
///
/// Its own type rather than a bare `RowId` so that a bucket and a row cannot be confused at a
/// `match` arm that would compile either way. They are addressed quite differently: a row id is
/// handed out once for the whole cluster and means nothing without the dictionary that issued
/// it, while a bucket is a pure function of the value and needs no issuer at all.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum GroupAt {
    /// A row of a keyed field's dictionary.
    Row(RowId),
    /// A calendar bucket of a bit-sliced temporal field: the first moment in it, in the units the
    /// field stores - days for a `DATE`, seconds for a `DATETIME`.
    ///
    /// **Needs no issuer, which is what lets a grouping exist over a field with no dictionary.**
    /// Two nodes that saw different halves of one month compute the same number for it, so the
    /// coordinator's merge folds them together without anybody having coordinated. Ordered by
    /// `start`, so buckets arrive in calendar order even before a key is attached.
    Bucket { start: i64, unit: TimeUnit },
}

/// One group of a `Distinct`, `TopN`, `GroupBy` or `GroupByBucket`.
#[derive(Clone, Debug)]
pub struct Group {
    pub at: GroupAt,
    /// The string the row was interned from, when the field has one.
    ///
    /// Always `None` for a [`GroupAt::Bucket`]: what a bucket is called is derivable from `at`
    /// and its unit, and storing it too would be two spellings of one fact that can disagree.
    pub key: Option<String>,
    /// What was measured about this group. A `Value` rather than a number so a group can carry
    /// a count, a sum, or eventually another grouping, without this type changing again.
    pub value: Box<Value>,
}

/// One cell of a projected row.
///
/// A cell rather than a number because a projection is no longer only over bit-sliced columns:
/// a table that keeps column segments can project a keyed column too, and a row key is a string
/// the dictionary translates rather than an integer anyone would want back.
///
/// **Renders additively.** An integer column still comes out as a JSON number and an absent cell
/// still as `null`, so no client that could read a projection before can be broken by one. The
/// two new shapes only appear where a projection used to be refused outright.
/// **Not `Eq`.** A float cell holds an `f64` and `PartialEq` on one is not reflexive. Nothing in
/// the tree needs the stronger bound - every use is an `assert_eq!` or a `match` - and a NaN
/// cannot reach here anyway, because a float field refuses to store one.
#[derive(Clone, Debug, PartialEq)]
pub enum Projection {
    /// The record holds nothing in this column - not zero, and the same absence a `min` over
    /// nothing answers with.
    Absent,
    /// A bit-sliced column's value.
    ///
    /// An `i128` for both integer classes, which is the one width that holds every value either
    /// can store. A decimal arrives in the units the field stores, exactly as a `Sum` over it
    /// does; nothing here rescales, because nothing here knows the scale.
    Int(i128),
    /// A float column's value, already decoded.
    ///
    /// Decoded here rather than carried out as its stored bits, because undoing the transform
    /// needs the field's declared width and this is the last layer that has a catalog. The same
    /// reason `decode_int` undoes the sign bias here.
    Real(f64),
    /// A keyed column holding one value - a mutex, or a set that happens to hold one.
    Text(String),
    /// A keyed column holding several. A set field is a set, so a record may hold any number.
    Texts(Vec<String>),
}

impl Projection {
    /// The integer, when this cell is one. `None` for absent and for both text shapes.
    pub fn as_int(&self) -> Option<i128> {
        match self {
            Self::Int(v) => Some(*v),
            _ => None,
        }
    }

    pub fn is_absent(&self) -> bool {
        matches!(self, Self::Absent)
    }
}

/// One record's stored values, in the order the projection asked for its columns.
#[derive(Clone, Debug, PartialEq)]
pub struct Projected {
    /// Which record.
    pub record: RecordId,
    /// One cell per projected column.
    pub values: Vec<Projection>,
}

/// One column's value in a tuple grouping: which group it is, and what it is called.
///
/// [`Group`] without the measure, because a tuple's measure belongs to the combination rather
/// than to any one of its columns - which is the awkwardness `Pair` had, where the left half's
/// `value` existed and was never read.
#[derive(Clone, Debug)]
pub struct GroupKey {
    pub at: GroupAt,
    /// The string the row was interned from, when the field has one. Always `None` for a bucket.
    pub key: Option<String>,
}

/// One group of a `GroupByTuple`: a value of each grouped column, and what the records holding
/// all of them measured.
#[derive(Clone, Debug)]
pub struct Tuple {
    /// One per level, outermost first. Never fewer than two.
    pub keys: Vec<GroupKey>,
    /// What the records under this combination measured.
    pub value: Box<Value>,
}

/// What a query answers with.
#[derive(Clone, Debug)]
pub enum Value {
    /// Unmaterialised, so the caller decides whether to count it, page it, or intersect it
    /// with something else.
    Rows(Matches),
    Count(u64),
    Sum(u128),
    /// The same total over a signed field.
    ///
    /// A separate variant rather than widening `Sum`: `u128` and `i128` each hold values the
    /// other cannot, and a single type would have to give one of them up for a field kind that
    /// does not need it. Both render as a JSON number, so nothing downstream has to care.
    SignedSum(i128),
    /// `Min` and `Max`: absent when nothing matched, which is not the same as zero.
    Extreme(Option<u64>),
    /// The same, over a signed field.
    SignedExtreme(Option<i64>),
    /// A total over a float field, folded from the values rather than counted off the bit
    /// planes - see `DbRead::sum_float_where` for why it cannot be the latter.
    RealSum(f64),
    /// `Min` and `Max` over a float field. Absent when nothing matched.
    ///
    /// A variant of its own rather than reusing `Extreme`: the stored numbers are ordered the
    /// same way the values are, so a merge would be right either way, but decoding one needs the
    /// field's declared width and this is the layer that has it. The same argument
    /// `SignedExtreme` makes.
    RealExtreme(Option<f64>),
    Groups(Vec<Group>),
    /// One group per combination of values two or more columns hold records for.
    Tuples(Vec<Tuple>),
    /// The stored values a `Project` read, one row per record, in record order.
    ///
    /// Ordered by record id rather than by anything the caller chose, because that is the order
    /// the bitmap yields and the only order two nodes' answers can be merged in without both of
    /// them materialising more than they were asked for.
    Table(Vec<Projected>),
}

impl Value {
    pub fn as_count(&self) -> Option<u64> {
        match self {
            Self::Count(n) => Some(*n),
            _ => None,
        }
    }

    pub fn as_sum(&self) -> Option<u128> {
        match self {
            Self::Sum(n) => Some(*n),
            _ => None,
        }
    }

    pub fn as_rows(&self) -> Option<&Matches> {
        match self {
            Self::Rows(m) => Some(m),
            _ => None,
        }
    }

    pub fn as_extreme(&self) -> Option<Option<u64>> {
        match self {
            Self::Extreme(v) => Some(*v),
            _ => None,
        }
    }

    pub fn as_groups(&self) -> Option<&[Group]> {
        match self {
            Self::Groups(g) => Some(g),
            _ => None,
        }
    }

    pub fn as_tuples(&self) -> Option<&[Tuple]> {
        match self {
            Self::Tuples(t) => Some(t),
            _ => None,
        }
    }

    pub fn as_table(&self) -> Option<&[Projected]> {
        match self {
            Self::Table(t) => Some(t),
            _ => None,
        }
    }
}

/// Lets the planner read a catalog without either crate knowing about the other.
pub struct CatalogSchema<'a>(pub &'a Catalog);

impl Schema for CatalogSchema<'_> {
    fn has_table(&self, table: &str) -> bool {
        self.0.lookup(table).is_some()
    }

    fn stores_values(&self, table: &str) -> bool {
        self.0.lookup(table).is_some_and(|t| t.engine.has_columns())
    }

    /// In field-id order, which is the order the fields were declared in - so `SELECT *` on a
    /// table built by a column list answers in the order that list was written.
    fn fields(&self, table: &str) -> Vec<String> {
        let Some(t) = self.0.lookup(table) else { return Vec::new() };
        self.0.fields_of(t.id).map(|f| f.name.clone()).collect()
    }

    /// Storage kinds collapse to the three classes a planner can tell apart. A decimal is an
    /// integer as far as `>` is concerned, and a mutex is a keyed field that happens to allow
    /// only one value at a time - a distinction the storage layer enforces on write, not one
    /// the query language needs a rule for.
    fn field_class(&self, table: &str, field: &str) -> Option<FieldClass> {
        let t = self.0.lookup(table)?.id;
        let f = self.0.field(t, field)?;
        Some(match f.kind {
            FieldKind::Int => FieldClass::Integer { scale: 0 },
            FieldKind::SignedInt => FieldClass::Signed,
            // A negative scale would mean digits before the point, which nothing here writes.
            FieldKind::Decimal => FieldClass::Integer { scale: f.scale.max(0) as u8 },
            // The width travels because the planner makes one decision with it that no other
            // class needs: a threshold a single-precision field cannot hold exactly has to be
            // rounded in the direction that keeps the records the comparison asked for.
            FieldKind::Float32 => FieldClass::Float { bits: 32 },
            FieldKind::Float64 => FieldClass::Float { bits: 64 },
            // Stored as a biased integer, planned as a date: the unit is the whole of what
            // separates the two, and it is what a written `'2024-01-15'` is converted against.
            FieldKind::Date => FieldClass::Temporal { unit: TimeUnit::Days },
            FieldKind::DateTime => FieldClass::Temporal { unit: TimeUnit::Seconds },
            FieldKind::Set => FieldClass::Keyed(Keyed::Set),
            FieldKind::Mutex => FieldClass::Keyed(Keyed::Mutex),
            FieldKind::TimeQuantum => FieldClass::Keyed(Keyed::Time),
            FieldKind::Bool => FieldClass::Boolean,
        })
    }
}

/// Parses, plans and runs in one step, for callers that have text rather than a plan.
pub fn query<P: Pager + Sync>(db: &DbRead<'_, P>, table: &str, text: &str) -> Result<Value> {
    let call = big_plan::parse(text)?;
    let plan = big_plan::plan(table, &call, &CatalogSchema(db.catalog()))?;
    execute(db, &plan)
}

pub fn execute<P: Pager + Sync>(db: &DbRead<'_, P>, plan: &Plan) -> Result<Value> {
    let table = plan.table();
    Ok(match plan {
        Plan::Rows { rows, .. } => Value::Rows(eval(db, table, rows)?),
        // `Count(All())` is the one shape that never needs a row set: it is asking how many
        // records the table holds, and the cardinality of every container is already cached in
        // the leaf cell above it. Anything else - `Count(Row(...))`, `Count(Intersect(...))` -
        // has to build the set before it can count it, so only this arm short-circuits.
        Plan::Count { rows: Rows::All, .. } => Value::Count(db.count_all(table)?),
        Plan::Count { rows, .. } => Value::Count(eval(db, table, rows)?.cardinality()),
        // All three route through the same place, which is also where `GroupBy` sends its
        // aggregate: whether a total comes back signed depends on the field, and that decision
        // should exist once rather than once per call site.
        Plan::Sum { rows, .. } | Plan::Min { rows, .. } | Plan::Max { rows, .. } => {
            aggregate_over(db, table, plan, &eval(db, table, rows)?)?
        }

        Plan::Distinct { rows, field, .. } => {
            let counts = db.group_counts(table, field, &eval(db, table, rows)?)?;
            Value::Groups(label(db, table, field, counts, Value::Count))
        }

        Plan::TopN { rows, field, n, .. } => {
            let counts = db.group_counts(table, field, &eval(db, table, rows)?)?;
            // Labelled before ranking, and ranked only after every shard has contributed. A
            // row that is merely second everywhere would otherwise be lost to one that leads a
            // single shard.
            //
            // Ties break on the key rather than the row id. Row ids are handed out in the
            // order values were first written, so two databases holding the same data would
            // rank it differently depending on the order it arrived - a difference the caller
            // cannot see and did not ask for.
            let mut groups = label(db, table, field, counts, Value::Count);
            rank_top_n(&mut groups, *n);
            Value::Groups(groups)
        }

        Plan::GroupBy { rows, field, aggregate, .. } => {
            let groups = db.group_matches(table, field, &eval(db, table, rows)?)?;
            let mut out = Vec::with_capacity(groups.len());
            for (row, hits) in groups {
                out.push(Group {
                    at: GroupAt::Row(row),
                    key: db.row_key(table, field, row).map(str::to_string),
                    value: Box::new(aggregate_over(db, table, aggregate, &hits)?),
                });
            }
            sort_by_key(&mut out);
            Value::Groups(out)
        }

        // One group per calendar bucket the column's values reach into.
        //
        // **The buckets come from the calendar, not from the data**, which is what lets this
        // group a column with no dictionary to walk. The range of values is two `Bsi::extreme`
        // reads; the calendar says which buckets that range covers; and each bucket's records
        // are a range on the bit planes, which is the read the field already answers.
        //
        // Empty buckets are dropped rather than answered with a zero. A month nothing happened
        // in is not a group - `GROUP BY` answers about the values present - and keeping them
        // would make a sparse column cost its whole span rather than its contents.
        Plan::GroupByBucket { rows, field, unit, max_buckets, aggregate, .. } => {
            let matched = eval(db, table, rows)?;
            let level =
                Level::Bucket { field: field.clone(), unit: *unit, max_buckets: *max_buckets };
            let mut out = Vec::new();
            for (at, key, within) in expand(db, table, &level, &matched)? {
                out.push(Group {
                    at,
                    key,
                    value: Box::new(aggregate_over(db, table, aggregate, &within)?),
                });
            }
            Value::Groups(out)
        }

        // One grouping per combination of values the levels hold records for, walked as a tree.
        //
        // **The frontier is the cost.** Every entry at level `k` is one pass over level `k + 1`,
        // so checking the frontier's size at the top of each level bounds the product as well as
        // the first column - the frontier after the first level *is* the pass count for the
        // second. One budget genuinely suffices, and for two levels it is exactly what
        // the pair grouping's own bound already checked before this generalised.
        Plan::GroupByTuple { rows, levels, aggregate, max_passes, .. } => {
            let matched = eval(db, table, rows)?;
            let (last, outer) = levels.split_last().expect("a tuple grouping has two or more");

            let mut frontier: Vec<(Vec<GroupKey>, Matches)> = vec![(Vec::new(), matched)];
            for level in outer {
                if frontier.len() > *max_passes {
                    return Err(ExecError::TooManyGroups {
                        field: level.field().to_string(),
                        found: frontier.len(),
                        limit: *max_passes,
                    });
                }
                let mut next = Vec::new();
                for (keys, within) in frontier {
                    for (at, key, hits) in expand(db, table, level, &within)? {
                        let mut keys = keys.clone();
                        keys.push(GroupKey { at, key });
                        next.push((keys, hits));
                    }
                }
                frontier = next;
            }
            // The last level costs one pass per entry too, so it is checked the same way.
            if frontier.len() > *max_passes {
                return Err(ExecError::TooManyGroups {
                    field: last.field().to_string(),
                    found: frontier.len(),
                    limit: *max_passes,
                });
            }

            let mut out = Vec::new();
            for (keys, within) in frontier {
                for (at, key, hits) in expand(db, table, last, &within)? {
                    let mut keys = keys.clone();
                    keys.push(GroupKey { at, key });
                    out.push(Tuple {
                        keys,
                        value: Box::new(aggregate_over(db, table, aggregate, &hits)?),
                    });
                }
            }
            sort_tuples(&mut out);
            Value::Tuples(out)
        }

        Plan::Project { rows, fields, limit, .. } => {
            let matched = eval(db, table, rows)?;
            // An unbounded projection reads every matching record, so it answers to the same
            // record ceiling every other unbounded read does - and the check costs nothing,
            // because a `Matches` knows its cardinality without naming one of them. This is
            // what bounds `SELECT c FROM t ORDER BY c`, whose cut cannot go into the plan: the
            // sort has to see every row before it knows which ones survive.
            if limit.is_none() {
                db.check_records(matched.cardinality())?;
            }
            let plans: Vec<ColumnPlan> =
                fields.iter().map(|f| ColumnPlan::of(db, table, f)).collect();
            let mut out = Vec::new();
            for record in matched.records_from(0).take(limit.unwrap_or(usize::MAX)) {
                let mut values = Vec::with_capacity(fields.len());
                for (field, how) in fields.iter().zip(&plans) {
                    values.push(how.read(db, table, field, record)?);
                }
                out.push(Projected { record, values });
            }
            Value::Table(out)
        }
    })
}

/// How one column of a projection is read, decided once per column rather than per record.
///
/// The decision costs a catalog lookup and the loop below runs it for every record, so making it
/// once is the difference between a projection of a thousand records doing one lookup per column
/// and doing a thousand.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ColumnPlan {
    /// Out of the column segment. Available whenever the table keeps one, and the only way a
    /// keyed column can be read back at all.
    Segment { keyed: bool, signed: bool, float: Option<u32> },
    /// Rebuilt from bit planes, one point read per plane. What every projection did before
    /// segments existed, and still what a bitmap-only table does.
    Planes { signed: bool, float: Option<u32> },
}

impl ColumnPlan {
    fn of<P: Pager + Sync>(db: &DbRead<'_, P>, table: &str, field: &str) -> Self {
        let signed = is_signed(db, table, field);
        let catalog = db.catalog();
        let def = catalog.lookup(table).and_then(|t| catalog.field(t.id, field));
        let keyed = def.is_some_and(|f| f.kind.is_keyed());
        // The declared width, carried rather than looked up again per record: undoing the float
        // transform needs it, and this whole type exists so that lookup happens once.
        let float =
            def.filter(|f| f.kind.is_float())
                .map(|f| if f.bit_depth == 0 { 64 } else { f.bit_depth });
        let has_columns = catalog.lookup(table).is_some_and(|t| t.engine.has_columns());
        if has_columns {
            Self::Segment { keyed, signed, float }
        } else {
            Self::Planes { signed, float }
        }
    }

    fn read<P: Pager + Sync>(
        self,
        db: &DbRead<'_, P>,
        table: &str,
        field: &str,
        record: RecordId,
    ) -> Result<Projection> {
        match self {
            Self::Planes { signed, float } => int_cell(db, table, field, record, signed, float),
            Self::Segment { keyed, signed, float } => {
                let Some(cell) = db.column_cell(table, field, record)? else {
                    // The table claimed columns and then had none, which only a catalog changing
                    // underneath this read could produce. Falling back is the safe answer.
                    return int_cell(db, table, field, record, signed, float);
                };
                Ok(match cell {
                    big_db::Cell::Null => Projection::Absent,
                    big_db::Cell::Value(v) if keyed => {
                        // A mutex: one row id, translated back through the dictionary.
                        match db.row_key(table, field, v) {
                            Some(name) => Projection::Text(name.to_string()),
                            None => Projection::Absent,
                        }
                    }
                    big_db::Cell::Value(v) if float.is_some() => {
                        Projection::Real(big_db::float::decode(v, float.expect("some")))
                    }
                    big_db::Cell::Value(v) => {
                        Projection::Int(decode_int(v, signed, db, table, field))
                    }
                    big_db::Cell::List(rows) => {
                        let names: Vec<String> = rows
                            .iter()
                            .filter_map(|r| db.row_key(table, field, *r).map(str::to_string))
                            .collect();
                        match names.len() {
                            0 => Projection::Absent,
                            // One value reads back as one value rather than a list of one: a
                            // client that asked for `country` wants `"GB"`, and whether the
                            // field could have held two is not something the answer should
                            // make them unwrap.
                            1 => Projection::Text(names.into_iter().next().expect("one")),
                            _ => Projection::Texts(names),
                        }
                    }
                })
            }
        }
    }
}

/// A bit-sliced column's value, rebuilt from its planes.
fn int_cell<P: Pager + Sync>(
    db: &DbRead<'_, P>,
    table: &str,
    field: &str,
    record: RecordId,
    signed: bool,
    float: Option<u32>,
) -> Result<Projection> {
    if let Some(bits) = float {
        let v = db.get_int(table, field, record)?.map(|v| big_db::float::decode(v, bits));
        return Ok(v.map_or(Projection::Absent, Projection::Real));
    }
    let v = if signed {
        db.get_signed(table, field, record)?.map(i128::from)
    } else {
        db.get_int(table, field, record)?.map(i128::from)
    };
    Ok(v.map_or(Projection::Absent, Projection::Int))
}

/// Undoes the offset-binary bias a signed column stores under.
///
/// The segment keeps what storage keeps, exactly as the bit planes do, so the sign convention is
/// applied here for the same reason and in the same place it always was.
fn decode_int<P: Pager + Sync>(
    stored: u64,
    signed: bool,
    db: &DbRead<'_, P>,
    table: &str,
    field: &str,
) -> i128 {
    if !signed {
        return stored as i128;
    }
    let declared = db
        .catalog()
        .lookup(table)
        .and_then(|t| db.catalog().field(t.id, field))
        .map_or(64, |f| if f.bit_depth == 0 { 64 } else { f.bit_depth });
    big_db::signed::decode(stored, declared) as i128
}

/// Merges two nodes' projected rows into one page.
///
/// Public for the reason [`rank_top_n`] is: the coordinator has to do exactly this, and two
/// implementations of one ordering would be two answers depending on how many nodes were asked.
/// A record lives on one node, so an id appearing twice is a duplicate rather than a row to
/// combine, and the first of them is kept.
pub fn merge_projected(
    mut a: Vec<Projected>,
    b: Vec<Projected>,
    limit: Option<usize>,
) -> Vec<Projected> {
    a.extend(b);
    a.sort_by_key(|p| p.record);
    a.dedup_by_key(|p| p.record);
    // Nothing to truncate to where the plan carried no cut: each node already read every match
    // it owns, and the page is all of them.
    if let Some(limit) = limit {
        a.truncate(limit);
    }
    a
}

/// Orders groups the way `TopN` answers them, and cuts the list to `n`.
///
/// Public because a coordinator has to do exactly this, one level up, after summing every
/// node's contribution to each group. Two implementations of one ordering would be two
/// answers to the same query depending on how many nodes were asked.
pub fn rank_top_n(groups: &mut Vec<Group>, n: usize) {
    groups.sort_by(|a, b| {
        let (an, bn) = (count_of(a), count_of(b));
        bn.cmp(&an).then_with(|| a.key.cmp(&b.key)).then(a.at.cmp(&b.at))
    });
    groups.truncate(n);
}

fn count_of(g: &Group) -> u64 {
    match g.value.as_ref() {
        Value::Count(n) => *n,
        _ => 0,
    }
}

/// Attaches the interned string to each row, so a caller never has to know row ids exist.
///
/// Ordered by key, not by row id. Row ids are handed out in the order values were first
/// written, so ordering by them would list the same data differently depending on the order it
/// arrived - a difference the caller cannot see and did not ask for.
fn label<P: Pager + Sync>(
    db: &DbRead<'_, P>,
    table: &str,
    field: &str,
    rows: Vec<(RowId, u64)>,
    measure: impl Fn(u64) -> Value,
) -> Vec<Group> {
    let mut out: Vec<Group> = rows
        .into_iter()
        .map(|(row, n)| Group {
            at: GroupAt::Row(row),
            key: db.row_key(table, field, row).map(str::to_string),
            value: Box::new(measure(n)),
        })
        .collect();
    sort_by_key(&mut out);
    out
}

/// A row with no interned name sorts after every named one, and by id among themselves.
///
/// Public for the same reason as [`rank_top_n`]: the coordinator's merge has to produce the
/// order this produces, and the only way to be sure of that is for it to be this.
pub fn sort_by_key(groups: &mut [Group]) {
    groups.sort_by(|a, b| match (&a.key, &b.key) {
        (Some(x), Some(y)) => x.cmp(y),
        (Some(_), None) => core::cmp::Ordering::Less,
        (None, Some(_)) => core::cmp::Ordering::Greater,
        (None, None) => a.at.cmp(&b.at),
    });
}

/// Runs a `GroupBy`'s aggregate over one group.
///
/// The aggregate was planned against a placeholder bitmap because no group existed yet; here
/// its own bitmap is replaced by the group's records and only its shape is reused.
/// Whether an aggregate over this field has to come back signed.
///
/// A field that is not there answers `false` and the call below fails with the proper "unknown
/// field" error rather than with something about signs.
fn is_signed<P: Pager + Sync>(db: &DbRead<'_, P>, table: &str, field: &str) -> bool {
    db.catalog()
        .lookup(table)
        .and_then(|t| db.catalog().field(t.id, field))
        .is_some_and(|f| f.kind.is_signed())
}

/// Whether an aggregate over this field is folded from values rather than from bit planes.
fn is_float<P: Pager + Sync>(db: &DbRead<'_, P>, table: &str, field: &str) -> bool {
    db.catalog()
        .lookup(table)
        .and_then(|t| db.catalog().field(t.id, field))
        .is_some_and(|f| f.kind.is_float())
}

fn aggregate_over<P: Pager + Sync>(
    db: &DbRead<'_, P>,
    table: &str,
    aggregate: &Plan,
    group: &Matches,
) -> Result<Value> {
    Ok(match aggregate {
        Plan::Count { .. } => Value::Count(group.cardinality()),

        // Split on the field's kind rather than on the call, because the sign is a property of
        // where the value is stored and not of what is being asked of it. Doing it here means
        // there is exactly one place that knows a signed aggregate exists - a `Sum` reaching
        // storage has already been routed.
        // A float is split off before the signed kinds for the same reason they are split off
        // from the plain ones: what a total costs is a property of how the value is stored, and
        // a float's is a fold over values rather than a walk over bit planes.
        Plan::Sum { field, .. } if is_float(db, table, field) => {
            Value::RealSum(db.sum_float_where(table, field, group)?)
        }
        Plan::Min { field, .. } if is_float(db, table, field) => {
            Value::RealExtreme(db.min_float_where(table, field, group)?)
        }
        Plan::Max { field, .. } if is_float(db, table, field) => {
            Value::RealExtreme(db.max_float_where(table, field, group)?)
        }

        Plan::Sum { field, .. } if is_signed(db, table, field) => {
            Value::SignedSum(db.sum_signed_where(table, field, group)?)
        }
        Plan::Min { field, .. } if is_signed(db, table, field) => {
            Value::SignedExtreme(db.min_signed_where(table, field, group)?)
        }
        Plan::Max { field, .. } if is_signed(db, table, field) => {
            Value::SignedExtreme(db.max_signed_where(table, field, group)?)
        }

        Plan::Sum { field, .. } => Value::Sum(db.sum_where(table, field, group)?),
        Plan::Min { field, .. } => Value::Extreme(db.min_where(table, field, group)?),
        Plan::Max { field, .. } => Value::Extreme(db.max_where(table, field, group)?),
        // Unreachable through the planner, which only ever puts an aggregate here. Refused
        // rather than asserted, because a plan can also be built by hand.
        _ => {
            return Err(PlanError::BadArgument {
                call: "GroupBy",
                want: "aggregate=<Count|Sum|Min|Max>",
            }
            .into())
        }
    })
}

/// One set of records, built from smaller ones.
///
/// Every branch either asks storage for a leaf or combines two answers. Nothing here
/// materialises a record id, so an intersection of two million-record sets costs a merge of
/// containers rather than a merge of lists.
fn eval<P: Pager + Sync>(db: &DbRead<'_, P>, table: &str, rows: &Rows) -> Result<Matches> {
    Ok(match rows {
        Rows::Compare { field, op, value } => db.matching(table, field, range_op(*op), *value)?,
        Rows::CompareSigned { field, op, value } => {
            db.matching_signed(table, field, range_op(*op), *value)?
        }
        // The threshold travelled as bits so the plan could keep its `Eq`; this is the one place
        // that reads it back, immediately, before anything is done with it.
        Rows::CompareFloat { field, op, bits } => {
            db.matching_float(table, field, range_op(*op), f64::from_bits(*bits))?
        }
        Rows::Key { field, value } => db.matching_key(table, field, value)?,
        Rows::KeyLike { field, pattern, fold } => {
            db.matching_key_like(table, field, pattern, *fold)?
        }
        Rows::KeyBetween { field, value, from, to } => {
            db.matching_key_between(table, field, value, *from, *to)?
        }
        Rows::Bool { field, value } => db.matching_bool(table, field, *value)?,
        Rows::All => db.all(table)?,

        // A bitmap cannot say what it does not contain, so the complement is taken against the
        // set of records that exist rather than against anything implicit.
        Rows::Not(inner) => db.all(table)?.andnot(&eval(db, table, inner)?),

        Rows::Difference(a, b) => eval(db, table, a)?.andnot(&eval(db, table, b)?),

        // An intersection that has already emptied can never come back, so the remaining
        // reads are skipped. A union can, so they are not - the same short circuit applied to
        // both would silently drop every part after the first empty one.
        Rows::Intersect(parts) => fold(db, table, parts, Matches::and, StopWhenEmpty::Yes)?,
        Rows::Union(parts) => fold(db, table, parts, Matches::or, StopWhenEmpty::No)?,

        // **One AND per container, and never a record decoded.** A container holds 65,536
        // consecutive record ids, so "every `stride`th id" is the same pattern in every
        // container - built once here and intersected with each one the inner set touches.
        //
        // That is also why the stride is a power of two: the pattern only repeats when it
        // divides the container, and a stride that did not would be right in one container and
        // wrong in the next. `big_plan` refuses the rest at the call.
        Rows::Sample { inner, stride } => sample(eval(db, table, inner)?, *stride),
    })
}

/// One record in every `stride`, by record id.
///
/// **The unit is the id's low bits, not a block of ids**, and that is the whole of why this is
/// a sample rather than a slice. Ids are handed out in write order, so every eighth *container*
/// would be every eighth window of time - a subset that leans whichever way the data drifted.
/// Every eighth *id* is spread evenly through each of those windows.
///
/// Costs one container-sized AND per container the set touches. Nothing is decoded, so a
/// sampled count is as cheap as the count it came from.
fn sample(matches: Matches, stride: u32) -> Matches {
    // A container is addressed by a `u16` offset, so this is its width - written from the type
    // rather than as a literal, because the two cannot then drift apart.
    const WIDTH: u64 = u16::MAX as u64 + 1;
    let stride = u64::from(stride.max(2));

    // **Keyed by phase, not by container.** A container beginning at id `c * 65536` starts
    // part-way through the repeating pattern unless the stride divides the width, and the
    // offset into it is arithmetic. There are at most `stride` distinct phases and usually far
    // fewer, so this caches what would otherwise be rebuilt per container - and the common case
    // of a stride that does divide the width has exactly one entry.
    let mut masks: BTreeMap<u64, big_db::Container> = BTreeMap::new();

    let mut out = Matches::new();
    // Collected first because the loop reads the same `matches` it is iterating the shards of.
    let shards: Vec<_> = matches.shards().collect();
    for shard in shards {
        let Some(rows) = matches.get(shard) else { continue };
        let mut per_shard = big_db::RowSet::new();
        for (slot, _) in rows.iter() {
            // The id of this container's first record, modulo the stride: where in the pattern
            // it begins. `slot` is the container's index within the shard, and a shard is
            // `SHARD_WIDTH` records, so the two together give the record id.
            let first = shard.wrapping_mul(big_db::SHARD_WIDTH).wrapping_add(slot * WIDTH);
            let phase = first % stride;
            // The first offset inside this container that is on the pattern.
            let start = (stride - phase) % stride;
            let mask = masks.entry(phase).or_insert_with(|| {
                big_db::Container::from_values(
                    (start..WIDTH).step_by(stride as usize).map(|v| v as u16),
                )
            });
            per_shard.insert(slot, mask.clone());
        }
        // `RowSet::insert` drops an empty container, so a shard that samples to nothing
        // contributes nothing - the same shape `Matches` already holds for an empty shard.
        out.insert(shard, rows.and(&per_shard));
    }
    out
}

/// Whether an empty accumulator can still be changed by what is left.
#[derive(Clone, Copy, PartialEq, Eq)]
enum StopWhenEmpty {
    /// Intersection: nothing can add records back.
    Yes,
    /// Union: everything can.
    No,
}

/// Combines a non-empty list left to right. The planner rejects an empty one, so there is no
/// identity element to invent here - which matters, because the identity for `Intersect` would
/// have to be "every record that exists" and that is a read, not a constant.
fn fold<P: Pager + Sync>(
    db: &DbRead<'_, P>,
    table: &str,
    parts: &[Rows],
    combine: impl Fn(&Matches, &Matches) -> Matches,
    stop: StopWhenEmpty,
) -> Result<Matches> {
    let mut it = parts.iter();
    let first = it.next().ok_or(ExecError::Plan(PlanError::Arity {
        call: "Intersect or Union",
        want: "at least one bitmap",
        got: 0,
    }))?;

    let mut acc = eval(db, table, first)?;
    for part in it {
        if stop == StopWhenEmpty::Yes && acc.is_empty() {
            break;
        }
        acc = combine(&acc, &eval(db, table, part)?);
    }
    Ok(acc)
}

fn range_op(op: CmpOp) -> RangeOp {
    match op {
        CmpOp::Gt => RangeOp::Gt,
        CmpOp::Ge => RangeOp::Ge,
        CmpOp::Lt => RangeOp::Lt,
        CmpOp::Le => RangeOp::Le,
        CmpOp::Eq => RangeOp::Eq,
        CmpOp::Ne => RangeOp::Ne,
    }
}

/// The start of the `unit` that a stored count falls in, in that column's own units.
///
/// **The two temporal classes count different things**, so the calendar is asked in the units the
/// field stores rather than converting both to one: a `DATE` is whole days and a `DATETIME` is
/// seconds, and rounding a day count through seconds would be two conversions where the calendar
/// question is the same one. `big_civil` keeps both spellings for exactly this reason.
fn truncate_to(value: i64, unit: big_civil::Unit, of: TimeUnit) -> i64 {
    match of {
        TimeUnit::Days => big_civil::truncate_days(value, unit),
        TimeUnit::Seconds => big_civil::truncate(value, unit),
    }
}

/// The start of the `unit` after the one a stored count falls in. The other end of a bucket.
fn next_after(value: i64, unit: big_civil::Unit, of: TimeUnit) -> i64 {
    match of {
        TimeUnit::Days => big_civil::next_days(value, unit),
        TimeUnit::Seconds => big_civil::next(value, unit),
    }
}

/// Every set of records one level's values make, inside a set the caller already narrowed to.
///
/// **The one place a column's values become groups**, whichever way it has them: a keyed column
/// has a dictionary to walk and a bit-sliced temporal one has an ordering to cut. Written once
/// because both groupings need it - a bucket grouping is this over one level, and a tuple
/// grouping is this once per level - and because mixing the two in one statement then costs
/// nothing: `GROUP BY country, date_trunc('month', ts)` asks each level the same question.
fn expand<P: Pager + Sync>(
    db: &DbRead<'_, P>,
    table: &str,
    level: &Level,
    within: &Matches,
) -> Result<Vec<(GroupAt, Option<String>, Matches)>> {
    match level {
        Level::Keyed { field } => Ok(db
            .group_matches(table, field, within)?
            .into_iter()
            .map(|(row, hits)| {
                (GroupAt::Row(row), db.row_key(table, field, row).map(str::to_string), hits)
            })
            .collect()),
        Level::Bucket { field, unit, max_buckets } => {
            // The column's own units, which decide whether the calendar is asked in days or in
            // seconds. Taken from the mapping the planner used, so the two cannot disagree about
            // what a `DATE` counts.
            let unit_of = match CatalogSchema(db.catalog()).field_class(table, field) {
                Some(FieldClass::Temporal { unit }) => unit,
                // The planner refused every other class before this plan was built.
                _ => unreachable!("a bucket level is planned only over a temporal column"),
            };
            let (lo, hi) = (
                db.min_signed_where(table, field, within)?,
                db.max_signed_where(table, field, within)?,
            );
            // No record here holds a value, so there is nothing to bucket. Not an empty first
            // bucket: a record with no value is in no bucket at all.
            let (Some(lo), Some(hi)) = (lo, hi) else { return Ok(Vec::new()) };

            // **The span is counted before any of it is read.** What this costs is one range read
            // per bucket the values *span*, not per bucket that turns out to hold something - two
            // records two years apart still walk every month between them. So the budget is
            // checked on the calendar alone, and a query past it is refused having read nothing.
            let first = truncate_to(lo, *unit, unit_of);
            let mut span = 0usize;
            let mut edge = first;
            while edge <= hi {
                span += 1;
                if span > *max_buckets {
                    return Err(ExecError::TooManyBuckets {
                        field: field.clone(),
                        limit: *max_buckets,
                    });
                }
                let next = next_after(edge, *unit, unit_of);
                // Guards the end of the calendar, where the next boundary cannot be past this one
                // and the walk would not terminate.
                if next <= edge {
                    break;
                }
                edge = next;
            }

            let mut out = Vec::with_capacity(span);
            let mut start = first;
            while start <= hi {
                let next = next_after(start, *unit, unit_of);
                let hits = db
                    .matching_signed(table, field, RangeOp::Ge, start)?
                    .and(&db.matching_signed(table, field, RangeOp::Lt, next)?)
                    .and(within);
                // An empty bucket is not a group. A month nothing happened in is not something
                // `GROUP BY` answers about, and keeping it would make a sparse column cost its
                // whole span in rows as well as in reads.
                if !hits.is_empty() {
                    out.push((GroupAt::Bucket { start, unit: unit_of }, None, hits));
                }
                if next <= start {
                    break;
                }
                start = next;
            }
            Ok(out)
        }
    }
}

/// Tuples in key order, outermost column first.
///
/// Public for the reason [`rank_top_n`] is: the coordinator's merge has to produce the order this
/// produces, and the only way to be sure of that is for it to be this.
pub fn sort_tuples(tuples: &mut [Tuple]) {
    tuples.sort_by(|a, b| cmp_tuple_keys(&a.keys, &b.keys));
}

/// Two tuples' keys, compared level by level.
///
/// A zip rather than a fixed tuple of fields, which is what makes the arity a number rather than
/// a shape: the six-field comparison a pair needed became a nine-field one for a triple, and this
/// is that generalised. Within a level the rule is [`sort_by_key`]'s - named groups by name,
/// unnamed after them by identity - so a tuple grouping orders exactly as a nesting of single
/// ones would.
pub fn cmp_tuple_keys(a: &[GroupKey], b: &[GroupKey]) -> core::cmp::Ordering {
    a.iter()
        .zip(b)
        .map(|(x, y)| match (&x.key, &y.key) {
            (Some(p), Some(q)) => p.cmp(q),
            (Some(_), None) => core::cmp::Ordering::Less,
            (None, Some(_)) => core::cmp::Ordering::Greater,
            (None, None) => x.at.cmp(&y.at),
        })
        .find(|o| o.is_ne())
        .unwrap_or(core::cmp::Ordering::Equal)
}
