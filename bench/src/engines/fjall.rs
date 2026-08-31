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

//! `fjall` behind the comparison trait: the LSM entrant.
//!
//! The reason it is worth having is that it is the only engine here that is *not* a B-tree, and
//! the trade shows up in two rows rather than one. Writes go to a memtable and are cheap; the
//! space they occupy is not reclaimed until compaction runs, so `disk_bytes` is meaningless
//! before `checkpoint` forces one. That is why the trait has `checkpoint` at all.
//!
//! Keys are big-endian so that `fjall`'s byte ordering is numeric ordering. Little-endian would
//! make the range scan return the right count by accident on small numbers and the wrong one as
//! soon as a byte boundary is crossed.

use crate::{dir_size, Durability, Engine, Record};
use fjall::{Config, Keyspace, PartitionCreateOptions, PartitionHandle, PersistMode};
use std::path::{Path, PathBuf};

/// `id -> value`.
const MAIN: &str = "main";
/// `(value, id) -> ()`, giving ordered access by value.
const BY_VALUE: &str = "by_value";

pub struct FjallEngine {
    keyspace: Keyspace,
    main: PartitionHandle,
    by_value: PartitionHandle,
    persist: PersistMode,
    dir: PathBuf,
}

fn value_key(value: u64, id: u64) -> [u8; 16] {
    let mut k = [0u8; 16];
    k[..8].copy_from_slice(&value.to_be_bytes());
    k[8..].copy_from_slice(&id.to_be_bytes());
    k
}

impl Engine for FjallEngine {
    fn name() -> &'static str {
        "fjall"
    }

    fn open(dir: &Path, durability: Durability) -> Self {
        let keyspace = Config::new(dir.join("fjall")).open().unwrap();
        let main = keyspace.open_partition(MAIN, PartitionCreateOptions::default()).unwrap();
        let by_value =
            keyspace.open_partition(BY_VALUE, PartitionCreateOptions::default()).unwrap();
        let persist = match durability {
            // `SyncAll` fsyncs the journal, which is the guarantee the others' full mode makes.
            Durability::Full => PersistMode::SyncAll,
            // Buffered leaves it to the OS: survives the process, not the machine.
            Durability::Relaxed => PersistMode::Buffer,
        };
        Self { keyspace, main, by_value, persist, dir: dir.to_path_buf() }
    }

    fn ingest(&mut self, batch: &[Record]) {
        for (id, value) in batch {
            self.main.insert(id.to_be_bytes(), value.to_be_bytes()).unwrap();
            self.by_value.insert(value_key(*value, *id), []).unwrap();
        }
        // One persist per batch, which is what makes this a transaction's worth of durability
        // rather than a record's. An LSM allowed to persist once per batch while its rivals
        // fsync per transaction would be measured on a promise it did not make.
        self.keyspace.persist(self.persist).unwrap();
    }

    fn get(&self, id: u64) -> Option<u64> {
        self.main
            .get(id.to_be_bytes())
            .unwrap()
            .map(|v| u64::from_be_bytes(v.as_ref().try_into().unwrap()))
    }

    fn count_ge(&self, k: u64) -> u64 {
        self.by_value.range(value_key(k, 0)..).count() as u64
    }

    /// The exact count, which for an LSM means a scan.
    ///
    /// `approximate_len` is cheap and would put a number in the `len()` column that is not the
    /// one every other engine reports: it counts tombstones as live, so it reads high by
    /// exactly the number of deletions until compaction catches up. Reporting it here would be
    /// answering a different question in the same column, so this pays what an exact count
    /// actually costs an LSM - and the row then says so.
    fn len(&self) -> u64 {
        self.main.len().unwrap() as u64
    }

    fn threaded_reads(&self, threads: usize, probes: &[u64]) -> u64 {
        crate::fan_out_reads(threads, probes, |id| self.get(id).is_some())
    }

    fn reopen(self, dir: &Path, durability: Durability) -> Self {
        // The keyspace holds its journal and its block cache; dropping it is what clears both.
        drop(self);
        // `open_partition` opens an existing partition, so this is create-or-open.
        Self::open(dir, durability)
    }

    fn remove(&mut self, ids: &[u64]) -> u64 {
        let mut n = 0;
        for id in ids {
            // The value has to be read back to reach the secondary index entry, exactly as in
            // `redb`: `by_value` cannot be addressed from the id alone.
            if let Some(v) = self.get(*id) {
                self.main.remove(id.to_be_bytes()).unwrap();
                self.by_value.remove(value_key(v, *id)).unwrap();
                n += 1;
            }
        }
        self.keyspace.persist(self.persist).unwrap();
        n
    }

    /// A major compaction, which is where an LSM finally drops what its tombstones cover.
    fn compact(&mut self) {
        self.checkpoint();
        self.main.major_compact().unwrap();
        self.by_value.major_compact().unwrap();
    }

    /// Flushing the memtables is the whole reason this method exists on the trait: a size taken
    /// before it measures how much of the data happened to still be in memory.
    fn checkpoint(&mut self) {
        self.keyspace.persist(PersistMode::SyncAll).unwrap();
        // Rotating seals the active memtable so the flush worker can write it out; without it a
        // size taken here measures how much of the data happened to still be in memory.
        //
        // Deliberately **no** major compaction. That is what `compact` is for, and doing it here
        // would make the `disk` and `compacted` columns the same measurement taken twice - which
        // for an LSM is the one distinction most worth keeping, since the gap between them is
        // the space its redundant tables are holding.
        for p in [&self.main, &self.by_value] {
            p.rotate_memtable().unwrap();
        }
    }

    fn disk_bytes(&self) -> u64 {
        dir_size(&self.dir)
    }
}
