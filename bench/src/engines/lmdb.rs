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

//! LMDB behind the comparison trait, through `heed`.
//!
//! The closest thing here to `big`'s own shape: an mmap'd copy-on-write B-tree with a single
//! writer, no WAL, and durability that rests on a meta page flip. Where they differ is what sits
//! on top - LMDB stores opaque bytes and this adapter has to build the secondary index by hand,
//! which is exactly the work `big` does not do because a bit-sliced index *is* the index.
//!
//! Keys are big-endian for the same reason as `fjall`: LMDB's default comparator is a byte
//! comparison, so little-endian keys would order wrongly the moment a byte boundary is crossed.

use crate::{dir_size, Durability, Engine, Record};
use heed::byteorder::BigEndian;
use heed::types::{Unit, U128, U64};
use heed::{Database, EnvFlags, EnvOpenOptions};
use std::path::{Path, PathBuf};

/// LMDB needs its map size fixed at open. Large enough for the workloads here and cheap on
/// 64-bit: it is address space, not disk.
const MAP_SIZE: usize = 4 << 30;

pub struct LmdbEngine {
    env: heed::Env,
    main: Database<U64<BigEndian>, U64<BigEndian>>,
    by_value: Database<U128<BigEndian>, Unit>,
    dir: PathBuf,
}

/// `(value, id)` as one big-endian key, so ordering by the pair is ordering by the number.
///
/// Packed into a `u128` rather than built as sixteen bytes because heed's range bounds have to
/// be sized, and because a numeric key makes the intent obvious: `value` is the major
/// component, `id` only breaks ties.
fn value_key(value: u64, id: u64) -> u128 {
    ((value as u128) << 64) | id as u128
}

impl Engine for LmdbEngine {
    fn name() -> &'static str {
        "lmdb"
    }

    /// `Relaxed` maps to `NO_SYNC`, which is LMDB's own name for the same trade the others make:
    /// the commit is in the page cache and survives the process, and a machine that goes down
    /// loses whatever the OS had not written. LMDB stays crash-consistent either way, because
    /// like `big` it flips a meta page rather than replaying a log.
    fn open(dir: &Path, durability: Durability) -> Self {
        let path = dir.join("lmdb");
        std::fs::create_dir_all(&path).unwrap();

        let mut opts = EnvOpenOptions::new();
        opts.map_size(MAP_SIZE).max_dbs(2);
        if durability == Durability::Relaxed {
            // SAFETY: the flag only weakens when data reaches the disk. Every other invariant
            // LMDB relies on is unchanged, and the process is the only writer.
            unsafe {
                opts.flags(EnvFlags::NO_SYNC);
            }
        }
        // SAFETY: the path is a directory this process just created and nothing else has it
        // open; `heed` requires the caller to promise no other process is mutating it.
        let env = unsafe { opts.open(&path) }.unwrap();

        let mut w = env.write_txn().unwrap();
        let main = env.create_database(&mut w, Some("main")).unwrap();
        let by_value = env.create_database(&mut w, Some("by_value")).unwrap();
        w.commit().unwrap();

        Self { env, main, by_value, dir: dir.to_path_buf() }
    }

    fn ingest(&mut self, batch: &[Record]) {
        let mut w = self.env.write_txn().unwrap();
        for (id, value) in batch {
            self.main.put(&mut w, id, value).unwrap();
            self.by_value.put(&mut w, &value_key(*value, *id), &()).unwrap();
        }
        w.commit().unwrap();
    }

    fn get(&self, id: u64) -> Option<u64> {
        let r = self.env.read_txn().unwrap();
        self.main.get(&r, &id).unwrap()
    }

    fn count_ge(&self, k: u64) -> u64 {
        let r = self.env.read_txn().unwrap();
        self.by_value.range(&r, &(value_key(k, 0)..)).unwrap().count() as u64
    }

    fn len(&self) -> u64 {
        let r = self.env.read_txn().unwrap();
        self.main.len(&r).unwrap()
    }

    fn threaded_reads(&self, threads: usize, probes: &[u64]) -> u64 {
        crate::fan_out_reads(threads, probes, |id| self.get(id).is_some())
    }

    fn reopen(self, dir: &Path, durability: Durability) -> Self {
        // The environment owns the mapping, so this is what actually makes the read cold on the
        // engine's side; the page cache is the harness's problem.
        drop(self);
        Self::open(dir, durability)
    }

    fn remove(&mut self, ids: &[u64]) -> u64 {
        let mut w = self.env.write_txn().unwrap();
        let mut n = 0;
        for id in ids {
            if let Some(v) = self.main.get(&w, id).unwrap() {
                self.main.delete(&mut w, id).unwrap();
                self.by_value.delete(&mut w, &value_key(v, *id)).unwrap();
                n += 1;
            }
        }
        w.commit().unwrap();
        n
    }

    /// **LMDB has no in-place compaction**, which is a real difference and not a gap in the
    /// adapter: freed pages are reused but the file never shrinks. The operator's answer is
    /// `mdb_copy`, a copy into a fresh environment - the same shape as `big compact`. That is
    /// what this does, and the copy is what the `compacted size` row then measures.
    fn compact(&mut self) {
        let dest = self.dir.join("compacted.mdb");
        let _ = std::fs::remove_file(&dest);
        let mut file = std::fs::File::create(&dest).unwrap();
        self.env.copy_to_file(&mut file, heed::CompactionOption::Enabled).unwrap();

        // The original is removed so `dir_size` reports what the operator would be left with,
        // matching how `big`'s own compaction is measured.
        std::fs::remove_dir_all(self.dir.join("lmdb")).unwrap();
    }

    /// Nothing is deferred: a committed transaction is already in the file. `sync` covers the
    /// relaxed mode, where the OS may not have written it yet.
    fn checkpoint(&mut self) {
        self.env.force_sync().unwrap();
    }

    fn disk_bytes(&self) -> u64 {
        dir_size(&self.dir)
    }
}
