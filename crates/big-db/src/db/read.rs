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

//! A read transaction, its ceilings, and the fan-out every scan goes through.
//!
//! The machinery rather than the verbs: what a query is allowed to spend, how it notices it has
//! been cancelled or has run out of time, and how one predicate becomes one job per candidate
//! fragment. The verbs that use it are in `super::query`, and they are three lines each
//! because this is where the work is.

use super::*;

/// What one read transaction is allowed to materialise.
///
/// Both ceilings exist because they fail differently. `max_bytes` is the fan-out: a scan
/// holds one row set per candidate shard at once, so a wide predicate over a large corpus can
/// ask for the whole corpus in bitmaps before anything has been reduced. `max_records` is the
/// materialisation: a query that finally names its answers turns compact bitmaps into eight
/// bytes per record, which is where a result that fitted comfortably stops fitting.
///
/// The defaults are generous - they are a guard against a runaway query, not a quota - and a
/// caller that knows its workload can set its own with [`DbRead::with_limits`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct QueryLimits {
    pub max_bytes: usize,
    pub max_records: usize,
}

impl Default for QueryLimits {
    fn default() -> Self {
        Self { max_bytes: 1 << 28, max_records: 1 << 24 }
    }
}

pub struct DbRead<'db, P: Pager> {
    #[allow(dead_code)]
    txn: ReadTxn<'db, P>,
    pub(super) catalog: Catalog,
    store: &'db Store<P>,
    limits: QueryLimits,
    /// Bitmap bytes materialised so far by this transaction.
    ///
    /// Atomic because a scan charges from inside the parallel fan-out, which is exactly where
    /// the peak is: every thread is holding row sets at the same moment.
    spent: std::sync::atomic::AtomicUsize,
    /// When this transaction started, so a timeout can say how long it actually ran rather
    /// than only that it ran too long.
    started: std::time::Instant,
    /// The moment after which this transaction gives up. `None` means it runs to completion,
    /// which stays the default: a timeout that nobody chose is a query that fails for reasons
    /// the caller cannot see.
    deadline: Option<std::time::Instant>,
    /// Set from outside when whoever asked for this read no longer wants it.
    ///
    /// Shared rather than owned because the thread that notices is never the thread running
    /// the query - the whole point is that the query is busy.
    cancelled: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
}

impl<P: PagerMut> Db<P> {
    /// Opens a read transaction, which holds the reclaim horizon down for as long as it lives.
    pub fn read(&self) -> DbRead<'_, P> {
        DbRead {
            txn: self.store.begin_read(),
            catalog: self.catalog.read().unwrap().clone(),
            store: &self.store,
            limits: QueryLimits::default(),
            spent: std::sync::atomic::AtomicUsize::new(0),
            started: std::time::Instant::now(),
            deadline: None,
            cancelled: None,
        }
    }
}

/// Read methods require a pager that can be shared between threads, because a scan fans out
/// across the fragments it has to visit. Both pagers here qualify; a hypothetical one that did
/// not would still be usable for writes.
impl<'db, P: Pager + Sync> DbRead<'db, P> {
    /// Answers this transaction from a set of shard ranges and no others.
    ///
    /// **What makes one node able to hold two ranges.** Every scan reaches a fragment through
    /// [`crate::Catalog::fragments_of_field`] or `fragments_of_table`, and both consult the
    /// scope set here - so this is one call rather than a parameter on every enumerator, and a
    /// scan cannot be written that forgets it.
    ///
    /// It is also what keeps a *leftover* honest. A node that has just handed a range to
    /// somebody else still holds those fragments until it deletes them, and a coordinator that
    /// asked it only about the ranges it still serves gets the right answer throughout - so
    /// the answer depends on what the node was asked for rather than on what happens to be on
    /// its disk.
    pub fn with_shards(mut self, ranges: Vec<big_engine::ShardRange>) -> Self {
        self.catalog.restrict_to_shards(ranges);
        self
    }

    /// Replaces the memory ceilings for this transaction.
    pub fn with_limits(mut self, limits: QueryLimits) -> Self {
        self.limits = limits;
        self
    }

    pub fn limits(&self) -> QueryLimits {
        self.limits
    }

    /// Gives this transaction `budget` of wall-clock time.
    ///
    /// Wall clock rather than CPU time, because what an operator is protecting is a worker
    /// thread and a socket, and both are held for wall-clock time whatever the query spends
    /// it on.
    pub fn with_deadline(mut self, budget: std::time::Duration) -> Self {
        self.deadline = Some(self.started + budget);
        self
    }

    /// Hands this transaction a flag that anyone may set to stop it.
    ///
    /// Cooperative, not pre-emptive: nothing here interrupts a thread. The flag is read at the
    /// same places the memory budget is charged, which is every point where the scan is about
    /// to do more work rather than less.
    pub fn with_cancel(mut self, flag: std::sync::Arc<std::sync::atomic::AtomicBool>) -> Self {
        self.cancelled = Some(flag);
        self
    }

    /// Whether this transaction is still wanted. Called wherever the scan is about to commit
    /// to more work.
    ///
    /// Two checks, in the order they cost: an atomic load, then a clock read. `Instant::now`
    /// is tens of nanoseconds and this runs once per fragment rather than once per container,
    /// so it is invisible next to reading a page.
    pub(super) fn checkpoint(&self) -> Result<()> {
        if let Some(flag) = &self.cancelled {
            if flag.load(std::sync::atomic::Ordering::Relaxed) {
                return Err(DbError::QueryCancelled);
            }
        }
        if let Some(deadline) = self.deadline {
            let now = std::time::Instant::now();
            if now >= deadline {
                return Err(DbError::QueryTimeout {
                    elapsed_ms: now.duration_since(self.started).as_millis() as u64,
                    limit_ms: deadline.duration_since(self.started).as_millis() as u64,
                });
            }
        }
        Ok(())
    }

    /// Bitmap bytes this transaction has materialised so far, for diagnosing a refusal or
    /// choosing a ceiling that fits a real workload rather than a guessed one.
    pub fn spent_bytes(&self) -> usize {
        self.spent.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    /// Books `bytes` against the transaction's ceiling, or refuses.
    ///
    /// Charged where a row set is produced rather than where it is finally assembled, because
    /// the fan-out is the peak: by the time the shards have been merged the worst is over.
    pub(super) fn charge(&self, bytes: usize) -> Result<()> {
        use std::sync::atomic::Ordering;
        // Charging is exactly where the scan is about to hold more memory, which makes it
        // exactly where it should first ask whether anyone still wants the answer.
        self.checkpoint()?;
        let total = self.spent.fetch_add(bytes, Ordering::Relaxed) + bytes;
        if total > self.limits.max_bytes {
            return Err(DbError::QueryTooLarge {
                limit: self.limits.max_bytes,
                needed: total,
                unit: "bytes",
            });
        }
        Ok(())
    }

    /// Checks a record count *before* materialising it. The cardinality of a `Matches` is
    /// known without naming a single record, so an answer too large to hold costs nothing to
    /// refuse.
    ///
    /// Public because the executor needs it too: a projection with no `LIMIT` reads every
    /// matching record, and the one ceiling on that should be the one every other unbounded
    /// read already answers to rather than a second number kept somewhere else.
    pub fn check_records(&self, n: u64) -> Result<()> {
        if n > self.limits.max_records as u64 {
            return Err(DbError::QueryTooLarge {
                limit: self.limits.max_records,
                needed: n as usize,
                unit: "records",
            });
        }
        Ok(())
    }

    /// Assembles per-shard scan results into one answer, charging as it goes.
    pub(super) fn gather(&self, per_shard: Vec<(ShardId, RowSet)>) -> Result<Matches> {
        let mut out = Matches::new();
        for (shard, rows) in per_shard {
            out.insert(shard, rows);
        }
        Ok(out)
    }

    pub(super) fn frag(&self, key: &FragmentKey) -> Option<FragmentRead<'_, P>> {
        self.txn.root(key).map(|r| FragmentRead::new(self.store.pager(), r, key.shard))
    }

    /// The column segment for one field and shard, when the table keeps one and it has data.
    pub(super) fn segment(&self, key: &FragmentKey) -> Option<ColumnRead<'_, P>> {
        self.txn.root(key).map(|r| ColumnRead::new(self.store.pager(), r))
    }

    /// One record's stored cell, read out of the segment rather than rebuilt from bit planes.
    ///
    /// `None` when the table keeps no columns, so a caller can fall back to the index without
    /// having to ask the catalog itself. `Some(Cell::Null)` is a different answer: the table has
    /// columns and this record holds nothing in that one.
    pub fn column_cell<'a>(
        &self,
        table: impl Into<TableRef<'a>>,
        field: &str,
        record: RecordId,
    ) -> Result<Option<Cell>> {
        let table = table.into();
        let (t, def) = resolve(&self.catalog, table, field)?;
        if !self.catalog.table_by_id(t).is_some_and(|x| x.engine.has_columns()) {
            return Ok(None);
        }
        let key =
            FragmentKey { table: t, field: def.id, view: COLUMN_VIEW, shard: shard_of(record) };
        let Some(seg) = self.segment(&key) else { return Ok(Some(Cell::Null)) };
        Ok(Some(seg.cell(local_of(record))?))
    }

    /// How many records of a table hold a value in one column.
    ///
    /// Off the cached cardinality in each leaf cell, so it reads no payload at all - the same
    /// trick `count_all` plays on the exists row.
    pub fn column_count<'a>(&self, table: impl Into<TableRef<'a>>, field: &str) -> Result<u64> {
        let table = table.into();
        let (t, def) = resolve(&self.catalog, table, field)?;
        let mut total = 0;
        for (key, _) in self.catalog.fragments_of_field(t, def.id, COLUMN_VIEW) {
            if let Some(seg) = self.segment(key) {
                total += seg.count()?;
            }
        }
        Ok(total)
    }

    /// A signed field's value for one record, unbiased on the way out.
    ///
    /// The bias comes from the field's **declared** depth, never from the fragment's. A
    /// fragment's depth grows as wider values arrive; a bias that moved with it would decode
    /// the same stored bits to different numbers in different shards.
    pub fn get_signed<'a>(
        &self,
        table: impl Into<TableRef<'a>>,
        field: &str,
        record: RecordId,
    ) -> Result<Option<i64>> {
        let table = table.into();
        let (_, def) = resolve(&self.catalog, table, field)?;
        expect_kind(&def, field, FieldKind::is_signed, "signed int")?;
        let declared = declared_depth(&def);
        Ok(self.get_int(table, field, record)?.map(|v| crate::signed::decode(v, declared)))
    }

    /// A float field's value for one record, decoded on the way out.
    pub fn get_float<'a>(
        &self,
        table: impl Into<TableRef<'a>>,
        field: &str,
        record: RecordId,
    ) -> Result<Option<f64>> {
        let table = table.into();
        let (_, def) = resolve(&self.catalog, table, field)?;
        expect_kind(&def, field, FieldKind::is_float, "float")?;
        let declared = declared_depth(&def);
        Ok(self.get_int(table, field, record)?.map(|v| crate::float::decode(v, declared)))
    }

    pub fn get_int<'a>(
        &self,
        table: impl Into<TableRef<'a>>,
        field: &str,
        record: RecordId,
    ) -> Result<Option<u64>> {
        let table = table.into();
        let (t, def) = resolve(&self.catalog, table, field)?;
        let key =
            FragmentKey { table: t, field: def.id, view: STANDARD_VIEW, shard: shard_of(record) };
        let Some(f) = self.frag(&key) else { return Ok(None) };
        let depth = self.catalog.fragment(&key).map_or(def.bit_depth, |m| m.bit_depth);
        Ok(Bsi::new(depth.max(1)).get(&f, record)?)
    }

    /// Visits every fragment of a field that could hold a match.
    ///
    /// The one place that knows how to turn a field name into a set of fragments, drop the
    /// ones a zone map rules out, and skip the ones with no root yet. Every scan goes through
    /// here, so when this loop becomes a thread pool it becomes one exactly once - which is
    /// what the `map` half of a map/reduce executor needs.
    ///
    /// `window` is the value range a zone map is tested against; `None, None` means every
    /// fragment is a candidate.
    pub(super) fn per_fragment<T>(
        &self,
        table: TableId,
        field: FieldId,
        window: (Option<u64>, Option<u64>),
        f: impl Fn(&FragmentRead<'_, P>, FragmentKey, u32) -> Result<T> + Sync,
    ) -> Result<Vec<T>>
    where
        P: Sync,
        T: Send,
    {
        self.per_fragment_in(table, field, STANDARD_VIEW, window, f)
    }

    /// The same, in a named view rather than the standard one.
    ///
    /// A time quantum field writes the same facts into one extra view per granularity, so
    /// reading them back is this loop again with a different view id - which is why the view
    /// had to stop being hard-coded here.
    pub(super) fn per_fragment_in<T>(
        &self,
        table: TableId,
        field: FieldId,
        view: ViewId,
        window: (Option<u64>, Option<u64>),
        f: impl Fn(&FragmentRead<'_, P>, FragmentKey, u32) -> Result<T> + Sync,
    ) -> Result<Vec<T>>
    where
        P: Sync,
        T: Send,
    {
        let (lo, hi) = window;
        // Collected before the loop: the borrow of the catalog cannot be held across `f`,
        // which may want the catalog itself.
        let candidates: Vec<(FragmentKey, u32)> = self
            .catalog
            .fragments_of_field(table, field, view)
            .filter(|(_, m)| lo.is_none() && hi.is_none() || m.may_contain(lo, hi))
            .map(|(k, m)| (*k, m.bit_depth.max(1)))
            .collect();

        // Shards are independent by construction - a fragment holds every fact about its own
        // records and none about anyone else's - so this is the map half of a map/reduce and
        // it needs no coordination at all. The reduce happens in the caller, which is why
        // every caller here folds an order-independent thing.
        // Before the fan-out, not only inside it. A scan with no candidates does no work and
        // would otherwise answer normally after its budget was already spent - which makes the
        // deadline a property of how much data happens to be there rather than a guarantee.
        self.checkpoint()?;

        let threads = Self::fan_out(candidates.len());
        if threads > 1 {
            return self.map_fragments_parallel(&candidates, threads, &f);
        }

        let mut out = Vec::with_capacity(candidates.len());
        for &(key, depth) in &candidates {
            // Per fragment, not per container: a fragment is the unit of work that is worth
            // abandoning, and a check any finer would put a clock read in the inner loop.
            self.checkpoint()?;
            let Some(frag) = self.frag(&key) else { continue };
            out.push(f(&frag, key, depth)?);
        }
        Ok(out)
    }

    /// How many threads are worth spawning for this many fragments.
    ///
    /// One below the threshold: spawning a thread costs tens of microseconds and a fragment
    /// often costs less, so a small query would spend more on the fan-out than on the work.
    fn fan_out(candidates: usize) -> usize {
        const MIN_PER_THREAD: usize = 4;
        if candidates < 2 * MIN_PER_THREAD {
            return 1;
        }
        let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
        cores.min(candidates / MIN_PER_THREAD).max(1)
    }

    fn map_fragments_parallel<T>(
        &self,
        candidates: &[(FragmentKey, u32)],
        threads: usize,
        f: &(impl Fn(&FragmentRead<'_, P>, FragmentKey, u32) -> Result<T> + Sync),
    ) -> Result<Vec<T>>
    where
        P: Sync,
        T: Send,
    {
        let chunk = candidates.len().div_ceil(threads);
        // Scoped threads so the pager, the catalog and `f` can all be borrowed rather than
        // shared through an `Arc`. Nothing outlives this call.
        let results: Vec<Result<Vec<T>>> = std::thread::scope(|scope| {
            let handles: Vec<_> = candidates
                .chunks(chunk)
                .map(|part| {
                    scope.spawn(move || {
                        let mut local = Vec::with_capacity(part.len());
                        for &(key, depth) in part {
                            // Every worker checks for itself. One worker noticing does not
                            // stop the others mid-fragment, but each stops at its own next
                            // one, so the whole scan unwinds within one fragment's work.
                            self.checkpoint()?;
                            let Some(frag) = self.frag(&key) else { continue };
                            local.push(f(&frag, key, depth)?);
                        }
                        Ok(local)
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().expect("a fragment scan panicked")).collect()
        });

        let mut out = Vec::with_capacity(candidates.len());
        for part in results {
            out.extend(part?);
        }
        Ok(out)
    }
}
