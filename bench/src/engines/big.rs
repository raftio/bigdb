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

//! `big-db` behind the comparison trait.

use crate::{dir_size, Durability, Engine, Record};
use big_db::catalog::FieldKind;
use big_db::Db;
use big_db::RangeOp;
use big_pager::{CountingPager, MmapPager};
use std::path::{Path, PathBuf};

pub const TABLE: &str = "t";
pub const FIELD: &str = "v";
/// Values are generated below 2^20, and the engine refuses anything wider than declared.
pub const BIT_DEPTH: u32 = 20;

pub struct BigEngine {
    db: Db<CountingPager<MmapPager>>,
    dir: PathBuf,
    /// Whether `compact` has already rewritten the file, so `checkpoint` does not try to
    /// truncate a copy that was just written compact.
    compacted: bool,
}

impl BigEngine {
    /// The handle underneath, so a benchmark can drive `big` through an API the comparison
    /// trait deliberately does not have. Buffered ingest has no counterpart in `redb`, so
    /// putting it on the trait would mean inventing one side of the comparison.
    pub fn db(&self) -> &Db<CountingPager<MmapPager>> {
        &self.db
    }
}

impl Engine for BigEngine {
    fn name() -> &'static str {
        "big"
    }

    /// `Relaxed` maps to `Durability::None`, which is the level that matches what the rivals
    /// mean by it: committed data survives the process dying, not the machine. `redb`'s
    /// `Eventual` makes the same trade, so the two are being asked the same question.
    ///
    /// `Barrier` has no counterpart on the other side and is therefore not offered here. A
    /// level only this engine has would be a level only this engine gets measured at.
    fn open(dir: &Path, durability: Durability) -> Self {
        let pager = MmapPager::open_default(dir.join("big.db")).unwrap();
        let db = Db::open(CountingPager::new(pager)).unwrap();
        db.set_durability(match durability {
            Durability::Full => big_db::Durability::Full,
            Durability::Relaxed => big_db::Durability::None,
        })
        .unwrap();
        db.create_table(TABLE).unwrap();
        db.create_field(TABLE, FIELD, FieldKind::Int, BIT_DEPTH).unwrap();
        Self { db, dir: dir.to_path_buf(), compacted: false }
    }

    fn ingest(&mut self, batch: &[Record]) {
        let mut w = self.db.write();
        for (id, value) in batch {
            w.set_int(TABLE, FIELD, *id, *value).unwrap();
        }
        w.commit().unwrap();
    }

    fn get(&self, id: u64) -> Option<u64> {
        self.db.read().get_int(TABLE, FIELD, id).unwrap()
    }

    fn count_ge(&self, k: u64) -> u64 {
        self.db.read().count(TABLE, FIELD, RangeOp::Ge, k).unwrap()
    }

    fn len(&self) -> u64 {
        self.db.read().count_all(TABLE).unwrap()
    }

    /// Every commit already fsynced, so there is nothing deferred to flush - but pages freed
    /// by copy-on-write still sit at the tail of the file. `redb` reclaims those in `compact`,
    /// so not calling the equivalent here would compare a compacted rival against an
    /// uncompacted `big` and blame the engine for the harness.
    /// Dropping the old handle first is not optional: the pager takes `flock(LOCK_EX)` on the
    /// file, so a second open while the first is alive fails outright rather than giving a
    /// second view. Taking `self` by value is what makes that ordering the type's problem
    /// instead of the caller's.
    fn threaded_reads(&self, threads: usize, probes: &[u64]) -> u64 {
        crate::fan_out_reads(threads, probes, |id| self.get(id).is_some())
    }

    fn reopen(self, dir: &Path, durability: Durability) -> Self {
        let path = self.dir.join("big.db");
        let compacted = self.compacted;
        drop(self);
        let db = Db::open(CountingPager::new(MmapPager::open_default(&path).unwrap())).unwrap();
        db.set_durability(match durability {
            Durability::Full => big_db::Durability::Full,
            Durability::Relaxed => big_db::Durability::None,
        })
        .unwrap();
        Self { db, dir: dir.to_path_buf(), compacted }
    }

    fn remove(&mut self, ids: &[u64]) -> u64 {
        let mut w = self.db.write();
        let n = w.delete(TABLE, ids).unwrap();
        w.commit().unwrap();
        n
    }

    /// A full compaction is a copy into a fresh file, which is the only operation that returns
    /// space to the filesystem. `truncate_tail` in `checkpoint` is the cheap online case and
    /// only reaches free pages already at the end.
    fn compact(&mut self) {
        let dest = self.dir.join("compacted.db");
        let _ = std::fs::remove_file(&dest);
        self.db.backup_to(&dest).unwrap();
        // Reopened rather than measured in place: the point of the row is the size of the file
        // an operator would be left holding.
        let db = Db::open(CountingPager::new(MmapPager::open_default(&dest).unwrap())).unwrap();
        std::fs::remove_file(self.dir.join("big.db")).unwrap();
        self.db = db;
        self.compacted = true;
    }

    fn checkpoint(&mut self) {
        // Nothing to truncate once the file has been rewritten, and `truncate_tail` on the copy
        // would only measure the copy's own tail rather than the engine's.
        if !self.compacted {
            self.db.store().truncate_tail().unwrap();
        }
    }

    fn disk_bytes(&self) -> u64 {
        dir_size(&self.dir)
    }

    fn bytes_written(&self) -> Option<u64> {
        Some(self.db.store().pager().counts().bytes_written())
    }
}
