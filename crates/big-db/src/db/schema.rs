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

//! Declaring and dropping, which are ordinary transactions.
//!
//! Every one of these opens a write, changes the catalog and commits, so a schema change is
//! atomic against the data it describes for the same reason a fact is: there is one commit
//! point and it is the meta page flip. Nothing here is a separate DDL path, and that is why a
//! dropped field's fragments go out in the same transaction that forgets its name.

use super::*;

impl<P: PagerMut> Db<P> {
    /// Runs one transaction against the catalog, committing only if the closure succeeds.
    ///
    /// **The escape hatch for catalog objects this crate deliberately has no verbs for.** Roles
    /// and grants are the case it exists for: where they are kept is this crate's, and what they
    /// *mean* is `big-rbac`'s, so the layer that administers them is above both and still needs
    /// the one thing only this crate can give it - a write that commits at the meta page flip
    /// like every other schema change.
    ///
    /// An `Err` from the closure drops the write without committing, which is how every other
    /// transaction here is abandoned: there is no rollback because nothing was written.
    pub fn transact<T>(&self, f: impl FnOnce(&mut Catalog) -> Result<T>) -> Result<T> {
        let mut w = self.write();
        let out = f(&mut w.catalog)?;
        w.commit()?;
        Ok(out)
    }

    /// Schema changes are ordinary transactions; there is no separate DDL path.
    ///
    /// Takes the default engine. See [`Db::create_table_with`] to choose one, and
    /// [`TableEngine`] for what the choice costs.
    pub fn create_table<'a>(&self, name: impl Into<TableRef<'a>>) -> Result<TableId> {
        self.create_table_with(name, TableEngine::default())
    }

    /// The same, with the storage engine named.
    ///
    /// Fixed at creation and never changed afterwards: switching would mean rewriting every
    /// fragment and every segment the table owns, which is a migration and not a setting. A
    /// second call naming a different engine is refused rather than ignored.
    pub fn create_table_with<'a>(
        &self,
        name: impl Into<TableRef<'a>>,
        engine: TableEngine,
    ) -> Result<TableId> {
        let name = name.into();
        let mut w = self.write();
        // The database has to exist first. Creating one implicitly would turn a typo in a
        // qualified name into a new namespace holding one table, which is the failure a
        // two-part name makes easy and which `CREATE DATABASE` exists to keep deliberate.
        let database = w.catalog.database_of(name)?;
        let id = w.catalog.intern_table_with(database, name.table, engine)?;
        w.commit()?;
        Ok(id)
    }

    /// Creates a database, or returns the id of the one already there.
    pub fn create_database(&self, name: &str) -> Result<DatabaseId> {
        let mut w = self.write();
        let id = w.catalog.intern_database(name)?;
        w.commit()?;
        Ok(id)
    }

    /// Removes a database and, with `cascade`, every table in it.
    ///
    /// `Ok(false)` means there was no such database. Without `cascade` a database that still
    /// holds tables is [`DbError::DatabaseNotEmpty`] rather than a silent mass drop - the same
    /// default Postgres and BigQuery take, and for the same reason: `DROP DATABASE` is one word
    /// away from being the most expensive typo on this surface.
    ///
    /// One transaction, so there is no window where the tables are gone and the database is
    /// still there to be found.
    pub fn drop_database(&self, name: &str, cascade: bool) -> Result<bool> {
        if name == DEFAULT_DATABASE_NAME {
            return Err(DbError::DropDefaultDatabase);
        }
        let mut w = self.write();
        let Some(id) = w.catalog.database(name) else { return Ok(false) };

        // A view counts as something held, for the reason `Catalog::drop_database` states: a
        // `DROP DATABASE` that was `RESTRICT` about tables and `CASCADE` about views would be
        // two rules wearing one word.
        let held = w.catalog.table_count(id) + w.catalog.saved_query_count(id);
        if held > 0 && !cascade {
            return Err(DbError::DatabaseNotEmpty { database: name.to_string(), tables: held });
        }
        // Collected first: dropping mutates the map these names come from.
        let tables: Vec<String> = w.catalog.tables_in(id).map(|t| t.name.clone()).collect();
        for table in &tables {
            // Each drop hands back the fragment keys whose pages have to be freed. Swallowing
            // them here would strand every page the table owned.
            let Some(keys) = w.catalog.drop_table(id, table) else { continue };
            w.discard(&keys)?;
        }
        let views: Vec<String> = w.catalog.saved_queries_in(id).map(|q| q.name.clone()).collect();
        for view in &views {
            // No `discard`: a view owns no pages. It is the one drop here with nothing to free.
            w.catalog.drop_saved_query(id, view);
        }
        let dropped = w.catalog.drop_database(name).is_some();
        w.commit()?;
        Ok(dropped)
    }

    /// Stores a `SELECT` under a name - what SQL calls `CREATE VIEW`.
    ///
    /// **This layer does not read the statement.** What a body may contain is a question about
    /// the dialect and it is answered where the dialect lives, in `big-sql`'s parser; whether
    /// the table it names exists is a question the caller has already asked. Here it is a
    /// string, checked only for length and for a name a table already holds.
    ///
    /// `replace` is `CREATE OR REPLACE`. Without it a name already holding a *different*
    /// statement is [`DbError::ViewRedefined`]; an identical one is idempotent either way.
    pub fn create_view<'a>(
        &self,
        name: impl Into<TableRef<'a>>,
        text: &str,
        replace: bool,
    ) -> Result<QueryId> {
        let name = name.into();
        let mut w = self.write();
        // The database has to exist first, for the reason `create_table_with` states.
        let database = w.catalog.database_of(name)?;
        let id = w.catalog.intern_saved_query(database, name.table, text, replace)?;
        w.commit()?;
        Ok(id)
    }

    /// Forgets a view. `Ok(false)` means there was no such view.
    ///
    /// The one drop in this module that frees nothing: a view owns no fragments, so there are
    /// no keys to hand to `DbWrite::discard` and no pages to return to the freelist. What it
    /// costs is the records the statement was written in, which the next commit reclaims like
    /// any other catalog change.
    pub fn drop_view<'a>(&self, name: impl Into<TableRef<'a>>) -> Result<bool> {
        let name = name.into();
        let mut w = self.write();
        let database = w.catalog.database_of(name)?;
        let dropped = w.catalog.drop_saved_query(database, name.table);
        w.commit()?;
        Ok(dropped)
    }

    /// A decimal field: stored as an integer, read back as a value with `scale` digits after
    /// the point.
    ///
    /// Nothing in storage knows about the point. The scale lives in the catalog so the query
    /// layer can turn `price > 5.25` into the integer comparison that means the same thing.
    pub fn create_decimal<'a>(
        &self,
        table: impl Into<TableRef<'a>>,
        name: &str,
        bit_depth: u32,
        scale: i8,
    ) -> Result<FieldId> {
        let table = table.into();
        self.declare(table, name, FieldKind::Decimal, bit_depth, scale, Vec::new())
    }

    /// A signed integer field.
    ///
    /// `bit_depth` counts the **whole** width including the sign, so a depth of 8 holds
    /// `-128..=127`. Zero means the full 64 bits.
    ///
    /// Worth knowing before choosing a depth: a signed field always uses every plane it
    /// declared, because the top one is the sign bit and every non-negative value sets it. An
    /// unsigned field of the same declared depth only pays for the planes its data reaches.
    pub fn create_signed<'a>(
        &self,
        table: impl Into<TableRef<'a>>,
        name: &str,
        bit_depth: u32,
    ) -> Result<FieldId> {
        let table = table.into();
        self.declare(table, name, FieldKind::SignedInt, bit_depth, 0, Vec::new())
    }

    /// A time quantum field: a keyed field that also writes into one view per granularity.
    pub fn create_time_quantum<'a>(
        &self,
        table: impl Into<TableRef<'a>>,
        name: &str,
        granularity: Vec<Granularity>,
    ) -> Result<FieldId> {
        let table = table.into();
        self.declare(table, name, FieldKind::TimeQuantum, 0, 0, granularity)
    }

    pub fn create_field<'a>(
        &self,
        table: impl Into<TableRef<'a>>,
        name: &str,
        kind: FieldKind,
        bit_depth: u32,
    ) -> Result<FieldId> {
        let table = table.into();
        self.declare(table, name, kind, bit_depth, 0, Vec::new())
    }

    fn declare<'a>(
        &self,
        table: impl Into<TableRef<'a>>,
        name: &str,
        kind: FieldKind,
        bit_depth: u32,
        scale: i8,
        granularity: Vec<Granularity>,
    ) -> Result<FieldId> {
        let table = table.into();
        let mut w = self.write();
        let table_id = w.catalog.require(table)?.id;
        let id = w.catalog.add_field(FieldDef {
            id: 0,
            table: table_id,
            name: name.to_string(),
            kind,
            bit_depth,
            scale,
            granularity,
        })?;
        w.commit()?;
        Ok(id)
    }

    /// Removes a table, everything in it, and the pages holding it.
    ///
    /// `Ok(false)` means there was no such table. Schema and data go in one transaction, so
    /// there is no window where the table is gone but its fragments are still reachable.
    pub fn drop_table<'a>(&self, name: impl Into<TableRef<'a>>) -> Result<bool> {
        let name = name.into();
        let mut w = self.write();
        let database = w.catalog.database_of(name)?;
        let Some(keys) = w.catalog.drop_table(database, name.table) else { return Ok(false) };
        w.discard(&keys)?;
        w.commit()?;
        Ok(true)
    }

    /// Empties a table, keeping the table and its fields.
    ///
    /// `Ok(None)` means there was no such table. The number is how many fragments went, which is
    /// the only count this can answer cheaply: a record count would mean reading the existence
    /// field of every shard before dropping it, and the caller asking to empty a table has not
    /// asked what was in it.
    ///
    /// **What stays, and why each one matters.** The table's id and its fields' ids, so that
    /// every name resolves to the same place it did before. Its **row keys**, because row ids
    /// are interned per `(table, field)` and the write path rests on them never being reused -
    /// see [`crate::catalog::Catalog::take_table_fragments`]. And its grants, because emptying a
    /// table is not a statement about who may read it.
    ///
    /// Schema and data go in one transaction, as they do for a drop, so there is no window where
    /// the fragments are unreachable but still allocated.
    pub fn truncate_table<'a>(&self, name: impl Into<TableRef<'a>>) -> Result<Option<u64>> {
        let name = name.into();
        let mut w = self.write();
        let database = w.catalog.database_of(name)?;
        let Some(id) = w.catalog.table(database, name.table).map(|t| t.id) else {
            return Ok(None);
        };
        let keys = w.catalog.take_table_fragments(id);
        let dropped = keys.len() as u64;
        w.discard(&keys)?;
        w.commit()?;
        Ok(Some(dropped))
    }

    /// Gives a table a different name. Nothing else about it moves.
    ///
    /// `Ok(false)` means there was no such table; renaming onto a name already taken is an error
    /// rather than an overwrite. See [`crate::catalog::Catalog::rename_table`] for why this costs
    /// one record: everything below the catalog is keyed by [`TableId`], and the id does not
    /// change. In particular the **row keys stay**, for the reason a truncate keeps them - a row
    /// id interned under this table is still interned under this table.
    pub fn rename_table<'a>(&self, name: impl Into<TableRef<'a>>, to: &str) -> Result<bool> {
        let name = name.into();
        let mut w = self.write();
        let database = w.catalog.database_of(name)?;
        if !w.catalog.rename_table(database, name.table, to)? {
            return Ok(false);
        }
        w.commit()?;
        Ok(true)
    }

    /// Swaps the names of two tables in one transaction.
    ///
    /// `Ok(false)` means one of them was not there, and then neither moved.
    ///
    /// **The atomic half of a rebuild.** Building a replacement table and putting it in place is
    /// otherwise a drop and a rename with a window in between where the name resolves to nothing;
    /// this closes the window, and it leaves the old table under the other name so the rebuild
    /// can be undone by running the same statement again.
    ///
    /// Both names are in one database, because a swap that also moved a table across databases
    /// would be two changes wearing one name - the same line [`Self::rename_table`] draws.
    pub fn exchange_tables<'a>(
        &self,
        a: impl Into<TableRef<'a>>,
        b: impl Into<TableRef<'a>>,
    ) -> Result<bool> {
        let (a, b) = (a.into(), b.into());
        let mut w = self.write();
        let database = w.catalog.database_of(a)?;
        // Judged here rather than in the catalog, which is handed one database id and could not
        // see the difference: two names in two databases are two tables this cannot swap.
        if w.catalog.database_of(b)? != database {
            return Err(DbError::UnknownTable(b.to_string()));
        }
        if !w.catalog.exchange_tables(database, a.table, b.table)? {
            return Ok(false);
        }
        w.commit()?;
        Ok(true)
    }

    /// Removes one field of a table, its row keys, and the pages holding it.
    /// Drops every per-day view of a time quantum field older than `unix_seconds`, and returns
    /// how many fragments went.
    ///
    /// **Retention, which is the thing a time quantum field was missing.** A field with a day
    /// granularity writes a copy of each fact into the view for its day, and nothing ever
    /// removed one - so a table that has been ingesting for two years holds two years of day
    /// views whether or not anybody will ask about the first one. This is how they go.
    ///
    /// The day `unix_seconds` falls in is **kept**: an operator saying "keep thirty days" means
    /// thirty, and a boundary that quietly took one more would be off by a day in the direction
    /// nobody checks.
    ///
    /// The standard view is untouched, so questions that carry no time still see every record.
    /// That is the honest shape of this operation and worth stating: it drops the *index by
    /// time*, not the records. A query with a `BETWEEN` stops finding them; a `count(*)` does
    /// not change. Deleting records is `DELETE FROM t WHERE ...`.
    ///
    /// **Every granularity the field declared**, not only its days - see
    /// [`crate::catalog::Catalog::drop_views_before`]. A view is dropped only when the whole
    /// period it covers ends before the cutoff, so a month the cutoff falls inside stays.
    pub fn drop_days_before<'a>(
        &self,
        table: impl Into<TableRef<'a>>,
        field: &str,
        unix_seconds: i64,
    ) -> Result<usize> {
        let table = table.into();
        let cutoff = big_engine::bitmap::field::day_view(unix_seconds);
        let mut w = self.write();
        let table_id = w.catalog.require(table)?.id;
        let def = w.catalog.field(table_id, field).ok_or_else(|| DbError::UnknownField {
            table: table.to_string(),
            field: field.to_string(),
        })?;
        // Refused rather than answering zero. Against a plain set field there are no day views
        // at all, so "nothing was dropped" and "this field never had days to drop" would be the
        // same answer - and they call for opposite actions. The same collapse is what
        // `Rows::KeyBetween` used to make.
        if def.kind != FieldKind::TimeQuantum {
            return Err(DbError::WrongFieldKind {
                field: field.to_string(),
                expected: "time quantum",
            });
        }
        let field_id = def.id;

        let keys = w.catalog.drop_views_before(table_id, field_id, &cutoff);
        let dropped = keys.len();
        w.discard(&keys)?;
        w.commit()?;
        Ok(dropped)
    }

    pub fn drop_field<'a>(&self, table: impl Into<TableRef<'a>>, field: &str) -> Result<bool> {
        let table = table.into();
        let mut w = self.write();
        let table_id = w.catalog.require(table)?.id;
        let Some(keys) = w.catalog.drop_field(table_id, field) else { return Ok(false) };
        w.discard(&keys)?;
        w.commit()?;
        Ok(true)
    }
}
