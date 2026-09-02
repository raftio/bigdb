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

//! What one node sends another.
//!
//! **Containers are not re-encoded.** A leaf cell already stores an array as little-endian
//! `u16`s, a run as pairs of them and a bitmap as its raw words - see `LeafBuilder::push_container`
//! in `big-page` - and that is exactly what goes on the wire, under the same type tag.
//! Inventing a second encoding for the same three shapes would mean two places that have to
//! agree about what a container is, and the second one is the one that drifts.
//!
//! **A plan travels, never query text.** Re-parsing per node would let two nodes disagree
//! about what was asked, and a result cannot show that it happened.
//!
//! **Every decoder here reads bytes it did not write.** A peer is not a trusted input: a
//! length is checked against what is left before anything is allocated on the strength of it,
//! recursion is bounded by the same [`MAX_DEPTH`] the query parser uses, and a container that
//! decodes into something malformed is refused rather than handed to the set algebra.
//!
//! Split by *what is being encoded* rather than by direction, because a codec's two halves are
//! one decision: a tag written in `plan` and read in `plan` can only disagree with itself if
//! they are in the same file to be compared. What lives here is the part every other module
//! spells the same way - the reader, the errors, and the fixed-width primitives.

mod agreement;
mod fact;
mod plan;
mod request;
mod value;

pub use agreement::*;
pub use fact::*;
pub use plan::*;
pub use request::*;
pub use value::*;

use big_embed::{
    ColumnCell, FieldKind, FragmentAddr, FragmentData, FragmentMeta, Granularity, Pair, Plan,
    Projected, Projection, RecordId, RowId, Rows, TableEngine, Value,
};
use big_container::{Container, ContainerRef, Interval};
use big_db::Matches;
use big_engine::bitmap::RowSet;
use big_engine::ShardId;
use big_exec::Group;
use big_plan::CmpOp;

/// The same ceiling the query parser applies to nesting, for the same reason: these decoders
/// recurse, so depth is call depth, and a peer could otherwise send a plan that overflows the
/// stack rather than failing.
pub use big_plan::parse::MAX_DEPTH;

/// Why a message could not be read.
///
/// Deliberately vague about *which* byte. A peer sending malformed bytes is either a bug or an
/// attacker, and neither is helped by an offset; what the operator needs is which message and
/// which node, and both are known one layer up.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum WireError {
    /// The message ended in the middle of a value.
    Truncated,
    /// A tag byte that names nothing. Carries what was being read, because "tag 9" alone
    /// cannot be looked up.
    BadTag { what: &'static str, tag: u8 },
    /// A string that was not UTF-8.
    BadUtf8,
    /// Nesting past [`MAX_DEPTH`].
    TooDeep,
    /// Structurally readable and still wrong - an array container whose values are not
    /// ascending, a run whose ends are the wrong way round.
    Malformed(&'static str),
}

impl core::fmt::Display for WireError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Truncated => write!(f, "the message ended early"),
            Self::BadTag { what, tag } => write!(f, "{tag} is not a {what}"),
            Self::BadUtf8 => write!(f, "a string was not valid UTF-8"),
            Self::TooDeep => write!(f, "nested deeper than {MAX_DEPTH}"),
            Self::Malformed(what) => write!(f, "{what}"),
        }
    }
}

impl core::error::Error for WireError {}

pub type Result<T> = core::result::Result<T, WireError>;

// ---------------------------------------------------------------------------------------
// Primitives
// ---------------------------------------------------------------------------------------

/// A cursor over bytes somebody else wrote.
///
/// Every read is bounds-checked and advances only on success, so a failed decode leaves the
/// reader where it was rather than half way through a value.
pub struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    pub fn remaining(&self) -> usize {
        self.bytes.len() - self.at
    }

    /// Whether every byte was consumed. Trailing bytes mean the two sides disagree about the
    /// shape of the message, which is worth refusing rather than ignoring.
    pub fn is_done(&self) -> bool {
        self.at == self.bytes.len()
    }

    pub fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.at.checked_add(n).ok_or(WireError::Truncated)?;
        let out = self.bytes.get(self.at..end).ok_or(WireError::Truncated)?;
        self.at = end;
        Ok(out)
    }

    pub fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    pub fn bool(&mut self) -> Result<bool> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            tag => Err(WireError::BadTag { what: "boolean", tag }),
        }
    }

    pub fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().expect("two bytes")))
    }

    pub fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().expect("four bytes")))
    }

    pub fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().expect("eight bytes")))
    }

    pub fn i64(&mut self) -> Result<i64> {
        Ok(self.u64()? as i64)
    }

    pub fn u128(&mut self) -> Result<u128> {
        Ok(u128::from_le_bytes(self.take(16)?.try_into().expect("sixteen bytes")))
    }

    pub fn i128(&mut self) -> Result<i128> {
        Ok(self.u128()? as i128)
    }

    pub fn str(&mut self) -> Result<String> {
        let n = self.u32()? as usize;
        core::str::from_utf8(self.take(n)?).map(str::to_string).map_err(|_| WireError::BadUtf8)
    }

    pub fn opt_str(&mut self) -> Result<Option<String>> {
        self.bool()?.then(|| self.str()).transpose()
    }

    pub fn opt_u64(&mut self) -> Result<Option<u64>> {
        self.bool()?.then(|| self.u64()).transpose()
    }

    pub fn opt_i64(&mut self) -> Result<Option<i64>> {
        self.bool()?.then(|| self.i64()).transpose()
    }

    pub fn opt_i128(&mut self) -> Result<Option<i128>> {
        self.bool()?.then(|| self.i128()).transpose()
    }

    /// A count, checked against what is left before anything is sized from it.
    ///
    /// Every element of every list here costs at least one byte, so a count larger than the
    /// bytes remaining is a lie, and refusing it is what keeps a four-byte header from asking
    /// for a four-gigabyte allocation.
    pub fn count(&mut self) -> Result<usize> {
        let n = self.u32()? as usize;
        if n > self.remaining() {
            return Err(WireError::Truncated);
        }
        Ok(n)
    }
}

pub fn put_u8(out: &mut Vec<u8>, v: u8) {
    out.push(v);
}

pub fn put_bool(out: &mut Vec<u8>, v: bool) {
    out.push(v as u8);
}

pub fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

pub fn put_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_le_bytes());
}

pub fn put_i64(out: &mut Vec<u8>, v: i64) {
    put_u64(out, v as u64);
}

pub fn put_u128(out: &mut Vec<u8>, v: u128) {
    out.extend_from_slice(&v.to_le_bytes());
}

pub fn put_i128(out: &mut Vec<u8>, v: i128) {
    put_u128(out, v as u128);
}

pub fn put_str(out: &mut Vec<u8>, s: &str) {
    put_u32(out, s.len() as u32);
    out.extend_from_slice(s.as_bytes());
}

pub fn put_opt_str(out: &mut Vec<u8>, s: Option<&str>) {
    put_bool(out, s.is_some());
    if let Some(s) = s {
        put_str(out, s);
    }
}

pub fn put_opt_u64(out: &mut Vec<u8>, v: Option<u64>) {
    put_bool(out, v.is_some());
    if let Some(v) = v {
        put_u64(out, v);
    }
}

pub fn put_opt_i64(out: &mut Vec<u8>, v: Option<i64>) {
    put_bool(out, v.is_some());
    if let Some(v) = v {
        put_i64(out, v);
    }
}

pub fn put_opt_i128(out: &mut Vec<u8>, v: Option<i128>) {
    put_bool(out, v.is_some());
    if let Some(v) = v {
        put_i128(out, v);
    }
}

fn put_count(out: &mut Vec<u8>, n: usize) {
    put_u32(out, n as u32);
}

/// Trailing bytes mean the two sides disagree about the shape of a message.
///
/// Ignoring them would let a version skew read the first half of a message, answer confidently,
/// and be wrong about the second - which is the failure this whole layer is built to refuse.
fn finished(r: &Reader<'_>) -> Result<()> {
    if r.is_done() {
        Ok(())
    } else {
        Err(WireError::Malformed("the message has bytes after its end"))
    }
}
