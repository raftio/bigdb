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

use big_db::catalog::{Catalog, FieldKind};
use big_db::{DbRead, Matches, RangeOp, RecordId, RowId};
use big_pager::Pager;
use big_plan::{CmpOp, FieldClass, Keyed, Plan, PlanError, Rows, Schema, TimeUnit};

pub mod error;
pub use error::{ExecError, Result};

/// One group of a `Distinct`, `TopN` or `GroupBy`.
#[derive(Clone, Debug)]
pub struct Group {
    pub row: RowId,
    /// The string the row was interned from, when the field has one.
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

/// One group of a `GroupByPair`: a value of each column, and what the records holding both
/// measured.
#[derive(Clone, Debug)]
pub struct Pair {
    /// The left column's row.
    pub left: Group,
    /// The right column's row. Its `value` is the pair's, and the left one's is not read.
    pub right: Group,
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
    /// One group per combination of two keyed columns that any record holds both of.
    Pairs(Vec<Pair>),
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

    pub fn as_pairs(&self) -> Option<&[Pair]> {
        match self {
            Self::Pairs(p) => Some(p),
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
                    row,
                    key: db.row_key(table, field, row).map(str::to_string),
                    value: Box::new(aggregate_over(db, table, aggregate, &hits)?),
                });
            }
            sort_by_key(&mut out);
            Value::Groups(out)
        }

        // One grouping per value of the left column, over the records holding that value.
        //
        // **Not a composite key.** Nothing here materialises a pair of rows: the records
        // holding a left value are a set, and grouping *those* by the right column is the
        // ordinary grouping the engine already does. The cost is therefore one pass over the
        // right column per value of the left one, which is what `left_max` bounds - and why
        // the bound is in the plan rather than applied to the answer.
        Plan::GroupByPair { rows, left, right, aggregate, left_max, .. } => {
            let matched = eval(db, table, rows)?;
            let outer = db.group_matches(table, left, &matched)?;
            // Refused rather than truncated: see `ExecError::TooManyGroups`.
            if outer.len() > *left_max {
                return Err(ExecError::TooManyGroups {
                    field: left.clone(),
                    found: outer.len(),
                    limit: *left_max,
                });
            }
            let mut out = Vec::new();
            for (row, hits) in outer {
                let left_group = Group {
                    row,
                    key: db.row_key(table, left, row).map(str::to_string),
                    // The left half carries no number of its own: the pair's number is the
                    // right half's, measured over the records both hold.
                    value: Box::new(Value::Count(hits.cardinality())),
                };
                for (inner, both) in db.group_matches(table, right, &hits)? {
                    out.push(Pair {
                        left: left_group.clone(),
                        right: Group {
                            row: inner,
                            key: db.row_key(table, right, inner).map(str::to_string),
                            value: Box::new(aggregate_over(db, table, aggregate, &both)?),
                        },
                    });
                }
            }
            sort_pairs(&mut out);
            Value::Pairs(out)
        }

        // The one arm that reads values back. Bounded by the plan's own limit rather than by a
        // shape applied afterwards: a projection's cost is a point read per record per column,
        // so the cut has to happen before the reads, not after them. No limit is a full scan,
        // which is the caller having asked for one - see [`Plan::Project`].
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

/// Orders pairs the way a `GroupByPair` answers them: by the left key, then the right.
///
/// Public for the reason [`sort_by_key`] is: the coordinator has to produce this order after
/// merging, and two implementations of one ordering would be two answers depending on how many
/// nodes were asked.
pub fn sort_pairs(pairs: &mut [Pair]) {
    pairs.sort_by(|a, b| {
        key_order(&a.left)
            .cmp(&key_order(&b.left))
            .then(key_order(&a.right).cmp(&key_order(&b.right)))
    });
}

/// A group's place in key order: named groups by name, unnamed after them by row.
fn key_order(g: &Group) -> (bool, Option<&str>, RowId) {
    (g.key.is_none(), g.key.as_deref(), g.row)
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
        bn.cmp(&an).then_with(|| a.key.cmp(&b.key)).then(a.row.cmp(&b.row))
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
            row,
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
        (None, None) => a.row.cmp(&b.row),
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
    })
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
