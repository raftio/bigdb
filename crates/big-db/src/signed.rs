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

//! The sign convention, which `Bsi` deliberately does not have.
//!
//! **Offset binary.** A value is stored as `value + 2^(depth-1)`, so the representable range
//! `[-2^(depth-1), 2^(depth-1) - 1]` maps onto `[0, 2^depth - 1]` and nothing below this module
//! ever sees a negative number.
//!
//! The reason it is this and not two's complement is one property: **the map is monotonic.**
//! `a < b` if and only if `encode(a) < encode(b)`. Every range query, every zone map, every
//! plane-by-plane narrowing in `Bsi::extreme` therefore works on the stored value unchanged -
//! there is no signed variant of any of them, and no `match` on field kind anywhere below the
//! boundary. Two's complement would have put the sign bit at the top with the wrong polarity and
//! made `-1 > 1`, which would have meant a signed copy of the whole comparison path.
//!
//! Two consequences worth knowing rather than discovering:
//!
//! - **The bias comes from the field's declared depth, never from a fragment's.** A fragment's
//!   `bit_depth` grows as wider values arrive; if the bias moved with it, the same value would
//!   encode differently in two fragments and ordering across shards would be nonsense.
//! - **A signed field always uses its full declared depth.** The top plane is exactly the sign
//!   bit - set for every non-negative value, clear for every negative one - so it is never
//!   absent. That is inherent to offset binary, not a defect: the plane carries a real bit of
//!   information for every record.

/// The offset added to every stored value: `2^(depth-1)`.
pub fn bias(bit_depth: u32) -> u64 {
    debug_assert!((1..=64).contains(&bit_depth), "a signed field needs at least a sign bit");
    1u64 << (bit_depth.clamp(1, 64) - 1)
}

/// Largest value a signed field of this depth can hold: `2^(depth-1) - 1`.
pub fn max_value(bit_depth: u32) -> i64 {
    (bias(bit_depth) - 1) as i64
}

/// Smallest value a signed field of this depth can hold: `-2^(depth-1)`.
pub fn min_value(bit_depth: u32) -> i64 {
    // Negated as `i128` because `-(1 << 63)` is representable as an `i64` but `1 << 63` is not,
    // so negating after the cast is the only order that does not overflow at the full width.
    -(bias(bit_depth) as i128) as i64
}

pub fn fits(value: i64, bit_depth: u32) -> bool {
    (min_value(bit_depth)..=max_value(bit_depth)).contains(&value)
}

/// A value as it is stored. `None` when it does not fit the declared depth.
///
/// `wrapping_add` rather than a checked cast through `i128`: for every depth up to 64 the two
/// agree, and at depth 64 the wrap is the whole point - it is the standard "flip the sign bit"
/// trick, which `i128` arithmetic would only reach by way of a cast back.
pub fn encode(value: i64, bit_depth: u32) -> Option<u64> {
    fits(value, bit_depth).then(|| (value as u64).wrapping_add(bias(bit_depth)))
}

/// A stored value read back as the number it stands for.
pub fn decode(stored: u64, bit_depth: u32) -> i64 {
    stored.wrapping_sub(bias(bit_depth)) as i64
}

/// A comparison threshold as it is stored, saturated to the field's range.
///
/// Saturating rather than refusing, because a threshold outside the range is a **legitimate**
/// query with an obvious answer - `> 10_000` against a field that cannot hold more than 127
/// matches nothing, and `>= -10_000` matches everything. Rejecting it would make a client check
/// a schema before it could ask a question whose answer does not depend on one.
pub fn encode_bound(value: i64, bit_depth: u32) -> u64 {
    encode(value.clamp(min_value(bit_depth), max_value(bit_depth)), bit_depth)
        .expect("clamped into range")
}

/// Whether a threshold sits outside the field entirely, and on which side.
///
/// A caller needs this to answer `> max` and `< min` correctly: both clamp to the same stored
/// bound as the extreme value itself, which would otherwise turn "greater than everything" into
/// "greater than or equal to the largest", and match a record.
pub fn out_of_range(value: i64, bit_depth: u32) -> Option<std::cmp::Ordering> {
    if value < min_value(bit_depth) {
        Some(std::cmp::Ordering::Less)
    } else if value > max_value(bit_depth) {
        Some(std::cmp::Ordering::Greater)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_map_round_trips() {
        for depth in [1u32, 2, 8, 20, 37, 63, 64] {
            // Filtered rather than hand-listed per depth: at depth 1 the whole range is
            // `[-1, 0]`, so `1` is not a value that should round-trip and asserting it would be
            // testing the probe list rather than the map.
            for v in [min_value(depth), -1, 0, 1, max_value(depth)] {
                let Some(e) = encode(v, depth) else { continue };
                assert_eq!(decode(e, depth), v, "depth {depth}, value {v}");
            }
        }
    }

    #[test]
    fn the_map_is_monotonic() {
        // The property everything else rests on. If this fails, every range query on a signed
        // field is wrong and no other test in the tree would say so.
        for depth in [2u32, 8, 20, 64] {
            let lo = min_value(depth);
            let hi = max_value(depth);
            let mut probes: Vec<i64> = [lo, lo + 1, -2, -1, 0, 1, 2, hi - 1, hi]
                .into_iter()
                .filter(|v| fits(*v, depth))
                .collect();
            probes.sort();
            probes.dedup();
            for w in probes.windows(2) {
                let (a, b) = (w[0], w[1]);
                assert!(
                    encode(a, depth).unwrap() < encode(b, depth).unwrap(),
                    "depth {depth}: {a} < {b} but their encodings are not ordered"
                );
            }
        }
    }

    #[test]
    fn the_range_is_the_declared_one() {
        assert_eq!((min_value(8), max_value(8)), (-128, 127));
        assert_eq!((min_value(1), max_value(1)), (-1, 0));
        assert_eq!((min_value(64), max_value(64)), (i64::MIN, i64::MAX));
        assert!(!fits(128, 8));
        assert!(!fits(-129, 8));
        assert!(fits(i64::MIN, 64));
        assert!(fits(i64::MAX, 64));
    }

    #[test]
    fn the_widest_field_is_the_sign_bit_flipped() {
        // At depth 64 offset binary and "flip the top bit" are the same operation, and the
        // encoding must not overflow on the way there.
        assert_eq!(encode(0, 64), Some(1u64 << 63));
        assert_eq!(encode(i64::MIN, 64), Some(0));
        assert_eq!(encode(i64::MAX, 64), Some(u64::MAX));
    }

    #[test]
    fn the_sign_lives_in_the_top_plane() {
        // Stated as a test because the storage cost depends on it: every non-negative value
        // sets the highest plane, so a signed field never gets to be narrower than it declared.
        for depth in [8u32, 20, 64] {
            let top = 1u64 << (depth - 1);
            assert!(encode(0, depth).unwrap() & top != 0);
            assert!(encode(max_value(depth), depth).unwrap() & top != 0);
            assert!(encode(-1, depth).unwrap() & top == 0);
            assert!(encode(min_value(depth), depth).unwrap() & top == 0);
        }
    }

    #[test]
    fn a_bound_outside_the_range_clamps_and_says_so() {
        assert_eq!(encode_bound(1_000, 8), encode(127, 8).unwrap());
        assert_eq!(encode_bound(-1_000, 8), encode(-128, 8).unwrap());
        assert_eq!(out_of_range(1_000, 8), Some(std::cmp::Ordering::Greater));
        assert_eq!(out_of_range(-1_000, 8), Some(std::cmp::Ordering::Less));
        assert_eq!(out_of_range(0, 8), None);
    }
}
