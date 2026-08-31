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

//! What a field stores.
//!
//! Here rather than in the catalog that persists it, because **an engine cannot route a fact
//! without it**: whether a second write to a field adds or replaces is the difference between a
//! set and a mutex, and that decision belongs beside the code that acts on it. `big-db`
//! re-exports this, the same way it re-exports [`crate::TableEngine`].

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum FieldKind {
    Set = 0,
    Mutex = 1,
    Bool = 2,
    Int = 3,
    Decimal = 4,
    TimeQuantum = 5,
    /// A signed integer, stored in the same bit planes as [`FieldKind::Int`] under an offset
    /// binary bias. See `big_db::signed`.
    SignedInt = 6,
}

impl FieldKind {
    /// Public because the number is not private: it is what the catalog stores and what a
    /// peer is told when a field is created across a cluster, so the mapping has exactly one
    /// definition and both readers use it.
    pub fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            0 => Self::Set,
            1 => Self::Mutex,
            2 => Self::Bool,
            3 => Self::Int,
            4 => Self::Decimal,
            5 => Self::TimeQuantum,
            6 => Self::SignedInt,
            _ => return None,
        })
    }

    pub fn is_bsi(self) -> bool {
        matches!(self, Self::Int | Self::Decimal | Self::SignedInt)
    }

    /// Whether values of this kind are biased on the way in and out.
    ///
    /// Only the sign convention needs it, and it is a property of the kind rather than of the
    /// value, which is what keeps the bias out of every arithmetic path below the boundary.
    pub fn is_signed(self) -> bool {
        matches!(self, Self::SignedInt)
    }

    /// Kinds addressed by a row key rather than by a value. A mutex is one of them: it is a
    /// set field that happens to allow only one row per record.
    pub fn is_keyed(self) -> bool {
        matches!(self, Self::Set | Self::Mutex | Self::TimeQuantum)
    }
}
