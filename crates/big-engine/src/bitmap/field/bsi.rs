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

use crate::bitmap::field::error::{FieldError, Result};
use crate::bitmap::{FragmentRead, FragmentWrite, RecordId, RowId, RowSet};
use big_pager::{Pager, PagerMut, WriteTxn};

/// Marks records that have a value at all, which is what makes NULL distinguishable from zero.
pub const EXISTS_ROW: RowId = 0;

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

    /// The same bits for a whole batch, plane by plane rather than record by record.
    ///
    /// **The contents are identical; the order is the point.** `pos_of` puts a plane a whole
    /// shard's width away from its neighbour, so walking one record's planes lands every one of
    /// its bits in a different container from the bit before it — and the grouping downstream
    /// then looks a container up per bit. Walking a plane's records instead keeps a long run of
    /// ascending records inside one container, so that grouping can stay on the container it is
    /// already holding. Nothing after this cares about the order: containers are built from
    /// offsets that get sorted anyway.
    ///
    /// Every value is checked before any bit is emitted, so a batch carrying one value too wide
    /// for the field is refused whole rather than half-written.
    pub fn bits_for_all(
        &self,
        values: &[(RecordId, u64)],
        set: &mut Vec<(RowId, RecordId)>,
        clear: &mut Vec<(RowId, RecordId)>,
    ) -> Result<()> {
        for (_, value) in values {
            if !self.fits(*value) {
                return Err(FieldError::ValueTooWide { value: *value, bit_depth: self.bit_depth });
            }
        }
        for (record, _) in values {
            set.push((EXISTS_ROW, *record));
        }
        for i in 0..self.bit_depth {
            let row = Self::plane_row(i);
            for (record, value) in values {
                if value >> i & 1 == 1 {
                    set.push((row, *record));
                } else {
                    clear.push((row, *record));
                }
            }
        }
        Ok(())
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
