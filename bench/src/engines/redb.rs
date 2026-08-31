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

//! `redb` behind the comparison trait.
//!
//! Two tables, not one. A key-value store answers `get` from the primary table, but
//! `count_ge` asks about *values*, which no primary table can answer without reading all of
//! it. A real user would add a secondary index, so this does too — and pays for it on every
//! ingest, which is precisely the trade `big` makes differently by keeping a bitmap.
//!
//! Indexing only the primary table and calling the resulting full scan "redb's range query"
//! would be the kind of comparison that proves whatever its author wanted.

use crate::{dir_size, Durability, Engine, Record};
use redb::{Database, Durability as RedbDurability, ReadableTableMetadata, TableDefinition};
use std::path::{Path, PathBuf};

/// record id -> value
const MAIN: TableDefinition<u64, u64> = TableDefinition::new("main");
/// (value, record id) -> (), giving ordered access by value
const BY_VALUE: TableDefinition<(u64, u64), ()> = TableDefinition::new("by_value");

pub struct RedbEngine {
    db: Database,
    durability: RedbDurability,
    dir: PathBuf,
}

impl Engine for RedbEngine {
    fn name() -> &'static str {
        "redb"
    }

    fn open(dir: &Path, durability: Durability) -> Self {
        let db = Database::create(dir.join("redb.db")).unwrap();
        let durability = match durability {
            Durability::Full => RedbDurability::Immediate,
            Durability::Relaxed => RedbDurability::Eventual,
        };
        Self { db, durability, dir: dir.to_path_buf() }
    }

    fn ingest(&mut self, batch: &[Record]) {
        let mut txn = self.db.begin_write().unwrap();
        txn.set_durability(self.durability);
        {
            let mut main = txn.open_table(MAIN).unwrap();
            let mut by_value = txn.open_table(BY_VALUE).unwrap();
            for (id, value) in batch {
                main.insert(id, value).unwrap();
                by_value.insert((*value, *id), ()).unwrap();
            }
        }
        txn.commit().unwrap();
    }

    fn get(&self, id: u64) -> Option<u64> {
        let txn = self.db.begin_read().unwrap();
        let t = txn.open_table(MAIN).unwrap();
        t.get(id).unwrap().map(|v| v.value())
    }

    fn count_ge(&self, k: u64) -> u64 {
        let txn = self.db.begin_read().unwrap();
        let t = txn.open_table(BY_VALUE).unwrap();
        t.range((k, 0)..).unwrap().count() as u64
    }

    fn len(&self) -> u64 {
        let txn = self.db.begin_read().unwrap();
        let t = txn.open_table(MAIN).unwrap();
        t.len().unwrap()
    }

    fn threaded_reads(&self, threads: usize, probes: &[u64]) -> u64 {
        // A read transaction per read rather than one shared across the batch: that is what a
        // server does per request, and holding one open for the whole fan-out would measure a
        // snapshot nobody keeps.
        crate::fan_out_reads(threads, probes, |id| self.get(id).is_some())
    }

    fn reopen(self, dir: &Path, durability: Durability) -> Self {
        drop(self);
        // `Database::create` opens an existing file rather than truncating it, and the tables
        // are declared inside each transaction, so `open` is already idempotent here.
        Self::open(dir, durability)
    }

    fn remove(&mut self, ids: &[u64]) -> u64 {
        let mut txn = self.db.begin_write().unwrap();
        txn.set_durability(self.durability);
        let mut n = 0;
        {
            let mut main = txn.open_table(MAIN).unwrap();
            let mut by_value = txn.open_table(BY_VALUE).unwrap();
            for id in ids {
                // The value has to come back before the secondary index entry can be found:
                // `by_value` is keyed by `(value, id)`, and there is no way to reach it from the
                // id alone. That is the cost of the secondary index, and it belongs in the row.
                if let Some(v) = main.remove(id).unwrap().map(|v| v.value()) {
                    by_value.remove(&(v, *id)).unwrap();
                    n += 1;
                }
            }
        }
        txn.commit().unwrap();
        n
    }

    fn compact(&mut self) {
        while self.db.compact().unwrap() {}
    }

    fn checkpoint(&mut self) {
        // Compacting reclaims pages a copy-on-write commit left behind, which is the only way
        // a size comparison against an engine that truncates its own tail means anything.
        while self.db.compact().unwrap() {}
    }

    fn disk_bytes(&self) -> u64 {
        dir_size(&self.dir)
    }
}
