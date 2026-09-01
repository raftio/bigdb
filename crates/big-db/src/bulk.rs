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

//! Loading a dataset into fragments that are empty, by writing each of them exactly once.
//!
//! **What this is not.** It is not a faster way to build one fragment: the tree is already
//! built bottom-up whenever it has no root - `put_many` sees `None` and calls `build`, which
//! fills the leaves in order and stacks branches on top without reading a page. That was true
//! before this file existed and remains the whole of the tree-level story.
//!
//! **What was missing is the scheduling.** [`Ingest`] buffers records and decides its own commit
//! points, but it commits in *arrival* order: a caller handing over a million records that span
//! sixty-four shards fills its buffer with a slice of all sixty-four, commits, and does it again.
//! Every commit after the first finds each fragment already rooted, so it descends, reads the
//! leaves back and rewrites the path - a hundred times over, per fragment. The buffer made the
//! commits fewer; it could not make them disjoint.
//!
//! This reorders the work so they are. Facts are grouped by fragment first and committed
//! **fragment-major**, so every fragment is touched by exactly one commit and therefore built by
//! exactly one bottom-up pass. Nothing is read back because there is never anything to read.
//!
//! The cost is that the whole load is held in memory before any of it is written, which is the
//! honest limit of this approach and the reason [`Ingest`] is still the right tool for a stream.
//! Use this for a first load whose size you know; use `Ingest` for one that does not end.
//!
//! [`Ingest`]: crate::Ingest

use crate::catalog::{FieldDef, FieldKind, TableId, EXISTS_FIELD, STANDARD_VIEW};
use crate::db::Db;
use crate::error::{DbError, Result};
use big_engine::bitmap::field::bsi::EXISTS_ROW;
use big_engine::bitmap::field::{BoolField, Bsi};
use big_engine::bitmap::FragmentKey;
use big_engine::{shard_of, RecordId, RowId};
use big_pager::PagerMut;
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// A value waiting to be placed, in the form the fragment will take it.
enum Value {
    /// Already biased if the field is signed: the sign convention is applied where the field is
    /// resolved, so nothing below here has to know the kind.
    Stored(u64),
    Row(RowId),
    /// A key that has not been interned yet, held as an index into [`BulkLoad::keys`].
    ///
    /// Interning mutates the catalog, so it cannot happen while facts are merely being
    /// collected - but *storing* the key need not wait, and storing it inline as a `String` was
    /// one heap allocation per fact on a path that holds the entire load in memory at once.
    /// A set field draws from a small alphabet by definition, so the same few hundred strings
    /// were being allocated millions of times over.
    Key(u32),
}

/// One fact, resolved as far as it can be without touching the catalog.
struct Fact {
    field: crate::catalog::FieldId,
    record: RecordId,
    value: Value,
}

/// What a setter needs from a field definition, in a form that fits in a register pair.
///
/// [`Catalog::field`] hands back a `&FieldDef`, which owns a name and a granularity list, so
/// caching one meant cloning it - two allocations - on every one of the millions of setter
/// calls a bulk load is made of, to learn three numbers that cannot change while it runs.
#[derive(Clone, Copy)]
struct FieldRef {
    id: crate::catalog::FieldId,
    bit_depth: u32,
}

/// How many pages one commit may carry before the next fragment starts a new one.
///
/// The transaction holds every page it dirties in memory until it commits, so a load of any
/// size in a single commit would trade the memory problem this avoids for a worse one. Fragments
/// are independent and each is finished by one commit either way, so splitting between them
/// costs nothing: the guarantee is "one commit per fragment", never "one commit per load".
const PAGES_PER_COMMIT: usize = 4_096;

/// Facts accumulated for one load.
///
/// Like [`Ingest`], `finish` is not optional and a dropped loader discards what it holds:
/// committing from a destructor turns a failed write into a silent one.
///
/// [`Ingest`]: crate::Ingest
pub struct BulkLoad<'db, P: PagerMut> {
    db: &'db Db<P>,
    table: TableId,
    table_name: String,
    /// Interned once per name pair, because a load names a handful of fields and holds millions
    /// of facts.
    fields: BTreeMap<String, FieldRef>,
    facts: Vec<Fact>,
    /// Every distinct key the load names, held once each. See [`Value::Key`].
    keys: Vec<String>,
    key_index: HashMap<String, u32>,
    records: BTreeSet<RecordId>,
    /// Whether `finish` was called. The destructor's job is catching a loader that was *dropped*
    /// holding facts, which is a caller who forgot; a `finish` that failed is a caller who was
    /// told, and asserting at them would turn a reported error into an abort.
    finished: bool,
}

impl<'db, P: PagerMut> BulkLoad<'db, P> {
    pub(crate) fn new(db: &'db Db<P>, table: &str) -> Result<Self> {
        let t = db
            .catalog()
            .table(table)
            .map(|t| t.id)
            .ok_or_else(|| DbError::UnknownTable(table.to_string()))?;

        // **Refused rather than written wrong.** Everything below writes *bitmap fragments* and
        // nothing else - `w.fragment(key)` and `set_bits`, and there is not a single column write
        // in this file. An engine that stores anything else is loaded incompletely, and the shape
        // of the failure is the worst one available: `finish` returns the number of records it
        // loaded and the table reads back empty.
        //
        // Measured, on a thousand records:
        //
        //   engine            loaded  count_all  column_count  matching >= k
        //   bitmap              1000       1000             0            500   correct
        //   bitmap+columnar     1000       1000             0            500   columns missing
        //   columnar            1000       1000             0              0   wrong answer
        //
        // The middle row is the dangerous one. Its answers are right because they come from the
        // index, so nothing looks broken - but its segments are empty, so a query the planner
        // routes to a scan answers wrongly, and a file measured for size is missing half of what
        // the engine is supposed to store.
        //
        // **What is refused here is the two rows that answer wrongly, and not the middle one.**
        // A `bitmap+columnar` load is incomplete rather than incorrect, and refusing it would be
        // a breaking change to a path that has always been offered - so it is recorded (see
        // `crates/big-db/tests/bulk.rs`) and left for a deliberate decision rather than taken
        // now. A caller who needs its columns should use `Ingest`.
        //
        // The refusal rather than the fix because the fix is not small: a bulk load that wrote
        // columns as well would need a second scheduler beside this one, and a part-based one a
        // third. `Ingest` writes every engine correctly and is grouped the same way, so there is
        // a right answer to point at.
        let engine = db.catalog().table_by_id(t).map(|x| x.engine).unwrap_or_default();
        if !engine.has_bitmap() {
            return Err(DbError::EngineCannotAnswer {
                table: table.to_string(),
                what: "a bulk load, which writes only bitmap fragments",
                engine: engine.as_str(),
                instead: "load it with `Db::ingest`, which writes everything the engine stores",
            });
        }

        Ok(Self {
            db,
            table: t,
            table_name: table.to_string(),
            fields: BTreeMap::new(),
            facts: Vec::new(),
            keys: Vec::new(),
            key_index: HashMap::new(),
            records: BTreeSet::new(),
            finished: false,
        })
    }

    pub fn set_int(&mut self, field: &str, record: RecordId, value: u64) -> Result<()> {
        let def = self.field(field, |k| k.is_bsi() && !k.is_signed(), "int")?;
        let declared = if def.bit_depth == 0 { 64 } else { def.bit_depth };
        if 64 - value.leading_zeros() > declared {
            return Err(big_engine::bitmap::field::FieldError::ValueTooWide {
                value,
                bit_depth: declared,
            }
            .into());
        }
        self.push(def.id, record, Value::Stored(value))
    }

    pub fn set_signed(&mut self, field: &str, record: RecordId, value: i64) -> Result<()> {
        let def = self.field(field, FieldKind::is_signed, "signed int")?;
        let declared = if def.bit_depth == 0 { 64 } else { def.bit_depth };
        let stored =
            crate::signed::encode(value, declared).ok_or(DbError::SignedValueOutOfRange {
                value,
                min: crate::signed::min_value(declared),
                max: crate::signed::max_value(declared),
            })?;
        self.push(def.id, record, Value::Stored(stored))
    }

    pub fn set_bool(&mut self, field: &str, record: RecordId, value: bool) -> Result<()> {
        let def = self.field(field, |k| k == FieldKind::Bool, "bool")?;
        self.push(def.id, record, Value::Row(BoolField::row_of(value)))
    }

    /// A set field's key.
    ///
    /// **Set fields only.** A mutex has to read its shadow to find the value it is replacing,
    /// and a time quantum writes into one extra view per granularity - both are shapes this
    /// path deliberately does not model, because getting either subtly wrong would produce a
    /// fragment that reads back plausibly and is not what was written. `Ingest` handles both
    /// correctly, and the refusal here says so.
    pub fn set_key(&mut self, field: &str, record: RecordId, value: &str) -> Result<()> {
        let def = self.field(field, |k| k == FieldKind::Set, "set")?;
        let value = self.intern_text(value);
        self.push(def.id, record, Value::Key(value))
    }

    /// Records held but not yet written.
    pub fn buffered(&self) -> usize {
        self.facts.len()
    }

    fn field(
        &mut self,
        name: &str,
        ok: impl Fn(FieldKind) -> bool,
        expected: &'static str,
    ) -> Result<FieldRef> {
        if let Some(def) = self.fields.get(name) {
            return Ok(*def);
        }
        let catalog = self.db.catalog();
        let def: &FieldDef = catalog.field(self.table, name).ok_or_else(|| {
            DbError::UnknownField { table: self.table_name.clone(), field: name.to_string() }
        })?;
        if !ok(def.kind) {
            return Err(DbError::WrongFieldKind { field: name.to_string(), expected });
        }
        let r = FieldRef { id: def.id, bit_depth: def.bit_depth };
        drop(catalog);
        self.fields.insert(name.to_string(), r);
        Ok(r)
    }

    /// Holds one copy of a key and hands back the index the facts refer to it by.
    fn intern_text(&mut self, key: &str) -> u32 {
        if let Some(i) = self.key_index.get(key) {
            return *i;
        }
        let i = self.keys.len() as u32;
        self.keys.push(key.to_string());
        self.key_index.insert(key.to_string(), i);
        i
    }

    fn push(
        &mut self,
        field: crate::catalog::FieldId,
        record: RecordId,
        value: Value,
    ) -> Result<()> {
        self.records.insert(record);
        self.facts.push(Fact { field, record, value });
        Ok(())
    }

    /// Writes everything, and reports how many records were loaded.
    ///
    /// Refuses outright if any fragment it would write already has data. Loading into one is a
    /// *merge*, which the ordinary write path already does correctly; doing it here would either
    /// read the fragment back - giving up the only thing this buys - or strand what was there.
    /// Neither is worth a silent choice, so the caller is told.
    pub fn finish(mut self) -> Result<u64> {
        self.finished = true;
        if self.facts.is_empty() {
            return Ok(0);
        }

        // Keys first, in their own transaction. Interning mutates the catalog, and a row id has
        // to exist before the fact that uses it can be placed.
        self.intern_keys()?;

        let plan = self.plan();
        // Before anything is written. A refusal that had already committed half the load would
        // be worse than no refusal at all, and the check is the reason nothing is read later.
        self.refuse_occupied(&plan)?;

        let loaded = self.records.len() as u64;
        self.write(plan)?;
        Ok(loaded)
    }

    /// Turns every key into a row id, in one commit.
    ///
    /// **Once per distinct `(field, key)`, not once per fact.** Interning a key already known is
    /// a lookup that returns the id it returned last time, so calling it per fact asked the
    /// catalog the same question millions of times and wrote down millions of identical answers.
    /// A load names a handful of fields and, being keyed, a bounded alphabet of values; the pair
    /// set is that product and nothing larger.
    fn intern_keys(&mut self) -> Result<()> {
        let mut wanted: BTreeSet<(crate::catalog::FieldId, u32)> = BTreeSet::new();
        for f in &self.facts {
            if let Value::Key(i) = &f.value {
                wanted.insert((f.field, *i));
            }
        }
        if wanted.is_empty() {
            return Ok(());
        }

        let db = self.db;
        let mut w = db.write();
        let mut rows: BTreeMap<(crate::catalog::FieldId, u32), RowId> = BTreeMap::new();
        for (field, i) in wanted {
            let row = w.catalog_mut().keys.intern(self.table, field, &self.keys[i as usize])?;
            rows.insert((field, i), row);
        }
        w.commit()?;

        for f in &mut self.facts {
            if let Value::Key(i) = &f.value {
                f.value = Value::Row(rows[&(f.field, *i)]);
            }
        }
        Ok(())
    }

    /// Every fragment this load will write, and the bits each of them gets.
    ///
    /// Grouped by fragment rather than by record, which is the whole point: this is what makes a
    /// commit able to finish a fragment instead of visiting all of them.
    fn plan(&self) -> BTreeMap<FragmentKey, Group> {
        let mut out: BTreeMap<FragmentKey, Group> = BTreeMap::new();
        for f in &self.facts {
            let key = FragmentKey {
                table: self.table,
                field: f.field,
                view: STANDARD_VIEW,
                shard: shard_of(f.record),
            };
            let group = out.entry(key).or_default();
            match &f.value {
                // Last write wins per record, exactly as it would inside one transaction.
                Value::Stored(v) => {
                    group.values.insert(f.record, *v);
                }
                Value::Row(row) => {
                    group.bits.insert((*row, f.record));
                }
                Value::Key(_) => unreachable!("keys are interned before planning"),
            }
        }

        // The exists field is written like any other, and gets its own fragment per shard.
        for record in &self.records {
            let key = FragmentKey {
                table: self.table,
                field: EXISTS_FIELD,
                view: STANDARD_VIEW,
                shard: shard_of(*record),
            };
            out.entry(key).or_default().bits.insert((EXISTS_ROW, *record));
        }
        out
    }

    /// Refuses the load if anything it would write is already there.
    fn refuse_occupied(&self, plan: &BTreeMap<FragmentKey, Group>) -> Result<()> {
        for key in plan.keys() {
            if self.db.store().roots().get(key).is_some() {
                return Err(DbError::BulkLoadNotEmpty {
                    table: self.table_name.clone(),
                    shard: key.shard,
                });
            }
        }
        Ok(())
    }

    /// One commit per group of fragments, each fragment finished by exactly one of them.
    fn write(&mut self, plan: BTreeMap<FragmentKey, Group>) -> Result<()> {
        let mut w = self.db.write();
        let mut pages = 0usize;

        for (key, group) in plan {
            // The depth is known before a single bit is placed, because every value for this
            // fragment is already in hand. The ordinary path cannot do that - it learns the
            // depth as records arrive - which is why it has to expand at whatever depth the
            // transaction ends on.
            let depth = group.values.values().copied().fold(1u32, |d, v| d.max(width(v)));
            let mut set: Vec<(RowId, RecordId)> = Vec::new();
            let mut clear = Vec::new();
            if !group.values.is_empty() {
                let meta = w.catalog_mut().fragment_mut(key);
                for v in group.values.values() {
                    meta.observe(*v);
                }
                let bsi = Bsi::new(depth);
                for (record, value) in &group.values {
                    bsi.bits_for(*record, *value, &mut set, &mut clear)?;
                }
            }
            set.extend(group.bits.iter().copied());

            // `bits_for` reports the zero planes of every value as bits to clear. On a fragment
            // that starts empty they are already clear, so they are dropped rather than written:
            // `set_bits` only ever turns bits on, and passing the clears would cost a pass over
            // every plane of every value to change nothing.
            drop(clear);

            pages += set.len().div_ceil(2);
            let mut f = w.fragment(key);
            f.set_bits(w.txn(), set)?;
            w.save_fragment(key, f);

            if pages >= PAGES_PER_COMMIT {
                w.commit()?;
                w = self.db.write();
                pages = 0;
            }
        }
        w.commit()?;
        Ok(())
    }
}

/// Bits destined for one fragment.
#[derive(Default)]
struct Group {
    /// Bit-sliced values, unexpanded: the depth is not known until every value is in.
    values: BTreeMap<RecordId, u64>,
    /// Everything addressed by a row directly.
    bits: BTreeSet<(RowId, RecordId)>,
}

/// Planes a value needs. One even for zero, because a bit-sliced index of depth zero has no
/// exists row to hang the value off.
fn width(v: u64) -> u32 {
    (64 - v.leading_zeros()).max(1)
}

impl<P: PagerMut> Drop for BulkLoad<'_, P> {
    fn drop(&mut self) {
        debug_assert!(
            self.finished || self.facts.is_empty(),
            "BulkLoad dropped holding {} unwritten facts - call finish()",
            self.facts.len()
        );
    }
}
