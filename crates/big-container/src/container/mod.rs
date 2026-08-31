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

//! The three representations of a 2^16-bit container.

pub mod array;

use crate::{Interval, BITMAP_WORDS};

/// Container kind as stored in the cell header; a dense one lives on its own page,
/// so the cell only holds a pointer to it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u16)]
pub enum ContainerType {
    Array = 0,
    Run = 1,
    BitmapPtr = 2,
    /// A dense container that lives on its own page, plus the bits changed since that page was
    /// last written. See [`crate::delta`].
    ///
    /// A build that predates this refuses the whole page with `UnknownContainerType` rather than
    /// reading the base and silently losing the delta - which is why the type is new instead of
    /// the payload of a `BitmapPtr` cell growing.
    BitmapDelta = 3,
    /// A block of column values, small enough to sit in the leaf cell.
    ///
    /// Not a set. The payload is an encoded run of *values* in record order, and `elem_n` is
    /// its length in bytes rather than a count of elements - which is why the two values types
    /// are separate tags and not a widened `Array`. Everything that reads a container has to
    /// refuse them by tag, so that a build with no column support says so rather than decoding
    /// values as offsets.
    ValuesInline = 4,
    /// The same, with the encoded values on a page of their own and the block's descriptor
    /// still in the cell. The page is raw bytes with its checksum in the cell above it -
    /// exactly the arrangement [`ContainerType::BitmapPtr`] uses, which is what lets the walk,
    /// the copy and the scrub handle it without being taught anything.
    ValuesPtr = 5,
}

impl ContainerType {
    pub fn from_u16(v: u16) -> Option<Self> {
        match v {
            0 => Some(Self::Array),
            1 => Some(Self::Run),
            2 => Some(Self::BitmapPtr),
            3 => Some(Self::BitmapDelta),
            4 => Some(Self::ValuesInline),
            5 => Some(Self::ValuesPtr),
            _ => None,
        }
    }

    /// Whether a cell of this type owns a page of its own, protected by the checksum the cell
    /// carries.
    ///
    /// One definition, because four places need the answer - the free walk, the scrub, the
    /// copy and the splice - and a new type that any one of them missed would leak a page on a
    /// free or lose one on a copy. That is the failure this method exists to make impossible.
    pub fn owns_page(self) -> bool {
        matches!(self, Self::BitmapPtr | Self::BitmapDelta | Self::ValuesPtr)
    }

    /// Whether this type carries column values rather than a set of offsets.
    ///
    /// The one question every container-shaped reader has to ask before it decodes a payload:
    /// a values block is not a container and reading one as an array is reading a number as a
    /// list of positions.
    pub fn is_values(self) -> bool {
        matches!(self, Self::ValuesInline | Self::ValuesPtr)
    }
}

/// A container borrowed straight out of a page; its lifetime is the transaction's.
#[derive(Clone, Copy, Debug)]
pub enum ContainerRef<'a> {
    Array(&'a [u16]),
    Bitmap(&'a [u64; BITMAP_WORDS]),
    Run(&'a [Interval]),
}

impl<'a> ContainerRef<'a> {
    /// Recounts from the payload. The cell caches this, so the hot path never calls it.
    /// How much heap this container occupies, near enough for a memory ceiling to be useful.
    ///
    /// Not a serialised size and not exact: it counts the payload, which is what actually
    /// grows with the data, and ignores the handful of bytes of enum tag and vector header.
    pub fn byte_size(&self) -> usize {
        match self {
            Self::Array(a) => core::mem::size_of_val(*a),
            Self::Bitmap(_) => crate::BITMAP_BYTES,
            Self::Run(r) => core::mem::size_of_val(*r),
        }
    }

    pub fn cardinality(&self) -> u32 {
        match self {
            Self::Array(a) => a.len() as u32,
            Self::Bitmap(b) => b.iter().map(|w| w.count_ones()).sum(),
            Self::Run(r) => r.iter().map(|i| i.len()).sum(),
        }
    }

    pub fn is_empty(&self) -> bool {
        match self {
            Self::Array(a) => a.is_empty(),
            Self::Bitmap(b) => b.iter().all(|w| *w == 0),
            Self::Run(r) => r.is_empty(),
        }
    }

    pub fn contains(&self, v: u16) -> bool {
        match self {
            Self::Array(a) => a.binary_search(&v).is_ok(),
            Self::Bitmap(b) => b[v as usize / 64] >> (v % 64) & 1 == 1,
            Self::Run(r) => r
                .binary_search_by(|i| {
                    if i.last < v {
                        core::cmp::Ordering::Less
                    } else if i.start > v {
                        core::cmp::Ordering::Greater
                    } else {
                        core::cmp::Ordering::Equal
                    }
                })
                .is_ok(),
        }
    }

    pub fn iter(&self) -> Iter<'a> {
        match *self {
            Self::Array(a) => Iter::Array(a.iter()),
            Self::Bitmap(b) => Iter::Bitmap { words: b, word: 0, cur: b[0] },
            Self::Run(r) => Iter::Run { runs: r, idx: 0, next: r.first().map_or(0, |i| i.start) },
        }
    }

    pub fn to_owned(&self) -> Container {
        match self {
            Self::Array(a) => Container::Array(a.to_vec()),
            Self::Bitmap(b) => Container::Bitmap(Box::new(**b)),
            Self::Run(r) => Container::Run(r.to_vec()),
        }
    }

    /// Per-representation invariants. For fuzz/proptest, not for the hot path.
    pub fn is_well_formed(&self) -> bool {
        match self {
            Self::Array(a) => a.windows(2).all(|w| w[0] < w[1]),
            Self::Bitmap(_) => true,
            Self::Run(r) => {
                r.iter().all(|i| i.start <= i.last)
                    && r.windows(2).all(|w| (w[0].last as u32) + 1 < w[1].start as u32)
            }
        }
    }
}

/// An owning container: the result of a mutation or a set operation.
#[derive(Clone, Debug)]
pub enum Container {
    Array(Vec<u16>),
    Bitmap(Box<[u64; BITMAP_WORDS]>),
    Run(Vec<Interval>),
}

impl Container {
    pub fn as_ref(&self) -> ContainerRef<'_> {
        match self {
            Self::Array(a) => ContainerRef::Array(a),
            Self::Bitmap(b) => ContainerRef::Bitmap(b),
            Self::Run(r) => ContainerRef::Run(r),
        }
    }

    pub fn cardinality(&self) -> u32 {
        self.as_ref().cardinality()
    }

    pub fn is_empty(&self) -> bool {
        self.as_ref().is_empty()
    }

    pub fn contains(&self, v: u16) -> bool {
        self.as_ref().contains(v)
    }

    pub fn empty() -> Self {
        Self::Array(Vec::new())
    }

    /// Values may arrive in any order; duplicates collapse.
    pub fn from_values(values: impl IntoIterator<Item = u16>) -> Self {
        let mut v: Vec<u16> = values.into_iter().collect();
        v.sort_unstable();
        v.dedup();
        Self::Array(v)
    }

    /// Returns true when the value was not already present.
    pub fn insert(&mut self, v: u16) -> bool {
        match self {
            Self::Array(a) => match a.binary_search(&v) {
                Ok(_) => false,
                Err(i) => {
                    a.insert(i, v);
                    true
                }
            },
            Self::Bitmap(b) => {
                let (w, bit) = (v as usize / 64, 1u64 << (v % 64));
                let had = b[w] & bit != 0;
                b[w] |= bit;
                !had
            }
            Self::Run(r) => {
                if self_run_contains(r, v) {
                    return false;
                }
                let mut vals: Vec<u16> = ContainerRef::Run(r).iter().collect();
                match vals.binary_search(&v) {
                    Ok(_) => false,
                    Err(i) => {
                        vals.insert(i, v);
                        *r = crate::optimize::to_runs(ContainerRef::Array(&vals));
                        true
                    }
                }
            }
        }
    }

    /// Returns true when the value was actually present.
    pub fn remove(&mut self, v: u16) -> bool {
        match self {
            Self::Array(a) => match a.binary_search(&v) {
                Ok(i) => {
                    a.remove(i);
                    true
                }
                Err(_) => false,
            },
            Self::Bitmap(b) => {
                let (w, bit) = (v as usize / 64, 1u64 << (v % 64));
                let had = b[w] & bit != 0;
                b[w] &= !bit;
                had
            }
            Self::Run(r) => {
                if !self_run_contains(r, v) {
                    return false;
                }
                let vals: Vec<u16> = ContainerRef::Run(r).iter().filter(|x| *x != v).collect();
                *r = crate::optimize::to_runs(ContainerRef::Array(&vals));
                true
            }
        }
    }
}

fn self_run_contains(r: &[Interval], v: u16) -> bool {
    ContainerRef::Run(r).contains(v)
}

/// Result of a set operation: borrow as-is when possible (`A | {}`), allocate only when forced.
#[derive(Debug)]
pub enum OpResult<'a> {
    Borrowed(ContainerRef<'a>),
    Owned(Container),
}

impl<'a> OpResult<'a> {
    pub fn as_ref(&self) -> ContainerRef<'_> {
        match self {
            Self::Borrowed(c) => *c,
            Self::Owned(c) => c.as_ref(),
        }
    }

    pub fn into_owned(self) -> Container {
        match self {
            Self::Borrowed(c) => c.to_owned(),
            Self::Owned(c) => c,
        }
    }

    pub fn is_borrowed(&self) -> bool {
        matches!(self, Self::Borrowed(_))
    }
}

/// Ascending bit iteration, shared by all three representations.
pub enum Iter<'a> {
    Array(core::slice::Iter<'a, u16>),
    Bitmap { words: &'a [u64; BITMAP_WORDS], word: usize, cur: u64 },
    Run { runs: &'a [Interval], idx: usize, next: u16 },
}

impl Iterator for Iter<'_> {
    type Item = u16;

    fn next(&mut self) -> Option<u16> {
        match self {
            Self::Array(it) => it.next().copied(),
            Self::Bitmap { words, word, cur } => {
                while *cur == 0 {
                    *word += 1;
                    if *word >= BITMAP_WORDS {
                        return None;
                    }
                    *cur = words[*word];
                }
                let bit = cur.trailing_zeros();
                *cur &= *cur - 1;
                Some((*word * 64 + bit as usize) as u16)
            }
            Self::Run { runs, idx, next } => {
                let run = runs.get(*idx)?;
                let v = *next;
                if v == run.last {
                    *idx += 1;
                    *next = runs.get(*idx).map_or(0, |i| i.start);
                } else {
                    *next = v + 1;
                }
                Some(v)
            }
        }
    }
}
