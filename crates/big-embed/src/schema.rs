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

//! The schema as something that can leave the process.

use big_db::catalog::{Catalog, FieldKind, TableEngine};
use big_db::Granularity;

/// One table as it looks on the way out: owned values, no lock held.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TableInfo {
    /// The database the table is in, which is what its name is unique within.
    ///
    /// Reported for the same reason `engine` is: it is not derivable from anything else a
    /// client can see, and it is what one node needs in order to recreate another's table in
    /// the place the first one had it.
    pub database: String,
    /// The table's name, unique within [`TableInfo::database`].
    pub name: String,
    /// What the table writes for every fact, and therefore which questions it answers cheaply.
    ///
    /// Reported because it is not derivable from anything else a client can see, and because it
    /// is what one node needs in order to recreate another's table exactly - the same argument
    /// `scale` carries on a field.
    pub engine: TableEngine,
    /// Its fields, in catalog order.
    pub fields: Vec<FieldInfo>,
}

/// One field as it looks on the way out.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct FieldInfo {
    /// The field's name.
    pub name: String,
    /// Which convention decides the row a value goes into.
    pub kind: FieldKind,
    /// Bits per value, for the kinds that store integers. Zero for the ones that do not.
    pub bit_depth: u32,
    /// Digits after the point, for a decimal. Zero for every other kind.
    ///
    /// Reported because a decimal without its scale is an integer wearing a different name:
    /// `price > 5` means `> 500` on a field with two of them, and a client that cannot see the
    /// scale cannot know that. It is also what lets one node recreate another's field exactly.
    pub scale: i8,
    /// The views a time quantum field writes. Empty for every other kind.
    pub granularity: Vec<Granularity>,
}

/// Copies the catalog into owned values, dropping the lock before returning.
/// Not public: `Catalog` comes from an unpublished crate, so no caller outside this one could
/// build an argument for it. [`crate::Api::schema`] is the reachable form of the same thing.
pub(crate) fn snapshot(catalog: &Catalog) -> Vec<TableInfo> {
    let mut out = Vec::new();
    let mut id = 0;
    // Tables are interned from zero upwards and never removed, so walking ids covers them all
    // without the catalog having to expose its map.
    while let Some(table) = catalog.table_by_id(id) {
        out.push(TableInfo {
            database: catalog
                .database_name(table.database)
                .unwrap_or(big_db::DEFAULT_DATABASE_NAME)
                .to_string(),
            name: table.name.clone(),
            engine: table.engine,
            fields: catalog
                .fields_of(id)
                .map(|f| FieldInfo {
                    name: f.name.clone(),
                    kind: f.kind,
                    bit_depth: f.bit_depth,
                    scale: f.scale,
                    granularity: f.granularity.clone(),
                })
                .collect(),
        });
        id += 1;
    }
    out
}
