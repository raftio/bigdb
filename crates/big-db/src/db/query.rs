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

//! What a read transaction can be asked.
//!
//! Every verb here is the same two steps: resolve the names, then hand one closure to the
//! fan-out in `super::read` and collapse what comes back. That is why they are short, and it
//! is also the constraint - a question that cannot be phrased as one pass over the candidate
//! fragments does not belong in this file, because it would not be one either.

use super::*;

impl<'db, P: Pager + Sync> DbRead<'db, P> {
    /// Which records satisfy a predicate, unmaterialised and per shard.
    ///
    /// This is the composable form: two of these can be intersected or unioned without either
    /// one naming a record. Everything below is a way of asking this and then collapsing the
    /// answer, which is why they are three lines each.
    pub fn matching(&self, table: &str, field: &str, op: RangeOp, k: u64) -> Result<Matches> {
        let (t, def) = resolve(&self.catalog, table, field)?;
        if self.scans(t) {
            return self.scan_matching(t, def.id, op, k);
        }
        let per_shard = self.per_fragment(t, def.id, zone_bounds(op, k), |frag, key, depth| {
            let rows = Bsi::new(depth).range(frag, op, k)?;
            self.charge(rows.byte_size())?;
            Ok((key.shard, rows))
        })?;
        self.gather(per_shard)
    }

    /// Records whose signed field satisfies a predicate.
    ///
    /// The bound is biased and the comparison then runs unchanged: the encoding is monotonic,
    /// so `value > k` and `stored > encode(k)` select the same records and there is no signed
    /// variant of `Bsi::range` anywhere.
    ///
    /// A bound outside the field's range is answered rather than refused - `> 10_000` against a
    /// field that stops at 127 is a legitimate question whose answer does not depend on the
    /// schema. Clamping alone would get it wrong, though: `> max` and `>= max` clamp to the same
    /// stored bound, and the first must match nothing where the second matches the largest
    /// records. So an out-of-range bound is decided here, before any fragment is read.
    pub fn matching_signed(
        &self,
        table: &str,
        field: &str,
        op: RangeOp,
        k: i64,
    ) -> Result<Matches> {
        let (_, def) = resolve(&self.catalog, table, field)?;
        expect_kind(&def, field, FieldKind::is_signed, "signed int")?;
        let declared = declared_depth(&def);

        if let Some(side) = crate::signed::out_of_range(k, declared) {
            use std::cmp::Ordering::{Greater, Less};
            // `everything` means every record that has a value, which is not every record in
            // the table: a record with no value is not less than anything.
            let everything = matches!(
                (op, side),
                (RangeOp::Gt | RangeOp::Ge | RangeOp::Ne, Less)
                    | (RangeOp::Lt | RangeOp::Le | RangeOp::Ne, Greater)
            );
            return if everything {
                self.matching_row(table, field, EXISTS_ROW)
            } else {
                Ok(Matches::new())
            };
        }

        self.matching(table, field, op, crate::signed::encode_bound(k, declared))
    }

    /// Smallest value of a signed field over a set of records.
    pub fn min_signed_where(
        &self,
        table: &str,
        field: &str,
        rows: &Matches,
    ) -> Result<Option<i64>> {
        let declared = self.signed_depth(table, field)?;
        Ok(self.min_where(table, field, rows)?.map(|v| crate::signed::decode(v, declared)))
    }

    /// Largest value of a signed field over a set of records.
    pub fn max_signed_where(
        &self,
        table: &str,
        field: &str,
        rows: &Matches,
    ) -> Result<Option<i64>> {
        let declared = self.signed_depth(table, field)?;
        Ok(self.max_where(table, field, rows)?.map(|v| crate::signed::decode(v, declared)))
    }

    /// Sum of a signed field over a set of records.
    ///
    /// `sum(value) = sum(stored) - count * bias`. The bias is per record, not per sum, which is
    /// why this needs a count that `sum_where` does not - and why the count has to be of records
    /// that actually **hold a value**, not of records in the filter. A record with no value
    /// contributes no stored bits and must contribute no bias either, or a table with nulls in
    /// it would sum to something arbitrarily far from the truth.
    ///
    /// `i128` because the sum of `u64`-wide values minus `n` biases needs more than 64 bits in
    /// either direction, and because refusing to overflow is cheaper than explaining it.
    pub fn sum_signed_where(&self, table: &str, field: &str, rows: &Matches) -> Result<i128> {
        let declared = self.signed_depth(table, field)?;
        let stored = self.sum_where(table, field, rows)? as i128;
        let n = self.count_values(table, field, rows)? as i128;
        Ok(stored - n * crate::signed::bias(declared) as i128)
    }

    /// How many records in `rows` hold a value for this field.
    ///
    /// Not the same as `rows.cardinality()`: a record can be in the filter and have nothing
    /// stored for this field, which is exactly what the exists row of a bit-sliced index is for.
    pub fn count_values(&self, table: &str, field: &str, rows: &Matches) -> Result<u64> {
        let (t, def) = resolve(&self.catalog, table, field)?;
        if self.scans(t) {
            return self.scan_count_values(t, def.id, rows);
        }
        let per_shard = self.per_fragment(t, def.id, (None, None), |frag, key, depth| {
            Ok(match rows.get(key.shard) {
                Some(filter) => Bsi::new(depth).count(frag, Some(filter))?,
                None => 0,
            })
        })?;
        Ok(per_shard.into_iter().sum())
    }

    /// The declared depth of a field that must be signed.
    fn signed_depth(&self, table: &str, field: &str) -> Result<u32> {
        let (_, def) = resolve(&self.catalog, table, field)?;
        expect_kind(&def, field, FieldKind::is_signed, "signed int")?;
        Ok(declared_depth(&def))
    }

    /// Every record that exists in a table, unmaterialised and per shard.
    ///
    /// `Not` has no meaning without this. A bitmap says which records matched, never how many
    /// records there could have been, so complementing one needs the universe stated
    /// explicitly - which is the whole reason `EXISTS_FIELD` is written on every insert.
    pub fn all(&self, table: &str) -> Result<Matches> {
        let t = self
            .catalog
            .table(table)
            .map(|t| t.id)
            .ok_or_else(|| DbError::UnknownTable(table.to_string()))?;

        let per_shard = self.per_fragment(t, EXISTS_FIELD, (None, None), |frag, key, _| {
            let rows = frag.row(EXISTS_ROW)?;
            self.charge(rows.byte_size())?;
            Ok((key.shard, rows))
        })?;
        self.gather(per_shard)
    }

    /// How many records a table holds, without naming or materialising any of them.
    ///
    /// `all().cardinality()` answers the same question, and pays for a `RowSet` to do it: every
    /// container of the exists row is decoded and held so that a count can be taken off it.
    /// None of that is necessary. A leaf cell already carries the cardinality of the container
    /// it points at, so the number is on the page the descent lands on, before any payload is
    /// touched. This reads those cached counts and stops there.
    ///
    /// Nothing is charged against the memory budget because nothing is held: the peak is one
    /// leaf page, which the pager owns whether this is called or not. The deadline and the
    /// cancellation flag are still honoured - `per_fragment` checkpoints once per fragment.
    pub fn count_all(&self, table: &str) -> Result<u64> {
        let t = self
            .catalog
            .table(table)
            .map(|t| t.id)
            .ok_or_else(|| DbError::UnknownTable(table.to_string()))?;

        let counts = self.per_fragment(t, EXISTS_FIELD, (None, None), |frag, _, _| {
            Ok(frag.row_count(EXISTS_ROW)?)
        })?;
        Ok(counts.into_iter().sum())
    }

    /// A page of record ids from a table, ascending, starting at `from`.
    ///
    /// The cursor. **There is no cursor object**, and that is the design rather than a
    /// shortcut: record ids are already ascending across shards, slots and containers, so the
    /// last id of a page is everything the next page needs. Nothing is held between calls, no
    /// read transaction outlives one request, and a client that walks away mid-scan costs
    /// nothing - which is the failure mode a stateful cursor over HTTP would have made
    /// expensive.
    ///
    /// Unlike [`all`], this does not materialise the table. Fragments are visited in shard
    /// order and the walk stops at the first one that fills the page, so the memory ceiling is
    /// one shard's exists row - bounded by `SHARD_WIDTH`, not by how many records the table
    /// holds. That is the whole reason it does not simply page over `all()`.
    ///
    /// Sequential on purpose. The fan-out in `per_fragment` is a map/reduce over an
    /// order-independent fold; this is neither order-independent nor a fold, and a parallel
    /// version would have to do every shard's work in order to throw most of it away.
    ///
    /// [`all`]: DbRead::all
    pub fn scan_records(&self, table: &str, from: RecordId, limit: usize) -> Result<Vec<RecordId>> {
        let t = self
            .catalog
            .table(table)
            .map(|t| t.id)
            .ok_or_else(|| DbError::UnknownTable(table.to_string()))?;

        let candidates: Vec<FragmentKey> = self
            .catalog
            .fragments_of_field(t, EXISTS_FIELD, STANDARD_VIEW)
            // Ascending by shard, because `FragmentKey` orders on shard last and the catalog
            // keeps them in a `BTreeMap`. Whole shards below the cursor are skipped here rather
            // than filtered later.
            .filter(|(k, _)| k.shard >= shard_of(from))
            .map(|(k, _)| *k)
            .collect();

        self.checkpoint()?;
        let mut out = Vec::with_capacity(limit.min(1024));
        for key in candidates {
            if out.len() >= limit {
                break;
            }
            self.checkpoint()?;
            let Some(frag) = self.frag(&key) else { continue };
            let rows = frag.row(EXISTS_ROW)?;
            // Charged like any other materialised row set, and released at the end of this
            // iteration rather than accumulated - the budget sees the real peak either way.
            self.charge(rows.byte_size())?;
            let floor = from.max(key.shard * SHARD_WIDTH);
            out.extend(rows.records_from(key.shard, floor).take(limit - out.len()));
        }
        Ok(out)
    }

    /// Records in one row of a field, unmaterialised and per shard.
    ///
    /// The primitive under every set-shaped read: a key lookup is this plus a name to resolve,
    /// a boolean is this plus knowing which of two rows means true.
    pub fn matching_row(&self, table: &str, field: &str, row: RowId) -> Result<Matches> {
        let (t, def) = resolve(&self.catalog, table, field)?;
        if self.scans(t) {
            // The exists row of a bit-sliced index has no segment twin: a scan answers "which
            // records hold a value here" by finding the cells that are not null.
            return if row == EXISTS_ROW && def.kind.is_bsi() {
                self.scan_present(t, def.id)
            } else {
                self.scan_matching_row(t, def.id, row)
            };
        }
        let per_shard = self.per_fragment(t, def.id, (None, None), |frag, key, _| {
            let rows = frag.row(row)?;
            self.charge(rows.byte_size())?;
            Ok((key.shard, rows))
        })?;
        self.gather(per_shard)
    }

    /// Records in one row of a field, in a named view.
    pub fn matching_row_in(
        &self,
        table: &str,
        field: &str,
        view: ViewId,
        row: RowId,
    ) -> Result<Matches> {
        let (t, def) = resolve(&self.catalog, table, field)?;
        if self.scans(t) {
            return Err(DbError::EngineCannotAnswer {
                table: table.to_string(),
                what: "a query against a named view",
                engine: "columnar",
                instead: "views are written into an index; create the table with the \
                          bitmap+columnar engine",
            });
        }
        let per_shard = self.per_fragment_in(t, def.id, view, (None, None), |frag, key, _| {
            let rows = frag.row(row)?;
            self.charge(rows.byte_size())?;
            Ok((key.shard, rows))
        })?;
        self.gather(per_shard)
    }

    /// Records carrying a row key at some point between two instants.
    ///
    /// The union of the day views the range covers. A range is answered by reading only the
    /// days in it, which is the entire reason a time quantum field pays to write them.
    pub fn matching_key_between(
        &self,
        table: &str,
        field: &str,
        value: &str,
        from: Option<i64>,
        to: Option<i64>,
    ) -> Result<Matches> {
        let (t, def) = resolve(&self.catalog, table, field)?;
        // Refused rather than answered empty. A segment records which keys a record holds and
        // never when it held them, so there is nothing here to read - and "no records in that
        // window" is a different fact from "this table cannot see time at all".
        if self.scans(t) {
            return Err(DbError::EngineCannotAnswer {
                table: table.to_string(),
                what: "a time window",
                engine: "columnar",
                instead: "a time quantum field writes its per-day views into an index; \
                          create the table with the bitmap+columnar engine",
            });
        }
        let Some(row) = self.catalog.keys.id(t, def.id, value) else { return Ok(Matches::new()) };

        let (lo, hi) = (
            from.map(big_engine::bitmap::field::day_view),
            to.map(big_engine::bitmap::field::day_view),
        );
        let mut out = Matches::new();
        for view in self.catalog.day_views_between(lo.as_deref(), hi.as_deref()) {
            out = out.or(&self.matching_row_in(table, field, view, row)?);
        }
        Ok(out)
    }

    /// Records carrying a given row key, unmaterialised and per shard.
    pub fn matching_key(&self, table: &str, field: &str, value: &str) -> Result<Matches> {
        let (t, def) = resolve(&self.catalog, table, field)?;
        // An unknown key matched nothing, which is not the same as an error: asking for a
        // value that was never written is a legitimate query with an empty answer.
        let Some(row) = self.catalog.keys.id(t, def.id, value) else { return Ok(Matches::new()) };
        self.matching_row(table, field, row)
    }

    /// Records whose boolean field holds `value`.
    pub fn matching_bool(&self, table: &str, field: &str, value: bool) -> Result<Matches> {
        let (t, def) = resolve(&self.catalog, table, field)?;
        // A boolean is two rows in an index and one value in a segment, so this is the one
        // predicate that cannot be spelled as a row lookup on both sides.
        if self.scans(t) {
            return self.scan_matching_bool(t, def.id, value);
        }
        self.matching_row(table, field, BoolField::row_of(value))
    }

    /// Every row of a keyed field that appears in `filter`, with how many records it holds
    /// there.
    ///
    /// Shards are summed **before** anything is sorted or truncated. Taking the top rows of
    /// each shard and merging those would be wrong: a row that is second everywhere can beat
    /// one that is first in a single shard.
    ///
    /// Only rows a fragment actually holds are probed, so a field with a large key space
    /// costs what its data costs rather than what its schema allows.
    pub fn group_counts(
        &self,
        table: &str,
        field: &str,
        filter: &Matches,
    ) -> Result<Vec<(RowId, u64)>> {
        let (t, def) = resolve(&self.catalog, table, field)?;
        if self.scans(t) {
            return self.scan_group_counts(t, def.id, filter);
        }
        // One pass over the fragment for every row, rather than one pass per row. See
        // `FragmentRead::row_counts_where`: the counts are what a grouping wants and the
        // intersections it used to build were allocated only to be measured and dropped.
        let per_shard = self.per_fragment(t, def.id, (None, None), |frag, key, _| {
            let Some(here) = filter.get(key.shard) else { return Ok(Vec::new()) };
            Ok(frag.row_counts_where(here)?)
        })?;

        let mut totals: BTreeMap<RowId, u64> = BTreeMap::new();
        for shard in per_shard {
            for (row, n) in shard {
                *totals.entry(row).or_default() += n;
            }
        }
        Ok(totals.into_iter().collect())
    }

    /// The same rows, but each carrying the records it holds rather than only a count.
    ///
    /// Strictly more expensive than `group_counts` and only worth it when something is going
    /// to be aggregated per group, because a `Matches` per row is built either way.
    pub fn group_matches(
        &self,
        table: &str,
        field: &str,
        filter: &Matches,
    ) -> Result<Vec<(RowId, Matches)>> {
        let (t, def) = resolve(&self.catalog, table, field)?;
        if self.scans(t) {
            return self.scan_group_matches(t, def.id, filter);
        }
        let per_shard = self.per_fragment(t, def.id, (None, None), |frag, key, _| {
            let Some(here) = filter.get(key.shard) else { return Ok(Vec::new()) };
            let mut local = Vec::new();
            for row in frag.rows()? {
                let hit = frag.row(row)?.and(here);
                if !hit.is_empty() {
                    // Charged per group: a grouping over a high-cardinality field builds one
                    // row set per distinct value, which is the one read that can outgrow the
                    // data it is reading.
                    self.charge(hit.byte_size())?;
                    local.push((row, key.shard, hit));
                }
            }
            Ok(local)
        })?;

        let mut groups: BTreeMap<RowId, Matches> = BTreeMap::new();
        for shard in per_shard {
            for (row, shard_id, hit) in shard {
                groups.entry(row).or_default().insert(shard_id, hit);
            }
        }
        Ok(groups.into_iter().collect())
    }

    /// The string a row id was interned from, when the field has one.
    pub fn row_key(&self, table: &str, field: &str, row: RowId) -> Option<&str> {
        let t = self.catalog.table(table)?.id;
        let def = self.catalog.field(t, field)?;
        self.catalog.keys.name(t, def.id, row)
    }

    /// Every fragment of a table, addressed by name, with the count that stands in for its
    /// contents.
    ///
    /// **The count is not a heuristic here, it is a proof.** Under this engine's clustering
    /// rule every write reaches the copy that serves the range before any other, so a copy
    /// that is behind holds a strict subset of what the serving copy holds - and a subset with
    /// the same cardinality is the same set. Two fragments with equal counts therefore need no
    /// further comparison, and a repair walks only what actually differs.
    pub fn fragments(&self, table: &str) -> Result<Vec<(FragmentAddr, u64)>> {
        let t = self
            .catalog
            .table(table)
            .map(|t| t.id)
            .ok_or_else(|| DbError::UnknownTable(table.to_string()))?;
        let keys: Vec<FragmentKey> = self.catalog.fragments_of_table(t).map(|(k, _)| *k).collect();
        let mut out = Vec::with_capacity(keys.len());
        for key in keys {
            let count = match self.frag(&key) {
                Some(f) => f.count()?,
                None => 0,
            };
            out.push((self.address(table, &key), count));
        }
        Ok(out)
    }

    /// One fragment's containers, which is everything it is.
    pub fn fragment_containers(
        &self,
        addr: &FragmentAddr,
    ) -> Result<Vec<(ContainerKey, Container)>> {
        let Some(key) = self.locate(addr) else { return Ok(Vec::new()) };
        let Some(f) = self.frag(&key) else { return Ok(Vec::new()) };
        let mut out = Vec::new();
        f.for_each(0, ContainerKey::MAX, |ckey, c| {
            out.push((ckey, c.to_owned()));
            core::ops::ControlFlow::Continue(())
        })?;
        Ok(out)
    }

    /// What a column segment holds, as the offset of each record within its shard and the cell
    /// it carries.
    ///
    /// Records rather than blocks, and offsets rather than encoded bytes. A repair that shipped
    /// the encoded form would tie two nodes to the same codec choice for ever; shipping what the
    /// records *are* lets the receiver encode with its own, which is what keeps the block format
    /// an implementation detail rather than a wire format.
    pub fn segment_cells(&self, addr: &FragmentAddr) -> Result<Vec<(u64, Cell)>> {
        let Some(key) = self.locate(addr) else { return Ok(Vec::new()) };
        let Some(seg) = self.segment(&key) else { return Ok(Vec::new()) };
        let mut out = Vec::new();
        seg.for_each_block(|block, decoded| {
            for (slot, cell) in decoded.slots().iter().enumerate() {
                if !cell.is_null() {
                    out.push((
                        block * big_engine::columnar::BLOCK_RECORDS + slot as u64,
                        cell.clone(),
                    ));
                }
            }
            Ok(core::ops::ControlFlow::Continue(()))
        })?;
        Ok(out)
    }

    /// The bit depth and zone map of one fragment, which a repair has to carry with the bits:
    /// the zone map is what lets `amount > k` skip a shard, and a copy whose map is missing
    /// would skip shards that hold matches.
    pub fn fragment_meta(&self, addr: &FragmentAddr) -> Option<crate::catalog::FragmentMeta> {
        self.locate(addr).and_then(|k| self.catalog.fragment(&k).cloned())
    }

    /// Every row key of a table, so a copy that was away can be told what it missed.
    pub fn row_keys(&self, table: &str) -> Result<Vec<(String, String, RowId)>> {
        let t =
            self.catalog.table(table).ok_or_else(|| DbError::UnknownTable(table.to_string()))?;
        let mut out = Vec::new();
        for field in self.catalog.fields_of(t.id) {
            for (row, name) in self.catalog.keys.rows(t.id, field.id) {
                out.push((field.name.clone(), name.to_string(), row));
            }
        }
        Ok(out)
    }

    /// A fragment key, as a name every node can resolve for itself.
    fn address(&self, table: &str, key: &FragmentKey) -> FragmentAddr {
        FragmentAddr {
            table: table.to_string(),
            field: self
                .catalog
                .fields_of(key.table)
                .find(|f| f.id == key.field)
                .map(|f| f.name.clone()),
            view: self.catalog.view_name(key.view).map(str::to_string),
            view_id: key.view,
            shard: key.shard,
            field_id: key.field,
        }
    }

    /// The other way, against this node's own numbering.
    fn locate(&self, addr: &FragmentAddr) -> Option<FragmentKey> {
        let t = self.catalog.table(&addr.table)?.id;
        let field = match &addr.field {
            Some(name) => self.catalog.field(t, name)?.id,
            // A reserved field - the existence row - has no name and the same id everywhere.
            None => addr.field_id,
        };
        let view = match &addr.view {
            Some(name) => self.catalog.view_id(name)?,
            None => addr.view_id,
        };
        Some(FragmentKey { table: t, field, view, shard: addr.shard })
    }

    /// The row a key was interned to, when this database has seen the key.
    ///
    /// The inverse of [`DbRead::row_key`], and the read half of the cache a schema leader
    /// fills: a node that already knows what a key means does not have to ask.
    pub fn key_row(&self, table: &str, field: &str, key: &str) -> Option<RowId> {
        let t = self.catalog.table(table)?.id;
        let def = self.catalog.field(t, field)?;
        self.catalog.keys.id(t, def.id, key)
    }

    /// Smallest and largest value of a field over a given set of records.
    ///
    /// `None` when nothing in `rows` holds a value. Like `sum_where`, the filter is applied
    /// inside the bit-sliced index, so this never names a record id.
    pub fn min_where(&self, table: &str, field: &str, rows: &Matches) -> Result<Option<u64>> {
        self.extreme(table, field, rows, Extreme::Min)
    }

    pub fn max_where(&self, table: &str, field: &str, rows: &Matches) -> Result<Option<u64>> {
        self.extreme(table, field, rows, Extreme::Max)
    }

    fn extreme(
        &self,
        table: &str,
        field: &str,
        rows: &Matches,
        which: Extreme,
    ) -> Result<Option<u64>> {
        let (t, def) = resolve(&self.catalog, table, field)?;
        if self.scans(t) {
            return self.scan_extreme(t, def.id, Some(rows), which == Extreme::Max);
        }
        let per_shard = self.per_fragment(t, def.id, (None, None), |frag, key, depth| {
            let bsi = Bsi::new(depth);
            Ok(match rows.get(key.shard) {
                Some(filter) => match which {
                    Extreme::Min => bsi.min(frag, Some(filter))?,
                    Extreme::Max => bsi.max(frag, Some(filter))?,
                },
                None => None,
            })
        })?;

        // Each shard answers for itself; the answer for the table is the extreme of those.
        Ok(per_shard.into_iter().flatten().reduce(|a, b| match which {
            Extreme::Min => a.min(b),
            Extreme::Max => a.max(b),
        }))
    }

    /// Sum of a field over a given set of records.
    ///
    /// The filter is applied plane by plane inside the bit-sliced index, so a sum over a
    /// selection costs the same as a sum over everything and never materialises a record id.
    pub fn sum_where(&self, table: &str, field: &str, rows: &Matches) -> Result<u128> {
        let (t, def) = resolve(&self.catalog, table, field)?;
        if self.scans(t) {
            return self.scan_sum(t, def.id, Some(rows));
        }
        let per_shard = self.per_fragment(t, def.id, (None, None), |frag, key, depth| {
            Ok(match rows.get(key.shard) {
                Some(filter) => Bsi::new(depth).sum(frag, Some(filter))?,
                None => 0,
            })
        })?;
        Ok(per_shard.into_iter().sum())
    }

    /// Runs a range predicate over every shard, skipping any whose zone map rules it out.
    pub fn range(&self, table: &str, field: &str, op: RangeOp, k: u64) -> Result<Vec<RecordId>> {
        self.materialise(self.matching(table, field, op, k)?)
    }

    /// Names every matching record, once it is certain the answer will fit.
    ///
    /// The check happens first and costs nothing: a `Matches` knows its cardinality without
    /// constructing a single record id, so an answer too large to hold is refused before a
    /// byte is allocated for it rather than part way through.
    fn materialise(&self, rows: Matches) -> Result<Vec<RecordId>> {
        self.check_records(rows.cardinality())?;
        Ok(rows.records().collect())
    }

    /// How many records match, without materialising which ones.
    pub fn count(&self, table: &str, field: &str, op: RangeOp, k: u64) -> Result<u64> {
        Ok(self.matching(table, field, op, k)?.cardinality())
    }

    pub fn sum(&self, table: &str, field: &str) -> Result<u128> {
        let (t, def) = resolve(&self.catalog, table, field)?;
        if self.scans(t) {
            return self.scan_sum(t, def.id, None);
        }
        let per_shard = self.per_fragment(t, def.id, (None, None), |frag, _, depth| {
            Ok(Bsi::new(depth).sum(frag, None)?)
        })?;
        Ok(per_shard.into_iter().sum())
    }

    /// Records carrying a given row key, across every shard.
    pub fn by_key(&self, table: &str, field: &str, value: &str) -> Result<Vec<RecordId>> {
        self.materialise(self.matching_key(table, field, value)?)
    }

    pub fn exists(&self, table: &str, record: RecordId) -> Result<bool> {
        let t = self
            .catalog
            .table(table)
            .map(|t| t.id)
            .ok_or_else(|| DbError::UnknownTable(table.to_string()))?;
        let key = FragmentKey {
            table: t,
            field: EXISTS_FIELD,
            view: STANDARD_VIEW,
            shard: shard_of(record),
        };
        let Some(f) = self.frag(&key) else { return Ok(false) };
        Ok(f.get(0, record)?)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Extreme {
    Min,
    Max,
}

/// Turns a predicate into the value window a zone map can be tested against.
pub(super) fn zone_bounds(op: RangeOp, k: u64) -> (Option<u64>, Option<u64>) {
    match op {
        RangeOp::Gt => (Some(k.saturating_add(1)), None),
        RangeOp::Ge => (Some(k), None),
        RangeOp::Lt => (None, Some(k.saturating_sub(1))),
        RangeOp::Le => (None, Some(k)),
        RangeOp::Eq => (Some(k), Some(k)),
        RangeOp::Ne => (None, None),
    }
}
