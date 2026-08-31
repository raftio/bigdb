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

//! The block format, round-tripped.
//!
//! Everything above this layer trusts that a block decodes to what was encoded, so this is the
//! test that has to be exhaustive rather than representative: the codec is chosen per block by
//! measurement, which means a block anybody writes can take any of the four paths and the one a
//! test happens to hit is not the one production will.

use big_column::block::{Part, PartRef, NULL_BYTES};
use big_column::*;
use proptest::prelude::*;

/// Encodes and decodes, going through the same inline-or-page split a real write does.
fn round_trip(block: &Block) -> Block {
    let parts = block.encode().unwrap();
    if parts.is_empty() {
        return Block::new();
    }
    let owned: Vec<(Vec<u8>, Vec<u8>)> = parts
        .iter()
        .map(|p: &Part| (p.inline.clone(), p.page.clone().unwrap_or_default()))
        .collect();
    let refs: Vec<PartRef<'_>> =
        owned.iter().map(|(i, pg)| PartRef { header: i, body: pg }).collect();
    Block::decode(&refs).unwrap()
}

#[test]
fn a_block_of_nulls_encodes_to_nothing() {
    let block = Block::new();
    assert!(block.encode().unwrap().is_empty(), "an absent block and a block of nulls are one");
    assert_eq!(block.present(), 0);
}

#[test]
fn a_scalar_block_is_always_one_cell() {
    // The widest a scalar block can be: every slot full, values spread over the whole range so
    // nothing but `Plain` can encode it. This is the case the block size was chosen for.
    let mut block = Block::new();
    for i in 0..BLOCK_RECORDS as usize {
        block.set(i, Cell::Value((i as u64).wrapping_mul(0x0123_4567_89AB_CDEF)));
    }
    let parts = block.encode().unwrap();
    assert_eq!(parts.len(), 1, "a scalar block never needs a second cell");
    assert!(parts[0].page.is_some(), "the widest block spills to a page");
    assert_eq!(parts[0].page.as_ref().unwrap().len(), PAGE_BYTES);
    assert_eq!(round_trip(&block), block);
}

/// A block small enough to stay in the cell costs no page at all: the descent that found it has
/// already read it. This is the common case and worth pinning.
#[test]
fn a_narrow_block_stays_inside_the_cell() {
    let mut block = Block::new();
    for i in 0..BLOCK_RECORDS as usize {
        block.set(i, Cell::Value((i % 2) as u64));
    }
    let parts = block.encode().unwrap();
    assert!(parts[0].page.is_none(), "a one-bit column should not need a page");
    assert!(parts[0].inline.len() < 300, "got {} bytes", parts[0].inline.len());
    assert_eq!(round_trip(&block), block);
}

#[test]
fn a_list_block_spills_into_parts_and_comes_back_whole() {
    let mut block = Block::new();
    // Enough values that the flat run needs several value parts.
    for i in 0..BLOCK_RECORDS as usize {
        let n = i % 7;
        if n > 0 {
            block.set(i, Cell::List((0..n as u64).map(|k| k * 1000 + i as u64).collect()));
        }
    }
    let parts = block.encode().unwrap();
    assert!(parts.len() > 2, "expected counts plus several value parts, got {}", parts.len());
    assert_eq!(round_trip(&block), block);
}

/// A record holding nothing and a record holding an empty list are the same absence. Storing
/// them differently would make `Project` answer `[]` for one and `null` for the other, which is
/// a distinction nothing above can act on.
#[test]
fn an_empty_list_is_the_same_absence_as_a_null() {
    let mut block = Block::new();
    block.set(0, Cell::List(Vec::new()));
    block.set(1, Cell::Null);
    assert!(block.encode().unwrap().is_empty());
    // Normalised on the way in, so the type has one spelling for absence rather than two.
    // Without this a decoded block could never be compared against the one that was written.
    assert_eq!(block.get(0), &Cell::Null);
}

/// The cached count is what makes `count_values` cost a descent and no payload, so it has to
/// agree with what the block actually holds.
#[test]
fn the_cached_count_agrees_with_the_slots() {
    let mut block = Block::new();
    for i in (0..BLOCK_RECORDS as usize).step_by(3) {
        block.set(i, Cell::Value(i as u64));
    }
    let parts = block.encode().unwrap();
    assert_eq!(parts[0].present, block.present());
    assert_eq!(block.present(), BLOCK_RECORDS.div_ceil(3) as u32);
}

/// A part whose header claims a shape or an encoding this build has no code for is refused.
/// Reading it as the nearest thing would answer with numbers that are not the ones stored.
#[test]
fn a_header_from_a_newer_build_is_refused() {
    let mut block = Block::new();
    block.set(0, Cell::Value(1));
    let parts = block.encode().unwrap();

    for (byte, bad) in [(0usize, 9u8), (1, 9)] {
        let mut inline = parts[0].inline.clone();
        inline[byte] = bad;
        let refs = [PartRef { header: &inline, body: &[] }];
        assert!(Block::decode(&refs).is_err(), "byte {byte} = {bad} should not decode");
    }
}

/// A cell whose payload has been cut short is refused rather than padded with zeroes, which
/// would read back as a block full of legitimate-looking values that were never written.
#[test]
fn a_truncated_part_is_refused() {
    let mut block = Block::new();
    for i in 0..64 {
        block.set(i, Cell::Value(i as u64 * 7919));
    }
    let parts = block.encode().unwrap();
    let full = &parts[0].inline;
    let cut = &full[..full.len() - 4];
    let refs = [PartRef { header: cut, body: &[] }];
    assert!(Block::decode(&refs).is_err());
}

/// A block that begins with a value part is a segment whose first cell has been lost. Every
/// record in it would read back short, so it is refused rather than partially answered.
#[test]
fn a_block_missing_its_first_part_is_refused() {
    let mut block = Block::new();
    block.set(0, Cell::List(vec![1, 2, 3]));
    let parts = block.encode().unwrap();
    assert!(parts.len() >= 2);
    let refs = [PartRef { header: &parts[1].inline, body: &[] }];
    assert!(Block::decode(&refs).is_err());
}

#[test]
fn the_null_bitmap_covers_exactly_one_block() {
    assert_eq!(NULL_BYTES * 8, BLOCK_RECORDS as usize);
}

proptest! {
    /// Any arrangement of present and absent scalars survives. The codec is picked by
    /// measuring, so the shape of the data decides which of the four paths this exercises -
    /// which is exactly why it is generated rather than written out.
    #[test]
    fn any_scalar_block_survives(
        cells in prop::collection::vec(prop::option::of(any::<u64>()), 0..300)
    ) {
        let mut block = Block::new();
        for (i, c) in cells.iter().enumerate() {
            block.set(i, c.map_or(Cell::Null, Cell::Value));
        }
        prop_assert_eq!(round_trip(&block), block);
    }

    /// The same for lists, including the empty ones that mean absent.
    #[test]
    fn any_list_block_survives(
        cells in prop::collection::vec(prop::collection::vec(any::<u64>(), 0..6), 0..200)
    ) {
        let mut block = Block::new();
        for (i, c) in cells.iter().enumerate() {
            block.set(i, Cell::List(c.clone()));
        }
        prop_assert_eq!(round_trip(&block), block);
    }

    /// Values near the ends of the range are where a frame-of-reference encoder is most likely
    /// to overflow the subtraction it is built on.
    #[test]
    fn extremes_survive(lo in 0u64..4, hi in (u64::MAX - 4)..=u64::MAX) {
        let mut block = Block::new();
        block.set(0, Cell::Value(lo));
        block.set(1, Cell::Value(hi));
        block.set(2, Cell::Value(u64::MAX / 2));
        prop_assert_eq!(round_trip(&block), block);
    }
}

/// A count read off the disk is a number nothing above has bounded, so the running offset it
/// feeds must be added with a check.
///
/// **Unchecked, this was a panic rather than a refusal.** In a debug build the addition
/// overflows outright; in a release build it wraps to a small end, slips past the length test,
/// and panics on a backwards slice. A decode of stored bytes has to refuse - the same contract
/// `big-page`'s own parser holds, and the reason it has a `parse_never_panics` test.
#[test]
fn a_corrupt_count_is_refused_rather_than_overflowing() {
    // Two present slots, Plain-encoded counts of 2 and `u64::MAX - 1`. The first consumes two
    // values, so the second addition is the one that goes over.
    let mut counts = vec![0u8; 16 + NULL_BYTES];
    counts[0] = 1; // Shape::Counts
    counts[1] = 3; // Codec::Plain
    counts[2..4].copy_from_slice(&2u16.to_le_bytes());
    counts[16] = 0b0000_0011; // slots 0 and 1 present
    counts.extend_from_slice(&2u64.to_le_bytes());
    counts.extend_from_slice(&(u64::MAX - 1).to_le_bytes());

    let mut values = vec![0u8; 16];
    values[0] = 2; // Shape::Values
    values[1] = 3; // Codec::Plain
    values[2..4].copy_from_slice(&2u16.to_le_bytes());
    values.extend_from_slice(&7u64.to_le_bytes());
    values.extend_from_slice(&8u64.to_le_bytes());

    let refs = [PartRef { header: &counts, body: &[] }, PartRef { header: &values, body: &[] }];
    assert!(Block::decode(&refs).is_err());
}
