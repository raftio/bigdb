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

//! Bit-sliced index: one row per bit plane, plus a row marking which records have a value.
//!
//! This is bit-packing transposed. The same information, the same order of magnitude of space,
//! but a range query reads the high planes first and stops as soon as the answer is settled
//! instead of reading every bit of every record.

use crate::base::coords::{
    ckey_of_slot, local_of, CONTAINERS_PER_ROW, CONTAINER_EXPONENT, CONTAINER_WIDTH,
};
use crate::bitmap::field::error::{FieldError, Result};
use crate::bitmap::write::{fan_out, Grouped};
use crate::bitmap::{FragmentRead, FragmentWrite, RecordId, RowId, RowSet};
use big_pager::{Pager, PagerMut, WriteTxn};

/// Marks records that have a value at all, which is what makes NULL distinguishable from zero.
pub const EXISTS_ROW: RowId = 0;

/// One chunk's set and clear halves, before they are keyed by container.
///
/// Each is indexed `slot * rows + row` - slot-major, because one record touches every row of a
/// single slot and nothing else. See [`Bsi::group_all`].
type Tables = (Vec<Vec<u16>>, Vec<Vec<u16>>);

/// Unsigned only. A signed field needs a sign convention, and that decision is deliberately
/// not baked in here.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Bsi {
    /// Per fragment and monotonically increasing. Kept in the catalog: a fragment cannot
    /// describe its own depth, because an all-zero high plane is indistinguishable from absent.
    pub bit_depth: u32,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RangeOp {
    Gt,
    Ge,
    Lt,
    Le,
    Eq,
    Ne,
}

impl Bsi {
    pub fn new(bit_depth: u32) -> Self {
        Self { bit_depth }
    }

    /// Row holding bit `i` of the value, least significant first.
    pub fn plane_row(i: u32) -> RowId {
        1 + i as u64
    }

    /// Rows a fragment of this depth uses, exists row included.
    pub fn rows(&self) -> u64 {
        1 + self.bit_depth as u64
    }

    pub fn fits(&self, value: u64) -> bool {
        self.bit_depth >= 64 || value < (1u64 << self.bit_depth)
    }

    pub fn set<P: PagerMut>(
        &self,
        txn: &mut WriteTxn<'_, P>,
        f: &mut FragmentWrite,
        record: RecordId,
        value: u64,
    ) -> Result<()> {
        let mut set = Vec::new();
        let mut clear = Vec::new();
        self.bits_for(record, value, &mut set, &mut clear)?;
        // One pass, not a clear pass followed by a set pass: every plane of this value lives
        // in its own container, and touching each of them twice doubles the cost of the write
        // for no gain. Both halves commit together regardless, so no intermediate state where
        // the value reads back larger is ever visible.
        f.write_bits(txn, set, clear)?;
        Ok(())
    }

    /// The bits one value implies, appended to `set` and `clear` without touching the tree.
    ///
    /// Split out so a caller can accumulate many records and apply them in a single pass. A
    /// record written on its own costs one pass over every plane it occupies; a thousand
    /// records accumulated first cost one pass in total, because the planes are shared.
    ///
    /// Every plane below `bit_depth` appears in exactly one of the two lists, so replacing a
    /// value can never leave a stale high bit behind.
    pub fn bits_for(
        &self,
        record: RecordId,
        value: u64,
        set: &mut Vec<(RowId, RecordId)>,
        clear: &mut Vec<(RowId, RecordId)>,
    ) -> Result<()> {
        if !self.fits(value) {
            return Err(FieldError::ValueTooWide { value, bit_depth: self.bit_depth });
        }
        set.push((EXISTS_ROW, record));
        for i in 0..self.bit_depth {
            let row = Self::plane_row(i);
            if value >> i & 1 == 1 {
                set.push((row, record));
            } else {
                clear.push((row, record));
            }
        }
        Ok(())
    }

    /// Every bit a batch of values implies, grouped by the container it lands in.
    ///
    /// **What this does not build is the pair buffer, which used to be most of the write.** The
    /// expansion this replaced emitted a `(RowId, RecordId)` per bit - sixteen bytes, twenty-one
    /// of them per record on a twenty-bit field - and the grouping downstream read every one
    /// back to recompute a position, a container key and the two-byte offset that is all it
    /// keeps. On a commit of a million records that is twenty megabytes written and read again
    /// to produce two.
    ///
    /// It is avoidable because **a record sits at the same offset in every plane.** `pos_of`
    /// adds `row * SHARD_WIDTH` and the shard width is a whole number of containers, so a
    /// record's slot and its offset inside that slot are fixed by the record alone; only the
    /// container key moves, by one row's worth, and the row is the loop variable. So the
    /// arithmetic happens once per record instead of once per bit, and the only thing written
    /// per bit is the offset that survives.
    ///
    /// The table is indexed slot-major - every row of one slot adjacent - because one record
    /// touches every row of a single slot and nothing else. Row-major would stride a whole
    /// row's worth of container keys between a record's own bits.
    ///
    /// `held` says, per entry of `values`, whether this fragment already holds that record.
    /// `None` answers "assume every one of them does", which is the safe reading.
    ///
    /// **Why it is worth asking.** A zero bit has to be written as a *clear* only if something
    /// might already be there. A record the fragment has never held has every plane clear
    /// already, so its zeros are no-ops - and there are `bit_depth` of them. On a twenty-bit
    /// field carrying small values that is nineteen wasted entries out of twenty-one, and they
    /// are not free downstream: the write unions the clears' container keys with the set's, then
    /// reads and rewrites every container that only a clear named.
    ///
    /// Every value is checked before any bit is emitted, so a batch carrying one value too wide
    /// for the field is refused whole rather than half-grouped.
    /// `threads` caps the fan-out. **A caller that is already running on every core has to be
    /// able to say so**: this is called from the parallel half of a commit, and a batch that
    /// spawned on its own account there would put four threads on two cores and make each of
    /// them slower. One means "do it here".
    pub fn group_all(
        &self,
        values: &[(RecordId, u64)],
        held: Option<&[bool]>,
        threads: usize,
    ) -> Result<(Grouped, Grouped)> {
        for (_, value) in values {
            if !self.fits(*value) {
                return Err(FieldError::ValueTooWide { value: *value, bit_depth: self.bit_depth });
            }
        }
        debug_assert!(held.is_none_or(|h| h.len() == values.len()));

        let rows = self.bit_depth as usize + 1;
        // A batch big enough to be worth splitting is split the same way the loose-bit grouping
        // splits one, and for the same reason: this is arithmetic over a slice with nothing
        // shared, which is the only half of a commit that can run on more than one core.
        let (set, clear) = match fan_out(values.len() * rows).min(threads.max(1)) {
            1 => self.tables_of(values, held, rows),
            n => self.tables_of_parallel(values, held, rows, n),
        };
        Ok((fold(set, rows), fold(clear, rows)))
    }

    /// One chunk into the slot-major tables `group_all` describes.
    fn tables_of(&self, values: &[(RecordId, u64)], held: Option<&[bool]>, rows: usize) -> Tables {
        const SLOTS: usize = CONTAINERS_PER_ROW as usize;
        let mut set: Vec<Vec<u16>> = vec![Vec::new(); rows * SLOTS];
        let mut clear: Vec<Vec<u16>> = vec![Vec::new(); rows * SLOTS];

        // Reserved from a counting pass, because the buffers are the size of the batch and
        // doubling into them memmoves roughly their own final length. Half of a value's bits
        // are set on average, so half is what each plane is given; the exists row takes every
        // record and is given all of them.
        let mut per_slot = [0usize; SLOTS];
        for (record, _) in values {
            per_slot[(local_of(*record) >> CONTAINER_EXPONENT) as usize] += 1;
        }
        let any_clears = held.is_none_or(|h| h.iter().any(|held| *held));
        for (slot, n) in per_slot.iter().enumerate() {
            if *n == 0 {
                continue;
            }
            let base = slot * rows;
            set[base].reserve(*n);
            for row in 1..rows {
                set[base + row].reserve(n / 2 + 8);
                if any_clears {
                    clear[base + row].reserve(n / 2 + 8);
                }
            }
        }

        for (j, (record, value)) in values.iter().enumerate() {
            let local = local_of(*record);
            let base = (local >> CONTAINER_EXPONENT) as usize * rows;
            let offset = (local & (CONTAINER_WIDTH - 1)) as u16;
            // Row zero of the slot: `EXISTS_ROW` is row zero, so its container key *is* the slot.
            set[base].push(offset);
            let held_here = held.is_none_or(|h| h[j]);
            for i in 0..self.bit_depth {
                let at = base + 1 + i as usize;
                if value >> i & 1 == 1 {
                    set[at].push(offset);
                } else if held_here {
                    clear[at].push(offset);
                }
            }
        }
        (set, clear)
    }

    /// The same tables, built by several threads and concatenated in chunk order.
    ///
    /// Order is preserved exactly: the chunks stay in order and each keeps its own offsets in
    /// the order they arrived, so the lists this produces are the ones the serial loop would
    /// have produced rather than merely equivalent ones.
    fn tables_of_parallel(
        &self,
        values: &[(RecordId, u64)],
        held: Option<&[bool]>,
        rows: usize,
        threads: usize,
    ) -> Tables {
        let chunk = values.len().div_ceil(threads);
        let parts: Vec<Tables> = std::thread::scope(|scope| {
            let mut handles = Vec::new();
            for (n, part) in values.chunks(chunk).enumerate() {
                let mine = held.map(|h| &h[n * chunk..n * chunk + part.len()]);
                handles.push(scope.spawn(move || self.tables_of(part, mine, rows)));
            }
            handles
                .into_iter()
                .map(|h| h.join().expect("grouping a bit-sliced batch panicked"))
                .collect()
        });

        let mut parts = parts.into_iter();
        let (mut set, mut clear) = parts.next().expect("at least one chunk");
        for (s, c) in parts {
            for (into, from) in set.iter_mut().zip(s) {
                into.extend_from_slice(&from);
            }
            for (into, from) in clear.iter_mut().zip(c) {
                into.extend_from_slice(&from);
            }
        }
        (set, clear)
    }

    pub fn clear<P: PagerMut>(
        &self,
        txn: &mut WriteTxn<'_, P>,
        f: &mut FragmentWrite,
        record: RecordId,
    ) -> Result<()> {
        let mut bits = vec![(EXISTS_ROW, record)];
        for i in 0..self.bit_depth {
            bits.push((Self::plane_row(i), record));
        }
        f.clear_bits(txn, bits)?;
        Ok(())
    }

    pub fn get<P: Pager>(&self, f: &FragmentRead<'_, P>, record: RecordId) -> Result<Option<u64>> {
        // One pass for every plane at once. Probing them one at a time descends the tree
        // afresh per plane, which for a twenty-bit value is twenty-one descents to read one
        // integer.
        let rows = core::iter::once(EXISTS_ROW).chain((0..self.bit_depth).map(Self::plane_row));
        let bits = f.get_many(rows, record)?;

        if !bits.get(&EXISTS_ROW).copied().unwrap_or(false) {
            return Ok(None);
        }
        let mut v = 0u64;
        for i in 0..self.bit_depth {
            if bits.get(&Self::plane_row(i)).copied().unwrap_or(false) {
                v |= 1u64 << i;
            }
        }
        Ok(Some(v))
    }

    pub fn exists<P: Pager>(&self, f: &FragmentRead<'_, P>) -> Result<RowSet> {
        Ok(f.row(EXISTS_ROW)?)
    }

    /// One pass from the most significant plane down, carrying "definitely greater" and
    /// "equal so far". Stops being able to change once `eq` empties out.
    pub fn range<P: Pager>(&self, f: &FragmentRead<'_, P>, op: RangeOp, k: u64) -> Result<RowSet> {
        let exists = self.exists(f)?;

        // A threshold wider than the fragment's depth would have its high bits silently
        // dropped by the loop below, so answer it directly: every stored value is smaller.
        if !self.fits(k) {
            return Ok(match op {
                RangeOp::Gt | RangeOp::Ge | RangeOp::Eq => RowSet::new(),
                RangeOp::Lt | RangeOp::Le | RangeOp::Ne => exists,
            });
        }

        let mut gt = RowSet::new();
        let mut eq = exists.clone();

        for i in (0..self.bit_depth).rev() {
            if eq.is_empty() && matches!(op, RangeOp::Eq) {
                break;
            }
            let plane = f.row(Self::plane_row(i))?;
            if k >> i & 1 == 1 {
                eq = eq.and(&plane);
            } else {
                gt = gt.or(&eq.and(&plane));
                eq = eq.andnot(&plane);
            }
        }

        Ok(match op {
            RangeOp::Gt => gt,
            RangeOp::Ge => gt.or(&eq),
            RangeOp::Le => exists.andnot(&gt),
            RangeOp::Lt => exists.andnot(&gt.or(&eq)),
            RangeOp::Eq => eq,
            RangeOp::Ne => exists.andnot(&eq),
        })
    }

    /// Records whose value lies in `[lo, hi]`, both ends inclusive.
    pub fn between<P: Pager>(&self, f: &FragmentRead<'_, P>, lo: u64, hi: u64) -> Result<RowSet> {
        if lo > hi {
            return Ok(RowSet::new());
        }
        Ok(self.range(f, RangeOp::Ge, lo)?.and(&self.range(f, RangeOp::Le, hi)?))
    }

    /// Sum over an optional filter. Each plane contributes its population times its weight, so
    /// this costs `bit_depth` intersections rather than one pass per record.
    pub fn sum<P: Pager>(&self, f: &FragmentRead<'_, P>, filter: Option<&RowSet>) -> Result<u128> {
        let mut total = 0u128;
        for i in 0..self.bit_depth {
            let plane = f.row(Self::plane_row(i))?;
            let n = match filter {
                Some(m) => plane.and(m).cardinality(),
                None => plane.cardinality(),
            };
            total += (n as u128) << i;
        }
        Ok(total)
    }

    pub fn count<P: Pager>(&self, f: &FragmentRead<'_, P>, filter: Option<&RowSet>) -> Result<u64> {
        let exists = self.exists(f)?;
        Ok(match filter {
            Some(m) => exists.and(m).cardinality(),
            None => exists.cardinality(),
        })
    }

    pub fn max<P: Pager>(
        &self,
        f: &FragmentRead<'_, P>,
        filter: Option<&RowSet>,
    ) -> Result<Option<u64>> {
        self.extreme(f, filter, true)
    }

    pub fn min<P: Pager>(
        &self,
        f: &FragmentRead<'_, P>,
        filter: Option<&RowSet>,
    ) -> Result<Option<u64>> {
        self.extreme(f, filter, false)
    }

    /// Narrows the candidate set one plane at a time from the top. Never materialises a value.
    fn extreme<P: Pager>(
        &self,
        f: &FragmentRead<'_, P>,
        filter: Option<&RowSet>,
        want_max: bool,
    ) -> Result<Option<u64>> {
        let mut cand = match filter {
            Some(m) => self.exists(f)?.and(m),
            None => self.exists(f)?,
        };
        if cand.is_empty() {
            return Ok(None);
        }
        let mut value = 0u64;
        for i in (0..self.bit_depth).rev() {
            let plane = f.row(Self::plane_row(i))?;
            let with = if want_max { cand.and(&plane) } else { cand.andnot(&plane) };
            if !with.is_empty() {
                cand = with;
                if want_max {
                    value |= 1u64 << i;
                }
            } else if !want_max {
                value |= 1u64 << i;
            }
        }
        Ok(Some(value))
    }
}

/// Turns a slot-major table into the map the write path takes, dropping the empties.
///
/// The index is `slot * rows + row` and the container key is `row * CONTAINERS_PER_ROW + slot`,
/// which is the whole of the translation: the table is laid out for the loop that fills it and
/// the map is keyed for the tree that consumes it.
fn fold(table: Vec<Vec<u16>>, rows: usize) -> Grouped {
    let mut out = Grouped::new();
    for (i, offsets) in table.into_iter().enumerate() {
        if offsets.is_empty() {
            continue;
        }
        let (slot, row) = (i / rows, i % rows);
        out.insert(ckey_of_slot(row as u64, slot as u64), offsets);
    }
    out
}

#[cfg(test)]
mod group_tests {
    use super::*;
    use crate::bitmap::write::group_offsets;

    /// What `group_all` claims to be, written out the slow way.
    ///
    /// Plane by plane into loose `(row, record)` pairs and then through the general grouping -
    /// which is the expansion `group_all` replaced, kept here rather than in the crate because a
    /// reference implementation with no caller but a test is a test fixture. Deliberately shares
    /// none of the slot arithmetic under test: if `tables_of` is wrong in any way that matters,
    /// the two disagree.
    fn reference(
        bsi: &Bsi,
        values: &[(RecordId, u64)],
        held: Option<&[bool]>,
    ) -> (Grouped, Grouped) {
        let mut set = Vec::new();
        let mut clear = Vec::new();
        for (record, _) in values {
            set.push((EXISTS_ROW, *record));
        }
        for i in 0..bsi.bit_depth {
            let row = Bsi::plane_row(i);
            for (j, (record, value)) in values.iter().enumerate() {
                if value >> i & 1 == 1 {
                    set.push((row, *record));
                } else if held.is_none_or(|h| h[j]) {
                    clear.push((row, *record));
                }
            }
        }
        (group_offsets(set), group_offsets(clear))
    }

    fn check(depth: u32, values: &[(RecordId, u64)], held: Option<&[bool]>) {
        let bsi = Bsi::new(depth);
        let (want_set, want_clear) = reference(&bsi, values, held);
        // Both fan-outs, because the parallel one has to reproduce the serial answer exactly.
        for threads in [1, 4] {
            let (got_set, got_clear) = bsi.group_all(values, held, threads).expect("values fit");
            assert_eq!(got_set, want_set, "set differs at depth {depth}, {threads} threads");
            assert_eq!(got_clear, want_clear, "clear differs at depth {depth}, {threads} threads");
        }
        let (got_set, got_clear) = bsi.group_all(values, held, usize::MAX).expect("values fit");
        assert_eq!(got_set, want_set, "set differs at depth {depth}");
        assert_eq!(got_clear, want_clear, "clear differs at depth {depth}");
    }

    /// Values chosen to exercise every plane: all-zero, all-one, and a spread between.
    fn corpus(n: u64, stride: u64, depth: u32) -> Vec<(RecordId, u64)> {
        let mask = if depth >= 64 { u64::MAX } else { (1u64 << depth) - 1 };
        (0..n).map(|i| (i * stride, (i * 2_654_435_761) & mask)).collect()
    }

    #[test]
    fn grouping_matches_the_expansion_it_replaces() {
        for depth in [1u32, 4, 20, 33] {
            check(depth, &corpus(500, 1, depth), None);
        }
    }

    /// A record's offset is its position inside a 65,536-wide container, so a batch that stays
    /// inside one container never exercises the slot arithmetic at all.
    #[test]
    fn records_spanning_several_containers_land_in_the_right_ones() {
        let values = corpus(3_000, 331, 20);
        assert!(
            values.last().unwrap().0 > CONTAINER_WIDTH * 4,
            "the corpus has to cross a container boundary or this proves nothing"
        );
        check(20, &values, None);
    }

    /// The whole shard, so every one of the sixteen slots is used.
    #[test]
    fn a_batch_covering_every_slot_of_a_shard() {
        let values = corpus(4_000, crate::base::coords::SHARD_WIDTH / 4_000, 12);
        check(12, &values, None);
    }

    /// `held` decides which zero planes become clears rather than no-ops, per record.
    #[test]
    fn held_records_keep_their_clears_and_new_ones_do_not() {
        let values = corpus(600, 7, 16);
        let mixed: Vec<bool> = (0..values.len()).map(|i| i % 3 == 0).collect();
        check(16, &values, Some(&mixed));
        check(16, &values, Some(&vec![false; values.len()]));
        check(16, &values, Some(&vec![true; values.len()]));
    }

    /// Above the fan-out threshold, where the tables are built on several threads and stitched
    /// back together. The concatenation has to reproduce the serial order exactly.
    #[test]
    fn the_parallel_path_produces_the_serial_answer() {
        let values = corpus(60_000, 3, 20);
        assert!(fan_out(values.len() * 21) > 1, "this test has to reach the parallel path");
        assert!(
            std::thread::available_parallelism().map_or(1, |n| n.get()) > 1,
            "and a machine that can take it"
        );
        check(20, &values, None);
        let mixed: Vec<bool> = (0..values.len()).map(|i| i % 2 == 0).collect();
        check(20, &values, Some(&mixed));
    }

    /// The check is made before any bit is emitted, so a batch with one bad value is refused
    /// whole rather than half-grouped.
    #[test]
    fn a_value_too_wide_is_refused() {
        let bsi = Bsi::new(4);
        let err = bsi.group_all(&[(0, 3), (1, 99), (2, 1)], None, 1);
        assert!(matches!(err, Err(FieldError::ValueTooWide { value: 99, bit_depth: 4 })));
    }

    #[test]
    fn an_empty_batch_groups_to_nothing() {
        let (set, clear) = Bsi::new(20).group_all(&[], None, 4).expect("empty fits");
        assert!(set.is_empty() && clear.is_empty());
    }
}
