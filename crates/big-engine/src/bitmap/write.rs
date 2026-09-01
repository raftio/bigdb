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

//! Writing a fragment. Batch is the only sane granularity: a single bit costs a whole page
//! rewrite plus the shadow path up to the root.

use crate::base::coords::*;
use crate::bitmap::rowset::RowSet;
use big_btree::{put_container, put_containers, remove, Result};
use big_container::{apply, Container, ContainerRef, SetOp};
use big_page::{ContainerKey, PageType, Pgno};
use big_pager::{PagerMut, WriteTxn};
use std::collections::{BTreeMap, BTreeSet};

/// A handle to one fragment inside a transaction.
///
/// Deliberately holds no borrow of the transaction: a batch touches many fragments of the same
/// shard in a single commit, and a handle that owned `&mut WriteTxn` would make the second one
/// impossible to create.
#[derive(Clone, Copy, Debug)]
pub struct FragmentWrite {
    root: Option<Pgno>,
    shard: ShardId,
}

impl FragmentWrite {
    pub fn new(root: Option<Pgno>, shard: ShardId) -> Self {
        Self { root, shard }
    }

    pub fn root(&self) -> Option<Pgno> {
        self.root
    }

    pub fn shard(&self) -> ShardId {
        self.shard
    }

    /// A read view over the transaction, so pages written earlier in the same batch are
    /// visible. Returns `None` until the fragment has a root at all.
    pub fn reader<'t, P>(
        &self,
        txn: &'t WriteTxn<'_, P>,
    ) -> Option<crate::bitmap::read::FragmentRead<'t, WriteTxn<'t, P>>>
    where
        P: PagerMut + 't,
    {
        self.root.map(|r| crate::bitmap::read::FragmentRead::new(txn, r, self.shard))
    }

    fn existing<P: PagerMut>(
        &self,
        txn: &WriteTxn<'_, P>,
        ckey: ContainerKey,
    ) -> Result<Option<Container>> {
        match self.reader(txn) {
            None => Ok(None),
            Some(r) => r.raw_container(ckey),
        }
    }

    /// Merges a container into whatever is already at `ckey`.
    pub fn merge_container<P: PagerMut>(
        &mut self,
        txn: &mut WriteTxn<'_, P>,
        ckey: ContainerKey,
        c: ContainerRef<'_>,
    ) -> Result<()> {
        let merged = match self.existing(txn, ckey)? {
            Some(old) => apply(SetOp::Or, old.as_ref(), c).into_owned(),
            None => c.to_owned(),
        };
        self.write_container(txn, ckey, merged.as_ref())
    }

    /// Replaces whatever is at `ckey`, dropping the container when it ends up empty.
    pub fn write_container<P: PagerMut>(
        &mut self,
        txn: &mut WriteTxn<'_, P>,
        ckey: ContainerKey,
        c: ContainerRef<'_>,
    ) -> Result<()> {
        if c.is_empty() {
            return self.clear_container(txn, ckey);
        }
        self.root = Some(put_container(txn, self.root, ckey, c)?);
        Ok(())
    }

    pub fn clear_container<P: PagerMut>(
        &mut self,
        txn: &mut WriteTxn<'_, P>,
        ckey: ContainerKey,
    ) -> Result<()> {
        if let Some(root) = self.root {
            self.root = Some(remove(txn, root, ckey)?);
            self.collapse_if_empty(txn)?;
        }
        Ok(())
    }

    /// Frees the tree once its last container is gone, so the fragment has no root at all.
    ///
    /// `big_btree::remove` always leaves a root page standing - a tree is never headless, and
    /// the write path relies on that. But a fragment whose every bit has been cleared is not a
    /// small tree, it is an absent one: leaving the empty leaf behind holds a page and a root
    /// record that no query can ever reach and no reclaim can ever take. `DbWrite::commit`
    /// already drops the root record for a fragment whose root is `None`; this is what makes
    /// that branch reachable.
    fn collapse_if_empty<P: PagerMut>(&mut self, txn: &mut WriteTxn<'_, P>) -> Result<()> {
        let Some(root) = self.root else { return Ok(()) };
        let empty = {
            let page = txn.read(root)?;
            // Only a leaf can be empty: `remove` collapses the path above it, so a branch
            // still standing means there is something underneath it.
            page.page_type()? == PageType::Leaf && page.cell_count() == 0
        };
        if empty {
            big_btree::free_tree(txn, root)?;
            self.root = None;
        }
        Ok(())
    }

    /// Applies a whole batch of finished containers in one descent per leaf.
    ///
    /// Writing them one at a time rewrites the root-to-leaf path per container, so a batch
    /// whose containers share a leaf pays for that leaf once per container and leaves the
    /// rest behind as garbage. Every caller here already has the batch grouped and sorted,
    /// so there is no reason to hand them to the tree singly.
    fn apply_all<P: PagerMut>(
        &mut self,
        txn: &mut WriteTxn<'_, P>,
        containers: Vec<(ContainerKey, Container)>,
    ) -> Result<()> {
        // A container that emptied out leaves the tree instead of being stored empty. Removal
        // is the rare path, so it stays one at a time.
        let (live, empty): (Vec<_>, Vec<_>) =
            containers.into_iter().partition(|(_, c)| !c.as_ref().is_empty());
        for (ckey, _) in empty {
            self.clear_container(txn, ckey)?;
        }
        if live.is_empty() {
            // `clear_container` has already collapsed the tree if that emptied it.
            return Ok(());
        }
        let refs: Vec<(ContainerKey, ContainerRef<'_>)> =
            live.iter().map(|(k, c)| (*k, c.as_ref())).collect();
        self.root = Some(put_containers(txn, self.root, refs)?);
        Ok(())
    }

    /// The main write path: a batch of facts, grouped so each container is touched once.
    pub fn set_bits<P: PagerMut>(
        &mut self,
        txn: &mut WriteTxn<'_, P>,
        bits: impl IntoIterator<Item = (RowId, RecordId)>,
    ) -> Result<()> {
        // Every read here happens before any write, so all of them see the same pre-batch
        // tree. `group` yields each container key once, so no key is merged twice.
        let mut merged = Vec::new();
        for (ckey, c) in containers_of(group_offsets(bits)) {
            let m = match self.existing(txn, ckey)? {
                Some(old) => apply(SetOp::Or, old.as_ref(), c.as_ref()).into_owned(),
                None => c,
            };
            merged.push((ckey, m));
        }
        self.apply_all(txn, merged)
    }

    /// Sets and clears in one pass over the tree.
    ///
    /// A BSI write is exactly this shape: for each bit plane the bit is either set or cleared,
    /// never both. Running it as `clear_bits` then `set_bits` walks every container twice and,
    /// when the container is dense, rewrites its whole bitmap page twice for one logical
    /// change. Both halves land in the same transaction either way, so there is nothing a
    /// partial write could expose that combining them makes worse.
    pub fn write_bits<P: PagerMut>(
        &mut self,
        txn: &mut WriteTxn<'_, P>,
        set: impl IntoIterator<Item = (RowId, RecordId)>,
        clear: impl IntoIterator<Item = (RowId, RecordId)>,
    ) -> Result<()> {
        self.write_grouped(txn, group_offsets(set), group_offsets(clear))
    }

    /// The same write, for a caller that has already grouped its bits by container.
    ///
    /// **Split out because the pairs are the expensive part and some callers never need them.**
    /// A bit-sliced value knows the container and the offset of every bit it implies without
    /// building `(row, record)` first - see [`crate::bitmap::field::Bsi::group_all`] - and this
    /// is the door that lets it hand the answer straight over.
    pub fn write_grouped<P: PagerMut>(
        &mut self,
        txn: &mut WriteTxn<'_, P>,
        set: Grouped,
        clear: Grouped,
    ) -> Result<()> {
        let set = containers_of(set);
        let clear = containers_of(clear);
        let keys: BTreeSet<ContainerKey> = set.keys().chain(clear.keys()).copied().collect();

        let mut out = Vec::with_capacity(keys.len());
        for ckey in keys {
            let mut c = self.existing(txn, ckey)?.unwrap_or_else(Container::empty);
            if let Some(s) = set.get(&ckey) {
                c = apply(SetOp::Or, c.as_ref(), s.as_ref()).into_owned();
            }
            if let Some(k) = clear.get(&ckey) {
                c = apply(SetOp::AndNot, c.as_ref(), k.as_ref()).into_owned();
            }
            out.push((ckey, c));
        }
        self.apply_all(txn, out)
    }

    pub fn clear_bits<P: PagerMut>(
        &mut self,
        txn: &mut WriteTxn<'_, P>,
        bits: impl IntoIterator<Item = (RowId, RecordId)>,
    ) -> Result<()> {
        self.clear_grouped(txn, containers_of(group_offsets(bits)))
    }

    /// Removes `records` from every row this fragment holds.
    ///
    /// This is what deleting a record means at the storage layer, and it is deliberately
    /// blind to what kind of field the fragment belongs to. A bit-sliced index stores its
    /// value in rows, a set field stores its membership in rows, a mutex stores its shadow in
    /// rows: clearing a record from all of them erases it from every one of those without a
    /// single `match` on a field kind.
    ///
    /// The cost is one pass over the fragment's row list, which for a keyed field is
    /// proportional to the distinct values that shard has seen. That is the price of a set
    /// field having no reverse map, and it is why deleting in a batch is worth far more than
    /// deleting one record at a time.
    pub fn clear_records<P: PagerMut>(
        &mut self,
        txn: &mut WriteTxn<'_, P>,
        records: &[RecordId],
    ) -> Result<()> {
        if records.is_empty() {
            return Ok(());
        }
        // Scoped so the read borrow of `txn` ends before the write below needs it.
        let rows = match self.reader(txn) {
            Some(reader) => reader.rows()?,
            None => return Ok(()),
        };
        if rows.is_empty() {
            return Ok(());
        }

        // A record sits at the same slot and the same offset inside every row: `pos_of` adds
        // `row * SHARD_WIDTH`, and the shard width is a whole number of containers. So the
        // containers are built once against row zero and reused for every row, instead of
        // rebuilding `rows * records` pairs and grouping them again.
        let mut by_slot: BTreeMap<u64, Vec<u16>> = BTreeMap::new();
        for record in records {
            let pos = pos_of(0, *record);
            by_slot.entry(ckey_of(pos)).or_default().push(offset_in_container(pos));
        }
        let by_slot: Vec<(u64, Container)> =
            by_slot.into_iter().map(|(slot, v)| (slot, Container::from_values(v))).collect();

        let mut groups = BTreeMap::new();
        for row in rows {
            for (slot, c) in &by_slot {
                groups.insert(ckey_of_slot(row, *slot), c.clone());
            }
        }
        self.clear_grouped(txn, groups)
    }

    /// Subtracts a batch of already-grouped containers from whatever is stored.
    fn clear_grouped<P: PagerMut>(
        &mut self,
        txn: &mut WriteTxn<'_, P>,
        groups: BTreeMap<ContainerKey, Container>,
    ) -> Result<()> {
        let mut left = Vec::new();
        for (ckey, c) in groups {
            // A container that was never written has nothing to subtract, and materialising an
            // empty one would only make `apply_all` remove a key that is not there.
            let Some(old) = self.existing(txn, ckey)? else { continue };
            left.push((ckey, apply(SetOp::AndNot, old.as_ref(), c.as_ref()).into_owned()));
        }
        self.apply_all(txn, left)
    }

    /// Replaces a whole row. Slots absent from `set` are cleared, so this is a replace and not
    /// a merge.
    pub fn write_row<P: PagerMut>(
        &mut self,
        txn: &mut WriteTxn<'_, P>,
        row: RowId,
        set: &RowSet,
    ) -> Result<()> {
        // A row is sixteen consecutive container keys, which is exactly the case the batch
        // path exists for: they are contiguous, so they mostly share a leaf.
        let containers: Vec<(ContainerKey, Container)> = (0..CONTAINERS_PER_ROW)
            .map(|slot| {
                let c = set.get(slot).map_or_else(Container::empty, |c| c.to_owned());
                (ckey_of_slot(row, slot), c)
            })
            .collect();
        self.apply_all(txn, containers)
    }

    /// Single bit. Exists for tests and for filling gaps; the write amplification of using this
    /// as a real ingest path is about four orders of magnitude.
    pub fn set<P: PagerMut>(
        &mut self,
        txn: &mut WriteTxn<'_, P>,
        row: RowId,
        record: RecordId,
    ) -> Result<()> {
        self.set_bits(txn, [(row, record)])
    }

    pub fn clear<P: PagerMut>(
        &mut self,
        txn: &mut WriteTxn<'_, P>,
        row: RowId,
        record: RecordId,
    ) -> Result<()> {
        self.clear_bits(txn, [(row, record)])
    }
}

/// Groups facts by container so a batch touches each container exactly once.
///
/// **Fanned out above a threshold, because this is where a large commit spends its time.** A
/// bit-sliced write hands this one pair per plane per record — a twenty-bit field over a
/// million records is twenty-one million of them in one commit — and the per-bit work is
/// irreducible: a position, a container key, a map slot, a push. What it is not is *ordered*.
/// Chunk the batch, let each thread build its own map, and concatenate the offset lists per key
/// afterwards; because the chunks stay in order and every chunk keeps its own offsets in the
/// order they arrived, the concatenation is the same sequence the serial loop would have built,
/// so the containers this yields are identical rather than merely equivalent.
///
/// A sort was measured here first and is 4% *slower*: a commit holds tens of millions of pairs
/// but only a few hundred distinct container keys, so the map is three levels deep and lives in
/// cache, and `n log n` over the pairs buys nothing a probe was paying.
pub fn group_offsets(bits: impl IntoIterator<Item = (RowId, RecordId)>) -> Grouped {
    // `collect` on a `Vec`'s own iterator is a move, so the batch path allocates nothing here;
    // the one-bit helpers pay for a Vec of one, which is not a path anything hot goes down.
    let bits: Vec<(RowId, RecordId)> = bits.into_iter().collect();
    match fan_out(bits.len()) {
        1 => offsets_of(&bits),
        threads => offsets_of_parallel(&bits, threads),
    }
}

/// Offsets grouped by the container they belong to, in the order they arrived.
///
/// The intermediate form of every bitmap write. It is two bytes per bit where the `(row,
/// record)` pair it replaces is sixteen, which is the whole reason it is named and passed
/// around rather than being a local inside the grouping.
pub type Grouped = BTreeMap<ContainerKey, Vec<u16>>;

/// Folds one grouped batch into another, keeping each container's arrival order.
pub fn merge_grouped(into: &mut Grouped, from: Grouped) {
    for (ckey, mut offsets) in from {
        into.entry(ckey).or_default().append(&mut offsets);
    }
}

/// Builds the containers a grouped batch describes.
pub fn containers_of(g: Grouped) -> BTreeMap<ContainerKey, Container> {
    g.into_iter().map(|(k, v)| (k, Container::from_values(v))).collect()
}

/// One chunk's worth: container key to the offsets landing in it, in arrival order.
///
/// **A run at a time, not a bit at a time.** The map is small — a few hundred keys — but it was
/// being probed once per bit, tens of millions of times, and a probe is a descent and a compare.
/// Consecutive bits usually belong to the same container, so this holds the run it is building
/// and hands it over only when the key actually changes. What makes "usually" true is the order
/// the batch arrives in; see [`crate::bitmap::field::Bsi::bits_for_all`], which exists to produce it.
fn offsets_of(bits: &[(RowId, RecordId)]) -> Grouped {
    let mut by_ckey: BTreeMap<ContainerKey, Vec<u16>> = BTreeMap::new();
    let mut open: Option<ContainerKey> = None;
    let mut run: Vec<u16> = Vec::new();
    for (row, rec) in bits {
        let pos = pos_of(*row, *rec);
        let ckey = ckey_of(pos);
        if open != Some(ckey) {
            if let Some(prev) = open {
                by_ckey.entry(prev).or_default().append(&mut run);
            }
            open = Some(ckey);
        }
        run.push(offset_in_container(pos));
    }
    if let Some(prev) = open {
        by_ckey.entry(prev).or_default().append(&mut run);
    }
    by_ckey
}

/// How many threads are worth spawning for this many bits.
///
/// The threshold is high on purpose. Spawning costs tens of microseconds, and a commit that
/// carries a few thousand bits finishes this loop in less than that — the fan-out has to be
/// invisible to the small writes, which are most of them, or it is a regression dressed as an
/// optimisation.
pub(crate) fn fan_out(bits: usize) -> usize {
    const MIN_PER_THREAD: usize = 250_000;
    if bits < 2 * MIN_PER_THREAD {
        return 1;
    }
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    cores.min(bits / MIN_PER_THREAD).max(1)
}

/// The same map, built by several threads and stitched back together in chunk order.
fn offsets_of_parallel(bits: &[(RowId, RecordId)], threads: usize) -> Grouped {
    let chunk = bits.len().div_ceil(threads);
    // Scoped threads so the batch is borrowed rather than shared through an `Arc`, and nothing
    // outlives this call. No transaction is touched in here: this is arithmetic over a slice,
    // which is exactly why it is the half of a commit that can be split at all.
    let parts: Vec<Grouped> = std::thread::scope(|scope| {
        let handles: Vec<_> =
            bits.chunks(chunk).map(|part| scope.spawn(move || offsets_of(part))).collect();
        handles.into_iter().map(|h| h.join().expect("grouping a batch panicked")).collect()
    });

    let mut out = Grouped::new();
    for part in parts {
        merge_grouped(&mut out, part);
    }
    out
}
