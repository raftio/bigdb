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
use big_engine::bitmap::field::Granularity;
use big_engine::bitmap::FragmentKey;
use big_keys::KeyStore;
use big_pager::{kind, CATALOG_ENTRY_BYTES};
use std::collections::BTreeMap;

pub type DatabaseId = u32;
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
pub const KIND_DATABASE: u8 = kind::DATABASE;

const NAME_AT: usize = 24;

/// Longest name a catalog entry can hold.
///
/// Names longer than this are refused rather than silently truncated, the same rule and the
/// same budget as `big_keys::MAX_KEY_LEN`. Truncating merges two different names into one and,
/// when the cut lands inside a character, destroys the entry outright on the next reload.
pub const MAX_NAME_LEN: usize = CATALOG_ENTRY_BYTES - NAME_AT;

/// The database a table is in when nobody said which, and the one every table written before
/// databases existed is in.
///
/// **Zero on purpose.** A table catalog entry written by an older build has zeroes in the word
/// that now names its database, so the number that word decodes to has to be the database those
/// tables have always been in. That makes the format additive in both directions with no
/// migration - the same argument [`TableEngine`]'s zero byte carries, one field over.
///
/// Never handed out by [`Catalog::intern_database`], which allocates from
/// [`FIRST_NAMED_DATABASE`], and never dropped: a table has to be in some database, and this is
/// the one that is always there to be in.
pub const DEFAULT_DATABASE: DatabaseId = 0;

/// What [`DEFAULT_DATABASE`] is called in SQL and in a listing.
///
/// `default` rather than `main` or `public` because it is ClickHouse's, and the clients that
/// introspect this surface - a BI tool populating a table tree - are the ones that already know
/// that name.
pub const DEFAULT_DATABASE_NAME: &str = "default";

/// First id [`Catalog::intern_database`] may allocate.
const FIRST_NAMED_DATABASE: DatabaseId = DEFAULT_DATABASE + 1;

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
/// Defined in `big-engine`, alongside the engines themselves, and re-exported here because the
/// catalog is what stores it. The byte on disk is [`TableEngine::code`]; **zero is
/// [`TableEngine::Bitmap`], and that is not the default for a new table** - every file written
/// before this existed carries a zero in the byte that now holds the engine, and those files
/// are bitmap-only, so the decoded default and the created default are deliberately different
/// numbers. See [`TableEngine::default`].
pub use big_engine::TableEngine;

/// What a field stores, which is what decides where a fact about it goes.
///
/// Defined in `big-engine` beside the engines that place it - an engine cannot route a fact
/// without knowing whether a second write to that field adds or replaces - and re-exported here
/// because the catalog is what stores it. The byte on disk is the discriminant.
pub use big_engine::FieldKind;

/// A table, under the database its name is unique within.
///
/// # Why this is a type and not a second parameter
///
/// Every method that reaches a table by name needs the database too, and there are about forty
/// of them across this crate and `big-api`. Adding a parameter to each would touch every call
/// site in the workspace to say `default` - noise that hides the handful of call sites where
/// the database is a real decision.
///
/// So a bare `&str` converts into one, meaning [`DEFAULT_DATABASE_NAME`], and the signatures
/// take `impl Into<TableRef<'_>>`. `db.count("tx")` still reads the way it did, and
/// `db.count(TableRef::new("sales", "orders"))` is the case that had something to say.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TableRef<'a> {
    pub database: &'a str,
    pub table: &'a str,
}

impl<'a> TableRef<'a> {
    pub fn new(database: &'a str, table: &'a str) -> Self {
        Self { database, table }
    }

    /// A table in [`DEFAULT_DATABASE`], which is what an unqualified name means.
    pub fn bare(table: &'a str) -> Self {
        Self { database: DEFAULT_DATABASE_NAME, table }
    }

    /// Reads `database.table`, or a bare name as one in the default database.
    ///
    /// **The one decoder, and the reason there is only one representation.** A table travels as
    /// a single `String` in three places that predate databases - a [`crate::FragmentAddr`], a
    /// `big_plan::Plan`, and a DDL message between nodes - and giving each of them its own
    /// second field would be three chances for a qualified name to mean one thing in the repair
    /// path and another in the planner. So the qualified name *is* the string form, [`Display`]
    /// writes it, and this reads it back.
    ///
    /// Unambiguous because [`check_name`] refuses a `.` in a name, so the first one can only be
    /// the separator.
    ///
    /// [`Display`]: core::fmt::Display
    pub fn parse(name: &'a str) -> Self {
        match name.split_once('.') {
            Some((database, table)) => Self { database, table },
            None => Self::bare(name),
        }
    }
}

impl<'a> From<&'a str> for TableRef<'a> {
    fn from(name: &'a str) -> Self {
        Self::parse(name)
    }
}

impl<'a> From<&'a String> for TableRef<'a> {
    fn from(name: &'a String) -> Self {
        Self::parse(name)
    }
}

impl core::fmt::Display for TableRef<'_> {
    /// Qualified only when it says something: a table in the default database prints as the
    /// name somebody typed, which is what an error message about it should say back.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        if self.database == DEFAULT_DATABASE_NAME {
            write!(f, "{}", self.table)
        } else {
            write!(f, "{}.{}", self.database, self.table)
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TableDef {
    pub id: TableId,
    pub name: String,
    /// What this table writes for every fact. Fixed at creation; see [`TableEngine`].
    pub engine: TableEngine,
    /// The namespace [`TableDef::name`] is unique within.
    ///
    /// **Not part of this table's identity below the catalog.** A [`TableId`] is unique across
    /// every database, so nothing keyed on one - no [`FragmentKey`], no row key, no root
    /// record - mentions a database at all. Which is why a namespace above tables cost the
    /// storage layer nothing.
    pub database: DatabaseId,
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
    /// Named databases only. [`DEFAULT_DATABASE`] is not in here: it exists whether or not
    /// anything was ever written, so storing it would mean every reload having to decide
    /// whether to put it back.
    databases: BTreeMap<DatabaseId, String>,
    database_ids: BTreeMap<String, DatabaseId>,
    tables: BTreeMap<TableId, TableDef>,
    /// Nested for the reason `field_ids` is, below: a lookup takes `&str` through
    /// `String: Borrow<str>` and allocates nothing, where a flat `(DatabaseId, String)` key
    /// would allocate a `String` on every probe just to find a table.
    table_ids: BTreeMap<DatabaseId, BTreeMap<String, TableId>>,
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
    database: DatabaseId,
    table: TableId,
    view: ViewId,
    /// Per table, because field ids are scoped to their table.
    field: BTreeMap<TableId, FieldId>,
}

impl Default for Sequences {
    fn default() -> Self {
        Self {
            database: FIRST_NAMED_DATABASE,
            table: 0,
            view: FIRST_NAMED_VIEW,
            field: BTreeMap::new(),
        }
    }
}

/// Which counter a `KIND_SEQ` record carries. Part of the on-disk format.
mod seq_of {
    pub const TABLE: u8 = 0;
    pub const VIEW: u8 = 1;
    pub const FIELD: u8 = 2;
    pub const DATABASE: u8 = 3;
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
    // A `.` is the separator in a qualified name, and [`TableRef::parse`] splits on the first
    // one. A table actually called `a.b` would be indistinguishable from table `b` in database
    // `a` everywhere a table travels as one string - so the name is refused, which is the only
    // answer that keeps the two apart.
    if name.contains('.') {
        return Err(DbError::NameSeparator(name.to_string()));
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

    /// The id of a named database, or [`DEFAULT_DATABASE`] for the name it always answers to.
    pub fn database(&self, name: &str) -> Option<DatabaseId> {
        if name == DEFAULT_DATABASE_NAME {
            return Some(DEFAULT_DATABASE);
        }
        self.database_ids.get(name).copied()
    }

    pub fn database_name(&self, id: DatabaseId) -> Option<&str> {
        if id == DEFAULT_DATABASE {
            return Some(DEFAULT_DATABASE_NAME);
        }
        self.databases.get(&id).map(|s| s.as_str())
    }

    /// Every database, [`DEFAULT_DATABASE`] first. It is prepended rather than stored, for the
    /// reason [`Catalog::databases`]' field documents.
    pub fn databases(&self) -> impl Iterator<Item = (DatabaseId, &str)> {
        core::iter::once((DEFAULT_DATABASE, DEFAULT_DATABASE_NAME))
            .chain(self.databases.iter().map(|(id, name)| (*id, name.as_str())))
    }

    pub fn table(&self, database: DatabaseId, name: &str) -> Option<&TableDef> {
        self.table_ids
            .get(&database)
            .and_then(|by_name| by_name.get(name))
            .and_then(|id| self.tables.get(id))
    }

    /// The database of a reference, or [`DbError::UnknownDatabase`].
    ///
    /// The one place a database name becomes an id, so that "no such database" is said once and
    /// in the same words wherever a qualified name is resolved.
    pub fn database_of(&self, r: TableRef<'_>) -> Result<DatabaseId> {
        self.database(r.database).ok_or_else(|| DbError::UnknownDatabase(r.database.to_string()))
    }

    /// A table by qualified reference. `Ok(None)` is "no such table"; the error is "no such
    /// database", which is a different fix.
    pub fn table_ref(&self, r: TableRef<'_>) -> Result<Option<&TableDef>> {
        Ok(self.table(self.database_of(r)?, r.table))
    }

    /// The same, with "no such table" promoted to an error - which is what almost every caller
    /// wants, since a table that is not there is not a question they can answer.
    pub fn require(&self, r: TableRef<'_>) -> Result<&TableDef> {
        self.table_ref(r)?.ok_or_else(|| DbError::UnknownTable(r.to_string()))
    }

    /// A table by its qualified string name, with an unknown database answering `None` rather
    /// than an error.
    ///
    /// For the callers whose whole question is "is this a table" - a planner asking whether a
    /// name resolves, a field class lookup - where an absent database and an absent table lead
    /// to the same next step. Callers who report the difference want [`Catalog::table_ref`].
    pub fn lookup(&self, name: &str) -> Option<&TableDef> {
        let r = TableRef::parse(name);
        self.table(self.database(r.database)?, r.table)
    }

    pub fn table_by_id(&self, id: TableId) -> Option<&TableDef> {
        self.tables.get(&id)
    }

    /// Every table, by id. The only way to enumerate a schema without knowing a name first,
    /// which is what a tool inspecting a file has to do.
    pub fn tables(&self) -> impl Iterator<Item = &TableDef> {
        self.tables.values()
    }

    /// Every table in one database, in id order.
    pub fn tables_in(&self, database: DatabaseId) -> impl Iterator<Item = &TableDef> {
        self.tables.values().filter(move |t| t.database == database)
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
            .filter(|(name, _)| name.len() == big_engine::bitmap::field::DAY_VIEW_LEN)
            .map(|(_, id)| *id)
            .collect()
    }

    pub fn view_id(&self, name: &str) -> Option<ViewId> {
        self.view_ids.get(name).copied()
    }

    pub fn view_name(&self, id: ViewId) -> Option<&str> {
        self.views.get(&id).map(|s| s.as_str())
    }

    /// Interns a database, returning the existing id if it is already there.
    ///
    /// Idempotent rather than an error on a repeat, which is the rule `IF NOT EXISTS` wants and
    /// which costs nothing here: a database has no engine and no columns, so there is no
    /// second declaration for a repeat to contradict.
    pub fn intern_database(&mut self, name: &str) -> Result<DatabaseId> {
        check_name(name)?;
        if let Some(id) = self.database(name) {
            return Ok(id);
        }
        let id = self.seq.database;
        self.seq.database += 1;
        self.databases.insert(id, name.to_string());
        self.database_ids.insert(name.to_string(), id);
        Ok(id)
    }

    /// Removes a database that holds no tables, with the fragments of none.
    ///
    /// **Emptiness is the caller's to arrange.** A cascading drop is a loop over
    /// [`Catalog::drop_table`], and each of those returns the fragment keys whose pages the
    /// caller has to free - so a drop that quietly swallowed its tables here would strand every
    /// one of those pages. [`DEFAULT_DATABASE`] is never dropped: a table has to be in some
    /// database, and it is the one that is always there.
    pub fn drop_database(&mut self, name: &str) -> Option<DatabaseId> {
        if name == DEFAULT_DATABASE_NAME {
            return None;
        }
        let id = *self.database_ids.get(name)?;
        if self.tables_in(id).next().is_some() {
            return None;
        }
        self.database_ids.remove(name);
        self.databases.remove(&id);
        self.table_ids.remove(&id);
        Some(id)
    }

    /// How many tables a database holds, which is what a `RESTRICT` refusal has to say.
    pub fn table_count(&self, database: DatabaseId) -> usize {
        self.tables_in(database).count()
    }

    /// Interns a table under the default engine. See [`Catalog::intern_table_with`].
    pub fn intern_table(&mut self, database: DatabaseId, name: &str) -> Result<TableId> {
        self.intern_table_with(database, name, TableEngine::default())
    }

    /// Idempotent for an identical declaration, an error for a contradicting one.
    ///
    /// The same rule [`Catalog::add_field`] follows, for the same reason: returning the existing
    /// id for a table declared with a different engine hands the caller a table they did not ask
    /// for - a bitmap-only one where they wrote `columnar` - and the mismatch would only surface
    /// later, as queries that are slower than they asked for or refusals they did not expect.
    pub fn intern_table_with(
        &mut self,
        database: DatabaseId,
        name: &str,
        engine: TableEngine,
    ) -> Result<TableId> {
        check_name(name)?;
        if let Some(id) = self.table_ids.get(&database).and_then(|by_name| by_name.get(name)) {
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
        self.tables.insert(id, TableDef { id, name: name.to_string(), engine, database });
        self.table_ids.entry(database).or_default().insert(name.to_string(), id);
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
    /// Within one database: a rename that also moved a table would be two changes wearing one
    /// name, and nothing above this asks for it.
    pub fn rename_table(&mut self, database: DatabaseId, old: &str, new: &str) -> Result<bool> {
        check_name(new)?;
        let by_name = self.table_ids.entry(database).or_default();
        if old != new && by_name.contains_key(new) {
            return Err(DbError::NameTaken(new.to_string()));
        }
        let Some(id) = by_name.remove(old) else { return Ok(false) };
        by_name.insert(new.to_string(), id);
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
    pub fn drop_table(&mut self, database: DatabaseId, name: &str) -> Option<Vec<FragmentKey>> {
        let id = self.table_ids.get_mut(&database)?.remove(name)?;
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
        for (id, name) in &self.databases {
            let mut b = vec![0u8; CATALOG_ENTRY_BYTES];
            b[0] = KIND_DATABASE;
            b[4..8].copy_from_slice(&id.to_le_bytes());
            put_name(&mut b, name);
            out.push(b);
        }
        for t in self.tables.values() {
            let mut b = vec![0u8; CATALOG_ENTRY_BYTES];
            b[0] = KIND_TABLE;
            // Byte 1 is where a field record keeps its kind, and it was unused here. Reusing it
            // rather than growing the record is what keeps this additive: an entry stays
            // `CATALOG_ENTRY_BYTES` wide and nothing about the chain's stride moves.
            b[1] = t.engine.code();
            b[4..8].copy_from_slice(&t.id.to_le_bytes());
            // Bytes 8..12 are where a field record keeps its table, and were unused here. Same
            // argument as the engine byte, and the same payoff: an entry written before
            // databases existed has zeroes here, and zero is `DEFAULT_DATABASE`.
            b[8..12].copy_from_slice(&t.database.to_le_bytes());
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
        let mut out = vec![
            (seq_of::TABLE, 0, self.seq.table),
            (seq_of::VIEW, 0, self.seq.view),
            (seq_of::DATABASE, 0, self.seq.database),
        ];
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
                    // Zero for every entry written before databases existed, which is
                    // `DEFAULT_DATABASE` - the database those tables have always been in.
                    let database = rd32(8);
                    c.table_ids.entry(database).or_default().insert(name.clone(), id);
                    c.tables.insert(id, TableDef { id, name, engine, database });
                }
                KIND_DATABASE => {
                    let (id, Some(name)) = (rd32(4), get_name(e)) else { continue };
                    // `DEFAULT_DATABASE` is never written, so an entry claiming it is a file
                    // this build did not produce. Skipped rather than inserted: keeping it
                    // would shadow the built-in name with a second entry for the same id.
                    if id == DEFAULT_DATABASE {
                        continue;
                    }
                    c.database_ids.insert(name.clone(), id);
                    c.databases.insert(id, name);
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
                        seq_of::DATABASE => c.seq.database = c.seq.database.max(value),
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
        c.seq.database = c
            .seq
            .database
            .max(c.databases.keys().next_back().map_or(FIRST_NAMED_DATABASE, |m| m + 1));
        for (table, field) in c.fields.keys() {
            let slot = c.seq.field.entry(*table).or_insert(0);
            *slot = (*slot).max(field + 1);
        }
        Ok(c)
    }
}
