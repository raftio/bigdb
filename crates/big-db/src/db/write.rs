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

/// What one record's column cell is about to become.
///
/// Keyed by record rather than appended, exactly as [`Pending`] is, so writing the same record
/// twice in a transaction keeps the last decision rather than replaying both.
#[derive(Clone, Debug)]
enum ColEdit {
    /// The cell becomes exactly this. Every scalar kind, and a mutex - which is a keyed field
    /// that holds one value at a time, so a second write replaces the first.
    Replace(Cell),
    /// These row ids join whatever the record already holds. A set field only ever adds, so its
    /// column has to merge with what is stored rather than replace it - which is the one place
    /// a column write is not simply the caller's last word.
    ///
    /// There is deliberately no `Clear`: a delete does not go through the buffer at all. It
    /// nulls slots a block at a time in [`DbWrite::clear_column_records`], because the records
    /// of one delete are already grouped and the buffer would only regroup them.
    Add(Vec<RowId>),
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
    /// Keyed, unlike the two above, and measured that way: a set field *adds*, so collapsing a
    /// buffer of these means folding rather than dropping all but the last, and the fold has to
    /// sort a `Vec` whose elements own a `Vec` of their own. Records arrive ascending in a load,
    /// which is the case a `BTreeMap` appends into its rightmost leaf for almost nothing, so the
    /// sort lost to it by 1.3x. The bits above are keyed on `(row, record)` and cycle through
    /// rows, which is why the same change wins there and loses here.
    cells: BTreeMap<RecordId, ColEdit>,
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
/// The sort is stable, so entries for one key keep their arrival order; reversing before the
/// dedup is what makes the survivor the *last* arrival rather than the first.
fn last_per_key<K: Ord, V>(buf: Vec<(K, V)>) -> Vec<(K, V)> {
    let mut buf = last_sorted(buf);
    buf.reverse();
    buf.dedup_by(|a, b| a.0 == b.0);
    buf.reverse();
    buf
}

/// Ascending by key, with entries for one key left in arrival order.
fn last_sorted<K: Ord, V>(mut buf: Vec<(K, V)>) -> Vec<(K, V)> {
    buf.sort_by(|a, b| a.0.cmp(&b.0));
    buf
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
    fn flush_fragment(&mut self, key: FragmentKey) -> Result<()> {
        let Some(p) = self.pending.remove(&key) else { return Ok(()) };
        let values = last_per_key(p.values);
        let bits = last_per_key(p.bits);

        // Reserved rather than grown from empty, because the size is known here and the batch is
        // large: a twenty-bit field over a million records pushes twenty-one million pairs, and
        // a `Vec` doubling its way there memmoves roughly its own final length in the process.
        //
        // Half the total in each, not the whole of it in both. `bits_for` sends every plane to
        // exactly one of the two, so together they receive `depth + 1` per record and neither
        // alone can be predicted — but a value sets about half its bits, so half is the estimate
        // that costs one doubling in the worst case instead of twenty-five, without reserving
        // twice the memory the pair will ever hold.
        let depth = self.catalog.fragment(&key).map_or(1, |m| m.bit_depth.max(1)) as usize;
        let planes = if values.is_empty() { 0 } else { values.len() * (depth + 1) };
        let each = (planes + bits.len()).div_ceil(2);
        let mut set = Vec::with_capacity(each);
        let mut clear = Vec::with_capacity(each);

        if !values.is_empty() {
            // The depth the catalog ended the transaction on, not the depth each individual
            // write saw. A record written before the depth grew still gets every plane
            // accounted for, so overwriting a large value with a small one cannot leave a
            // stale high bit behind.
            let bsi = Bsi::new(depth as u32);
            for (record, value) in &values {
                bsi.bits_for(*record, *value, &mut set, &mut clear)?;
            }
        }
        for ((row, record), on) in &bits {
            if *on {
                set.push((*row, *record));
            } else {
                clear.push((*row, *record));
            }
        }

        if !p.cells.is_empty() {
            self.flush_cells(key, p.cells)?;
        }

        // A segment-only key has no bitmap half to write. Returning before `fragment_raw`
        // matters: registering it would put a standard-view fragment in the catalog that holds
        // nothing, and every scan would then visit a tree that does not exist.
        if set.is_empty() && clear.is_empty() && values.is_empty() {
            return Ok(());
        }
        let mut f = self.fragment_raw(key);
        f.write_bits(&mut self.txn, set, clear)?;
        self.save_fragment(key, f);
        Ok(())
    }

    /// Applies one fragment's buffered column edits, a block at a time.
    ///
    /// Grouped by block first, so a thousand records landing in one block cost one decode and
    /// one encode rather than a thousand of each. That grouping is the entire reason the edits
    /// were buffered instead of written where they were made.
    fn flush_cells(&mut self, key: FragmentKey, cells: BTreeMap<RecordId, ColEdit>) -> Result<()> {
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

    fn flush_all(&mut self) -> Result<()> {
        let keys: Vec<FragmentKey> = self.pending.keys().copied().collect();
        for key in keys {
            self.flush_fragment(key)?;
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
        let slot = self.pending.entry(key).or_default().cells.entry(record);
        use std::collections::btree_map::Entry;
        match (slot, edit) {
            (Entry::Occupied(mut e), ColEdit::Add(more)) => {
                if let ColEdit::Add(have) = e.get_mut() {
                    have.extend(more);
                } else {
                    e.insert(ColEdit::Add(more));
                }
            }
            (Entry::Occupied(mut e), other) => {
                e.insert(other);
            }
            (Entry::Vacant(e), edit) => {
                e.insert(edit);
            }
        }
    }

    /// Records a value in the zone map of whichever trees this table actually keeps.
    ///
    /// Both engines want it. An index uses it to skip a shard without reading a page; a scan
    /// uses it to skip a segment for exactly the same reason. Registering the key is also what
    /// puts the fragment or the segment on the list a reader discovers shards from, so a
    /// segment that never observed anything would be invisible to a scan.
    fn observe(&mut self, t: TableId, def: &FieldDef, record: RecordId, value: u64) {
        let engine = self.engine(t);
        let shard = shard_of(record);
        if engine.has_bitmap() {
            self.catalog.fragment_mut(self.key(t, def.id, shard)).observe(value);
        }
        if engine.has_columns() {
            self.catalog.fragment_mut(self.column_key(t, def.id, shard)).observe(value);
        }
    }

    /// Buffers a scalar column write when the table keeps columns at all.
    ///
    /// One call rather than a branch at every setter: the engine test belongs in one place, and
    /// a setter that forgot it would be a field silently missing from its table's segments.
    fn buffer_value(&mut self, t: TableId, def: &FieldDef, record: RecordId, value: u64) {
        if !self.engine(t).has_columns() {
            return;
        }
        let key = self.column_key(t, def.id, shard_of(record));
        self.buffer_cell(key, record, ColEdit::Replace(Cell::Value(value)));
    }

    /// Resolves a write to the field it names and the fragment it lands in.
    ///
    /// Every setter started with these three lines and then diverged, which is how `set_bool`
    /// and `set_key` ended up without the kind check `set_int` had.
    fn target(
        &self,
        table: &str,
        field: &str,
        record: RecordId,
    ) -> Result<(TableId, FieldDef, FragmentKey)> {
        let (t, def) = resolve(&self.catalog, table, field)?;
        let key = self.key(t, def.id, shard_of(record));
        Ok((t, def, key))
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
    pub fn mark_exists(&mut self, table: &str, record: RecordId) -> Result<()> {
        let t = self
            .catalog
            .table(table)
            .map(|t| t.id)
            .ok_or_else(|| DbError::UnknownTable(table.to_string()))?;
        let key = self.key(t, EXISTS_FIELD, shard_of(record));
        self.buffer_bit(key, EXISTS_ROW, record, true);
        Ok(())
    }

    pub fn set_int(
        &mut self,
        table: &str,
        field: &str,
        record: RecordId,
        value: u64,
    ) -> Result<()> {
        let (_, def, key) = self.target(table, field, record)?;
        // `is_bsi` now covers the signed kind too, and this setter must not: the value it takes
        // is already the stored value, so writing one to a signed field would store a number
        // that reads back as something else entirely. `set_signed` is the way in.
        expect_kind(&def, field, |k| k.is_bsi() && !k.is_signed(), "int")?;

        // A value wider than the field was declared for is refused, not truncated.
        let declared = if def.bit_depth == 0 { 64 } else { def.bit_depth };
        if 64 - value.leading_zeros() > declared {
            return Err(big_field::FieldError::ValueTooWide { value, bit_depth: declared }.into());
        }

        // Widen the fragment and record the zone map in the same breath. Depth only ever
        // grows, and the buffered value is expanded at whatever depth the transaction ends
        // on, so a record buffered now is not stranded at today's narrower depth.
        //
        // The zone map is recorded whichever engine this is: a scan wants to skip a shard
        // whose range rules the predicate out every bit as much as an index does.
        let t = self.catalog.table(table).map_or(0, |x| x.id);
        self.observe(t, &def, record, value);
        if self.engine(t).has_bitmap() {
            self.pending.entry(key).or_default().values.push((record, value));
        }
        self.buffer_value(t, &def, record, value);
        self.mark_exists(table, record)
    }

    /// Writes a signed value, biased on the way in.
    ///
    /// Everything below this line sees an ordinary unsigned bit-sliced index; the sign lives in
    /// [`crate::signed`] and nowhere else. Refused rather than wrapped when the value does not
    /// fit the declared range, for the same reason `set_int` refuses a value too wide: a number
    /// that comes back as a different number is worse than a write that failed.
    pub fn set_signed(
        &mut self,
        table: &str,
        field: &str,
        record: RecordId,
        value: i64,
    ) -> Result<()> {
        let (_, def, key) = self.target(table, field, record)?;
        expect_kind(&def, field, FieldKind::is_signed, "signed int")?;

        let declared = if def.bit_depth == 0 { 64 } else { def.bit_depth };
        let stored =
            crate::signed::encode(value, declared).ok_or(DbError::SignedValueOutOfRange {
                value,
                min: crate::signed::min_value(declared),
                max: crate::signed::max_value(declared),
            })?;

        // The zone map observes the *stored* value, which is what makes it work unchanged: the
        // encoding is monotonic, so a window in stored space is the same window in value space.
        let t = self.catalog.table(table).map_or(0, |x| x.id);
        self.observe(t, &def, record, stored);
        if self.engine(t).has_bitmap() {
            self.pending.entry(key).or_default().values.push((record, stored));
        }
        self.buffer_value(t, &def, record, stored);
        self.mark_exists(table, record)
    }

    pub fn set_bool(
        &mut self,
        table: &str,
        field: &str,
        record: RecordId,
        value: bool,
    ) -> Result<()> {
        let (_, def, key) = self.target(table, field, record)?;
        expect_kind(&def, field, |k| k == FieldKind::Bool, "bool")?;

        let t = self.catalog.table(table).map_or(0, |x| x.id);
        if self.engine(t).has_bitmap() {
            self.buffer_bit(key, BoolField::row_of(value), record, true);
            self.buffer_bit(key, BoolField::row_of(!value), record, false);
        }
        // A boolean's column is one bit wide after the codec measures it, so this is close to
        // free next to the two rows the index spends.
        self.buffer_value(t, &def, record, value as u64);
        self.mark_exists(table, record)
    }

    /// Interns the row key and sets the bit. Row keys are the one thing that must mean the same
    /// in every shard, which is why they go through the catalog rather than being derived.
    pub fn set_key(
        &mut self,
        table: &str,
        field: &str,
        record: RecordId,
        value: &str,
    ) -> Result<RowId> {
        let (t, def, key) = self.target(table, field, record)?;
        expect_kind(&def, field, FieldKind::is_keyed, "set, mutex or time quantum")?;
        let row = self.catalog.keys.intern(t, def.id, value)?;

        let engine = self.engine(t);
        if engine.has_bitmap() {
            if def.kind == FieldKind::Mutex {
                // A mutex has to read its shadow to find the value it is replacing, so it
                // cannot wait with the rest of the batch. `fragment` flushes anything already
                // buffered for these fragments first, so it still sees the transaction as it
                // stands.
                let shadow_key = FragmentKey { view: MUTEX_SHADOW_VIEW, ..key };
                let m = MutexField::new(SHADOW_DEPTH);
                let mut values = self.fragment(key);
                let mut shadow = self.fragment(shadow_key);
                m.put(&mut self.txn, &mut values, &mut shadow, record, row)?;
                self.save_fragment(key, values);
                self.save_fragment(shadow_key, shadow);
            } else {
                // A set field only ever turns bits on, so there is nothing to serialise
                // against and the write can wait with the rest of the batch.
                self.buffer_bit(key, row, record, true);
            }
        }
        if engine.has_columns() {
            let col = self.column_key(t, def.id, shard_of(record));
            // A mutex holds one value at a time, so its column is a replace and needs no
            // shadow at all - the segment already knows what the record held, which is the one
            // place a column is strictly simpler than the index it sits beside.
            let edit = if def.kind == FieldKind::Mutex {
                ColEdit::Replace(Cell::Value(row))
            } else {
                ColEdit::Add(vec![row])
            };
            self.buffer_cell(col, record, edit);
        }

        self.mark_exists(table, record)?;
        Ok(row)
    }

    /// Assigns a row id to a key without writing any fact about it.
    ///
    /// The schema leader's half of the row-key agreement. A coordinator resolves every key in
    /// a batch here first, so that the facts it then sends to the shard owners carry ids that
    /// already mean the same thing everywhere. Interning is a catalog write, so this takes a
    /// transaction like any other - and it is a *separate* transaction from the import,
    /// because the import happens on other machines.
    pub fn intern_key(&mut self, table: &str, field: &str, value: &str) -> Result<RowId> {
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
    pub fn assign_key(&mut self, table: &str, field: &str, value: &str, row: RowId) -> Result<()> {
        let (t, def) = resolve(&self.catalog, table, field)?;
        expect_kind(&def, field, FieldKind::is_keyed, "set, mutex or time quantum")?;
        Ok(self.catalog.keys.assign(t, def.id, value, row)?)
    }

    /// Writes a keyed fact that also happened at a moment in time.
    ///
    /// The fact goes into the standard view exactly as `set_key` would, and additionally into
    /// one view per granularity the field declared. Those extra views are what make a range
    /// query read only the days it asks about instead of every record ever written.
    pub fn set_time(
        &mut self,
        table: &str,
        field: &str,
        record: RecordId,
        value: &str,
        unix_seconds: i64,
    ) -> Result<RowId> {
        let (t, def, _) = self.target(table, field, record)?;
        expect_kind(&def, field, |k| k == FieldKind::TimeQuantum, "time quantum")?;

        let row = self.set_key(table, field, record, value)?;

        let granularity = if def.granularity.is_empty() {
            big_field::DEFAULT_GRANULARITY.to_vec()
        } else {
            def.granularity.clone()
        };
        for name in big_field::views(unix_seconds, &granularity) {
            let view = self.catalog.intern_view(&name)?;
            let key = FragmentKey { table: t, field: def.id, view, shard: shard_of(record) };
            self.buffer_bit(key, row, record, true);
        }
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
                .entry(big_column::block_of(*local))
                .or_default()
                .push((big_column::slot_of(*local), cell.clone()));
        }

        let mut w = ColumnWrite::new(None);
        for (block, slots) in by_block {
            let mut value = big_column::Block::new();
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
        let t = self
            .catalog
            .table(&addr.table)
            .map(|t| t.id)
            .ok_or_else(|| DbError::UnknownTable(addr.table.clone()))?;
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
    pub fn delete(&mut self, table: &str, records: &[RecordId]) -> Result<u64> {
        let t = self
            .catalog
            .table(table)
            .map(|t| t.id)
            .ok_or_else(|| DbError::UnknownTable(table.to_string()))?;

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
    pub fn delete_where(&mut self, table: &str, rows: &Matches) -> Result<u64> {
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
