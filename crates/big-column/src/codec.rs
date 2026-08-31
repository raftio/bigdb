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

//! How a run of values becomes bytes.
//!
//! Four encodings, chosen per block by measuring all of them and keeping the smallest. Choosing
//! by measurement rather than by a rule about the data is what makes the choice safe to extend:
//! a fifth encoding is correct the moment it can encode and decode, because nothing downstream
//! knows which one a block used.
//!
//! **There is no general-purpose compressor here, and that is a decision rather than an
//! omission.** The whole engine ships two dependencies; a compression library would be the
//! largest thing in the tree by a wide margin. What these four give up against LZ4 on
//! already-columnar data is small, and what they buy is that every byte of the format is
//! readable by looking at this file.

use crate::error::{ColumnError, Result};

/// How the present values of one block are laid out.
///
/// Part of the on-disk format: the number is what a block header stores.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Codec {
    /// Every present value is the same. The payload is empty and the value is the header's
    /// base - which makes a boolean column, or any column a shard happens to hold one value
    /// of, cost nothing beyond its null bitmap.
    Constant = 0,
    /// `base + n` bits per value, values in slot order with nulls skipped.
    ///
    /// This is frame-of-reference plus bit packing in one step, and it is the workhorse. A
    /// timestamp column whose block spans an hour needs 22 bits rather than 64; an id column
    /// dense in a range needs the bits of the range and not of the ids.
    BitPacked = 1,
    /// Runs of `(value, length)`. Chosen when a column is sorted or repetitive enough that
    /// runs beat packing - which for a low-cardinality key arriving in bulk is usual.
    Rle = 2,
    /// Eight bytes per present value. The fallback, and the ceiling every other encoding is
    /// measured against.
    Plain = 3,
}

impl Codec {
    pub fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            0 => Self::Constant,
            1 => Self::BitPacked,
            2 => Self::Rle,
            3 => Self::Plain,
            _ => return None,
        })
    }
}

/// One encoded run of values, with what the header has to record to read it back.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Encoded {
    pub codec: Codec,
    /// The subtrahend for [`Codec::BitPacked`], and the value itself for [`Codec::Constant`].
    pub base: u64,
    /// Bits per packed value. Zero for every codec but [`Codec::BitPacked`], and zero there too
    /// when the range is a single value - which `Constant` would have won anyway.
    pub width: u8,
    pub bytes: Vec<u8>,
}

/// Encodes `values` the smallest way any of the four can.
///
/// Every encoding is built and measured rather than predicted. That costs four passes over at
/// most a thousand values, which is nothing next to the page write it is deciding the size of,
/// and it means the choice cannot be wrong in the direction that matters - a rule that guessed
/// would sometimes pick an encoding larger than `Plain`, and `Plain` is always available.
pub fn encode(values: &[u64]) -> Encoded {
    if values.is_empty() {
        return Encoded { codec: Codec::Constant, base: 0, width: 0, bytes: Vec::new() };
    }

    let min = *values.iter().min().expect("non-empty");
    let max = *values.iter().max().expect("non-empty");

    if min == max {
        return Encoded { codec: Codec::Constant, base: min, width: 0, bytes: Vec::new() };
    }

    // The range and not the magnitude: subtracting the minimum first is what turns a column of
    // large, close numbers into a column of small ones.
    let width = bits_for(max - min);
    let packed = pack(values, min, width);
    let mut best = Encoded { codec: Codec::BitPacked, base: min, width, bytes: packed };

    let runs = encode_rle(values);
    if runs.len() < best.bytes.len() {
        best = Encoded { codec: Codec::Rle, base: 0, width: 0, bytes: runs };
    }

    let plain = encode_plain(values);
    if plain.len() < best.bytes.len() {
        best = Encoded { codec: Codec::Plain, base: 0, width: 0, bytes: plain };
    }
    best
}

/// Reverses [`encode`] into `out`, which is cleared first.
///
/// `n` is how many values the header says are there. It is passed rather than derived because
/// every encoding but `Plain` needs it - a packed run has no self-delimiting end, and trusting
/// the payload length to imply a count would let a padded byte become a value.
pub fn decode(e: &Encoded, n: usize, out: &mut Vec<u64>) -> Result<()> {
    out.clear();
    out.reserve(n);
    match e.codec {
        Codec::Constant => out.extend(core::iter::repeat_n(e.base, n)),
        Codec::BitPacked => unpack(&e.bytes, e.base, e.width, n, out)?,
        Codec::Rle => decode_rle(&e.bytes, n, out)?,
        Codec::Plain => decode_plain(&e.bytes, n, out)?,
    }
    Ok(())
}

/// How many bits it takes to hold every value up to `max`.
///
/// Zero for zero, which is right: a range of one value needs no bits at all, and the packer
/// writes nothing for it.
pub fn bits_for(max: u64) -> u8 {
    (64 - max.leading_zeros()) as u8
}

/// How many bytes `n` values of `width` bits occupy.
pub fn packed_len(n: usize, width: u8) -> usize {
    (n * width as usize).div_ceil(8)
}

fn pack(values: &[u64], base: u64, width: u8) -> Vec<u8> {
    let mut out = vec![0u8; packed_len(values.len(), width)];
    if width == 0 {
        return out;
    }
    let mut bit = 0usize;
    for v in values {
        let mut residual = v - base;
        // Little-endian bit order, written a byte at a time so the buffer never has to be
        // aligned and a value may straddle any number of byte boundaries.
        for _ in 0..width {
            if residual & 1 == 1 {
                out[bit / 8] |= 1 << (bit % 8);
            }
            residual >>= 1;
            bit += 1;
        }
    }
    out
}

fn unpack(bytes: &[u8], base: u64, width: u8, n: usize, out: &mut Vec<u64>) -> Result<()> {
    if width > 64 {
        return Err(ColumnError::BadWidth(width));
    }
    let need = packed_len(n, width);
    if bytes.len() < need {
        return Err(ColumnError::Truncated { need, have: bytes.len() });
    }
    if width == 0 {
        out.extend(core::iter::repeat_n(base, n));
        return Ok(());
    }
    let mut bit = 0usize;
    for _ in 0..n {
        let mut v = 0u64;
        for k in 0..width {
            if bytes[bit / 8] >> (bit % 8) & 1 == 1 {
                v |= 1u64 << k;
            }
            bit += 1;
        }
        out.push(base + v);
    }
    Ok(())
}

/// `(value, run length)` pairs, each ten bytes. A run is capped at `u16::MAX` so the length
/// stays two bytes; a longer stretch simply becomes two runs.
const RLE_ENTRY: usize = 10;

fn encode_rle(values: &[u64]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < values.len() {
        let v = values[i];
        let mut run = 1usize;
        while i + run < values.len() && values[i + run] == v && run < u16::MAX as usize {
            run += 1;
        }
        out.extend_from_slice(&v.to_le_bytes());
        out.extend_from_slice(&(run as u16).to_le_bytes());
        i += run;
    }
    out
}

fn decode_rle(bytes: &[u8], n: usize, out: &mut Vec<u64>) -> Result<()> {
    let mut at = 0usize;
    while out.len() < n {
        if at + RLE_ENTRY > bytes.len() {
            return Err(ColumnError::Truncated { need: at + RLE_ENTRY, have: bytes.len() });
        }
        let v = u64::from_le_bytes(bytes[at..at + 8].try_into().expect("eight bytes"));
        let run = u16::from_le_bytes(bytes[at + 8..at + 10].try_into().expect("two bytes"));
        at += RLE_ENTRY;
        // A run that would overshoot is truncated to what the header asked for rather than
        // trusted: the count in the header is the authority on how many values there are.
        let take = (run as usize).min(n - out.len());
        out.extend(core::iter::repeat_n(v, take));
        if run == 0 {
            return Err(ColumnError::Truncated { need: n, have: out.len() });
        }
    }
    Ok(())
}

fn encode_plain(values: &[u64]) -> Vec<u8> {
    let mut out = Vec::with_capacity(values.len() * 8);
    for v in values {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

fn decode_plain(bytes: &[u8], n: usize, out: &mut Vec<u64>) -> Result<()> {
    let need = n * 8;
    if bytes.len() < need {
        return Err(ColumnError::Truncated { need, have: bytes.len() });
    }
    for i in 0..n {
        out.push(u64::from_le_bytes(bytes[i * 8..i * 8 + 8].try_into().expect("eight bytes")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(values: &[u64]) -> Vec<u64> {
        let e = encode(values);
        let mut out = Vec::new();
        decode(&e, values.len(), &mut out).unwrap();
        out
    }

    #[test]
    fn every_shape_of_run_survives_a_round_trip() {
        for case in [
            vec![],
            vec![0],
            vec![u64::MAX],
            vec![7; 1024],
            (0..1024u64).collect(),
            (0..1024u64).rev().collect(),
            vec![0, u64::MAX, 0, u64::MAX],
            (0..1024u64).map(|i| 1_700_000_000 + i).collect(),
        ] {
            assert_eq!(round_trip(&case), case, "{:?}", &case[..case.len().min(4)]);
        }
    }

    /// The choice is made by measuring, so what this pins down is that each encoding actually
    /// wins the case it exists for. If one of them stops being chosen, it is dead weight.
    #[test]
    fn each_encoding_wins_the_case_it_exists_for() {
        assert_eq!(encode(&[9u64; 500]).codec, Codec::Constant);

        // Large numbers in a narrow range: the frame of reference is the whole point.
        let clustered: Vec<u64> = (0..1024).map(|i| 1_700_000_000_000 + i).collect();
        let packed = encode(&clustered);
        assert_eq!(packed.codec, Codec::BitPacked);
        assert!(packed.bytes.len() < clustered.len() * 8 / 4, "{}", packed.bytes.len());

        // A few long runs of far-apart values: packing cannot help, runs can.
        let mut runs = Vec::new();
        for v in [0u64, u64::MAX / 2, u64::MAX] {
            runs.extend(core::iter::repeat_n(v, 300));
        }
        assert_eq!(encode(&runs).codec, Codec::Rle);
    }

    /// Nothing may encode larger than `Plain`, because `Plain` is always available and the
    /// block-size argument - a scalar block never needs two pages - rests on that ceiling.
    #[test]
    fn no_encoding_is_ever_larger_than_plain() {
        let cases: Vec<Vec<u64>> = vec![
            (0..1024u64).map(|i| i.wrapping_mul(2_654_435_761)).collect(),
            (0..1024u64).map(|i| if i % 2 == 0 { 0 } else { u64::MAX }).collect(),
            (0..1024u64).collect(),
        ];
        for case in cases {
            let e = encode(&case);
            assert!(e.bytes.len() <= case.len() * 8, "{:?} grew to {}", e.codec, e.bytes.len());
        }
    }

    #[test]
    fn a_truncated_payload_is_refused_rather_than_padded() {
        let e = encode(&(0..100u64).collect::<Vec<_>>());
        let short = Encoded { bytes: e.bytes[..e.bytes.len() / 2].to_vec(), ..e };
        let mut out = Vec::new();
        assert!(matches!(decode(&short, 100, &mut out), Err(ColumnError::Truncated { .. })));
    }

    #[test]
    fn bits_for_counts_what_it_says() {
        assert_eq!(bits_for(0), 0);
        assert_eq!(bits_for(1), 1);
        assert_eq!(bits_for(255), 8);
        assert_eq!(bits_for(256), 9);
        assert_eq!(bits_for(u64::MAX), 64);
    }
}
