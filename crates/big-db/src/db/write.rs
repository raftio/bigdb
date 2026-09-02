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

//! One transaction, committed by hand.
//!
//! The buffering is the substance here rather than an optimisation. A commit rewrites the
//! catalog and the root record of *every* fragment the database holds, so the cost that matters
//! is commits, not writes - and a write that reaches its fragment immediately walks that
//! fragment's tree once per fact. [`Pending`] holds facts by the place they land until commit
//! knows the shape of the whole batch.

use super::*;
use big_engine::base::engine::{Fact, Half, Sink};
use big_engine::columnar::ColEdit;

/// What an engine decided about one fact.
///
/// A recorder rather than a writer, which is what lets [`big_engine::Engine::place`] be
/// infallible: an engine says *what* should happen and [`DbWrite::route`] makes it happen, where
/// the transaction and the catalog are.
#[derive(Default)]
struct Placed {
    observe_bitmap: Option<u64>,
    observe_columns: Option<u64>,
    planes: Option<u64>,
    bits: Vec<(RowId, bool)>,
    bits_in: Vec<(u32, RowId, bool)>,
    mutex: Option<RowId>,
    cell: Option<ColEdit>,
}

impl Placed {
    /// Empties the record without giving up its buffers.
    ///
    /// **The point is the `Vec`s.** A `Placed` was built per fact, and a keyed or boolean fact
    /// pushes into `bits` - so routing a four-field record allocated and freed three times,
    /// per record, for buffers that never hold more than two entries. `clear` keeps the
    /// capacity, so the first record of a transaction pays for them and the rest do not.
    fn reset(&mut self) {
        self.observe_bitmap = None;
        self.observe_columns = None;
        self.planes = None;
        self.bits.clear();
        self.bits_in.clear();
        self.mutex = None;
        self.cell = None;
    }
}

impl Sink for Placed {
    fn observe(&mut self, half: Half, value: u64) {
        match half {
            Half::Bitmap => self.observe_bitmap = Some(value),
            Half::Columns => self.observe_columns = Some(value),
        }
    }

    fn planes(&mut self, value: u64) {
        self.planes = Some(value);
    }

    fn bit(&mut self, row: RowId, on: bool) {
        self.bits.push((row, on));
    }

    fn bit_in(&mut self, view: u32, row: RowId, on: bool) {
        self.bits_in.push((view, row, on));
    }

    fn mutex(&mut self, row: RowId) {
        self.mutex = Some(row);
    }

    fn cell(&mut self, edit: ColEdit) {
        self.cell = Some(edit);
    }
}

/// Facts waiting to reach one fragment.
///
/// Writing the same place twice in a transaction keeps only the last write, however the buffer
/// below is spelled. That is what makes deferring safe: the tree is shown the final state of the
/// transaction, never an intermediate one it would have to undo.
#[derive(Default)]
struct Pending {
    /// BSI fragments, as record and value. Kept unexpanded because the bit depth can still
    /// grow later in the transaction, and expanding early would write a record at a depth
    /// that turns out to be too narrow.
    ///
    /// A `Vec` in arrival order rather than a map keyed by record, and the flush collapses it
    /// with [`last_per_key`]. Same answer, and see that function for why the ordering is bought
    /// once for the batch instead of once per write.
    values: Vec<(RecordId, u64)>,
    /// Everything else, as a bit and whether it ends up set. Also arrival order; also collapsed
    /// at the flush.
    bits: Vec<((RowId, RecordId), bool)>,
    /// Column cells, for a table whose engine keeps them. Buffered for the same reason the bits
    /// are: a block holds a thousand records, so a write that reached storage per record would
    /// re-encode a thousand values to change one of them.
    ///
    /// Arrival order like the two above, folded rather than collapsed: a set field *adds*, so
    /// two edits for one record are both part of what it holds. See [`fold_edits`].
    ///
    /// This was a `BTreeMap` until the fold learned to skip its sort. Sorting unconditionally
    /// lost to the map by 1.3x, and the reason is the same one that now makes the `Vec` win: a
    /// load hands records over ascending, which is both the case a map appends into its
    /// rightmost leaf for almost nothing *and* the case a fold can detect in one pass and do
    /// nothing about.
    cells: Vec<(RecordId, ColEdit)>,
    /// The zone map this fragment's facts imply, as the lowest and highest value seen.
    ///
    /// **Folded here rather than applied to the catalog per fact.** `Catalog::fragment_mut` is a
    /// map keyed exactly as this one is, so observing a value where it was observed meant a
    /// second descent of a second `BTreeMap` for every fact - the same key, the same depth, the
    /// same comparisons. `observe` is a min and a max, which fold: seeing every value once at
    /// the flush leaves the identical `(min, max, bit_depth)`, because a value's bit width is
    /// monotonic in the value and the widest is therefore the largest.
    observe: Option<(u64, u64)>,
    /// The same for the column half, which lives at this fragment's address one view over.
    observe_cols: Option<(u64, u64)>,
}

/// Widens a folded zone map to include one more value.
fn note_zone(slot: &mut Option<(u64, u64)>, value: u64) {
    match slot {
        Some((lo, hi)) => {
            *lo = (*lo).min(value);
            *hi = (*hi).max(value);
        }
        None => *slot = Some((value, value)),
    }
}

/// Collapses a buffer of writes to one entry per key, keeping the last — which is exactly what
/// a map keyed on the same thing would have held.
///
/// **A `Vec` and one sort, rather than a map kept ordered all along.** A commit buffers a fact
/// per field per record and a bit per plane on top of that, so this is tens of millions of
/// writes; a `BTreeMap` pays a descent and, whenever a node fills, an allocation for every one
/// of them, and the allocator traffic that produces was a fifth of an import. A push is a
/// bounds check. The order still has to be paid for, but once for the batch rather than once
/// per write — and the flush was going to walk the whole buffer anyway.
///
/// The sort is stable, so entries for one key keep their arrival order, and [`dedup_last`] keeps
/// the last of each run rather than the first. An already-ordered buffer skips the sort and keeps
/// that property for free, having never been reordered.
fn last_per_key<K: Ord + Copy, V>(mut buf: Vec<(K, V)>) -> Vec<(K, V)> {
    // One pass first, because the common case needs no work at all: a load hands records over
    // ascending and writes each field of one record once, so the buffer arrives strictly
    // increasing and every entry survives. Establishing that costs one comparison per entry and
    // skips an `n log n` sort of tens of millions — which is most of what the sort was for.
    let (sorted, repeats) = shape_of(&buf);
    if sorted && !repeats {
        return buf;
    }
    if !sorted {
        buf.sort_by_key(|entry| entry.0);
    }
    dedup_last(buf)
}

/// Whether a buffer is already ascending by key, and whether any key repeats.
///
/// One comparison per entry, to find out whether the expensive half of collapsing it can be
/// skipped outright. It usually can: a load hands records over in order and writes each field
/// of one record once.
fn shape_of<K: Ord, V>(buf: &[(K, V)]) -> (bool, bool) {
    let mut repeats = false;
    for pair in buf.windows(2) {
        match pair[0].0.cmp(&pair[1].0) {
            core::cmp::Ordering::Less => {}
            core::cmp::Ordering::Equal => repeats = true,
            core::cmp::Ordering::Greater => return (false, repeats),
        }
    }
    (true, repeats)
}

/// Prepares every buffered fragment, on several threads when there is enough work to pay for
/// them.
///
/// **The threshold is not decoration.** Spawning a thread costs tens of microseconds and a
/// commit of three small fragments is over in less than that, so an unconditional fan-out makes
/// the small case slower - and the small case is every commit an interactive writer makes. Below
/// the threshold this is the loop it replaced, exactly.
fn prepare_all(
    catalog: &Catalog,
    pending: BTreeMap<FragmentKey, Pending>,
    held: &BTreeMap<FragmentKey, RowSet>,
) -> Result<Vec<Prepared>> {
    let work: Vec<(FragmentKey, Pending)> = pending.into_iter().collect();
    let total: usize = work.iter().map(|(_, p)| p.values.len() + p.bits.len()).sum();

    let threads =
        core::cmp::min(work.len(), std::thread::available_parallelism().map_or(1, |n| n.get()));
    if threads < 2 || total < PARALLEL_FLUSH_MIN {
        // Nothing else is running, so the one fragment being prepared may have the whole box.
        let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
        return work
            .into_iter()
            .map(|(key, p)| prepare_fragment(catalog, key, p, held.get(&key), cores))
            .collect();
    }

    // Chunked rather than work-stolen. One fragment is usually most of a commit and the rest are
    // small, so a queue would spend its time on the handful that finish instantly; chunking by
    // count keeps the common shape - a few big fragments - spread across the threads.
    // Split into owned groups before spawning. A `chunks()` slice would hand each thread a
    // borrow and force a `clone` of the buffers - which are the millions of entries this whole
    // function exists to process, so cloning them would cost more than the threads save.
    let chunk = work.len().div_ceil(threads);
    let mut groups: Vec<Vec<(FragmentKey, Pending)>> = Vec::with_capacity(threads);
    let mut work = work;
    while !work.is_empty() {
        let take = core::cmp::min(chunk, work.len());
        groups.push(work.drain(..take).collect());
    }
    let count: usize = groups.iter().map(Vec::len).sum();

    let mut out: Vec<Result<Vec<Prepared>>> = Vec::with_capacity(groups.len());
    std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(groups.len());
        for group in groups {
            handles.push(scope.spawn(move || -> Result<Vec<Prepared>> {
                // One core each: the fan-out has already been spent, out here.
                group
                    .into_iter()
                    .map(|(key, p)| prepare_fragment(catalog, key, p, held.get(&key), 1))
                    .collect()
            }));
        }
        for h in handles {
            // A panic in a prepare is a bug in this file, not a condition to report: there is no
            // I/O in there to fail. Propagating it keeps the transaction from committing half a
            // batch, which is what swallowing it would do.
            out.push(h.join().expect("preparing a fragment must not panic"));
        }
    });

    let mut prepared = Vec::with_capacity(count);
    for group in out {
        prepared.extend(group?);
    }
    Ok(prepared)
}

/// Buffered entries below which a commit prepares on one thread. Measured on the import path,
/// where a chunk carries millions and an interactive write carries one.
const PARALLEL_FLUSH_MIN: usize = 50_000;

/// One fragment's flush, computed but not yet written.
///
/// The split exists so the expensive half can run on another core. Everything in here is
/// arithmetic over the buffer this fragment collected - collapsing duplicates, expanding values
/// into bit planes - and touches neither the pager nor the transaction. What remains for the
/// serial half is allocating pages and walking a tree, which one writer has to do alone.
struct Prepared {
    key: FragmentKey,
    /// Already grouped by container, because grouping is the expensive half and it is pure.
    /// See [`big_engine::bitmap::write::Grouped`].
    set: Grouped,
    clear: Grouped,
    cells: Vec<(RecordId, ColEdit)>,
    /// Whether the bitmap half has nothing in it. Not `set.is_empty() && clear.is_empty()`,
    /// which would also be true of a fragment whose values collapsed to nothing - and that
    /// distinction is what keeps an empty tree out of the catalog.
    nothing_to_write: bool,
}

/// The pure half of a flush. See [`Prepared`].
fn prepare_fragment(
    catalog: &Catalog,
    key: FragmentKey,
    p: Pending,
    held: Option<&RowSet>,
    threads: usize,
) -> Result<Prepared> {
    let values = last_per_key(p.values);
    let bits = last_per_bit(p.bits);

    // Reserved rather than grown from empty, because the size is known here and the batch is
    // large: a twenty-bit field over a million records pushes twenty-one million pairs, and
    // a `Vec` doubling its way there memmoves roughly its own final length in the process.
    //
    // Half the total in each, not the whole of it in both. `bits_for` sends every plane to
    // exactly one of the two, so together they receive `depth + 1` per record and neither
    // alone can be predicted - but a value sets about half its bits, so half is the estimate
    // that costs one doubling in the worst case instead of twenty-five, without reserving
    // twice the memory the pair will ever hold.
    let depth = catalog.fragment(&key).map_or(1, |m| m.bit_depth.max(1)) as usize;

    let (mut set, mut clear) = if values.is_empty() {
        (Grouped::new(), Grouped::new())
    } else {
        // The depth the catalog ended the transaction on, not the depth each individual write
        // saw. A record written before the depth grew still gets every plane accounted for, so
        // overwriting a large value with a small one cannot leave a stale high bit behind.
        //
        // One membership test per record, not one per plane: the answer is the same for every
        // plane of a record, so asking twenty times would be asking the same question twenty
        // times.
        let already: Option<Vec<bool>> = held.map(|rows| {
            values.iter().map(|(record, _)| rows.contains(key.shard, *record)).collect()
        });
        Bsi::new(depth as u32).group_all(&values, already.as_deref(), threads)?
    };

    // The loose bits keep the general grouping. A keyed field's rows are drawn from an alphabet
    // as wide as the shard has distinct values, which is the case a table indexed by row cannot
    // serve - and there is one of them per record rather than twenty-one, so it is not the case
    // worth serving.
    if !bits.is_empty() {
        merge_grouped(&mut set, group_offsets(bits.iter().filter(|(_, on)| *on).map(|(b, _)| *b)));
        merge_grouped(
            &mut clear,
            group_offsets(bits.iter().filter(|(_, on)| !*on).map(|(b, _)| *b)),
        );
    }

    let nothing_to_write = set.is_empty() && clear.is_empty();
    Ok(Prepared { key, set, clear, cells: fold_edits(p.cells), nothing_to_write })
}

/// [`last_per_key`] for bits, which are keyed by `(row, record)` and are the one buffer whose
/// fast path never fires.
///
/// **Why this needed its own function.** `last_per_key` skips its sort when the buffer already
/// ascends, and for values it usually does: records arrive in order. Bits do not. A keyed field
/// writes `(row, record)` where the row is whichever key that record happened to name, so the
/// row column jumps about and the buffer is unsorted by construction - the sort was 5.6% of an
/// import, every time, with no case in which it was skipped.
///
/// The rows come from a small alphabet, though: a keyed field's rows are dense from zero, a
/// bool has two, the exists row is one. So this buckets by row - counting sort, one pass to
/// count and one to scatter - and sorts nothing at all in the ordinary case. Within a bucket
/// the records arrive ascending already, which is checked rather than assumed; a bucket that
/// is out of order is sorted on its own, and it is small.
fn last_per_bit(buf: Vec<((RowId, RecordId), bool)>) -> Vec<((RowId, RecordId), bool)> {
    let (sorted, repeats) = shape_of(&buf);
    if sorted && !repeats {
        return buf;
    }
    if !sorted {
        // The bucket table is indexed by row, so a sparse or enormous row space would allocate
        // more than the buffer itself. Comparison sort is the honest fallback there.
        //
        // Compared as a `u64` before any conversion. `max as usize + 1` wraps to zero for a row
        // id near `u64::MAX`, and a wrapped zero passes both bounds below and then indexes off
        // the end of a one-element table - which is a memory bug reachable from a row id, not a
        // theoretical one. `a_row_space_too_wide_to_bucket_falls_back` is that case.
        let max_row = buf.iter().map(|e| e.0 .0).max().unwrap_or(0);
        let bucketable = max_row < MAX_BUCKETED_ROWS as u64 && (max_row as usize) < buf.len();
        if bucketable {
            return dedup_last(bucket_by_row(buf, max_row as usize + 1));
        }
        let mut buf = buf;
        buf.sort_by_key(|entry| entry.0);
        return dedup_last(buf);
    }
    dedup_last(buf)
}

/// Above this many distinct rows, the counting sort's table costs more than the sort it saves.
const MAX_BUCKETED_ROWS: usize = 1 << 16;

/// Stable counting sort by row, then by record within each row.
fn bucket_by_row(
    buf: Vec<((RowId, RecordId), bool)>,
    rows: usize,
) -> Vec<((RowId, RecordId), bool)> {
    debug_assert!(!buf.is_empty(), "an empty buffer is sorted and never reaches here");
    let mut starts = vec![0usize; rows + 1];
    for entry in &buf {
        starts[entry.0 .0 as usize + 1] += 1;
    }
    for i in 0..rows {
        starts[i + 1] += starts[i];
    }

    let mut out = vec![buf[0]; buf.len()];
    let mut cursor = starts.clone();
    for entry in buf {
        let row = entry.0 .0 as usize;
        out[cursor[row]] = entry;
        cursor[row] += 1;
    }

    // Scattering in arrival order leaves each bucket in arrival order, which for a load is
    // already ascending by record. Checked per bucket rather than assumed, because "already
    // ascending" is a property of the caller and this function's answer has to be right for
    // every caller.
    for row in 0..rows {
        let bucket = &mut out[starts[row]..starts[row + 1]];
        if bucket.windows(2).any(|w| w[0].0 .1 > w[1].0 .1) {
            bucket.sort_by_key(|entry| entry.0 .1);
        }
    }
    out
}

/// Keeps the last arrival of each key in a buffer already ordered by key.
///
/// **Reverse, dedup, reverse - and the obvious improvement measured slower.** `dedup_by` keeps
/// the *first* of each run, so this reverses to bring the last arrival to the front and reverses
/// back. That moves every element twice, which looks like exactly the thing to remove: the same
/// answer comes out of a single forward pass if the predicate swaps, so that the later entry
/// lands in the earlier slot and the later slot is the one dropped.
///
/// Interleaved on one machine, 5,000,000 records, two pairs:
///
/// | | pair 1 | pair 2 |
/// |---|---|---|
/// | one pass, swapping in the predicate | 37.6s | 27.2s |
/// | reverse, dedup, reverse | **36.0s** | **25.9s** |
///
/// `reverse` is a tight loop over a contiguous buffer and the compiler vectorises it. A
/// `dedup_by` whose predicate branches and calls `swap` is a comparison and a branch per
/// element, and it does not. Two vectorised passes beat one scalar pass.
///
/// The box drifted 38% between the pairs, which is why they are *pairs* run minutes apart
/// rather than two timings taken an hour apart, and why the direction is what they are offered
/// for rather than the margin.
fn dedup_last<K: PartialEq, V>(mut buf: Vec<(K, V)>) -> Vec<(K, V)> {
    buf.reverse();
    buf.dedup_by(|a, b| a.0 == b.0);
    buf.reverse();
    buf
}

/// Collapses a buffer of column edits to one per record, ascending.
///
/// Not [`last_per_key`], because the last edit is not always the whole answer: a set field adds,
/// so two `Add`s for one record are both part of what it holds. Everything else is the caller's
/// last word and replaces — including an `Add` landing on a `Replace`, which the map-keyed
/// version this replaced also treated as a fresh list rather than a merge into a scalar.
fn fold_edits(mut buf: Vec<(RecordId, ColEdit)>) -> Vec<(RecordId, ColEdit)> {
    let (sorted, repeats) = shape_of(&buf);
    if sorted && !repeats {
        return buf;
    }
    if !sorted {
        buf.sort_by_key(|entry| entry.0);
    }
    let mut out: Vec<(RecordId, ColEdit)> = Vec::with_capacity(buf.len());
    for (record, edit) in buf {
        match out.last_mut() {
            Some((prev, held)) if *prev == record => held.absorb(edit),
            _ => out.push((record, edit)),
        }
    }
    out
}

/// A field resolved to what the setters need, so a caller writing many facts at one field pays
/// for the catalog walk once instead of once per fact.
///
/// **The saving is the clone, more than the lookup.** [`resolve`] hands back an owned
/// [`FieldDef`], which owns a name and a granularity list — so every setter call allocated twice
/// to learn something that cannot change inside one transaction. A field's id, kind and declared
/// width are fixed once it exists, and a replay does no DDL; the zone map and the bit depth that
/// *do* move during a transaction live in the catalog's fragment entry, which is read at the
/// flush rather than from here.
#[derive(Clone, Debug)]
pub struct At {
    table: TableId,
    def: FieldDef,
}

pub struct DbWrite<'db, P: PagerMut> {
    txn: WriteTxn<'db, P>,
    pub(super) catalog: Catalog,
    frags: BTreeMap<FragmentKey, FragmentWrite>,
    /// Buffered writes, applied once per fragment at commit.
    ///
    /// Without this, a thousand integers written to one fragment walk its tree a thousand
    /// times over the same handful of containers. The containers are shared, so the work is
    /// shared - but only if the writes are held until the shape of the batch is known.
    pending: BTreeMap<FragmentKey, Pending>,
    /// Column segments this transaction has touched, keyed exactly as fragments are and
    /// differing only in the view. See [`COLUMN_VIEW`].
    cols: BTreeMap<FragmentKey, ColumnWrite>,
    /// The scratch an engine records one fact into, reused across facts. See [`Placed::reset`].
    placed: Placed,
    /// The last record marked as existing, so a record with four fields buffers the bit once.
    ///
    /// **Invalidated wherever `pending` shrinks**, which is `flush_fragment`, `flush_all` and
    /// `discard`. A memo that outlived the buffer it describes would skip a bit that is no
    /// longer there, and the record would read back as never written.
    exists_memo: Option<(TableId, RecordId)>,
    db: &'db Db<P>,
}

impl<P: PagerMut> Db<P> {
    /// Opens a write transaction. Nothing it writes is visible until [`DbWrite::commit`].
    pub fn write(&self) -> DbWrite<'_, P> {
        DbWrite {
            txn: self.store.begin_write(),
            catalog: self.catalog.read().unwrap().clone(),
            frags: BTreeMap::new(),
            pending: BTreeMap::new(),
            cols: BTreeMap::new(),
            placed: Placed::default(),
            exists_memo: None,
            db: self,
        }
    }
}

impl<'db, P: PagerMut> DbWrite<'db, P> {
    pub fn txn(&mut self) -> &mut WriteTxn<'db, P> {
        &mut self.txn
    }

    pub fn catalog_mut(&mut self) -> &mut Catalog {
        &mut self.catalog
    }

    /// `FragmentWrite` is plain data, so handing out a copy lets several fragments of the same
    /// shard be edited inside one transaction.
    ///
    /// Anything buffered for this fragment is applied first: a caller reaching for the handle
    /// wants to see the fragment as the transaction has left it, not as it was before the
    /// buffered writes.
    pub fn fragment(&mut self, key: FragmentKey) -> FragmentWrite {
        if self.pending.contains_key(&key) {
            self.flush_fragment(key).expect("buffered writes must apply");
        }
        self.fragment_raw(key)
    }

    fn fragment_raw(&mut self, key: FragmentKey) -> FragmentWrite {
        // Register it in the catalog too: that list is how a reader discovers which shards a
        // field actually has data in, and a fragment missing from it is invisible to queries.
        self.catalog.fragment_mut(key);
        *self.frags.entry(key).or_insert_with(|| FragmentWrite::new(self.txn.root(&key), key.shard))
    }

    /// Applies everything buffered for one fragment in a single pass over its tree.
    /// Applies everything buffered for one fragment in a single pass over its tree.
    fn flush_fragment(&mut self, key: FragmentKey) -> Result<()> {
        let Some(p) = self.pending.remove(&key) else { return Ok(()) };
        // Before preparing: the depth `prepare_fragment` expands at is the one this leaves.
        self.apply_zone(key, &p);
        self.exists_memo = None;
        // The single-fragment path, taken by `fragment()` when a caller wants a handle that
        // reflects what is buffered. It keeps every clear: this is one fragment being settled
        // mid-transaction rather than a load, so the read that would let it drop them is not
        // worth the walk.
        let prepared = prepare_fragment(&self.catalog, key, p, None, 1)?;
        self.apply_prepared(prepared)
    }

    /// The half of a flush that has to be serial: it allocates pages and writes trees.
    fn apply_prepared(&mut self, pr: Prepared) -> Result<()> {
        if !pr.cells.is_empty() {
            self.flush_cells(pr.key, pr.cells)?;
        }
        // A segment-only key has no bitmap half to write. Returning before `fragment_raw`
        // matters: registering it would put a standard-view fragment in the catalog that holds
        // nothing, and every scan would then visit a tree that does not exist.
        if pr.nothing_to_write {
            return Ok(());
        }
        let mut f = self.fragment_raw(pr.key);
        f.write_grouped(&mut self.txn, pr.set, pr.clear)?;
        self.save_fragment(pr.key, f);
        Ok(())
    }

    /// Applies one fragment's buffered column edits, a block at a time.
    ///
    /// Grouped by block first, so a thousand records landing in one block cost one decode and
    /// one encode rather than a thousand of each. That grouping is the entire reason the edits
    /// were buffered instead of written where they were made.
    fn flush_cells(&mut self, key: FragmentKey, cells: Vec<(RecordId, ColEdit)>) -> Result<()> {
        let mut by_block: BTreeMap<u64, Vec<(usize, ColEdit)>> = BTreeMap::new();
        for (record, edit) in cells {
            let (block, slot) = column_site(record);
            by_block.entry(block).or_default().push((slot, edit));
        }

        // Register it, exactly as `fragment_raw` registers a fragment. That list is how a
        // reader discovers which shards hold data, how `delete` finds what to erase, and how
        // `drop_table` finds what to free - so a segment missing from it is one that answers
        // nothing, keeps deleted records, and leaks its pages on a drop.
        self.catalog.fragment_mut(key);

        let mut w = *self.cols.entry(key).or_insert_with(|| ColumnWrite::new(self.txn.root(&key)));
        for (block, edits) in by_block {
            w.edit_block(&mut self.txn, block, |b| {
                for (slot, edit) in edits {
                    let next = match edit {
                        ColEdit::Replace(cell) => cell,
                        ColEdit::AddOne(one) => {
                            let mut all = b.get(slot).list().to_vec();
                            all.push(one);
                            all.sort_unstable();
                            all.dedup();
                            Cell::List(all)
                        }
                        // The one edit that reads what is already there. A set field adds, so
                        // the stored list is part of the answer rather than something the
                        // write is replacing.
                        ColEdit::Add(more) => {
                            let mut all = b.get(slot).list().to_vec();
                            all.extend(more);
                            all.sort_unstable();
                            all.dedup();
                            Cell::List(all)
                        }
                    };
                    b.set(slot, next);
                }
            })?;
        }
        self.cols.insert(key, w);
        Ok(())
    }

    /// Applies every buffered fragment, preparing them on as many cores as are worth using.
    ///
    /// **The split is what makes this safe.** [`prepare_fragment`] reads the catalog and its own
    /// fragment's buffer and touches nothing else - no pager, no transaction, no shared mutable
    /// state - so fragments can be prepared in any order and on any thread. Applying them is
    /// serial and stays serial: one writer allocates the pages, and that is the engine's design
    /// rather than a lock this could remove.
    ///
    /// Order is preserved across the parallel section. Fragments are independent, so the answer
    /// does not depend on it - but the *file* does, through the order pages are allocated in,
    /// and a benchmark that produces a different layout on every run is one nobody can compare.
    fn flush_all(&mut self) -> Result<()> {
        let pending = core::mem::take(&mut self.pending);
        self.exists_memo = None;
        if pending.is_empty() {
            return Ok(());
        }

        // **The zone maps first, and all of them.** `prepare_fragment` reads the bit depth a
        // fragment ended the transaction on, so a value observed after the fragment beside it
        // was prepared would be expanded at yesterday's depth. Settling every one of them here
        // is one pass over a map of a few hundred entries.
        for (key, p) in &pending {
            self.apply_zone(*key, p);
        }

        // **Read before preparing, because preparing cannot read.**
        //
        // A bit-sliced value writes a *clear* for every zero plane, and a clear is only needed
        // where something might already be there. This asks each fragment which records it
        // already holds - one row read, `EXISTS_ROW`, walked once - so the preparation can drop
        // the clears for records that are new. On an append-only load that is every clear.
        //
        // Serial and here rather than inside `prepare_fragment`, which runs on other threads and
        // is pure by design: reading the row needs the transaction, and handing a transaction to
        // several threads is the thing the single writer exists to prevent.
        let mut held: BTreeMap<FragmentKey, RowSet> = BTreeMap::new();
        for (key, p) in &pending {
            // Only a bit-sliced fragment produces clears, and only one with a root has anything
            // to clear. Calling `fragment_raw` on the others would register empty fragments in
            // the catalog, which is the mistake `apply_prepared` returns early to avoid.
            if p.values.is_empty() {
                continue;
            }
            let f = self.fragment_raw(*key);
            if let Some(r) = f.reader(&self.txn) {
                held.insert(*key, r.row(EXISTS_ROW)?);
            }
        }

        for prepared in prepare_all(&self.catalog, pending, &held)? {
            self.apply_prepared(prepared)?;
        }
        Ok(())
    }

    pub fn save_fragment(&mut self, key: FragmentKey, f: FragmentWrite) {
        self.frags.insert(key, f);
    }

    pub fn with_fragment<R>(
        &mut self,
        key: FragmentKey,
        body: impl FnOnce(&mut WriteTxn<'db, P>, &mut FragmentWrite) -> Result<R>,
    ) -> Result<R> {
        let mut f = self.fragment(key);
        let out = body(&mut self.txn, &mut f)?;
        self.save_fragment(key, f);
        Ok(out)
    }

    fn key(&self, table: TableId, field: FieldId, shard: ShardId) -> FragmentKey {
        FragmentKey { table, field, view: STANDARD_VIEW, shard }
    }

    /// The segment key for the same field and shard: the same address, one view over.
    fn column_key(&self, table: TableId, field: FieldId, shard: ShardId) -> FragmentKey {
        FragmentKey { table, field, view: COLUMN_VIEW, shard }
    }

    /// What this table stores. A table that has been dropped mid-transaction answers with the
    /// narrow engine, which writes nothing extra - the write is about to fail on the name
    /// anyway, and inventing a segment for a table that is going is worse than not.
    fn engine(&self, table: TableId) -> TableEngine {
        self.catalog.table_by_id(table).map_or(TableEngine::Bitmap, |t| t.engine)
    }

    /// Buffers a column edit, merging it with anything this transaction already decided.
    ///
    /// Merging matters for exactly one case and it is the case a set field is: two `set_key`
    /// calls for one record are two values it now holds, not the second replacing the first.
    fn buffer_cell(&mut self, key: FragmentKey, record: RecordId, edit: ColEdit) {
        self.pending.entry(key).or_default().cells.push((record, edit));
    }

    /// Resolves a field once, for a caller about to write many facts at it.
    pub fn at<'a>(&self, table: impl Into<TableRef<'a>>, field: &str) -> Result<At> {
        let table = table.into();
        let (t, def) = resolve(&self.catalog, table, field)?;
        Ok(At { table: t, def })
    }

    fn buffer_bit(&mut self, key: FragmentKey, row: RowId, record: RecordId, on: bool) {
        self.pending.entry(key).or_default().bits.push(((row, record), on));
    }

    /// Marks a record as existing. Without this, `NOT` would match every id never written.
    ///
    /// Every setter calls this, so a four-field record buffers the same bit four times. Skipping
    /// the repeats was tried, with a memo of the last bit buffered: it measured *slower* once
    /// `bits` became a `Vec`, because comparing a `FragmentKey` costs more than the push it
    /// avoids — and the memo had to be invalidated everywhere `pending` shrinks, which is not
    /// only `flush_fragment` but `discard` too. Paying for three pushes is the cheaper and the
    /// safer of the two.
    pub fn mark_exists<'a>(
        &mut self,
        table: impl Into<TableRef<'a>>,
        record: RecordId,
    ) -> Result<()> {
        let table = table.into();
        let t = self.catalog.require(table)?.id;
        self.mark_exists_at(t, record);
        Ok(())
    }

    /// The same, for a caller that has already resolved the table.
    ///
    /// Every setter marks existence, and every setter had already resolved the table to reach
    /// its field — so taking the name again meant a second walk of the catalog per column per
    /// record for an id sitting in the caller's hand. It cannot fail here, which is the other
    /// half of the point: the name was checked when it was resolved.
    /// Hands one fact to the table's engine and applies whatever it decided.
    ///
    /// **The only place the write path asks an engine anything.** It used to ask
    /// `has_bitmap()`/`has_columns()` at nine separate setters and buffer the halves itself,
    /// which meant a fourth engine was nine edits in this file. Now the engine is handed a
    /// [`Placed`] and says what to put in it; this applies the answer.
    fn route(
        &mut self,
        t: TableId,
        def: &FieldDef,
        record: RecordId,
        fact: big_engine::base::engine::Fact<'_>,
    ) -> Result<()> {
        // **Written into where it lives, not moved out and back.** The first version of this
        // took the scratch with `mem::take` and put it back at the end, which is two copies of
        // the whole struct per fact - two `Vec` headers, four `Option<u64>` and an
        // `Option<ColEdit>` that owns a `Vec` of its own. A call graph put those copies at 6% of
        // the daemon, more than the three allocations the scratch exists to avoid. The engine
        // descriptor is `&'static`, so resolving it first ends the borrow of `self` before the
        // scratch is borrowed, and the two field borrows below are disjoint.
        let spec = self.engine(t).spec();
        self.placed.reset();
        spec.place(&mut self.placed, fact);

        let shard = shard_of(record);
        let key = self.key(t, def.id, shard);
        // **One descent, for everything this fact leaves at its fragment.** The zone map, the
        // value and the bits all address the same fragment, and each used to find it on its own
        // - two walks of `pending` and one of the catalog, per fact, for a key already in hand.
        let placed = &self.placed;
        if placed.observe_bitmap.is_some()
            || placed.observe_columns.is_some()
            || placed.planes.is_some()
            || !placed.bits.is_empty()
        {
            // Two fields of `self`, borrowed at once. Disjoint field borrows are what make this
            // spelling possible at all, and they are why the scratch has to stay a field rather
            // than be handed to a method.
            let p = self.pending.entry(key).or_default();
            if let Some(v) = placed.observe_bitmap {
                note_zone(&mut p.observe, v);
            }
            if let Some(v) = placed.observe_columns {
                note_zone(&mut p.observe_cols, v);
            }
            if let Some(v) = placed.planes {
                p.values.push((record, v));
            }
            for (row, on) in &placed.bits {
                p.bits.push(((*row, record), *on));
            }
        }
        let mutex = placed.mutex;
        // Moved out only when there is something to move. A time quantum field is the only one
        // that fills `bits_in`, and `buffer_bit` needs `&mut self`, so the empty case - which is
        // every other field kind - must not pay for the `take` that case would need.
        if !self.placed.bits_in.is_empty() {
            for (view, row, on) in core::mem::take(&mut self.placed.bits_in) {
                self.buffer_bit(FragmentKey { view, ..key }, row, record, on);
            }
        }
        let cell = self.placed.cell.take();
        if let Some(row) = mutex {
            self.apply_mutex(key, record, row)?;
        }
        if let Some(edit) = cell {
            self.buffer_cell(self.column_key(t, def.id, shard), record, edit);
        }
        Ok(())
    }

    /// The mutex protocol, which is the one write that has to read before it writes.
    ///
    /// Performed here rather than inside the engine because it needs the transaction: finding
    /// the value a record is leaving means reading the shadow view, and `fragment` flushes
    /// anything already buffered for these fragments first so it sees the transaction as it
    /// stands.
    fn apply_mutex(&mut self, key: FragmentKey, record: RecordId, row: RowId) -> Result<()> {
        let shadow_key = FragmentKey { view: MUTEX_SHADOW_VIEW, ..key };
        let m = MutexField::new(SHADOW_DEPTH);
        let mut values = self.fragment(key);
        let mut shadow = self.fragment(shadow_key);
        m.put(&mut self.txn, &mut values, &mut shadow, record, row)?;
        self.save_fragment(key, values);
        self.save_fragment(shadow_key, shadow);
        Ok(())
    }

    fn mark_exists_at(&mut self, table: TableId, record: RecordId) {
        // **The repeat is skipped on the record, not on the fragment key.** The memo this
        // replaced compared a `FragmentKey` - four fields, and it lost to the push it was
        // avoiding. A table id and a record id are two integers, and they answer the same
        // question: every setter of one record marks the same existence bit, so only the first
        // has anything to say.
        //
        // A miss is harmless rather than wrong. Records arriving interleaved defeat the memo and
        // buffer the bit again, which is what `last_per_bit` collapses; the saving is a property
        // of the ordinary case, the correctness is not.
        if self.exists_memo == Some((table, record)) {
            return;
        }
        self.exists_memo = Some((table, record));
        let key = self.key(table, EXISTS_FIELD, shard_of(record));
        self.buffer_bit(key, EXISTS_ROW, record, true);
    }

    /// Applies one fragment's folded zone map to the catalog. See [`Pending::observe`].
    ///
    /// Both ends, because `FragmentMeta::observe` folds a single value: the pair is the whole
    /// of what the batch saw, and the bit depth follows from the larger of them.
    fn apply_zone(&mut self, key: FragmentKey, p: &Pending) {
        if let Some((lo, hi)) = p.observe {
            let m = self.catalog.fragment_mut(key);
            m.observe(lo);
            m.observe(hi);
        }
        if let Some((lo, hi)) = p.observe_cols {
            let m = self.catalog.fragment_mut(FragmentKey { view: COLUMN_VIEW, ..key });
            m.observe(lo);
            m.observe(hi);
        }
    }

    pub fn set_int<'a>(
        &mut self,
        table: impl Into<TableRef<'a>>,
        field: &str,
        record: RecordId,
        value: u64,
    ) -> Result<()> {
        let table = table.into();
        let at = self.at(table, field)?;
        self.set_int_at(&at, record, value)
    }

    /// The same, at a field resolved once. See [`At`].
    pub fn set_int_at(&mut self, at: &At, record: RecordId, value: u64) -> Result<()> {
        let (t, def) = (at.table, &at.def);
        let field = def.name.as_str();
        // `is_bsi` now covers the signed and float kinds too, and this setter must not: the
        // value it takes is already the stored value, so writing one to a field with an
        // encoding would store a number that reads back as something else entirely.
        // `set_signed` and `set_float` are the ways in.
        expect_kind(def, field, |k| k.is_bsi() && !k.is_signed() && !k.is_float(), "int")?;

        // A value wider than the field was declared for is refused, not truncated.
        let declared = if def.bit_depth == 0 { 64 } else { def.bit_depth };
        if 64 - value.leading_zeros() > declared {
            return Err(big_engine::bitmap::field::FieldError::ValueTooWide {
                value,
                bit_depth: declared,
            }
            .into());
        }

        // Widen the fragment and record the zone map in the same breath. Depth only ever grows,
        // and the buffered value is expanded at whatever depth the transaction ends on, so a
        // record buffered now is not stranded at today's narrower depth.
        self.route(t, def, record, Fact::Value { value, kind: def.kind })?;
        self.mark_exists_at(t, record);
        Ok(())
    }

    /// Writes a signed value, biased on the way in.
    ///
    /// Everything below this line sees an ordinary unsigned bit-sliced index; the sign lives in
    /// [`crate::signed`] and nowhere else. Refused rather than wrapped when the value does not
    /// fit the declared range, for the same reason `set_int` refuses a value too wide: a number
    /// that comes back as a different number is worse than a write that failed.
    pub fn set_signed<'a>(
        &mut self,
        table: impl Into<TableRef<'a>>,
        field: &str,
        record: RecordId,
        value: i64,
    ) -> Result<()> {
        let table = table.into();
        let at = self.at(table, field)?;
        self.set_signed_at(&at, record, value)
    }

    /// The same, at a field resolved once. See [`At`].
    pub fn set_signed_at(&mut self, at: &At, record: RecordId, value: i64) -> Result<()> {
        let (t, def) = (at.table, &at.def);
        expect_kind(def, &def.name, FieldKind::is_signed, "signed int")?;

        let declared = if def.bit_depth == 0 { 64 } else { def.bit_depth };
        let stored =
            crate::signed::encode(value, declared).ok_or(DbError::SignedValueOutOfRange {
                value,
                min: crate::signed::min_value(declared),
                max: crate::signed::max_value(declared),
            })?;

        // Everything below sees the *stored* value, which is what makes the zone map work
        // unchanged: the encoding is monotonic, so a window in stored space is the same window in
        // value space.
        self.route(t, def, record, Fact::Value { value: stored, kind: def.kind })?;
        self.mark_exists_at(t, record);
        Ok(())
    }

    /// Writes a float value, encoded on the way in.
    ///
    /// Everything below this line sees an ordinary unsigned bit-sliced index, exactly as it does
    /// for a signed field; the transform lives in [`crate::float`] and nowhere else. That is
    /// what keeps the zone map, the range scan and `Bsi::extreme` working unchanged - the
    /// encoding is monotonic, so a window in stored space is the same window in value space.
    pub fn set_float<'a>(
        &mut self,
        table: impl Into<TableRef<'a>>,
        field: &str,
        record: RecordId,
        value: f64,
    ) -> Result<()> {
        let table = table.into();
        let at = self.at(table, field)?;
        self.set_float_at(&at, record, value)
    }

    /// The same, at a field resolved once. See [`At`].
    pub fn set_float_at(&mut self, at: &At, record: RecordId, value: f64) -> Result<()> {
        let (t, def) = (at.table, &at.def);
        expect_kind(def, &def.name, FieldKind::is_float, "float")?;

        let declared = if def.bit_depth == 0 { 64 } else { def.bit_depth };
        let stored = crate::float::encode(value, declared)
            .ok_or(DbError::FloatValueOutOfRange { value, bit_depth: declared })?;

        self.route(t, def, record, Fact::Value { value: stored, kind: def.kind })?;
        self.mark_exists_at(t, record);
        Ok(())
    }

    pub fn set_bool<'a>(
        &mut self,
        table: impl Into<TableRef<'a>>,
        field: &str,
        record: RecordId,
        value: bool,
    ) -> Result<()> {
        let table = table.into();
        let at = self.at(table, field)?;
        self.set_bool_at(&at, record, value)
    }

    /// The same, at a field resolved once. See [`At`].
    pub fn set_bool_at(&mut self, at: &At, record: RecordId, value: bool) -> Result<()> {
        let (t, def) = (at.table, &at.def);
        expect_kind(def, &def.name, |k| k == FieldKind::Bool, "bool")?;

        self.route(t, def, record, Fact::Bool(value))?;
        self.mark_exists_at(t, record);
        Ok(())
    }

    /// Interns the row key and sets the bit. Row keys are the one thing that must mean the same
    /// in every shard, which is why they go through the catalog rather than being derived.
    pub fn set_key<'a>(
        &mut self,
        table: impl Into<TableRef<'a>>,
        field: &str,
        record: RecordId,
        value: &str,
    ) -> Result<RowId> {
        let table = table.into();
        let at = self.at(table, field)?;
        self.set_key_at(&at, record, value)
    }

    /// Interns a key at a field resolved once, and writes nothing.
    ///
    /// For a caller replaying many facts that name the same handful of keys: interning is
    /// idempotent, so `set_key_at` per fact asks the catalog to look up a string it has already
    /// been given - two map lookups and a string hash - to be told the row it was told last
    /// time. Resolving each distinct key once and writing through [`set_row_at`] moves that off
    /// the per-fact path entirely.
    ///
    /// [`set_row_at`]: DbWrite::set_row_at
    pub fn intern_key_at(&mut self, at: &At, value: &str) -> Result<RowId> {
        expect_kind(&at.def, &at.def.name, FieldKind::is_keyed, "set, mutex or time quantum")?;
        Ok(self.catalog.keys.intern(at.table, at.def.id, value)?)
    }

    /// Sets the bit for a row this transaction has already interned. See [`intern_key_at`].
    ///
    /// Identical to [`set_key_at`] in every respect but the lookup - same kind check, same
    /// routing, same exists bit - so a mutex still reads its shadow and a caller cannot use this
    /// to bypass a rule the keyed path enforces.
    ///
    /// [`intern_key_at`]: DbWrite::intern_key_at
    /// [`set_key_at`]: DbWrite::set_key_at
    pub fn set_row_at(&mut self, at: &At, record: RecordId, row: RowId) -> Result<()> {
        let (t, def) = (at.table, &at.def);
        expect_kind(def, &def.name, FieldKind::is_keyed, "set, mutex or time quantum")?;
        self.route(t, def, record, Fact::Row { row, kind: def.kind, views: &[] })?;
        self.mark_exists_at(t, record);
        Ok(())
    }

    /// The same, at a field resolved once. See [`At`].
    pub fn set_key_at(&mut self, at: &At, record: RecordId, value: &str) -> Result<RowId> {
        let (t, def) = (at.table, &at.def);
        expect_kind(def, &def.name, FieldKind::is_keyed, "set, mutex or time quantum")?;
        let row = self.catalog.keys.intern(t, def.id, value)?;

        self.route(t, def, record, Fact::Row { row, kind: def.kind, views: &[] })?;
        self.mark_exists_at(t, record);
        Ok(row)
    }

    /// Assigns a row id to a key without writing any fact about it.
    ///
    /// The schema leader's half of the row-key agreement. A coordinator resolves every key in
    /// a batch here first, so that the facts it then sends to the shard owners carry ids that
    /// already mean the same thing everywhere. Interning is a catalog write, so this takes a
    /// transaction like any other - and it is a *separate* transaction from the import,
    /// because the import happens on other machines.
    pub fn intern_key<'a>(
        &mut self,
        table: impl Into<TableRef<'a>>,
        field: &str,
        value: &str,
    ) -> Result<RowId> {
        let table = table.into();
        let (t, def) = resolve(&self.catalog, table, field)?;
        expect_kind(&def, field, FieldKind::is_keyed, "set, mutex or time quantum")?;
        Ok(self.catalog.keys.intern(t, def.id, value)?)
    }

    /// Records what a key already means, as decided somewhere else.
    ///
    /// The other half: a shard owner is *told* the mapping rather than choosing one, which is
    /// what keeps a row id the same on every node. Refuses a mapping that contradicts one this
    /// node already holds instead of overwriting it - see [`big_keys::KeyError::Conflict`].
    ///
    /// In the same transaction as the facts that use it, so a batch that is refused leaves
    /// neither the fact nor the mapping behind.
    pub fn assign_key<'a>(
        &mut self,
        table: impl Into<TableRef<'a>>,
        field: &str,
        value: &str,
        row: RowId,
    ) -> Result<()> {
        let table = table.into();
        let (t, def) = resolve(&self.catalog, table, field)?;
        expect_kind(&def, field, FieldKind::is_keyed, "set, mutex or time quantum")?;
        Ok(self.catalog.keys.assign(t, def.id, value, row)?)
    }

    /// Writes a keyed fact that also happened at a moment in time.
    ///
    /// The fact goes into the standard view exactly as `set_key` would, and additionally into
    /// one view per granularity the field declared. Those extra views are what make a range
    /// query read only the days it asks about instead of every record ever written.
    pub fn set_time<'a>(
        &mut self,
        table: impl Into<TableRef<'a>>,
        field: &str,
        record: RecordId,
        value: &str,
        unix_seconds: i64,
    ) -> Result<RowId> {
        let table = table.into();
        let at = self.at(table, field)?;
        self.set_time_at(&at, record, value, unix_seconds)
    }

    /// The same, at a field resolved once. See [`At`].
    pub fn set_time_at(
        &mut self,
        at: &At,
        record: RecordId,
        value: &str,
        unix_seconds: i64,
    ) -> Result<RowId> {
        let (t, def) = (at.table, &at.def);
        expect_kind(def, &def.name, |k| k == FieldKind::TimeQuantum, "time quantum")?;

        let row = self.catalog.keys.intern(t, def.id, value)?;

        // Interned before the fact is routed, because naming a view mutates the catalog and an
        // engine is handed ids rather than strings.
        let granularity = if def.granularity.is_empty() {
            big_engine::bitmap::field::DEFAULT_GRANULARITY.to_vec()
        } else {
            def.granularity.clone()
        };
        let mut views = Vec::with_capacity(granularity.len());
        for name in big_engine::bitmap::field::views(unix_seconds, &granularity) {
            views.push(self.catalog.intern_view(&name)?);
        }

        // **One route, views included, rather than a key write followed by loose bits.** The
        // views are a bitmap construct, so an engine with no bitmaps drops them - which the old
        // shape could not do, and a columnar table grew a fragment per day that no read of it
        // could ever reach.
        self.route(t, def, record, Fact::Row { row, kind: def.kind, views: &views })?;
        self.mark_exists_at(t, record);
        Ok(row)
    }

    /// Replaces a fragment's contents outright, and its zone map with them.
    ///
    /// **A copy, not a merge.** Choosing consistency is what makes that the right shape: every
    /// write reaches the copy serving the range first, so that copy is the truth and a repair
    /// is not reconciling two opinions - it is replacing one. A union would be wrong in the one
    /// direction that matters, because a copy that missed a *deletion* holds bits the truth
    /// does not, and merging would put them back.
    pub fn replace_fragment(
        &mut self,
        addr: &FragmentAddr,
        meta: crate::catalog::FragmentMeta,
        containers: &[(ContainerKey, Container)],
    ) -> Result<()> {
        let key = self.locate_for_write(addr)?;
        // The old tree goes first, pages and all. Writing over it container by container would
        // leave whatever the source no longer holds.
        self.discard(&[key])?;
        let mut f = self.fragment(key);
        for (ckey, c) in containers {
            f.write_container(&mut self.txn, *ckey, c.as_ref())?;
        }
        self.save_fragment(key, f);
        *self.catalog.fragment_mut(key) = meta;
        Ok(())
    }

    /// Replaces a column segment's contents outright.
    ///
    /// A copy, not a merge, for exactly the reason [`DbWrite::replace_fragment`] is one: the
    /// copy serving the range is the truth, and a copy that missed a *deletion* holds cells the
    /// truth does not.
    pub fn replace_segment(
        &mut self,
        addr: &FragmentAddr,
        meta: crate::catalog::FragmentMeta,
        cells: &[(u64, Cell)],
    ) -> Result<()> {
        let key = self.locate_for_write(addr)?;
        self.discard(&[key])?;

        let mut by_block: BTreeMap<u64, Vec<(usize, Cell)>> = BTreeMap::new();
        for (local, cell) in cells {
            by_block
                .entry(big_engine::columnar::block_of(*local))
                .or_default()
                .push((big_engine::columnar::slot_of(*local), cell.clone()));
        }

        let mut w = ColumnWrite::new(None);
        for (block, slots) in by_block {
            let mut value = big_engine::columnar::Block::new();
            for (slot, cell) in slots {
                value.set(slot, cell);
            }
            w.write_block(&mut self.txn, block, &value)?;
        }
        self.cols.insert(key, w);
        *self.catalog.fragment_mut(key) = meta;
        Ok(())
    }

    /// The same lookup the reader does, except that a view this node has never seen is
    /// interned rather than refused: a copy that was away when a time quantum first wrote a
    /// day's view has to be able to take it now.
    fn locate_for_write(&mut self, addr: &FragmentAddr) -> Result<FragmentKey> {
        let t = self.catalog.require(addr.table_ref())?.id;
        let field = match &addr.field {
            Some(name) => {
                self.catalog
                    .field(t, name)
                    .ok_or_else(|| DbError::UnknownField {
                        table: addr.table.clone(),
                        field: name.clone(),
                    })?
                    .id
            }
            None => addr.field_id,
        };
        let view = match &addr.view {
            Some(name) => self.catalog.intern_view(name)?,
            None => addr.view_id,
        };
        Ok(FragmentKey { table: t, field, view, shard: addr.shard })
    }

    /// Frees the trees behind a set of fragments and forgets their root records.
    ///
    /// The pages are stamped with this transaction rather than released outright, so a reader
    /// still on the old meta page keeps seeing them until it is gone. That is the same rule
    /// every copy-on-write rewrite follows; dropping a table is not special.
    pub(super) fn discard(&mut self, keys: &[FragmentKey]) -> Result<()> {
        for key in keys {
            // Anything buffered or cached for this fragment is about to become a root record
            // pointing at freed pages, so it goes first.
            self.pending.remove(key);
            self.exists_memo = None;
            self.frags.remove(key);
            self.cols.remove(key);
            if let Some(root) = self.txn.root(key) {
                big_btree::free_tree(&mut self.txn, root)?;
                self.txn.remove_root(key);
            }
        }
        Ok(())
    }

    /// Removes records from every field of a table, and returns how many of them existed.
    ///
    /// Deliberately blind to field kinds. A record is erased by clearing its bit from every
    /// row of every fragment the table owns in its shard - which covers a bit-sliced index's
    /// planes, a set field's rows, a mutex's shadow and a time quantum field's per-day views
    /// without a single test on what kind of field it is. A kind this does not know about yet
    /// is therefore already handled.
    ///
    /// The count is of records that existed, so deleting the same record twice, or one that
    /// was never written, is not an error and does not inflate the answer.
    pub fn delete<'a>(
        &mut self,
        table: impl Into<TableRef<'a>>,
        records: &[RecordId],
    ) -> Result<u64> {
        let table = table.into();
        let t = self.catalog.require(table)?.id;

        // Everything buffered lands first. A fragment that has only been written to in this
        // transaction is not in the catalog yet - `buffer_bit` defers registering it until the
        // flush - and the loop below finds fragments through the catalog. Without this, a
        // record written and then deleted in one transaction would have its buffered bits
        // applied *after* the delete had already looked for them.
        self.flush_all()?;

        let mut by_shard: BTreeMap<ShardId, Vec<RecordId>> = BTreeMap::new();
        for record in records {
            by_shard.entry(shard_of(*record)).or_default().push(*record);
        }

        let mut removed = 0;
        for (shard, mut recs) in by_shard {
            // Sorted and deduplicated so a caller repeating an id neither costs extra work nor
            // gets counted twice.
            recs.sort_unstable();
            recs.dedup();
            removed += self.delete_in_shard(t, shard, &recs)?;
        }
        Ok(removed)
    }

    /// The same, over the answer to a query. This is the shape of undoing a wrong import:
    /// name the records with a predicate, then remove exactly those.
    pub fn delete_where<'a>(
        &mut self,
        table: impl Into<TableRef<'a>>,
        rows: &Matches,
    ) -> Result<u64> {
        let table = table.into();
        let records: Vec<RecordId> = rows.records().collect();
        self.delete(table, &records)
    }

    fn delete_in_shard(
        &mut self,
        table: TableId,
        shard: ShardId,
        records: &[RecordId],
    ) -> Result<u64> {
        // Counted before anything is cleared, and from the reserved existence field, which is
        // the one place that knows whether a record was ever written at all.
        let existed = self.count_existing(table, shard, records)?;

        // Every fragment this table owns in this shard, whatever view it belongs to. Collected
        // up front because the loop below needs the catalog mutably.
        let keys: Vec<FragmentKey> = self
            .catalog
            .fragments_of_table(table)
            .filter(|(k, _)| k.shard == shard)
            .map(|(k, _)| *k)
            .collect();

        for key in keys {
            // A segment is not a set of bits and `clear_records` would read its cells as
            // containers. Erasing a record there means nulling its slot, which is the same
            // erasure spelled in the units the segment stores.
            if key.view == COLUMN_VIEW {
                self.clear_column_records(key, records)?;
                continue;
            }
            // `fragment` applies anything buffered for this fragment first, so a value written
            // earlier in this same transaction is cleared rather than left to land afterwards.
            let mut f = self.fragment(key);
            f.clear_records(&mut self.txn, records)?;
            self.save_fragment(key, f);
        }
        Ok(existed)
    }

    /// Nulls a set of records' slots in one segment, a block at a time.
    ///
    /// Grouped by block for the reason every other column write is: the records of one delete
    /// usually fall in a handful of blocks, and a decode-encode per record would pay for a
    /// thousand slots to clear one.
    fn clear_column_records(&mut self, key: FragmentKey, records: &[RecordId]) -> Result<()> {
        let mut by_block: BTreeMap<u64, Vec<usize>> = BTreeMap::new();
        for record in records {
            let (block, slot) = column_site(*record);
            by_block.entry(block).or_default().push(slot);
        }

        let mut w = *self.cols.entry(key).or_insert_with(|| ColumnWrite::new(self.txn.root(&key)));
        for (block, slots) in by_block {
            w.edit_block(&mut self.txn, block, |b| {
                for slot in slots {
                    b.set(slot, Cell::Null);
                }
            })?;
        }
        self.cols.insert(key, w);
        Ok(())
    }

    fn count_existing(
        &mut self,
        table: TableId,
        shard: ShardId,
        records: &[RecordId],
    ) -> Result<u64> {
        let key = self.key(table, EXISTS_FIELD, shard);
        let f = self.fragment(key);
        self.save_fragment(key, f);
        let Some(r) = f.reader(&self.txn) else { return Ok(0) };
        let mut n = 0;
        for record in records {
            if r.get(EXISTS_ROW, *record)? {
                n += 1;
            }
        }
        Ok(n)
    }

    pub fn commit(mut self) -> Result<TxnId> {
        self.flush_all()?;
        for (key, f) in &self.frags {
            match f.root() {
                Some(root) => self.txn.set_root(*key, root),
                None => {
                    self.txn.remove_root(key);
                }
            }
        }
        // Segments publish exactly as fragments do. They share the root-record namespace, which
        // is what makes the backup walk, the freelist and the reclaim horizon cover them without
        // knowing they exist.
        for (key, c) in &self.cols {
            match c.root() {
                Some(root) => self.txn.set_root(*key, root),
                None => {
                    self.txn.remove_root(key);
                }
            }
        }
        self.txn.set_catalog(self.catalog.encode());
        let id = self.txn.commit()?;
        *self.db.catalog.write().unwrap() = self.catalog;
        Ok(id)
    }
}

#[cfg(test)]
mod bit_order_tests {
    use super::*;

    /// What `last_per_bit` has to agree with, written out rather than borrowed.
    ///
    /// Deliberately the slow, obvious implementation: sort, reverse, keep the first of each run,
    /// reverse back. Calling `last_per_key` instead would share `dedup_last` with the code under
    /// test, and a reference that shares the suspect's machinery proves nothing about it.
    fn reference(mut buf: Vec<((RowId, RecordId), bool)>) -> Vec<((RowId, RecordId), bool)> {
        buf.sort_by_key(|entry| entry.0);
        buf.reverse();
        buf.dedup_by(|a, b| a.0 == b.0);
        buf.reverse();
        buf
    }

    fn mixer(state: &mut u64) -> u64 {
        *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = *state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    #[test]
    fn rows_that_jump_about_are_still_ordered() {
        // The shape a keyed field actually produces: records ascending, rows arbitrary.
        let buf: Vec<((RowId, RecordId), bool)> =
            (0..1000u64).map(|i| ((i % 7, i), true)).collect();
        assert_eq!(last_per_bit(buf.clone()), reference(buf));
    }

    #[test]
    fn the_last_arrival_wins() {
        // Three writes to one bit, the last of them clearing it. Getting this backwards would
        // set a bit the caller asked to clear, and nothing downstream would notice.
        let buf = vec![((3u64, 9u64), true), ((3, 9), false), ((3, 9), true), ((3, 9), false)];
        assert_eq!(last_per_bit(buf), vec![((3, 9), false)]);
    }

    #[test]
    fn a_bucket_out_of_order_is_sorted_rather_than_trusted() {
        // Records descending inside one row - which a bulk caller can produce even though a
        // load does not. Scattering alone would leave this bucket unsorted.
        let buf: Vec<((RowId, RecordId), bool)> =
            (0..64u64).rev().map(|i| ((1u64, i), true)).collect();
        assert_eq!(last_per_bit(buf.clone()), reference(buf));
    }

    #[test]
    fn it_agrees_with_the_reference_on_random_input() {
        let mut state = 0x1234_5678_9ABC_DEF0u64;
        for case in 0..200 {
            let n = (mixer(&mut state) % 400) as usize;
            // Rows deliberately narrow so keys repeat and the dedup is exercised.
            let buf: Vec<((RowId, RecordId), bool)> = (0..n)
                .map(|_| {
                    let row = mixer(&mut state) % 9;
                    let rec = mixer(&mut state) % 40;
                    ((row, rec), mixer(&mut state) & 1 == 0)
                })
                .collect();
            assert_eq!(last_per_bit(buf.clone()), reference(buf), "case {case}");
        }
    }

    #[test]
    fn a_row_space_too_wide_to_bucket_falls_back() {
        // One entry, one enormous row id: the bucket table would be gigabytes, so the
        // comparison sort has to take over. A panic or an allocation failure here is the bug.
        let buf = vec![((u64::MAX, 1u64), true), ((0, 2), true)];
        assert_eq!(last_per_bit(buf.clone()), reference(buf));
    }
}
