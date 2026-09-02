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

//! The float convention, which `Bsi` deliberately does not have either.
//!
//! **Order-preserving bits.** A float is stored as its IEEE-754 bits with the sign bit flipped
//! when it is positive and every bit flipped when it is negative. That maps the whole of `f64`
//! onto `[0, 2^64)` with one property, which is the same property [`crate::signed`] exists for:
//! **the map is monotonic.** `a < b` if and only if `encode(a) < encode(b)`.
//!
//! Every range query, every zone map and every plane-by-plane narrowing in `Bsi::extreme`
//! therefore works on the stored value unchanged. `min` and `max` over a float column need no
//! code of their own at all: the extreme of the encodings *is* the encoding of the extreme.
//!
//! The raw bits do not have that property, which is what the design note in the SQL cheatsheet
//! meant by saying a bit-sliced float is monotonic only for positive numbers. Raw, negatives run
//! backwards and sort above every positive. Flipping fixes both halves at once, and this module
//! is the one place that knows it.
//!
//! # What this does not buy
//!
//! **`sum` cannot use the bit planes.** A BSI sums by weighting each plane by its power of two,
//! which is a sum of the *stored* numbers - and that is only the sum of the real ones when the
//! encoding is affine, as the offset-binary one in [`crate::signed`] is. This one is not, so
//! `sum(encode(v))` means nothing. A total over a float column is a scan, and the query layer
//! routes it to one.
//!
//! # Three values that need a decision
//!
//! - **NaN is refused on write.** A value with no position in an order has no place in an
//!   ordered index: sorting it first, last, or nowhere is a wrong answer to some query, and the
//!   one that is never wrong is not storing it.
//! - **`-0.0` is stored as `+0.0`.** Raw IEEE total order puts them apart, so without this
//!   `WHERE f = 0.0` would miss a row written `-0.0`. SQL says they are one value.
//! - **Infinities are stored.** They encode at the two ends and compare the way they should, and
//!   they are why a float field has no "outside its range": a threshold past `f32::MAX` is not
//!   clamped into a wrong answer, it just selects the infinities. See [`encode_bound`].

use big_engine::bitmap::field::RangeOp;

const SIGN64: u64 = 1 << 63;
const SIGN32: u32 = 1 << 31;

/// Whether a field of this depth holds single-precision values. 32 planes is an `f32` and 64 is
/// an `f64`; nothing else is a float field.
fn is_single(bit_depth: u32) -> bool {
    bit_depth <= 32
}

/// A value as it is stored. `None` when the field cannot hold it - a NaN, or a magnitude past
/// the range of a single-precision field.
///
/// Rounds to the field's precision otherwise, which is what writing `0.1` into a `FLOAT` has
/// always meant. Refusing every value that is not exactly representable would refuse `0.1`.
pub fn encode(value: f64, bit_depth: u32) -> Option<u64> {
    if value.is_nan() {
        return None;
    }
    // `+ 0.0` rather than a comparison against zero: it maps `-0.0` to `+0.0` and leaves every
    // other value, infinities included, exactly as it was.
    let value = value + 0.0;
    if is_single(bit_depth) {
        let narrow = value as f32;
        // A finite value that overflowed to an infinity is out of the field's range, and is the
        // same mistake as writing 300 into a `TINYINT`. An infinity that was already one is not.
        if narrow.is_infinite() && value.is_finite() {
            return None;
        }
        let bits = narrow.to_bits();
        Some(u64::from(if bits & SIGN32 != 0 { !bits } else { bits ^ SIGN32 }))
    } else {
        let bits = value.to_bits();
        Some(if bits & SIGN64 != 0 { !bits } else { bits ^ SIGN64 })
    }
}

/// A stored value read back as the number it stands for.
pub fn decode(stored: u64, bit_depth: u32) -> f64 {
    if is_single(bit_depth) {
        let stored = stored as u32;
        let bits = if stored & SIGN32 != 0 { stored ^ SIGN32 } else { !stored };
        f64::from(f32::from_bits(bits))
    } else {
        let bits = if stored & SIGN64 != 0 { stored ^ SIGN64 } else { !stored };
        f64::from_bits(bits)
    }
}

/// What a comparison threshold becomes once rounded to what the field can hold.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Bound {
    /// Compare against this stored value, with the operator as it was written.
    At(u64),
    /// The comparison is already decided, without reading a single bitmap.
    Always(bool),
}

/// A threshold as it is stored, rounded so that the answer is the one the written number asks
/// for rather than the one the field's precision would have given.
///
/// **The rounding has a direction, and it is not "nearest".** `WHERE temp > 3.14` against a
/// single-precision column asks for every value above 3.14; 3.14 is not an `f32`, so the
/// question becomes one about the two `f32`s either side of it. Rounding to nearest could land
/// above the real threshold and drop a row that satisfies it. Rounding *down* for `>` and `>=`
/// and *up* for `<` and `<=` cannot: there is no value of the field strictly between the written
/// number and the neighbour chosen, so the two comparisons select the same records.
///
/// Equality is the case where precision is visible to the caller: a value the field cannot hold
/// exactly is a value no record can be holding, so `= 3.14` on an `f32` column answers nothing
/// rather than answering about 3.1400001049041748.
///
/// There is no clamping here and no `out_of_range` companion, because a float field has no
/// outside. A threshold past `f32::MAX` rounds to `f32::MAX` or to an infinity, and either way
/// the comparison that follows is the true one - the infinities are storable values and they are
/// exactly the records such a threshold is asking about.
pub fn encode_bound(value: f64, bit_depth: u32, op: RangeOp) -> Bound {
    // Nothing compares true with a NaN, and `!=` is the one that inverts to true. This is the
    // three-valued answer collapsed to two, which is what the rest of this engine does with a
    // comparison against a value that is not there.
    if value.is_nan() {
        return Bound::Always(matches!(op, RangeOp::Ne));
    }
    let value = value + 0.0;
    let rounded = match op {
        RangeOp::Gt | RangeOp::Ge => floor_to_field(value, bit_depth),
        RangeOp::Lt | RangeOp::Le => ceil_to_field(value, bit_depth),
        RangeOp::Eq | RangeOp::Ne => {
            let near = floor_to_field(value, bit_depth);
            if near != value {
                return Bound::Always(matches!(op, RangeOp::Ne));
            }
            near
        }
    };
    match encode(rounded, bit_depth) {
        Some(stored) => Bound::At(stored),
        // Unreachable: `rounded` came out of the field's own value set. Answered rather than
        // asserted, because a bound that cannot be encoded excludes nothing and matches
        // everything, and that is the pair of answers a `match` here has to give.
        None => Bound::Always(matches!(op, RangeOp::Ne)),
    }
}

/// The largest value the field can hold that is not above `value`.
pub fn floor_to_field(value: f64, bit_depth: u32) -> f64 {
    if !is_single(bit_depth) {
        return value;
    }
    let narrow = f64::from(value as f32);
    if narrow > value {
        f64::from((value as f32).next_down())
    } else {
        narrow
    }
}

/// The smallest value the field can hold that is not below `value`.
pub fn ceil_to_field(value: f64, bit_depth: u32) -> f64 {
    if !is_single(bit_depth) {
        return value;
    }
    let narrow = f64::from(value as f32);
    if narrow < value {
        f64::from((value as f32).next_up())
    } else {
        narrow
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Values that between them cover every case the encoding has: both signs, both zeroes, the
    /// subnormal boundary, the extremes, and the infinities.
    const PROBES: [f64; 11] = [
        f64::NEG_INFINITY,
        f64::MIN,
        -1.5,
        -f64::MIN_POSITIVE,
        -0.0,
        0.0,
        f64::MIN_POSITIVE,
        1.5,
        std::f64::consts::PI,
        f64::MAX,
        f64::INFINITY,
    ];

    #[test]
    fn the_map_round_trips() {
        for v in PROBES {
            let e = encode(v, 64).expect("not a NaN");
            // `-0.0` is the one value that does not come back as itself, and that is the point:
            // it is stored as `+0.0` because SQL has one zero.
            let want = if v == 0.0 { 0.0 } else { v };
            assert_eq!(decode(e, 64), want, "{v}");
        }
    }

    #[test]
    fn the_map_is_monotonic() {
        // The property everything else rests on. If this fails, every range query on a float
        // field is wrong, every zone map prunes a fragment that had the answer in it, and no
        // other test in the tree would say so.
        let mut sorted: Vec<f64> = PROBES.into_iter().filter(|v| *v != 0.0).collect();
        sorted.push(0.0);
        sorted.sort_by(|a, b| a.partial_cmp(b).expect("no NaN in the probes"));
        for w in sorted.windows(2) {
            let (a, b) = (w[0], w[1]);
            assert!(
                encode(a, 64).unwrap() < encode(b, 64).unwrap(),
                "{a} < {b} but their encodings are not ordered"
            );
        }
    }

    #[test]
    fn single_precision_is_monotonic_too() {
        let mut sorted: Vec<f64> =
            [f64::NEG_INFINITY, -3.5, -1.0, 0.0, 1.0, 3.5, f64::from(f32::MAX), f64::INFINITY]
                .to_vec();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
        for w in sorted.windows(2) {
            assert!(
                encode(w[0], 32).unwrap() < encode(w[1], 32).unwrap(),
                "{} < {} but their encodings are not ordered",
                w[0],
                w[1]
            );
        }
    }

    #[test]
    fn the_two_zeroes_are_one_value() {
        // Without this `WHERE f = 0.0` misses a row written `-0.0`, which is a wrong answer
        // rather than a rounding difference.
        assert_eq!(encode(-0.0, 64), encode(0.0, 64));
        assert_eq!(encode(-0.0, 32), encode(0.0, 32));
        assert_eq!(decode(encode(-0.0, 64).unwrap(), 64), 0.0);
    }

    #[test]
    fn a_nan_is_not_storable() {
        assert_eq!(encode(f64::NAN, 64), None);
        assert_eq!(encode(-f64::NAN, 32), None);
    }

    #[test]
    fn a_value_too_wide_for_the_field_is_refused() {
        // The same refusal 300 into a `TINYINT` gets, and it must not be confused with storing
        // an infinity that was written as one.
        assert_eq!(encode(1e300, 32), None);
        assert_eq!(encode(-1e300, 32), None);
        assert!(encode(f64::INFINITY, 32).is_some());
        assert!(encode(1e300, 64).is_some());
    }

    #[test]
    fn single_precision_rounds_a_written_value() {
        // Refusing everything not exactly representable would refuse `0.1`.
        let e = encode(0.1, 32).expect("0.1 is writable");
        assert_eq!(decode(e, 32), f64::from(0.1f32));
    }

    #[test]
    fn a_bound_rounds_away_from_the_records_it_must_not_drop() {
        // 0.7 is not an f32. The f32 either side of it are what the question becomes about.
        let below = floor_to_field(0.7, 32);
        let above = ceil_to_field(0.7, 32);
        assert!(below < 0.7 && 0.7 < above, "{below} < 0.7 < {above}");
        assert_eq!((above as f32).next_down(), below as f32, "they must be neighbours");

        // `> 0.7` rounds down, so the f32 just above 0.7 is still selected by `>`.
        assert_eq!(encode_bound(0.7, 32, RangeOp::Gt), Bound::At(encode(below, 32).unwrap()));
        // `< 0.7` rounds up, so the f32 just below is still selected by `<`.
        assert_eq!(encode_bound(0.7, 32, RangeOp::Lt), Bound::At(encode(above, 32).unwrap()));
        // At full precision there is nothing to round.
        assert_eq!(encode_bound(0.7, 64, RangeOp::Gt), Bound::At(encode(0.7, 64).unwrap()));
    }

    #[test]
    fn equality_against_a_value_the_field_cannot_hold_answers_nothing() {
        assert_eq!(encode_bound(0.7, 32, RangeOp::Eq), Bound::Always(false));
        assert_eq!(encode_bound(0.7, 32, RangeOp::Ne), Bound::Always(true));
        // A value it can hold is compared normally.
        assert_eq!(encode_bound(0.5, 32, RangeOp::Eq), Bound::At(encode(0.5, 32).unwrap()));
    }

    #[test]
    fn nothing_compares_true_with_a_nan() {
        for op in [RangeOp::Gt, RangeOp::Ge, RangeOp::Lt, RangeOp::Le, RangeOp::Eq] {
            assert_eq!(encode_bound(f64::NAN, 64, op), Bound::Always(false), "{op:?}");
        }
        assert_eq!(encode_bound(f64::NAN, 64, RangeOp::Ne), Bound::Always(true));
    }

    #[test]
    fn a_threshold_past_the_field_selects_the_infinities() {
        // A float field has no outside, so this is a true answer rather than a clamped one.
        let past = encode_bound(1e300, 32, RangeOp::Gt);
        assert_eq!(past, Bound::At(encode(f64::from(f32::MAX), 32).unwrap()));
        assert!(encode(f64::INFINITY, 32).unwrap() > encode(f64::from(f32::MAX), 32).unwrap());
    }
}
