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

//! Answering out of column segments instead of out of an index.
//!
//! **This is a second way to answer the same questions, and that is the thing to be careful
//! about.** Every verb here has a twin in `super::query` that reads bitmaps, and the two must
//! agree exactly - a database with two paths to one number is a database that is quietly wrong
//! half the time. What keeps them honest is not this file: it is
//! `crates/big-db/tests/engines.rs`, which asks every verb of every engine over the same data
//! and requires the answers to be identical.
//!
//! # Why it is worth having at all
//!
//! An index answers "which records" without reading any values, which is unbeatable when the
//! predicate is selective. A scan reads every value in range, which is unbeatable when the
//! predicate is not - and it is the *only* thing that can answer at all when the table keeps no
//! index. The zone map cuts the difference: a segment whose range rules the predicate out is
//! skipped without a page being read, exactly as it is on the index side.
//!
//! # What a scan still cannot do
//!
//! A time window. `Row(f="k", from=…, to=…)` reads the per-day views a time quantum field
//! writes, and those are an index construct - a segment stores which keys a record holds and
//! never when it held them. That is refused by name rather than answered emptily, because an
//! empty answer and "this engine cannot see time" call for opposite actions.

use super::*;
use big_engine::columnar::BLOCK_RECORDS;

/// How one record's cell is tested. Returning a plain `bool` keeps every predicate below a
/// one-liner and keeps the block loop in one place.
type Keep<'a> = &'a (dyn Fn(&Cell) -> bool + Sync);

impl<'db, P: Pager + Sync> DbRead<'db, P> {
    /// Whether this table answers a predicate by scanning rather than by index.
    ///
    /// **This is the whole routing rule, and it is deliberately the dumbest one that is
    /// defensible: if there is an index, use it; otherwise scan.**
    ///
    /// **The index is not always the cheaper path, and this rule knowingly ignores that.** A
    /// bit-sliced index costs one read per bit plane whatever the predicate selects; a segment
    /// costs one read per handful of blocks and does not care how wide the values are. Measured
    /// across both dimensions in `tests/engines.rs` -
    /// `what_an_index_and_a_scan_each_cost_across_width_and_size` - and the answer is not close:
    /// over one `Eq`, the scan reads fewer pages at **every** width and size in that matrix, by
    /// three times at the near end and thirty at the far one.
    ///
    /// So the reason this rule survives is not that it is usually right. It is that the number
    /// which decides where the two paths cross - **how many records a fragment holds** - is not
    /// in the catalog, and putting it there is a format change rather than a new record kind:
    /// `FragmentMeta` is stored, and the policy for changing a stored type is dump and reload.
    /// Until that is paid for, choosing per query would be guessing, and a rule that guesses is
    /// a performance cliff nobody can see coming where this one is merely predictable.
    ///
    /// The matrix is written down so that whoever pays for it starts from a measurement rather
    /// than from this paragraph, and so that a change in either path shows up as a diff there
    /// rather than as a slower query nobody attributes.
    ///
    /// What is *not* left to this rule: a projection always reads columns where they exist, and
    /// that decision lives in `big_exec::ColumnPlan` rather than here. It is not a cost
    /// judgement at all - reconstructing a value from bit planes is strictly more work than
    /// reading it, and for a keyed column it is not possible.
    pub(super) fn scans(&self, table: TableId) -> bool {
        self.catalog.table_by_id(table).is_some_and(|t| !t.engine.has_bitmap())
    }

    /// Visits every segment of a field that could hold a match.
    ///
    /// The scan-side twin of [`DbRead::per_fragment`], down to the zone map and the fan-out, so
    /// that a scan is cancelled, deadlined and parallelised by exactly the machinery an index
    /// read already is.
    pub(super) fn per_segment<T>(
        &self,
        table: TableId,
        field: FieldId,
        window: (Option<u64>, Option<u64>),
        f: impl Fn(&ColumnRead<'_, P>, FragmentKey) -> Result<T> + Sync,
    ) -> Result<Vec<T>>
    where
        T: Send,
    {
        let (lo, hi) = window;
        let candidates: Vec<FragmentKey> = self
            .catalog
            .fragments_of_field(table, field, COLUMN_VIEW)
            .filter(|(_, m)| lo.is_none() && hi.is_none() || m.may_contain(lo, hi))
            .map(|(k, _)| *k)
            .collect();

        self.checkpoint()?;
        let mut out = Vec::with_capacity(candidates.len());
        for key in candidates {
            self.checkpoint()?;
            let Some(seg) = self.segment(&key) else { continue };
            out.push(f(&seg, key)?);
        }
        Ok(out)
    }

    /// The core of every scan-shaped predicate: which records' cells pass `keep`.
    ///
    /// One pass per segment, one decode per block, and the row set built directly in the
    /// container grid the rest of the engine speaks - so what comes back is an ordinary
    /// [`Matches`] that intersects with an index's answer without either side knowing.
    pub(super) fn scan_rows(
        &self,
        table: TableId,
        field: FieldId,
        window: (Option<u64>, Option<u64>),
        keep: Keep<'_>,
    ) -> Result<Matches> {
        let per_shard = self.per_segment(table, field, window, |seg, key| {
            let mut hits: BTreeMap<u64, Vec<u16>> = BTreeMap::new();
            seg.for_each_block(|block, decoded| {
                for (slot, cell) in decoded.slots().iter().enumerate() {
                    if cell.is_null() || !keep(cell) {
                        continue;
                    }
                    let local = block * BLOCK_RECORDS + slot as u64;
                    hits.entry(local >> big_engine::CONTAINER_EXPONENT)
                        .or_default()
                        .push((local & (big_engine::CONTAINER_WIDTH - 1)) as u16);
                }
                Ok(core::ops::ControlFlow::Continue(()))
            })?;

            let mut rows = RowSet::new();
            for (container, offsets) in hits {
                rows.insert(container, Container::from_values(offsets));
            }
            self.charge(rows.byte_size())?;
            Ok((key.shard, rows))
        })?;
        self.gather(per_shard)
    }

    /// Visits every present cell of a field, with the record it belongs to, honouring a filter.
    ///
    /// The aggregate primitive. The filter is tested against the container the record sits in
    /// rather than against a materialised list of ids, so a `sum` over a selection never names a
    /// record - the same property the bit-sliced path has and for the same reason.
    fn scan_cells(
        &self,
        table: TableId,
        field: FieldId,
        filter: Option<&Matches>,
        mut visit: impl FnMut(RecordId, &Cell) -> Result<()>,
    ) -> Result<()> {
        let keys: Vec<FragmentKey> =
            self.catalog.fragments_of_field(table, field, COLUMN_VIEW).map(|(k, _)| *k).collect();

        for key in keys {
            self.checkpoint()?;
            let Some(seg) = self.segment(&key) else { continue };
            let here = filter.map(|f| f.get(key.shard));
            // A filter that names this shard not at all rules out every record in it, which is
            // cheaper to notice here than once per block.
            if matches!(here, Some(None)) {
                continue;
            }
            let here = here.flatten();

            let mut err = None;
            seg.for_each_block(|block, decoded| {
                for (slot, cell) in decoded.slots().iter().enumerate() {
                    if cell.is_null() {
                        continue;
                    }
                    let local = block * BLOCK_RECORDS + slot as u64;
                    if let Some(rows) = here {
                        let container = local >> big_engine::CONTAINER_EXPONENT;
                        let offset = (local & (big_engine::CONTAINER_WIDTH - 1)) as u16;
                        if !rows.get(container).is_some_and(|c| c.as_ref().contains(offset)) {
                            continue;
                        }
                    }
                    if let Err(e) = visit(key.shard * SHARD_WIDTH + local, cell) {
                        err = Some(e);
                        return Ok(core::ops::ControlFlow::Break(()));
                    }
                }
                Ok(core::ops::ControlFlow::Continue(()))
            })?;
            if let Some(e) = err {
                return Err(e);
            }
        }
        Ok(())
    }

    // ---------------------------------------------------------------------------------------
    // The verbs, as a scan answers them
    // ---------------------------------------------------------------------------------------

    pub(super) fn scan_matching(
        &self,
        table: TableId,
        field: FieldId,
        op: RangeOp,
        k: u64,
    ) -> Result<Matches> {
        self.scan_rows(table, field, super::query::zone_bounds(op, k), &|c: &Cell| {
            c.value().is_some_and(|v| compare(op, v, k))
        })
    }

    /// Records holding a given row id, whether the column stores one value or a list.
    ///
    /// One predicate for both keyed shapes on purpose: a mutex is a set field that happens to
    /// allow one value, and making the caller pick would be making them know which.
    pub(super) fn scan_matching_row(
        &self,
        table: TableId,
        field: FieldId,
        row: RowId,
    ) -> Result<Matches> {
        self.scan_rows(table, field, (None, None), &move |c: &Cell| {
            c.value() == Some(row) || c.list().contains(&row)
        })
    }

    pub(super) fn scan_matching_bool(
        &self,
        table: TableId,
        field: FieldId,
        value: bool,
    ) -> Result<Matches> {
        let want = value as u64;
        self.scan_rows(table, field, (None, None), &move |c: &Cell| c.value() == Some(want))
    }

    /// Every record that holds any value at all in this field - the segment's own exists row.
    pub(super) fn scan_present(&self, table: TableId, field: FieldId) -> Result<Matches> {
        self.scan_rows(table, field, (None, None), &|_: &Cell| true)
    }

    pub(super) fn scan_count_values(
        &self,
        table: TableId,
        field: FieldId,
        rows: &Matches,
    ) -> Result<u64> {
        let mut n = 0u64;
        self.scan_cells(table, field, Some(rows), |_, _| {
            n += 1;
            Ok(())
        })?;
        Ok(n)
    }

    pub(super) fn scan_sum(
        &self,
        table: TableId,
        field: FieldId,
        rows: Option<&Matches>,
    ) -> Result<u128> {
        let mut total = 0u128;
        self.scan_cells(table, field, rows, |_, c| {
            if let Some(v) = c.value() {
                total += v as u128;
            }
            Ok(())
        })?;
        Ok(total)
    }

    pub(super) fn scan_extreme(
        &self,
        table: TableId,
        field: FieldId,
        rows: Option<&Matches>,
        max: bool,
    ) -> Result<Option<u64>> {
        let mut best: Option<u64> = None;
        self.scan_cells(table, field, rows, |_, c| {
            if let Some(v) = c.value() {
                best = Some(match best {
                    None => v,
                    Some(b) if max => b.max(v),
                    Some(b) => b.min(v),
                });
            }
            Ok(())
        })?;
        Ok(best)
    }

    /// Every row of a keyed field appearing in `filter`, with how many records hold it.
    ///
    /// A hash aggregate over the scan, which is what a column store does instead of one pass per
    /// distinct value. Shards are summed before anything is ranked, exactly as on the index
    /// side: a row that is second everywhere can beat one that is first in a single shard.
    pub(super) fn scan_group_counts(
        &self,
        table: TableId,
        field: FieldId,
        filter: &Matches,
    ) -> Result<Vec<(RowId, u64)>> {
        let mut totals: BTreeMap<RowId, u64> = BTreeMap::new();
        self.scan_cells(table, field, Some(filter), |_, c| {
            for row in rows_of(c) {
                *totals.entry(row).or_default() += 1;
            }
            Ok(())
        })?;
        Ok(totals.into_iter().collect())
    }

    /// The same, each row carrying the records that hold it.
    pub(super) fn scan_group_matches(
        &self,
        table: TableId,
        field: FieldId,
        filter: &Matches,
    ) -> Result<Vec<(RowId, Matches)>> {
        let mut hits: BTreeMap<RowId, BTreeMap<ShardId, BTreeMap<u64, Vec<u16>>>> = BTreeMap::new();
        // Charged as it accumulates, not once at the end.
        //
        // A grouping over a high-cardinality column is the one read that can outgrow its own
        // input, and the index side charges each group as it builds it. Charging only after the
        // whole scan finished would make the ceiling a report of what was already held rather
        // than a guard against holding it - which is the opposite of what it is for.
        //
        // Batched because `charge` is an atomic add and this loop runs once per record: the
        // counter is flushed every `CHARGE_EVERY` offsets, so the overshoot is bounded at a few
        // kilobytes and the atomic is paid once per few thousand records rather than per record.
        const CHARGE_EVERY: usize = 4096;
        let mut pending = 0usize;
        self.scan_cells(table, field, Some(filter), |record, c| {
            let shard = shard_of(record);
            let local = big_engine::local_of(record);
            for row in rows_of(c) {
                hits.entry(row)
                    .or_default()
                    .entry(shard)
                    .or_default()
                    .entry(local >> big_engine::CONTAINER_EXPONENT)
                    .or_default()
                    .push((local & (big_engine::CONTAINER_WIDTH - 1)) as u16);
                pending += 1;
            }
            if pending >= CHARGE_EVERY {
                self.charge(pending * core::mem::size_of::<u16>())?;
                pending = 0;
            }
            Ok(())
        })?;
        self.charge(pending * core::mem::size_of::<u16>())?;

        let mut out = Vec::with_capacity(hits.len());
        for (row, shards) in hits {
            let mut m = Matches::new();
            for (shard, containers) in shards {
                let mut rows = RowSet::new();
                for (container, offsets) in containers {
                    rows.insert(container, Container::from_values(offsets));
                }
                m.insert(shard, rows);
            }
            out.push((row, m));
        }
        Ok(out)
    }
}

/// The row ids a keyed cell holds, whichever shape it stores them in.
fn rows_of(c: &Cell) -> Vec<RowId> {
    match c {
        Cell::Null => Vec::new(),
        Cell::Value(v) => vec![*v],
        Cell::List(v) => v.clone(),
    }
}

/// One value against one bound, in the units storage keeps.
///
/// The same monotonic encoding the bit-sliced path relies on means a signed field needs no arm
/// here either: the bound is biased before it arrives and the comparison is unchanged.
fn compare(op: RangeOp, v: u64, k: u64) -> bool {
    match op {
        RangeOp::Gt => v > k,
        RangeOp::Ge => v >= k,
        RangeOp::Lt => v < k,
        RangeOp::Le => v <= k,
        RangeOp::Eq => v == k,
        RangeOp::Ne => v != k,
    }
}
