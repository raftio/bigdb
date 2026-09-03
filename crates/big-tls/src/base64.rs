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

//! Base64, the standard alphabet, strictly.
//!
//! **Why it is here and not in `big-http`.** Two things in this workspace need it and they are
//! at different layers: PEM is base64 between two dashed lines, and an HTTP `Authorization:
//! Basic` header is base64 of `user:password`. This crate is under both of them, so putting it
//! here is what keeps there being one implementation rather than two that agree until they do
//! not.
//!
//! **Why hand-written.** It is forty lines, and what matters about it is not the encoding - which
//! every implementation agrees on - but the refusals. A decoder that accepts `YWJj=` or
//! `YW Jj` or a padding character in the middle is a decoder that makes two different strings
//! mean the same credential, and that is a thing worth writing down rather than importing.

/// The standard alphabet. Not URL-safe: both callers are wire formats that specify this one.
const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// What was wrong with the input, in the caller's vocabulary rather than a byte offset.
#[derive(Debug, PartialEq, Eq)]
pub enum Base64Error {
    /// A byte outside the alphabet, or a `=` somewhere other than the end.
    Alphabet,
    /// Not a multiple of four bytes. Padding is required, not optional.
    Length,
    /// Longer than the caller said it would accept, refused before anything was allocated.
    TooLong,
}

impl core::fmt::Display for Base64Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Alphabet => f.write_str("not base64"),
            Self::Length => f.write_str("base64 of the wrong length; padding is required"),
            Self::TooLong => f.write_str("too long"),
        }
    }
}

impl std::error::Error for Base64Error {}

/// Encodes with padding.
pub fn encode(input: &[u8]) -> String {
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = u32::from(b[0]) << 16 | u32::from(b[1]) << 8 | u32::from(b[2]);
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        // The last chunk is padded rather than truncated: a decoder that requires padding, as
        // the one below does, needs an encoder that emits it.
        out.push(if chunk.len() > 1 { ALPHABET[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { ALPHABET[n as usize & 63] as char } else { '=' });
    }
    out
}

/// Decodes, refusing anything that is not exactly one encoding of one byte string.
///
/// `limit` is checked against the *encoded* length before a byte is allocated, so a caller
/// handling a header from a stranger can bound the work without bounding it in the caller.
///
/// Refused, deliberately: whitespace of any kind, a byte outside the alphabet, a length that is
/// not a multiple of four, `=` anywhere but the last two positions, and non-zero bits in the
/// part of a padded group that the padding says is not there. That last one is what stops
/// `YWJjZA==` and `YWJjZB==` from decoding to the same four bytes.
pub fn decode(input: &str, limit: usize) -> Result<Vec<u8>, Base64Error> {
    let s = input.as_bytes();
    if s.len() > limit {
        return Err(Base64Error::TooLong);
    }
    if !s.len().is_multiple_of(4) {
        return Err(Base64Error::Length);
    }
    if s.is_empty() {
        return Ok(Vec::new());
    }

    // Padding is only ever the last one or two bytes, so it is settled once here rather than
    // being a special case inside the loop.
    let pad = match (s[s.len() - 1], s[s.len() - 2]) {
        (b'=', b'=') => 2,
        (b'=', _) => 1,
        _ => 0,
    };

    let mut out = Vec::with_capacity(s.len() / 4 * 3 - pad);
    for group in s[..s.len() - pad].chunks(4) {
        let mut n: u32 = 0;
        for &c in group {
            let Some(v) = value(c) else { return Err(Base64Error::Alphabet) };
            n = n << 6 | u32::from(v);
        }
        // A short final group carries its bits in the high end; shifting up by the six bits per
        // missing character puts them where the three-byte split below expects them.
        n <<= 6 * (4 - group.len());
        let bytes = [(n >> 16) as u8, (n >> 8) as u8, n as u8];
        out.extend_from_slice(&bytes[..3 - (4 - group.len())]);
        // The bits the padding claims are absent must actually be zero, or two encodings mean
        // one string.
        if group.len() < 4 && bytes[3 - (4 - group.len())..].iter().any(|&b| b != 0) {
            return Err(Base64Error::Alphabet);
        }
    }
    Ok(out)
}

/// The alphabet, as a lookup. `=` is not here: padding is handled by position, not by value, so
/// a `=` in the middle of the input falls through to `None` and is refused.
fn value(c: u8) -> Option<u8> {
    Some(match c {
        b'A'..=b'Z' => c - b'A',
        b'a'..=b'z' => c - b'a' + 26,
        b'0'..=b'9' => c - b'0' + 52,
        b'+' => 62,
        b'/' => 63,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 4648 section 10, which is the only table anybody should be inventing this against.
    #[test]
    fn the_rfc_4648_vectors() {
        for (plain, encoded) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(encode(plain.as_bytes()), encoded, "encoding {plain:?}");
            assert_eq!(decode(encoded, 64).unwrap(), plain.as_bytes(), "decoding {encoded:?}");
        }
    }

    #[test]
    fn every_byte_survives_a_round_trip() {
        let all: Vec<u8> = (0..=255).collect();
        for n in 0..all.len() {
            let slice = &all[..n];
            assert_eq!(decode(&encode(slice), 1024).unwrap(), slice, "at length {n}");
        }
    }

    #[test]
    fn whitespace_is_not_ignored() {
        // Some decoders skip it. One that does makes `YWJj` and `YW Jj` the same credential,
        // and a credential with two spellings is a credential an audit log cannot match.
        // A space inside a group is refused by the alphabet; one that makes the input the wrong
        // length is refused before the alphabet is consulted at all. Both are refusals, and
        // which one a given input gets is not worth making uniform.
        assert_eq!(decode("YW J", 64), Err(Base64Error::Alphabet));
        assert_eq!(decode("YW Jj", 64), Err(Base64Error::Length));
        assert_eq!(decode("YWJj\n", 64), Err(Base64Error::Length));
        assert_eq!(decode("\tYWJj", 64), Err(Base64Error::Length));
    }

    #[test]
    fn padding_is_required_and_only_at_the_end() {
        assert_eq!(decode("Zg", 64), Err(Base64Error::Length), "unpadded");
        assert_eq!(decode("Z=g=", 64), Err(Base64Error::Alphabet), "padding in the middle");
        assert_eq!(decode("=Zg=", 64), Err(Base64Error::Alphabet));
    }

    #[test]
    fn a_second_spelling_of_the_same_bytes_is_refused() {
        // `Zg==` is "f". `Zh==` would decode to "f" too if the discarded bits were not checked,
        // which would make one byte string have sixteen spellings.
        assert_eq!(decode("Zg==", 64).unwrap(), b"f");
        assert_eq!(decode("Zh==", 64), Err(Base64Error::Alphabet));
    }

    #[test]
    fn the_limit_is_checked_before_anything_is_allocated() {
        let long = "A".repeat(4096);
        assert_eq!(decode(&long, 1024), Err(Base64Error::TooLong));
    }

    #[test]
    fn bytes_outside_the_alphabet_are_refused() {
        assert_eq!(decode("YWJ-", 64), Err(Base64Error::Alphabet), "url-safe alphabet");
        assert_eq!(decode("YWJ_", 64), Err(Base64Error::Alphabet));
        assert_eq!(decode("YWJ\u{0}", 64), Err(Base64Error::Alphabet));
    }
}
