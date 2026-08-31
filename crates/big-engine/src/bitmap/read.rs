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

//! Reading a fragment. One fragment is one b-tree, and a row is a contiguous span of its keys.

use crate::bitmap::rowset::RowSet;
use crate::coords::*;
use big_btree::{find_many, scan, Result};
use big_container::{and_cardinality, Container, ContainerRef};
use big_page::{ContainerKey, ContainerType, LeafCell, Pgno};
use big_pager::Pager;
use core::ops::ControlFlow;
use std::collections::BTreeMap;

#[derive(Clone, Copy)]
pub struct FragmentRead<'t, P: Pager> {
    pager: &'t P,
    root: Pgno,
    shard: ShardId,
}

impl<'t, P: Pager> FragmentRead<'t, P> {
    pub fn new(pager: &'t P, root: Pgno, shard: ShardId) -> Self {
        Self { pager, root, shard }
    }

    pub fn shard(&self) -> ShardId {
        self.shard
    }

    pub fn root(&self) -> Pgno {
        self.root
    }

    /// Visits every container in `[lo, hi]`, resolving dense ones through their own page.
    ///
    /// The borrow lives only for the callback, which is what lets a bounded pager release the
    /// page afterwards. Anything that must outlive the call has to be materialised.
    pub fn for_each<F>(&self, lo: ContainerKey, hi: ContainerKey, mut f: F) -> Result<()>
    where
        F: FnMut(ContainerKey, ContainerRef<'_>) -> ControlFlow<()>,
    {
        let mut err = None;
        scan(self.pager, self.root, lo, hi, |cell: LeafCell<'_>| {
            match self.visit(&cell, &mut f) {
                Ok(flow) => flow,
                Err(e) => {
                    err = Some(e);
                    ControlFlow::Break(())
                }
            }
        })?;
        match err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    fn visit<F>(&self, cell: &LeafCell<'_>, f: &mut F) -> Result<ControlFlow<()>>
    where
        F: FnMut(ContainerKey, ContainerRef<'_>) -> ControlFlow<()>,
    {
        match cell.ty {
            // A plain dense cell is still borrowed straight off its page: nothing has to be
            // applied, so nothing has to be copied.
            ContainerType::BitmapPtr => {
                let page = self.verified_base(cell)?;
                let c = cell.bitmap_checked(&page)?;
                Ok(f(cell.key, c))
            }
            // A delta cell has to be materialised, because the container it stands for is not
            // the bytes of any page. Owned here and borrowed to `f`, so the copy lives exactly
            // as long as the visit.
            ContainerType::BitmapDelta => {
                let page = self.verified_base(cell)?;
                let owned = cell.resolve_checked(Some(&page))?;
                Ok(f(cell.key, owned.as_ref()))
            }
            _ => match cell.container()? {
                Some(c) => Ok(f(cell.key, c)),
                None => Ok(ControlFlow::Continue(())),
            },
        }
    }

    /// Reads a cell's dense page and checks the parent-to-child link before handing it back.
    ///
    /// The check goes through the pager rather than through the cell so that a pager able to
    /// remember it does not repeat a CRC over 8 KiB for every bit a bit-sliced read touches.
    /// See [`big_pager::Pager::verify_bitmap`]; the guarantee is unchanged, only its cost.
    fn verified_base(&self, cell: &LeafCell<'_>) -> Result<P::Ref<'_>> {
        let page = self.pager.read(cell.bitmap_pgno)?;
        if !self.pager.verify_bitmap(cell.bitmap_pgno, &page, cell.bitmap_checksum) {
            return Err(cell.checksum_error(&page).into());
        }
        Ok(page)
    }

    pub fn container(&self, ckey: ContainerKey) -> Result<Option<Container>> {
        let mut out = None;
        self.for_each(ckey, ckey, |_, c| {
            out = Some(c.to_owned());
            ControlFlow::Break(())
        })?;
        Ok(out)
    }

    /// One row is `CONTAINERS_PER_ROW` consecutive keys, so this is a single contiguous walk.
    pub fn row(&self, row: RowId) -> Result<RowSet> {
        let span = row_ckeys(row);
        let mut set = RowSet::new();
        self.for_each(*span.start(), *span.end(), |k, c| {
            set.insert(slot_of_ckey(k), c.to_owned());
            ControlFlow::Continue(())
        })?;
        Ok(set)
    }

    /// Every row's count of the records `filter` keeps, in one pass.
    ///
    /// **The shape of the loop, not a micro-optimisation.** The obvious way to write this is
    /// `for row in rows() { row(row).and(filter).cardinality() }`, which is one b-tree range
    /// scan per row, a copy of every container of that row, and a whole intersection allocated
    /// and dropped for a number. A field with a thousand distinct values pays that a thousand
    /// times per fragment - and grouping is what `Distinct`, `TopN`, `GroupBy`, a distinct
    /// count and a join all run on.
    ///
    /// This is one scan, no copies, and no intersection: containers arrive in key order, a row
    /// is sixteen consecutive keys, and [`and_cardinality`] counts an overlap without building
    /// it. A slot the filter holds nothing at is skipped rather than intersected with nothing.
    ///
    /// The result is ordered by row and holds only rows the filter left something of, which is
    /// what the caller wanted anyway.
    pub fn row_counts_where(&self, filter: &RowSet) -> Result<Vec<(RowId, u64)>> {
        let mut out: Vec<(RowId, u64)> = Vec::new();
        self.for_each(0, u64::MAX, |ckey, c| {
            let Some(here) = filter.get(slot_of_ckey(ckey)) else {
                return ControlFlow::Continue(());
            };
            let n = u64::from(and_cardinality(c, here.as_ref()));
            if n > 0 {
                let row = row_of_ckey(ckey);
                // Keys ascend, and a row's keys are consecutive, so the row being accumulated
                // is always the last one pushed. No map, and the result comes out sorted.
                match out.last_mut() {
                    Some((r, total)) if *r == row => *total += n,
                    _ => out.push((row, n)),
                }
            }
            ControlFlow::Continue(())
        })?;
        Ok(out)
    }

    /// Cardinality only, without materialising anything. Reads cached counts, not payloads.
    pub fn row_count(&self, row: RowId) -> Result<u64> {
        let span = row_ckeys(row);
        let mut total = 0u64;
        scan(self.pager, self.root, *span.start(), *span.end(), |c| {
            total += c.cardinality as u64;
            ControlFlow::Continue(())
        })?;
        Ok(total)
    }

    pub fn count(&self) -> Result<u64> {
        big_btree::count(self.pager, self.root)
    }

    /// Whether one record has the bit for `row` set.
    pub fn get(&self, row: RowId, record: RecordId) -> Result<bool> {
        let pos = pos_of(row, record);
        let ckey = ckey_of(pos);
        let want = offset_in_container(pos);
        let Some(hit) = big_btree::find(self.pager, self.root, ckey)? else { return Ok(false) };
        self.cell_contains(&hit.cell()?, want)
    }

    /// Asks one cell whether it holds an offset, without building the container.
    ///
    /// Deliberately not routed through `for_each`. That path resolves a cell into a container,
    /// which for a delta cell means copying eight kilobytes and applying the delta to it - and a
    /// bit-sliced point read asks this question once per bit plane, so going through it would
    /// have made a twenty-bit read copy 170 KB to answer twenty-one yes-or-no questions. The
    /// cell can answer directly: at most `MAX_DELTA` sorted entries, then one word of the base.
    fn cell_contains(&self, cell: &LeafCell<'_>, offset: u16) -> Result<bool> {
        match cell.ty {
            ContainerType::BitmapPtr | ContainerType::BitmapDelta => {
                let page = self.verified_base(cell)?;
                Ok(cell.contains_checked(Some(&page), offset)?)
            }
            _ => Ok(cell.contains_checked(None, offset)?),
        }
    }

    /// Probes many rows for one record in a single pass over the tree.
    ///
    /// `get` per row descends the whole tree each time. A BSI point read asks for one row per
    /// bit plane, and those rows sit at a fixed stride inside one fragment, so most of them
    /// share a leaf and can share the descent.
    ///
    /// Rows missing from the fragment come back `false`, the same answer `get` gives.
    pub fn get_many(
        &self,
        rows: impl IntoIterator<Item = RowId>,
        record: RecordId,
    ) -> Result<BTreeMap<RowId, bool>> {
        // Two rows can never share a container key for the same record, so this is a bijection
        // and nothing is lost by keying on the container.
        let mut want: BTreeMap<ContainerKey, (RowId, u16)> = BTreeMap::new();
        for row in rows {
            let pos = pos_of(row, record);
            want.insert(ckey_of(pos), (row, offset_in_container(pos)));
        }
        let keys: Vec<ContainerKey> = want.keys().copied().collect();

        let mut out = BTreeMap::new();
        find_many(self.pager, self.root, &keys, |ckey, cell| {
            let (row, offset) = want[&ckey];
            let found = match cell {
                Some(cell) => self.cell_contains(&cell, offset)?,
                None => false,
            };
            out.insert(row, found);
            Ok(())
        })?;
        Ok(out)
    }

    /// Rows that actually hold anything, in ascending order.
    pub fn rows(&self) -> Result<Vec<RowId>> {
        let mut out: Vec<RowId> = Vec::new();
        scan(self.pager, self.root, 0, u64::MAX, |c| {
            let r = row_of_ckey(c.key);
            if out.last() != Some(&r) && c.cardinality > 0 {
                out.push(r);
            }
            ControlFlow::Continue(())
        })?;
        Ok(out)
    }

    /// Bare container access without resolving dense pages; used by the writer.
    pub(crate) fn raw_container(&self, ckey: ContainerKey) -> Result<Option<Container>> {
        self.container(ckey)
    }
}
