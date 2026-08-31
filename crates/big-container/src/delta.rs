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

//! Bits changed since a dense container was last written whole.
//!
//! **The problem this exists for.** A dense container owns an 8 KiB page, and copy-on-write
//! rewrites all of it to flip one bit. That is bad on its own and much worse in a bit-sliced
//! index, where writing one record touches one container per bit plane: a twenty-bit field costs
//! twenty-one whole-page rewrites for a single value. The report's `where a commit's pages go`
//! section measures it - fifty-nine pages to write one record, of which two are the fixed chains
//! and the rest are this.
//!
//! **Why a delta fixes it and a smaller page would not.** The twenty-one containers a record
//! touches are twenty-one *different* pages but they sit in the same handful of *leaves*, because
//! their keys are `row * 16 + slot` and consecutive rows land next to each other. Moving the
//! change from the container's own page into its leaf cell therefore collapses twenty-one page
//! rewrites into one: the leaf was going to be rewritten anyway, since the cell's checksum
//! changes whatever happens.
//!
//! **What it costs.** A read has to apply the delta, and a delta that grows without bound would
//! turn every read into a merge. So it is capped, and folding a full delta back into a fresh base
//! page is the amortised cost - one whole-page write per `MAX_DELTA` changes rather than per
//! change. A *point* read pays nothing: at most `MAX_DELTA` sorted entries are searched before
//! the base word is touched, and nothing is copied.
//!
//! **On its own this bought almost nothing, and the reason is worth knowing.** With the delta
//! working exactly as designed - nineteen or twenty deltas kept per commit against a third of a
//! fold - a one-record commit still wrote forty-four pages instead of five. The containers were
//! sharing a leaf in theory and not in practice: the b-tree splits leaves and never merged them,
//! so a fragment whose containers had once been large kept one cell per leaf forever, and each
//! of those leaves cost a rewrite and a root-to-leaf path. Making the cells small did not put
//! them back together. See `merge_right` in `big-btree`; the two changes are only worth anything
//! as a pair.

/// One changed bit: its offset in the container, and what it was changed to.
///
/// Four bytes rather than two-plus-a-bitmask because every one of the sixteen bits of an offset
/// is meaningful - a container holds 65,536 of them - so there is nowhere to steal a flag from,
/// and a parallel bitmask would need its own length and its own bounds check.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DeltaEntry {
    pub offset: u16,
    pub set: bool,
}

/// Bytes one entry occupies on a page.
pub const DELTA_ENTRY_BYTES: usize = 4;

/// How many changes a container carries before it is written whole again.
///
/// The trade is a read cost against a write cost, and it is deliberately small. At thirty-two, a
/// delta cell's payload is 128 bytes - so a leaf still holds a useful number of them - and the
/// amortised write cost of a dense container falls from one 8 KiB page per change to one per
/// thirty-two. Raising it makes writes cheaper and every read of that container slower, and the
/// reads are the ones with no upper bound on how often they happen.
pub const MAX_DELTA: usize = 32;

/// Encodes entries into a page payload.
///
/// There is deliberately no "merge into an existing delta". A delta is always recomputed as
/// `new XOR base`, which means it carries no history and cannot drift: whatever the old delta
/// said, the new one is a plain subtraction against the page as written. A merge would be a
/// second way to arrive at the same list, with its own way of being wrong.
///
/// Sorted by offset, and later entries win. Both matter: sorted gives a read a binary search
/// instead of a scan, and last-write-wins is what makes a bit that was set and then cleared in
/// the same batch end up clear.
pub fn encode(entries: &[DeltaEntry]) -> Vec<u8> {
    let mut out = Vec::with_capacity(entries.len() * DELTA_ENTRY_BYTES);
    for e in entries {
        out.extend_from_slice(&e.offset.to_le_bytes());
        out.extend_from_slice(&u16::from(e.set).to_le_bytes());
    }
    out
}

/// Decodes a payload written by [`encode`]. Returns `None` if it is not a whole number of
/// entries, which means the page is damaged rather than merely old.
pub fn decode(payload: &[u8]) -> Option<Vec<DeltaEntry>> {
    if !payload.len().is_multiple_of(DELTA_ENTRY_BYTES) {
        return None;
    }
    let mut out = Vec::with_capacity(payload.len() / DELTA_ENTRY_BYTES);
    for c in payload.as_chunks::<DELTA_ENTRY_BYTES>().0 {
        out.push(DeltaEntry {
            offset: u16::from_le_bytes([c[0], c[1]]),
            // Anything other than zero reads as set, so a future writer that puts a richer flag
            // here degrades to "changed" rather than to a wrong answer.
            set: u16::from_le_bytes([c[2], c[3]]) != 0,
        });
    }
    Some(out)
}

/// Whether `offset` is set, given a base bitmap and a delta over it.
///
/// The point read path. A delta of at most thirty-two entries is searched before the base word
/// is touched, so a point read never materialises anything.
pub fn contains(base: &[u64; crate::BITMAP_WORDS], delta: &[DeltaEntry], offset: u16) -> bool {
    if let Ok(i) = delta.binary_search_by_key(&offset, |e| e.offset) {
        return delta[i].set;
    }
    base[offset as usize / 64] >> (offset % 64) & 1 == 1
}

/// Applies a delta to a copy of the base.
///
/// Deliberately does **not** report the resulting cardinality. The only caller that materialises
/// a container is a read, and a read already has the effective cardinality cached in the cell -
/// so returning it here would mean popcounting the whole base page, once per read, to recompute
/// a number that was already on the page above it.
pub fn apply(
    base: &[u64; crate::BITMAP_WORDS],
    delta: &[DeltaEntry],
) -> Box<[u64; crate::BITMAP_WORDS]> {
    let mut words = Box::new(*base);
    for e in delta {
        let (w, bit) = (e.offset as usize / 64, e.offset % 64);
        if e.set {
            words[w] |= 1u64 << bit;
        } else {
            words[w] &= !(1u64 << bit);
        }
    }
    words
}

/// The cardinality a delta produces, without building the bitmap.
///
/// Needed when a cell is written: the cell caches its own cardinality so that counting never
/// reads a payload, and that number has to be right before the page is sealed.
pub fn cardinality_after(
    base: &[u64; crate::BITMAP_WORDS],
    base_cardinality: u32,
    delta: &[DeltaEntry],
) -> u32 {
    let mut n = i64::from(base_cardinality);
    for e in delta {
        let was = base[e.offset as usize / 64] >> (e.offset % 64) & 1 == 1;
        if was != e.set {
            n += if e.set { 1 } else { -1 };
        }
    }
    n.max(0) as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::BITMAP_WORDS;

    fn base() -> [u64; BITMAP_WORDS] {
        let mut b = [0u64; BITMAP_WORDS];
        // Bits 0, 64 and 129, so the tests cross word boundaries rather than staying in word 0.
        b[0] = 1;
        b[1] = 1;
        b[2] = 2;
        b
    }

    #[test]
    fn a_delta_round_trips_through_a_payload() {
        let d = vec![
            DeltaEntry { offset: 0, set: false },
            DeltaEntry { offset: 7, set: true },
            DeltaEntry { offset: 65_535, set: true },
        ];
        assert_eq!(decode(&encode(&d)).unwrap(), d);
    }

    #[test]
    fn a_truncated_payload_is_refused() {
        assert!(decode(&[0u8; 3]).is_none());
        assert!(decode(&[0u8; 5]).is_none());
        assert_eq!(decode(&[]), Some(Vec::new()));
    }

    #[test]
    fn applying_agrees_with_asking_one_bit_at_a_time() {
        let b = base();
        let delta = vec![
            DeltaEntry { offset: 0, set: false },
            DeltaEntry { offset: 5, set: true },
            DeltaEntry { offset: 64, set: false },
            DeltaEntry { offset: 129, set: true },
        ];
        let words = apply(&b, &delta);
        for offset in [0u16, 5, 64, 129, 200] {
            assert_eq!(
                words[offset as usize / 64] >> (offset % 64) & 1 == 1,
                contains(&b, &delta, offset),
                "offset {offset}"
            );
        }
        // Started with three bits {0, 64, 129}: cleared 0 and 64, set 5, and set 129 which was
        // already set. Two removed, one added.
        assert_eq!(cardinality_after(&b, 3, &delta), 2);
    }

    #[test]
    fn a_delta_that_changes_nothing_changes_no_count() {
        // Setting a bit that is already set, and clearing one that is already clear. Both are
        // legitimate - a caller writing the same record twice produces exactly this - and both
        // must leave the cardinality alone, or the cached count drifts from the container.
        let b = base();
        let delta =
            vec![DeltaEntry { offset: 0, set: true }, DeltaEntry { offset: 900, set: false }];
        assert_eq!(cardinality_after(&b, 3, &delta), 3);
    }

    #[test]
    fn the_cardinality_never_goes_negative() {
        // It cannot happen from a correct base, but the base comes off a disk: a cell whose
        // cached cardinality is too low must not turn into a huge unsigned number.
        let b = [0u64; BITMAP_WORDS];
        let delta = vec![DeltaEntry { offset: 0, set: false }];
        assert_eq!(cardinality_after(&b, 0, &delta), 0);
    }
}
