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

//! SQLite behind the comparison trait.
//!
//! The odd one out, and worth having for exactly that reason: it is the only entrant with a
//! query planner and the only one where the range query is answered by a B-tree index the
//! engine chose to use rather than by a structure the adapter built by hand. Every other engine
//! here is a key-value store with a secondary index the benchmark maintains itself.
//!
//! Like `redb`, it gets a second index on `value` so that `count_ge` is a range scan rather than
//! a table scan. Indexing only the primary key and calling the resulting full scan "SQLite's
//! range query" would be the kind of comparison that proves whatever its author wanted.

use crate::{dir_size, Durability, Engine, Record};
use rusqlite::Connection;
use std::path::{Path, PathBuf};

pub struct SqliteEngine {
    conn: Connection,
    dir: PathBuf,
}

impl Engine for SqliteEngine {
    fn name() -> &'static str {
        "sqlite"
    }

    /// `synchronous = FULL` against `NORMAL`, both in WAL mode.
    ///
    /// `FULL` fsyncs the WAL on every commit, which is the guarantee the other engines' full
    /// mode makes. `NORMAL` in WAL mode syncs only at a checkpoint, so a commit survives the
    /// process but not the machine - the same trade `redb`'s `Eventual` and `big`'s `None` make.
    fn open(dir: &Path, durability: Durability) -> Self {
        let conn = Connection::open(dir.join("sqlite.db")).unwrap();
        conn.pragma_update(None, "journal_mode", "WAL").unwrap();
        let sync = match durability {
            Durability::Full => "FULL",
            Durability::Relaxed => "NORMAL",
        };
        conn.pragma_update(None, "synchronous", sync).unwrap();
        // `IF NOT EXISTS` because `open` is also the reopen path, and a reopen has to find the
        // schema already there rather than fail on it.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS t (id INTEGER PRIMARY KEY, v INTEGER NOT NULL);
             CREATE INDEX IF NOT EXISTS t_v ON t (v, id);",
        )
        .unwrap();
        Self { conn, dir: dir.to_path_buf() }
    }

    fn ingest(&mut self, batch: &[Record]) {
        let txn = self.conn.transaction().unwrap();
        {
            // Prepared once per transaction rather than once per row: re-planning the same
            // insert for every record would measure the planner, not the storage.
            let mut stmt =
                txn.prepare_cached("INSERT OR REPLACE INTO t (id, v) VALUES (?1, ?2)").unwrap();
            for (id, value) in batch {
                stmt.execute((*id as i64, *value as i64)).unwrap();
            }
        }
        txn.commit().unwrap();
    }

    fn get(&self, id: u64) -> Option<u64> {
        self.conn
            .query_row("SELECT v FROM t WHERE id = ?1", [id as i64], |r| r.get::<_, i64>(0))
            .ok()
            .map(|v| v as u64)
    }

    fn count_ge(&self, k: u64) -> u64 {
        self.conn
            .query_row("SELECT COUNT(*) FROM t WHERE v >= ?1", [k as i64], |r| r.get::<_, i64>(0))
            .unwrap() as u64
    }

    fn len(&self) -> u64 {
        self.conn.query_row("SELECT COUNT(*) FROM t", [], |r| r.get::<_, i64>(0)).unwrap() as u64
    }

    /// A connection per thread, because `rusqlite::Connection` is not `Sync`.
    ///
    /// Not a handicap invented by the harness - it is how SQLite is used concurrently, and the
    /// cost of opening those connections belongs in the measurement for the same reason the
    /// secondary index does.
    fn threaded_reads(&self, threads: usize, probes: &[u64]) -> u64 {
        let path = self.dir.join("sqlite.db");
        if threads <= 1 {
            return probes.iter().filter(|p| self.get(**p).is_some()).count() as u64;
        }
        let chunk = probes.len().div_ceil(threads);
        std::thread::scope(|scope| {
            let handles: Vec<_> = probes
                .chunks(chunk)
                .map(|part| {
                    let path = path.clone();
                    scope.spawn(move || {
                        let conn = Connection::open(&path).unwrap();
                        let mut stmt = conn.prepare("SELECT v FROM t WHERE id = ?1").unwrap();
                        part.iter()
                            .filter(|id| {
                                stmt.query_row([**id as i64], |r| r.get::<_, i64>(0)).is_ok()
                            })
                            .count() as u64
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().expect("a read thread panicked")).sum()
        })
    }

    fn reopen(self, dir: &Path, durability: Durability) -> Self {
        // SQLite keeps a page cache per connection, so closing the connection is the whole of
        // the engine-side drop.
        drop(self);
        Self::open(dir, durability)
    }

    fn remove(&mut self, ids: &[u64]) -> u64 {
        let txn = self.conn.transaction().unwrap();
        let mut n = 0u64;
        {
            let mut stmt = txn.prepare_cached("DELETE FROM t WHERE id = ?1").unwrap();
            for id in ids {
                n += stmt.execute([*id as i64]).unwrap() as u64;
            }
        }
        txn.commit().unwrap();
        n
    }

    fn compact(&mut self) {
        self.conn.execute_batch("VACUUM").unwrap();
    }

    /// Checkpointing the WAL is what makes `disk_bytes` honest: without it the main file is
    /// short and the WAL holds everything that was just written.
    fn checkpoint(&mut self) {
        self.conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)").unwrap();
    }

    fn disk_bytes(&self) -> u64 {
        dir_size(&self.dir)
    }
}
