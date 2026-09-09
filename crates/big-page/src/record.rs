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
    /// The namespace a table's name is unique within. Purely additive in both directions: a
    /// reader that predates this kind skips the entry, and every table record written before
    /// it carries a zero in the word that now names its database - which is
    /// `big_db::catalog::DEFAULT_DATABASE`, the database such a table has always been in.
    pub const DATABASE: u8 = 7;
    /// A `SELECT` kept under a name - what SQL calls a view, and what this layer calls a saved
    /// query because [`VIEW`] is already taken and means a bitmap partition by time quantum.
    ///
    /// The header: id, database and name, laid out exactly as a table record is. The statement
    /// itself does not fit in a record and follows in [`SAVED_QUERY_TEXT`] chunks.
    pub const SAVED_QUERY: u8 = 8;
    /// One 104-byte slice of a saved query's statement, under the same id as its header.
    ///
    /// **The first payload in this format that spans records.** A record is a fixed width, and
    /// a `SELECT` is not, so the text is cut into chunks and reassembled on load. A chunk
    /// boundary falls wherever it falls, including mid-character, which is why the bytes are
    /// joined before they are validated as UTF-8 rather than one record at a time.
    pub const SAVED_QUERY_TEXT: u8 = 9;
    /// A named set of privileges: id and name, laid out exactly as a database record is.
    ///
    /// The privileges themselves are not here. A role holds them on *objects*, and how many
    /// objects is not known when the role is made, so each one is its own [`GRANT`] - the same
    /// shape a table takes, where the columns are records of their own rather than a list
    /// crammed into the header.
    pub const ROLE: u8 = 10;
    /// One role's privileges on one object: role id, database id, table id, and a bitset.
    ///
    /// **The only catalog record with no name in it.** Every id it carries was interned by the
    /// records above, so a grant is four words and stops well short of the name field. Keying
    /// by id rather than by name is what makes a grant vanish with the table it is about: ids
    /// are never reissued, so a table dropped and recreated under the same name gets a new id
    /// and cannot inherit the privileges the old one carried.
    pub const GRANT: u8 = 11;
    /// One 104-byte slice of a field's declared enum members, under its table and field id.
    ///
    /// **The second payload in this format that spans records**, and it follows
    /// [`SAVED_QUERY_TEXT`]'s shape for the same reason: a record is a fixed width and a list of
    /// member names is not. The members are joined by a NUL byte, which is the one byte a
    /// member may not contain, and cut wherever 104 bytes falls - including mid-character, so
    /// the chunks are joined before they are validated.
    ///
    /// Purely additive. A reader that predates this kind skips the entry and sees the field as
    /// the `MUTEX` it is stored as, which is what it was before enums existed - the same
    /// backward story [`DATABASE`] and [`SEQ`] tell.
    pub const FIELD_ENUM: u8 = 12;
}

/// Every kind, so adding one without checking it against the others is not possible.
pub const ALL_KINDS: [u8; 12] = [
    kind::TABLE,
    kind::FIELD,
    kind::VIEW,
    kind::ROW_KEY,
    kind::FRAGMENT,
    kind::SEQ,
    kind::DATABASE,
    kind::SAVED_QUERY,
    kind::SAVED_QUERY_TEXT,
    kind::ROLE,
    kind::GRANT,
    kind::FIELD_ENUM,
];

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
