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

//! Accumulating writes across calls, so that a commit carries as many records per fragment as
//! it can.
//!
//! The engine's write cost is not per record and not really per commit either: a commit
//! rewrites every container it touched in full, and a container costs the same whether one
//! record in it changed or ten thousand did. What actually gets amortised is *records per
//! fragment per commit*.
//!
//! That distinction is invisible when ids are dense, because a batch of a thousand consecutive
//! ids lands in one shard and therefore one fragment. It is the whole story when they are not:
//! the same thousand records spread across sixty-four shards put sixteen records in each
//! fragment, and cost accordingly. Raising the batch size does not fix that, because the extra
//! records land in new fragments rather than in fuller ones.
//!
//! So this buffers in plain memory and decides for itself when to commit, which decouples how
//! often the caller hands over records from how often the engine pays for a commit. It
//! deliberately does *not* hold a write transaction open while it accumulates: the store allows
//! one writer, and holding that slot for the length of an ingest would stall every other
//! writer for the same reason it would help this one.

use crate::catalog::TableRef;
use crate::db::Db;
use crate::error::{DbError, Result};
use big_engine::{shard_of, RecordId, RowId, ShardId};
use big_pager::PagerMut;
use std::collections::{BTreeMap, HashMap};

/// A buffered write, in the form it will be replayed in.
///
/// **Text is held by index, not by value.** A key or a time value is a short string drawn from
/// a small alphabet - a hundred categories, a few thousand days - repeated across every record
/// that mentions it. Storing the `String` inline cost one heap allocation *per fact*, which on
/// a load whose whole point is volume is the single largest per-fact cost in this file, and it
/// bought nothing: the same twenty bytes were allocated, copied and freed millions of times to
/// hold a value already in hand. [`Pool`] holds each distinct string once and the op holds a
/// four-byte index into it.
///
/// It also halves the buffer. `Value` was 40 bytes because of the `String`; it is 16 now, so
/// `Op` fell from 56 to 32 - which is not only memory but the replay pass, which walks every
/// op in order and is bound by how much of it fits in cache.
enum Value {
    Int(u64),
    Bool(bool),
    Key(u32),
    Time { value: u32, unix_seconds: i64 },
}

struct Op {
    /// Index into `targets`, so an op costs four bytes rather than two `String`s.
    target: u32,
    record: RecordId,
    value: Value,
}

/// Distinct strings buffered by this ingest, each held once.
///
/// Bounded by the buffer rather than by the run: [`Pool::clear`] runs whenever the op buffer is
/// drained in full, so a stream of a billion distinct time values holds at most one buffer-full
/// of them at a time. A partial flush leaves it alone, because the ops it retained still index
/// into it.
#[derive(Default)]
struct Pool {
    texts: Vec<String>,
    index: HashMap<String, u32>,
}

impl Pool {
    fn intern(&mut self, text: &str) -> u32 {
        if let Some(i) = self.index.get(text) {
            return *i;
        }
        let i = self.texts.len() as u32;
        self.texts.push(text.to_string());
        self.index.insert(text.to_string(), i);
        i
    }

    fn get(&self, i: u32) -> &str {
        &self.texts[i as usize]
    }

    fn clear(&mut self) {
        self.texts.clear();
        self.index.clear();
    }
}

/// Buffers writes and commits them in batches sized for the engine rather than for the caller.
///
/// Two things change relative to writing through [`Db::write`] directly:
///
/// - **Nothing is readable until it is flushed.** Buffered records are not in any transaction
///   yet, so a read between calls will not see them.
/// - **[`finish`] is not optional.** Committing from a destructor would turn a failed write
///   into a silent one, so a dropped `Ingest` discards whatever it still holds. Debug builds
///   assert rather than let that pass unnoticed.
///
/// Order is preserved exactly: ops are replayed in the order they arrived, so last-write-wins
/// per record means what it would have meant inside one transaction.
///
/// [`finish`]: Ingest::finish
pub struct Ingest<'db, P: PagerMut> {
    db: &'db Db<P>,
    /// Interned `(table, field)` pairs. There are a handful of these and thousands of ops, so
    /// a linear scan to intern is cheaper than the allocations it avoids.
    targets: Vec<(String, String)>,
    ops: Vec<Op>,
    /// Every key and time value the buffered ops name, held once each. See [`Value`].
    pool: Pool,
    capacity: usize,
    /// Fraction of the buffered shards a flush commits, fullest first. `1.0` commits all of
    /// them, which is what this type did before the knob existed. See
    /// [`Ingest::with_flush_fraction`].
    flush_fraction: f64,
    commits: u64,
    records: u64,
}

impl<'db, P: PagerMut> Ingest<'db, P> {
    /// Buffers up to `capacity` records before committing on its own.
    ///
    /// `capacity` is the knob that matters: divided by the number of shards the workload
    /// spreads across, it is how many records each fragment gets per commit.
    pub fn new(db: &'db Db<P>, capacity: usize) -> Self {
        Self {
            db,
            targets: Vec::new(),
            ops: Vec::new(),
            pool: Pool::default(),
            // A zero would mean "flush before anything is buffered", which is not a policy.
            capacity: capacity.max(1),
            flush_fraction: 1.0,
            commits: 0,
            records: 0,
        }
    }

    /// Commits only the fullest `fraction` of the buffered shards at each flush, staggering
    /// them instead of committing all at once.
    ///
    /// **What it buys.** A commit's cost has a floor of roughly four to six pages *per fragment
    /// it touches* - the copy-on-write rewrite of that fragment's root-to-leaf path - paid
    /// whether that fragment received a thousand records or one. Flushing every shard together
    /// pays that floor for all of them at `capacity / shards` records each. Flushing a fraction
    /// and leaving the rest to accumulate means a shard is committed having gathered records
    /// from several buffer-fulls rather than one, so the floor is amortised over more of them.
    ///
    /// **A threshold does not work here and a fraction does, which is worth knowing before
    /// changing this.** The obvious knob is "withhold a shard until it holds N records", and it
    /// is useless: when arrivals are spread evenly, every shard holds the same count, so a
    /// threshold either selects all of them or none, and the flush is the one it already was.
    /// Ranking by fullness and cutting the list is what breaks the symmetry and lets some shards
    /// run ahead of others. It was measured both ways.
    ///
    /// **It is off by default because it is a trade, not an improvement.** It buys bytes with
    /// *commits*, and a commit costs two fsyncs. Roughly `1 / fraction` times as many, since a
    /// flush that frees a fraction of the buffer refills after that fraction of arrivals - so
    /// `0.25` is about four times the fsyncs.
    ///
    /// At `0.25`, against the same ingest with the knob off:
    ///
    /// | fan-out | bytes/record | wall clock, full durability |
    /// |---|---|---|
    /// | 1 shard (dense) | unchanged | unchanged |
    /// | 64 shards | 2,272 -> 1,574 (**1.4x better**) | 147ms -> 175ms (**1.2x worse**) |
    /// | 256 shards | 10,302 -> 7,295 (**1.4x better**) | 322ms -> 276ms (1.2x better) |
    /// | 1,024 shards | 45,130 -> 29,767 (**1.5x better**) | - |
    /// | 4,096 shards | - | 4,822ms -> 3,369ms (**1.4x better**) |
    ///
    /// So it pays once the fan-out is wide enough that the bytes saved outweigh the extra
    /// flushes, and costs time when it is not. There is no constant that is right for both,
    /// which is why this is a knob rather than a default. The byte figures are gated in
    /// `tests/amplification.rs`; the timings are from one machine and are the half that does
    /// not transfer.
    ///
    /// **The ceiling is modest and worth knowing before reaching for it.** With `capacity`
    /// records in hand spread over `shards` shards, no policy can average more than
    /// `capacity / shards` records per fragment per commit. **Capacity is still the lever that
    /// matters**; this is a second-order correction to how it is spent. `Db::bulk_load` beats
    /// both by a wide margin when the whole load is known up front, because it can give each
    /// fragment its entire share in one commit - which a stream cannot.
    ///
    /// Clamped to `[0.1, 1.0]`: below a tenth the commit multiplier stops being a trade and
    /// starts being this engine's worst case, which is a commit per record.
    pub fn with_flush_fraction(mut self, fraction: f64) -> Self {
        self.flush_fraction = if fraction.is_nan() { 1.0 } else { fraction.clamp(0.1, 1.0) };
        self
    }

    pub fn set_int(
        &mut self,
        table: &str,
        field: &str,
        record: RecordId,
        value: u64,
    ) -> Result<()> {
        let target = self.target(table, field)?;
        self.push(target, record, Value::Int(value))
    }

    pub fn set_bool(
        &mut self,
        table: &str,
        field: &str,
        record: RecordId,
        value: bool,
    ) -> Result<()> {
        let target = self.target(table, field)?;
        self.push(target, record, Value::Bool(value))
    }

    /// Unlike [`DbWrite::set_key`], this cannot hand back the interned `RowId`: the row is not
    /// interned until the flush. A caller that needs the id has to write through `Db::write`.
    ///
    /// [`DbWrite::set_key`]: crate::db::DbWrite::set_key
    pub fn set_key(
        &mut self,
        table: &str,
        field: &str,
        record: RecordId,
        value: &str,
    ) -> Result<()> {
        let target = self.target(table, field)?;
        let value = self.pool.intern(value);
        self.push(target, record, Value::Key(value))
    }

    pub fn set_time(
        &mut self,
        table: &str,
        field: &str,
        record: RecordId,
        value: &str,
        unix_seconds: i64,
    ) -> Result<()> {
        let target = self.target(table, field)?;
        let value = self.pool.intern(value);
        self.push(target, record, Value::Time { value, unix_seconds })
    }

    /// Commits whatever is buffered. A no-op when there is nothing to write, so it is safe to
    /// call at a checkpoint that may or may not have received records.
    ///
    /// On failure the buffer is left intact rather than cleared: the transaction is gone, but
    /// the records are still the caller's to retry or report.
    pub fn flush(&mut self) -> Result<()> {
        self.flush_shards(None)
    }

    /// Commits every buffered op, whatever [`Ingest::with_min_per_shard`] says.
    ///
    /// What [`Ingest::finish`] calls, and the reason the knob cannot lose records: whatever a
    /// partial flush withheld is committed here.
    fn flush_all(&mut self) -> Result<()> {
        self.flush_shards(Some(1.0))
    }

    /// `fraction` overrides the configured one. `None` uses it.
    fn flush_shards(&mut self, fraction: Option<f64>) -> Result<()> {
        if self.ops.is_empty() {
            return Ok(());
        }
        let fraction = fraction.unwrap_or(self.flush_fraction);

        // The fast path is also the default one: committing everything needs no grouping, and
        // grouping would be work done to reach the answer it already has.
        let selected: Option<Vec<ShardId>> = if fraction >= 1.0 {
            None
        } else {
            let mut per_shard: BTreeMap<ShardId, usize> = BTreeMap::new();
            for op in &self.ops {
                *per_shard.entry(shard_of(op.record)).or_insert(0) += 1;
            }
            // One shard is not worth staggering: there is nothing for it to run ahead of, and
            // withholding it would be a commit that wrote nothing.
            if per_shard.len() < 2 {
                None
            } else {
                let mut by_fullness: Vec<(ShardId, usize)> = per_shard.into_iter().collect();
                by_fullness.sort_by_key(|(shard, n)| (core::cmp::Reverse(*n), *shard));
                // At least one, so a flush always makes progress and the buffer cannot grow
                // without bound.
                let take = ((by_fullness.len() as f64 * fraction).ceil() as usize)
                    .clamp(1, by_fullness.len());
                Some(by_fullness.into_iter().take(take).map(|(s, _)| s).collect())
            }
        };

        let db = self.db;
        let mut w = db.write();
        // Every target resolved once, before a single op is replayed. There are a handful of
        // these and millions of ops, and resolving by name per op walked the catalog twice and
        // cloned a `FieldDef` — a name and a granularity list allocated per fact — to learn
        // something fixed for the length of the transaction. Doing it up front also means a
        // batch naming a field that does not exist is refused before any of it is applied.
        let at: Vec<crate::db::At> =
            self.targets.iter().map(|(table, field)| w.at(table, field)).collect::<Result<_>>()?;

        // Row ids for the keys this flush names, resolved on first use and reused after.
        //
        // Interning is idempotent, so replaying a key per fact asked the catalog for an answer
        // it had already given - once per fact, for the whole buffer. Keyed fields draw from a
        // small alphabet by definition, so what this holds is that alphabet: one entry per
        // `(field, distinct key)`, filled lazily so a flush that names three keys allocates
        // three slots rather than one per buffered value.
        let mut rows: Vec<Vec<Option<RowId>>> = vec![Vec::new(); at.len()];
        let mut written = 0u64;
        for op in &self.ops {
            if let Some(shards) = &selected {
                if !shards.contains(&shard_of(op.record)) {
                    continue;
                }
            }
            let at = &at[op.target as usize];
            match &op.value {
                Value::Int(v) => w.set_int_at(at, op.record, *v)?,
                Value::Bool(v) => w.set_bool_at(at, op.record, *v)?,
                Value::Key(v) => {
                    let slot = &mut rows[op.target as usize];
                    let i = *v as usize;
                    if slot.len() <= i {
                        slot.resize(i + 1, None);
                    }
                    let row = match slot[i] {
                        Some(row) => row,
                        None => {
                            let row = w.intern_key_at(at, self.pool.get(*v))?;
                            slot[i] = Some(row);
                            row
                        }
                    };
                    w.set_row_at(at, op.record, row)?;
                }
                Value::Time { value, unix_seconds } => {
                    w.set_time_at(at, op.record, self.pool.get(*value), *unix_seconds)?;
                }
            }
            written += 1;
        }
        w.commit()?;

        self.records += written;
        self.commits += 1;
        match &selected {
            // Nothing indexes into the pool once the buffer is empty, so it goes with it. That
            // is what keeps it bounded by the buffer rather than by the length of the run.
            None => {
                self.ops.clear();
                self.pool.clear();
            }
            // Retained in arrival order. Ops on one record are always in one shard - a record
            // belongs to exactly one - so per-record last-write-wins is preserved even though
            // ops on *different* shards no longer interleave the way they arrived.
            Some(shards) => self.ops.retain(|op| !shards.contains(&shard_of(op.record))),
        }
        Ok(())
    }

    /// Flushes the remainder and reports how many records were committed in total.
    pub fn finish(mut self) -> Result<u64> {
        self.flush_all()?;
        Ok(self.records)
    }

    /// Records held but not yet committed.
    pub fn buffered(&self) -> usize {
        self.ops.len()
    }

    /// Commits made so far. The point of the whole type is for this to be small.
    pub fn commits(&self) -> u64 {
        self.commits
    }

    /// Interns a `(table, field)` pair, resolving it once so that a misspelled name fails at
    /// the call that made it rather than at a flush thousands of records later.
    ///
    /// Only existence is checked here. Whether the value suits the field's kind is left to the
    /// setter at flush time, because that is where the value actually gets used and duplicating
    /// the rule would give it two places to drift.
    fn target(&mut self, table: &str, field: &str) -> Result<u32> {
        if let Some(i) = self.targets.iter().position(|(t, f)| t == table && f == field) {
            return Ok(i as u32);
        }
        {
            let catalog = self.db.catalog();
            let t = catalog.require(TableRef::bare(table))?;
            catalog.field(t.id, field).ok_or_else(|| DbError::UnknownField {
                table: table.to_string(),
                field: field.to_string(),
            })?;
        }
        self.targets.push((table.to_string(), field.to_string()));
        Ok((self.targets.len() - 1) as u32)
    }

    fn push(&mut self, target: u32, record: RecordId, value: Value) -> Result<()> {
        self.ops.push(Op { target, record, value });
        if self.ops.len() >= self.capacity {
            self.flush()?;
        }
        Ok(())
    }
}

impl<P: PagerMut> Drop for Ingest<'_, P> {
    fn drop(&mut self) {
        // Not a flush: a commit from a destructor has nowhere to report a failure, and a write
        // that fails silently is worse than one that never happened.
        debug_assert!(
            self.ops.is_empty(),
            "Ingest dropped holding {} uncommitted records - call finish()",
            self.ops.len()
        );
    }
}
