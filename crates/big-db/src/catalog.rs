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

//! Schema and per-fragment metadata, stored as fixed-width records in one chain.
//!
//! Interning ids here is what makes renaming a table free: the name lives in one record, and
//! not a single byte of data mentions it.

use crate::error::{DbError, Result};
use big_field::Granularity;
use big_fragment::FragmentKey;
use big_keys::KeyStore;
use big_pager::{kind, CATALOG_ENTRY_BYTES};
use std::collections::BTreeMap;

pub type TableId = u32;
pub type FieldId = u32;
pub type ViewId = u32;

// The numbers themselves live in `big-page`, next to the record layout, because `big-keys`
// writes into the same stream and neither crate can see the other's constants.
pub const KIND_TABLE: u8 = kind::TABLE;
pub const KIND_FIELD: u8 = kind::FIELD;
pub const KIND_VIEW: u8 = kind::VIEW;
pub const KIND_FRAGMENT: u8 = kind::FRAGMENT;
pub const KIND_SEQ: u8 = kind::SEQ;

const NAME_AT: usize = 24;

/// Longest name a catalog entry can hold.
///
/// Names longer than this are refused rather than silently truncated, the same rule and the
/// same budget as `big_keys::MAX_KEY_LEN`. Truncating merges two different names into one and,
/// when the cut lands inside a character, destroys the entry outright on the next reload.
pub const MAX_NAME_LEN: usize = CATALOG_ENTRY_BYTES - NAME_AT;

/// The default view every field has; time quantum views are extra ones alongside it.
///
/// Never handed out by `intern_view`: a named view that collided with it would be read back as
/// the standard view, and a time range would silently answer with every record ever written.
pub const STANDARD_VIEW: ViewId = 0;

/// First id `intern_view` may allocate.
const FIRST_NAMED_VIEW: ViewId = STANDARD_VIEW + 1;

/// How many ids at the top of the range the allocator never hands out.
///
/// Reserved rather than merely unlikely: a named view colliding with one of these would be read
/// back *as* the reserved one - a time range answering out of a mutex's shadow, or out of a
/// column segment - and nothing underneath would be able to tell. The reserved ids themselves
/// live next to the code that reads them, in `crate::db`; what lives here is the promise that
/// the allocator will not reach them.
pub const RESERVED_VIEWS: u32 = 2;
/// Reserved field marking which record ids exist at all. Without it `NOT` would include every
/// record that was never written.
pub const EXISTS_FIELD: FieldId = u32::MAX;

/// What a table writes to disk for every fact it takes.
///
/// A property of the table rather than of a field, because it is a decision about *where the
/// answers come from* and a table whose columns disagreed about that would need a plan per
/// column. Chosen once, at creation, and never changed: switching would mean rewriting every
/// fragment the table owns, which is a migration rather than a setting.
///
/// **Zero is [`TableEngine::Bitmap`], and that is not the default for a new table.** Every file
/// written before this existed carries a zero in the byte that now holds the engine, and those
/// files are bitmap-only - so the decoded default and the created default are deliberately
/// different numbers. See [`TableEngine::default`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum TableEngine {
    /// Bitmaps and bit-sliced indexes, and nothing else. What every table was before there was
    /// a choice.
    Bitmap = 0,
    /// Both. The index answers what an index is good at, the columns answer what a scan is good
    /// at, and the planner picks. Costs a second copy of every fact.
    BitmapColumnar = 1,
    /// Column segments, plus the existence row and nothing else.
    ///
    /// The existence row stays even here, and deliberately: it is one bit per record, and it is
    /// what `Not`, `count(*)` and the record cursor stand on. Dropping it would cost far more
    /// than the bit it saves.
    Columnar = 2,
}

impl TableEngine {
    /// Public because the number is not private: it is what the catalog stores and what a peer
    /// is told when a table is created across a cluster, so the mapping has one definition and
    /// both readers use it.
    pub fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            0 => Self::Bitmap,
            1 => Self::BitmapColumnar,
            2 => Self::Columnar,
            _ => return None,
        })
    }

    /// Whether this engine maintains bitmap and bit-sliced fragments for declared fields.
    pub fn has_bitmap(self) -> bool {
        matches!(self, Self::Bitmap | Self::BitmapColumnar)
    }

    /// Whether this engine maintains column segments.
    pub fn has_columns(self) -> bool {
        matches!(self, Self::BitmapColumnar | Self::Columnar)
    }

    /// The name this engine is written and read as, at every surface outside the engine.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Bitmap => "bitmap",
            Self::BitmapColumnar => "bitmap+columnar",
            Self::Columnar => "columnar",
        }
    }

    /// The inverse of [`TableEngine::as_str`], for a caller holding text.
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "bitmap" => Self::Bitmap,
            "bitmap+columnar" => Self::BitmapColumnar,
            "columnar" => Self::Columnar,
            _ => return None,
        })
    }
}

impl Default for TableEngine {
    /// What a table gets when the caller does not choose.
    ///
    /// Not [`TableEngine::Bitmap`], which is what a *decoded* zero means. A caller who says
    /// nothing wants the engine that answers the widest range of questions well, and a file that
    /// says nothing predates the choice entirely - two different questions that happen to share
    /// a type.
    fn default() -> Self {
        Self::BitmapColumnar
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum FieldKind {
    Set = 0,
    Mutex = 1,
    Bool = 2,
    Int = 3,
    Decimal = 4,
    TimeQuantum = 5,
    /// A signed integer, stored in the same bit planes as [`FieldKind::Int`] under an offset
    /// binary bias. See [`crate::signed`].
    SignedInt = 6,
}

impl FieldKind {
    /// Public because the number is not private: it is what the catalog stores and what a
    /// peer is told when a field is created across a cluster, so the mapping has exactly one
    /// definition and both readers use it.
    pub fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            0 => Self::Set,
            1 => Self::Mutex,
            2 => Self::Bool,
            3 => Self::Int,
            4 => Self::Decimal,
            5 => Self::TimeQuantum,
            6 => Self::SignedInt,
            _ => return None,
        })
    }

    pub fn is_bsi(self) -> bool {
        matches!(self, Self::Int | Self::Decimal | Self::SignedInt)
    }

    /// Whether values of this kind are biased on the way in and out.
    ///
    /// Only the sign convention needs it, and it is a property of the kind rather than of the
    /// value, which is what keeps the bias out of every arithmetic path below the boundary.
    pub fn is_signed(self) -> bool {
        matches!(self, Self::SignedInt)
    }

    /// Kinds addressed by a row key rather than by a value. A mutex is one of them: it is a
    /// set field that happens to allow only one row per record.
    pub fn is_keyed(self) -> bool {
        matches!(self, Self::Set | Self::Mutex | Self::TimeQuantum)
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TableDef {
    pub id: TableId,
    pub name: String,
    /// What this table writes for every fact. Fixed at creation; see [`TableEngine`].
    pub engine: TableEngine,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct FieldDef {
    pub id: FieldId,
    pub table: TableId,
    pub name: String,
    pub kind: FieldKind,
    /// Declared ceiling. What a fragment actually uses is recorded per fragment.
    pub bit_depth: u32,
    /// Fixed scale for a decimal, so it can be stored as an integer.
    pub scale: i8,
    pub granularity: Vec<Granularity>,
}

/// Per fragment, because the same field legitimately has different depths in different shards.
/// That is fragment independence working as intended, and it is why any protocol exchanging a
/// partial BSI result has to carry the depth with it.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct FragmentMeta {
    pub bit_depth: u32,
    /// Zone map. A query for `amount > k` skips a whole shard whose max is below k without
    /// reading a single page.
    pub min: u64,
    pub max: u64,
    pub has_values: bool,
}

impl FragmentMeta {
    pub fn observe(&mut self, value: u64) {
        if !self.has_values {
            self.min = value;
            self.max = value;
            self.has_values = true;
        } else {
            self.min = self.min.min(value);
            self.max = self.max.max(value);
        }
        let needed = 64 - value.leading_zeros();
        self.bit_depth = self.bit_depth.max(needed.max(1));
    }

    /// Whether this fragment can possibly hold a value satisfying the predicate.
    pub fn may_contain(&self, lo: Option<u64>, hi: Option<u64>) -> bool {
        if !self.has_values {
            return false;
        }
        !(lo.is_some_and(|l| self.max < l) || hi.is_some_and(|h| self.min > h))
    }
}

#[derive(Clone, Default, Debug)]
pub struct Catalog {
    tables: BTreeMap<TableId, TableDef>,
    table_ids: BTreeMap<String, TableId>,
    fields: BTreeMap<(TableId, FieldId), FieldDef>,
    /// Nested rather than keyed on `(TableId, String)`, and that shape is load-bearing.
    ///
    /// A flat map cannot be probed without building the whole key, so every `field` lookup
    /// allocated a `String` just to *find* one — and `field` runs once per setter, which is once
    /// per column per record on an ingest. Nesting lets the inner map take `&str` through
    /// `String: Borrow<str>`, so the common case allocates nothing at all.
    field_ids: BTreeMap<TableId, BTreeMap<String, FieldId>>,
    views: BTreeMap<ViewId, String>,
    view_ids: BTreeMap<String, ViewId>,
    fragments: BTreeMap<FragmentKey, FragmentMeta>,
    /// Id high-water marks, persisted rather than derived.
    ///
    /// Ids used to be handed out as `max(existing) + 1`, which is correct only while nothing
    /// is ever removed. Once a table can be dropped, dropping the highest one and creating
    /// another hands the new table the old id - and with it any fragment, row key or root
    /// record that outlived the drop. The new table would silently answer with the old one's
    /// data. Counters that only ever go up make that unrepresentable.
    seq: Sequences,
    pub keys: KeyStore,
}

#[derive(Clone, Debug)]
struct Sequences {
    table: TableId,
    view: ViewId,
    /// Per table, because field ids are scoped to their table.
    field: BTreeMap<TableId, FieldId>,
}

impl Default for Sequences {
    fn default() -> Self {
        Self { table: 0, view: FIRST_NAMED_VIEW, field: BTreeMap::new() }
    }
}

/// Which counter a `KIND_SEQ` record carries. Part of the on-disk format.
mod seq_of {
    pub const TABLE: u8 = 0;
    pub const VIEW: u8 = 1;
    pub const FIELD: u8 = 2;
}

/// Panics rather than truncating if a name got this far over-long: every public entry point
/// checks first, so reaching here with one is a bug in this file and not bad input.
fn put_name(b: &mut [u8], name: &str) {
    let n = name.len();
    debug_assert!(n <= MAX_NAME_LEN, "names are checked on the way in, not on the way out");
    let n = n.min(MAX_NAME_LEN);
    b[2..4].copy_from_slice(&(n as u16).to_le_bytes());
    b[NAME_AT..NAME_AT + n].copy_from_slice(&name.as_bytes()[..n]);
}

fn check_name(name: &str) -> Result<()> {
    if name.len() > MAX_NAME_LEN {
        return Err(DbError::NameTooLong { name: name.to_string(), max: MAX_NAME_LEN });
    }
    Ok(())
}

fn get_name(b: &[u8]) -> Option<String> {
    let n = u16::from_le_bytes(b[2..4].try_into().unwrap()) as usize;
    if n > MAX_NAME_LEN {
        return None;
    }
    core::str::from_utf8(&b[NAME_AT..NAME_AT + n]).ok().map(|s| s.to_string())
}

fn gran_mask(g: &[Granularity]) -> u8 {
    g.iter().fold(0u8, |m, x| {
        m | match x {
            Granularity::Year => 1,
            Granularity::Month => 2,
            Granularity::Day => 4,
            Granularity::Hour => 8,
        }
    })
}

fn gran_from_mask(m: u8) -> Vec<Granularity> {
    let mut out = Vec::new();
    for (bit, g) in [
        (1u8, Granularity::Year),
        (2, Granularity::Month),
        (4, Granularity::Day),
        (8, Granularity::Hour),
    ] {
        if m & bit != 0 {
            out.push(g);
        }
    }
    out
}

impl Catalog {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn table(&self, name: &str) -> Option<&TableDef> {
        self.table_ids.get(name).and_then(|id| self.tables.get(id))
    }

    pub fn table_by_id(&self, id: TableId) -> Option<&TableDef> {
        self.tables.get(&id)
    }

    /// Every table, by id. The only way to enumerate a schema without knowing a name first,
    /// which is what a tool inspecting a file has to do.
    pub fn tables(&self) -> impl Iterator<Item = &TableDef> {
        self.tables.values()
    }

    pub fn field(&self, table: TableId, name: &str) -> Option<&FieldDef> {
        self.field_ids
            .get(&table)
            .and_then(|by_name| by_name.get(name))
            .and_then(|id| self.fields.get(&(table, *id)))
    }

    pub fn field_by_id(&self, table: TableId, id: FieldId) -> Option<&FieldDef> {
        self.fields.get(&(table, id))
    }

    pub fn fields_of(&self, table: TableId) -> impl Iterator<Item = &FieldDef> {
        self.fields.range((table, 0)..=(table, FieldId::MAX)).map(|(_, f)| f)
    }

    /// Day views whose names fall between two bounds, inclusive.
    ///
    /// Scans the views that exist rather than generating the days in the range, so the cost
    /// follows the data and an open-ended range is not an attempt to allocate a name per day
    /// since the epoch. Length filters out the coarser views, whose names are shorter and
    /// would otherwise sort into the middle of a multi-month range.
    pub fn day_views_between(&self, lo: Option<&str>, hi: Option<&str>) -> Vec<ViewId> {
        use core::ops::Bound;
        let start = lo.map_or(Bound::Unbounded, |s| Bound::Included(s.to_string()));
        let end = hi.map_or(Bound::Unbounded, |s| Bound::Included(s.to_string()));
        self.view_ids
            .range((start, end))
            .filter(|(name, _)| name.len() == big_field::DAY_VIEW_LEN)
            .map(|(_, id)| *id)
            .collect()
    }

    pub fn view_id(&self, name: &str) -> Option<ViewId> {
        self.view_ids.get(name).copied()
    }

    pub fn view_name(&self, id: ViewId) -> Option<&str> {
        self.views.get(&id).map(|s| s.as_str())
    }

    /// Interns a table under the default engine. See [`Catalog::intern_table_with`].
    pub fn intern_table(&mut self, name: &str) -> Result<TableId> {
        self.intern_table_with(name, TableEngine::default())
    }

    /// Idempotent for an identical declaration, an error for a contradicting one.
    ///
    /// The same rule [`Catalog::add_field`] follows, for the same reason: returning the existing
    /// id for a table declared with a different engine hands the caller a table they did not ask
    /// for - a bitmap-only one where they wrote `columnar` - and the mismatch would only surface
    /// later, as queries that are slower than they asked for or refusals they did not expect.
    pub fn intern_table_with(&mut self, name: &str, engine: TableEngine) -> Result<TableId> {
        check_name(name)?;
        if let Some(id) = self.table_ids.get(name) {
            let existing = self.tables[id].engine;
            return if existing == engine {
                Ok(*id)
            } else {
                Err(DbError::TableRedefined {
                    table: name.to_string(),
                    existing: existing.as_str(),
                    asked: engine.as_str(),
                })
            };
        }
        let id = self.seq.table;
        self.seq.table += 1;
        self.tables.insert(id, TableDef { id, name: name.to_string(), engine });
        self.table_ids.insert(name.to_string(), id);
        Ok(id)
    }

    pub fn intern_view(&mut self, name: &str) -> Result<ViewId> {
        check_name(name)?;
        if let Some(id) = self.view_ids.get(name) {
            return Ok(*id);
        }
        let id = self.seq.view;
        // The top of the range is reserved - the mutex shadow view and the column segment view.
        if id > ViewId::MAX - RESERVED_VIEWS {
            return Err(DbError::NameTaken(name.to_string()));
        }
        self.seq.view += 1;
        self.views.insert(id, name.to_string());
        self.view_ids.insert(name.to_string(), id);
        Ok(id)
    }

    /// Idempotent for an identical definition, an error for a different one.
    ///
    /// Returning the existing id for a field declared differently hands the caller a field
    /// they did not ask for - an `Int` where they wrote `Bool` - and the mismatch only shows
    /// up later as a write into the wrong shape.
    pub fn add_field(&mut self, mut def: FieldDef) -> Result<FieldId> {
        check_name(&def.name)?;
        if let Some(id) = self.field_ids.get(&def.table).and_then(|m| m.get(&def.name)) {
            let existing = &self.fields[&(def.table, *id)];
            let same = existing.kind == def.kind
                && existing.bit_depth == def.bit_depth
                && existing.scale == def.scale
                && existing.granularity == def.granularity;
            return if same {
                Ok(*id)
            } else {
                Err(DbError::FieldRedefined {
                    table: self
                        .tables
                        .get(&def.table)
                        .map_or_else(|| def.table.to_string(), |t| t.name.clone()),
                    field: def.name.clone(),
                })
            };
        }
        let slot = self.seq.field.entry(def.table).or_insert(0);
        let id = *slot;
        // `FieldId::MAX` is `EXISTS_FIELD`, which is reserved and never handed out.
        if id == EXISTS_FIELD {
            return Err(DbError::NameTaken(def.name.clone()));
        }
        *slot += 1;
        def.id = id;
        self.field_ids.entry(def.table).or_default().insert(def.name.clone(), id);
        self.fields.insert((def.table, id), def);
        Ok(id)
    }

    /// Renaming touches one record and no data at all.
    ///
    /// `Ok(false)` means there was no such table. Renaming onto a name someone else holds is
    /// an error, not a silent overwrite: the map would point at the new holder and the old one
    /// would keep its data with no way to reach it.
    pub fn rename_table(&mut self, old: &str, new: &str) -> Result<bool> {
        check_name(new)?;
        if old != new && self.table_ids.contains_key(new) {
            return Err(DbError::NameTaken(new.to_string()));
        }
        let Some(id) = self.table_ids.remove(old) else { return Ok(false) };
        self.table_ids.insert(new.to_string(), id);
        if let Some(t) = self.tables.get_mut(&id) {
            t.name = new.to_string();
        }
        Ok(true)
    }

    /// Removes a table, its fields, its row keys and its fragment metadata.
    ///
    /// Returns every `FragmentKey` that was registered under it, because those are exactly the
    /// trees the caller now has to free and the root records it has to remove. Forgetting the
    /// catalog entry without freeing them would leave the data on disk and unreachable, which
    /// is the state this whole operation exists to avoid.
    ///
    /// `None` means there was no such table.
    pub fn drop_table(&mut self, name: &str) -> Option<Vec<FragmentKey>> {
        let id = self.table_ids.remove(name)?;
        self.tables.remove(&id);

        let fields: Vec<FieldId> =
            self.fields.range((id, 0)..=(id, FieldId::MAX)).map(|((_, f), _)| *f).collect();
        for f in fields {
            if let Some(def) = self.fields.remove(&(id, f)) {
                if let Some(by_name) = self.field_ids.get_mut(&id) {
                    by_name.remove(&def.name);
                }
            }
        }
        self.keys.remove_table(id);

        // The counter for this table's fields goes too. It is scoped to an id that will never
        // be handed out again, so keeping it would only be dead weight in the catalog chain.
        self.seq.field.remove(&id);

        Some(self.take_fragments(
            FragmentKey { table: id, field: 0, view: 0, shard: 0 },
            FragmentKey { table: id, field: FieldId::MAX, view: ViewId::MAX, shard: u64::MAX },
        ))
    }

    /// Removes one field of a table, with its row keys and its fragment metadata.
    ///
    /// The reserved `EXISTS_FIELD` is not reachable through this: it is not a field anybody
    /// declared, and dropping it would make `NOT` answer with every id never written.
    pub fn drop_field(&mut self, table: TableId, name: &str) -> Option<Vec<FragmentKey>> {
        let id = self.field_ids.get_mut(&table).and_then(|m| m.remove(name))?;
        self.fields.remove(&(table, id));
        self.keys.remove_scope(table, id);

        // Every view of this field, the mutex shadow and the time quantum views included: they
        // share the field id and differ only in the view, so one range covers all of them.
        Some(self.take_fragments(
            FragmentKey { table, field: id, view: 0, shard: 0 },
            FragmentKey { table, field: id, view: ViewId::MAX, shard: u64::MAX },
        ))
    }

    /// Removes the fragments of one time quantum field for every day strictly before `cutoff`.
    ///
    /// **What it does not touch.** The view *names* stay interned, because a view id is global
    /// across every table and field: `20260830` is one id that a dozen fields may be using, and
    /// forgetting it here would make the others unreadable. What is dropped is this field's
    /// fragments in those views - the per-day copies - and nothing else. The standard view is
    /// never in the list, so the field keeps answering questions that carry no time at all.
    ///
    /// Returns the keys, for the caller to free the trees behind them: this half is catalog
    /// bookkeeping and cannot reach a transaction.
    pub fn drop_days_before(
        &mut self,
        table: TableId,
        field: FieldId,
        cutoff: &str,
    ) -> Vec<FragmentKey> {
        // `day_views_between` is inclusive at both ends, and retention is not: an operator
        // asking to keep everything from `cutoff` onwards has to still have `cutoff` itself.
        let doomed: Vec<ViewId> = self
            .day_views_between(None, Some(cutoff))
            .into_iter()
            .filter(|v| self.view_name(*v) != Some(cutoff))
            .collect();

        let mut keys = Vec::new();
        for view in doomed {
            keys.extend(self.take_fragments(
                FragmentKey { table, field, view, shard: 0 },
                FragmentKey { table, field, view, shard: u64::MAX },
            ));
        }
        keys
    }

    fn take_fragments(&mut self, lo: FragmentKey, hi: FragmentKey) -> Vec<FragmentKey> {
        let keys: Vec<FragmentKey> = self.fragments.range(lo..=hi).map(|(k, _)| *k).collect();
        for k in &keys {
            self.fragments.remove(k);
        }
        keys
    }

    pub fn fragment(&self, key: &FragmentKey) -> Option<&FragmentMeta> {
        self.fragments.get(key)
    }

    pub fn fragment_mut(&mut self, key: FragmentKey) -> &mut FragmentMeta {
        self.fragments.entry(key).or_default()
    }

    /// Every fragment of a table, across every field and every view.
    ///
    /// The reserved existence field is included: it is a fragment like any other and a record
    /// has to leave it too.
    pub fn fragments_of_table(
        &self,
        table: TableId,
    ) -> impl Iterator<Item = (&FragmentKey, &FragmentMeta)> {
        let lo = FragmentKey { table, field: 0, view: 0, shard: 0 };
        let hi = FragmentKey { table, field: FieldId::MAX, view: ViewId::MAX, shard: u64::MAX };
        self.fragments.range(lo..=hi)
    }

    pub fn fragments_of_field(
        &self,
        table: TableId,
        field: FieldId,
        view: ViewId,
    ) -> impl Iterator<Item = (&FragmentKey, &FragmentMeta)> {
        let lo = FragmentKey { table, field, view, shard: 0 };
        let hi = FragmentKey { table, field, view, shard: u64::MAX };
        self.fragments.range(lo..=hi)
    }

    pub fn encode(&self) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        for t in self.tables.values() {
            let mut b = vec![0u8; CATALOG_ENTRY_BYTES];
            b[0] = KIND_TABLE;
            // Byte 1 is where a field record keeps its kind, and it was unused here. Reusing it
            // rather than growing the record is what keeps this additive: an entry stays
            // `CATALOG_ENTRY_BYTES` wide and nothing about the chain's stride moves.
            b[1] = t.engine as u8;
            b[4..8].copy_from_slice(&t.id.to_le_bytes());
            put_name(&mut b, &t.name);
            out.push(b);
        }
        for (id, name) in &self.views {
            let mut b = vec![0u8; CATALOG_ENTRY_BYTES];
            b[0] = KIND_VIEW;
            b[4..8].copy_from_slice(&id.to_le_bytes());
            put_name(&mut b, name);
            out.push(b);
        }
        for f in self.fields.values() {
            let mut b = vec![0u8; CATALOG_ENTRY_BYTES];
            b[0] = KIND_FIELD;
            b[1] = f.kind as u8;
            b[4..8].copy_from_slice(&f.id.to_le_bytes());
            b[8..12].copy_from_slice(&f.table.to_le_bytes());
            b[12..16].copy_from_slice(&f.bit_depth.to_le_bytes());
            b[16] = f.scale as u8;
            b[17] = gran_mask(&f.granularity);
            put_name(&mut b, &f.name);
            out.push(b);
        }
        for (k, m) in &self.fragments {
            let mut b = vec![0u8; CATALOG_ENTRY_BYTES];
            b[0] = KIND_FRAGMENT;
            b[4..8].copy_from_slice(&k.table.to_le_bytes());
            b[8..12].copy_from_slice(&k.field.to_le_bytes());
            b[12..16].copy_from_slice(&k.view.to_le_bytes());
            b[16..24].copy_from_slice(&k.shard.to_le_bytes());
            b[24..28].copy_from_slice(&m.bit_depth.to_le_bytes());
            b[28] = m.has_values as u8;
            b[32..40].copy_from_slice(&m.min.to_le_bytes());
            b[40..48].copy_from_slice(&m.max.to_le_bytes());
            out.push(b);
        }
        for (which, scope, value) in self.seq_records() {
            let mut b = vec![0u8; CATALOG_ENTRY_BYTES];
            b[0] = KIND_SEQ;
            b[1] = which;
            b[4..8].copy_from_slice(&scope.to_le_bytes());
            b[8..12].copy_from_slice(&value.to_le_bytes());
            out.push(b);
        }
        out.extend(self.keys.encode());
        out
    }

    /// The counters as flat records. One per table for fields, plus one each for tables and
    /// views, so the whole thing costs a handful of entries rather than one per object.
    fn seq_records(&self) -> Vec<(u8, u32, u32)> {
        let mut out = vec![(seq_of::TABLE, 0, self.seq.table), (seq_of::VIEW, 0, self.seq.view)];
        out.extend(self.seq.field.iter().map(|(t, n)| (seq_of::FIELD, *t, *n)));
        out
    }

    /// Rebuilds a catalog from its records.
    ///
    /// Fallible for exactly one reason: a field kind this build does not recognise. Every other
    /// malformed entry here is skipped, which is right for a record that might be from a newer
    /// version and additive - the `KIND_SEQ` case the format policy was written around. A field
    /// is not additive. Skipping one makes its data unreachable while the file goes on claiming
    /// to be healthy, and every query naming it answers "unknown field" as though the schema had
    /// never had it. That is a file from a newer build being mistaken for a file missing a
    /// field, and the two call for opposite actions.
    pub fn from_entries(entries: &[Vec<u8>]) -> crate::error::Result<Self> {
        let mut c = Self::new();
        c.keys = KeyStore::from_entries(entries);

        for e in entries {
            if e.len() < CATALOG_ENTRY_BYTES {
                continue;
            }
            let rd32 = |o: usize| u32::from_le_bytes(e[o..o + 4].try_into().unwrap());
            match e[0] {
                KIND_TABLE => {
                    let (id, Some(name)) = (rd32(4), get_name(e)) else { continue };
                    // Fallible for the reason a field kind is: an engine this build does not
                    // recognise is not an additive record to skip past. Defaulting it to
                    // `Bitmap` would make a columnar table's data unreachable while the file
                    // went on claiming to be healthy, and every query against it would answer
                    // "nothing here" rather than "this file is from a newer build".
                    let Some(engine) = TableEngine::from_u8(e[1]) else {
                        return Err(crate::error::DbError::UnknownTableEngine {
                            table: id,
                            engine: e[1],
                        });
                    };
                    c.table_ids.insert(name.clone(), id);
                    c.tables.insert(id, TableDef { id, name, engine });
                }
                KIND_VIEW => {
                    let (id, Some(name)) = (rd32(4), get_name(e)) else { continue };
                    c.view_ids.insert(name.clone(), id);
                    c.views.insert(id, name);
                }
                KIND_FIELD => {
                    let Some(name) = get_name(e) else { continue };
                    let Some(kind) = FieldKind::from_u8(e[1]) else {
                        return Err(crate::error::DbError::UnknownFieldKind {
                            table: rd32(8),
                            field: rd32(4),
                            kind: e[1],
                        });
                    };
                    let def = FieldDef {
                        id: rd32(4),
                        table: rd32(8),
                        name: name.clone(),
                        kind,
                        bit_depth: rd32(12),
                        scale: e[16] as i8,
                        granularity: gran_from_mask(e[17]),
                    };
                    c.field_ids.entry(def.table).or_default().insert(name, def.id);
                    c.fields.insert((def.table, def.id), def);
                }
                KIND_FRAGMENT => {
                    let key = FragmentKey {
                        table: rd32(4),
                        field: rd32(8),
                        view: rd32(12),
                        shard: u64::from_le_bytes(e[16..24].try_into().unwrap()),
                    };
                    c.fragments.insert(
                        key,
                        FragmentMeta {
                            bit_depth: rd32(24),
                            has_values: e[28] != 0,
                            min: u64::from_le_bytes(e[32..40].try_into().unwrap()),
                            max: u64::from_le_bytes(e[40..48].try_into().unwrap()),
                        },
                    );
                }
                KIND_SEQ => {
                    let (scope, value) = (rd32(4), rd32(8));
                    match e[1] {
                        seq_of::TABLE => c.seq.table = c.seq.table.max(value),
                        seq_of::VIEW => c.seq.view = c.seq.view.max(value),
                        seq_of::FIELD => {
                            let slot = c.seq.field.entry(scope).or_insert(0);
                            *slot = (*slot).max(value);
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }

        // A file written before `KIND_SEQ` existed carries no counters, so derive them the way
        // every reader used to. Taking the max of the two rather than choosing means a file
        // that has both is governed by whichever is further along, and a hand-edited counter
        // can never hand out an id that is already in use.
        c.seq.table = c.seq.table.max(c.tables.keys().next_back().map_or(0, |m| m + 1));
        c.seq.view = c.seq.view.max(c.views.keys().next_back().map_or(FIRST_NAMED_VIEW, |m| m + 1));
        for (table, field) in c.fields.keys() {
            let slot = c.seq.field.entry(*table).or_insert(0);
            *slot = (*slot).max(field + 1);
        }
        Ok(c)
    }
}
