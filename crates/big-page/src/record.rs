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

//! The catalog record: one fixed-width slot, and who is allowed to fill it.
//!
//! The pager carries catalog entries without reading them, and two crates above write into the
//! same stream - `big-db` for schema and fragment metadata, `big-keys` for row keys. Neither
//! of them can see the other's numbers, so the registry lives down here where the layout does,
//! and a collision is a compile error rather than a corrupt reload.

/// Every catalog entry is this wide, whatever kind it is. Fixed width is what lets the chain
/// be split across pages without a length prefix per record.
pub const CATALOG_ENTRY_BYTES: usize = 128;

/// Byte 0 of every catalog entry. A reader skips kinds it does not recognise, so these numbers
/// are part of the on-disk format and may never be reused for something else.
pub mod kind {
    pub const TABLE: u8 = 1;
    pub const FIELD: u8 = 2;
    pub const VIEW: u8 = 3;
    /// Owned by `big-keys`.
    pub const ROW_KEY: u8 = 4;
    pub const FRAGMENT: u8 = 5;
    /// Id high-water marks. Purely additive: a reader that predates this kind skips it and
    /// falls back to deriving the next id from the largest one it can see, which is exactly
    /// what every reader did before dropping existed.
    pub const SEQ: u8 = 6;
}

/// Every kind, so adding one without checking it against the others is not possible.
pub const ALL_KINDS: [u8; 6] =
    [kind::TABLE, kind::FIELD, kind::VIEW, kind::ROW_KEY, kind::FRAGMENT, kind::SEQ];

// Distinctness, checked at compile time. Two crates allocate out of this space and cannot see
// each other's constants; without this a duplicate would surface as records vanishing on
// reload, which is the least debuggable failure this format has.
const _: () = {
    let mut i = 0;
    while i < ALL_KINDS.len() {
        let mut j = i + 1;
        while j < ALL_KINDS.len() {
            assert!(ALL_KINDS[i] != ALL_KINDS[j], "two catalog record kinds collide");
            j += 1;
        }
        i += 1;
    }
    // Zero is what an unwritten byte reads as, so it must not mean anything.
    let mut i = 0;
    while i < ALL_KINDS.len() {
        assert!(ALL_KINDS[i] != 0, "catalog record kind 0 is reserved for empty");
        i += 1;
    }
};
