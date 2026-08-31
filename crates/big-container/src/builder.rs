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

//! Output buffer for a set operation.
//!
//! The representation is chosen from a predicted upper bound *before* the operation runs, so a
//! small result never gets built as a bitmap and then downgraded.

use crate::{Container, BITMAP_WORDS};

pub enum Out {
    Array(Vec<u16>),
    Bitmap(Box<[u64; BITMAP_WORDS]>),
}

impl Out {
    /// `bound` is an upper bound on the result cardinality, not the exact value.
    pub fn with_bound(bound: usize) -> Self {
        if bound <= crate::ARRAY_BREAK_EVEN {
            Self::Array(Vec::with_capacity(bound))
        } else {
            Self::Bitmap(Box::new([0u64; BITMAP_WORDS]))
        }
    }

    /// Values must arrive in ascending order when the output is an array.
    pub fn push(&mut self, v: u16) {
        match self {
            Self::Array(a) => a.push(v),
            Self::Bitmap(b) => b[v as usize / 64] |= 1u64 << (v % 64),
        }
    }

    pub fn as_bitmap_mut(&mut self) -> Option<&mut [u64; BITMAP_WORDS]> {
        match self {
            Self::Bitmap(b) => Some(b),
            Self::Array(_) => None,
        }
    }

    pub fn is_bitmap(&self) -> bool {
        matches!(self, Self::Bitmap(_))
    }

    pub fn finish(self) -> Container {
        match self {
            Self::Array(a) => Container::Array(a),
            Self::Bitmap(b) => Container::Bitmap(b),
        }
    }
}
