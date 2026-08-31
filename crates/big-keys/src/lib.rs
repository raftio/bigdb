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

//! Row key translation: the string value of a set or mutex field to the row it lives in.
//!
//! Record ids are used raw, so this is the *only* translation layer in the engine. A row key
//! has to mean the same thing in every shard, which makes assigning one the single point in
//! the write path that needs agreement. Whatever distributed design comes later gets decided
//! here rather than the other way round.

#![deny(unsafe_code)]

use big_page::{kind, CATALOG_ENTRY_BYTES};
use std::collections::BTreeMap;

pub type TableId = u32;
pub type FieldId = u32;
pub type RowId = u64;

/// This crate's slot in the catalog record space, which `big-page` owns and checks for
/// collisions at compile time.
pub const KIND_ROW_KEY: u8 = kind::ROW_KEY;

/// Keys longer than this are refused rather than silently truncated, which would merge two
/// different values into one row.
pub const MAX_KEY_LEN: usize = 104;

#[derive(Debug, PartialEq, Eq)]
pub enum KeyError {
    TooLong {
        len: usize,
    },
    /// A key was assigned a row id that disagrees with the one it already has here.
    ///
    /// Only [`KeyStore::assign`] can produce this, which means only a node being told what a
    /// key means by someone else can. It is the failure that per-node row id ranges were
    /// rejected for, caught rather than absorbed: two ids for one string is a wrong answer
    /// nothing downstream could notice, so the write stops here.
    Conflict {
        name_len: usize,
        assigned: RowId,
        held: RowId,
    },
    /// This store already holds as many keys as it was allowed to.
    ///
    /// Only [`KeyStore::intern`] can produce it, and deliberately: interning *invents* a key,
    /// and inventing is the operation that grows what this process holds in memory.
    /// [`KeyStore::assign`] is a node being told what a key means by the schema leader, and a
    /// follower that refused there would hold a different dictionary from the rest of the
    /// cluster - which is the one failure the whole interning design exists to prevent.
    TooManyKeys {
        /// The ceiling that was reached.
        limit: usize,
    },
    /// The row id is already taken by a different key in the same field.
    ///
    /// The other half of the same check. Accepting it would give one row two meanings, which
    /// is the same wrong answer read from the other end.
    RowTaken {
        row: RowId,
    },
}

impl core::fmt::Display for KeyError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::TooLong { len } => {
                write!(f, "row key is {len} bytes, longer than the {MAX_KEY_LEN}-byte limit")
            }
            // The key itself is not printed. It is caller data on a path that is already
            // going wrong, and its length is the part that helps identify which one it was.
            Self::Conflict { name_len, assigned, held } => write!(
                f,
                "a {name_len}-byte row key was assigned row {assigned} but already holds row \
                 {held} here"
            ),
            Self::TooManyKeys { limit } => write!(
                f,
                "this database already holds {limit} row keys, which is the configured \
                 ceiling. Every key is resident in memory in both directions, so the ceiling \
                 is on what this process will hold rather than on what the file can store"
            ),
            Self::RowTaken { row } => {
                write!(f, "row {row} is already held by a different key in this field")
            }
        }
    }
}

/// See [`big_page::PageError::code`].
impl KeyError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::TooLong { .. } => "row_key_too_long",
            Self::Conflict { .. } => "row_key_conflict",
            Self::RowTaken { .. } => "row_id_taken",
            Self::TooManyKeys { .. } => "too_many_row_keys",
        }
    }
}

impl core::error::Error for KeyError {}

pub type Result<T> = core::result::Result<T, KeyError>;

type Scope = (TableId, FieldId);

/// Bidirectional and append-only: a row id is never reused, because doing so would hand a new
/// value all the bits of the old one.
#[derive(Clone, Default, Debug)]
pub struct KeyStore {
    ids: BTreeMap<(Scope, String), RowId>,
    names: BTreeMap<(Scope, RowId), String>,
    next: BTreeMap<Scope, RowId>,
    /// Running total of what the two maps hold, kept rather than computed.
    ///
    /// A scraper asks for this every few seconds and the answer is O(keys) to derive; on a
    /// dictionary of millions that is a walk over every string on every scrape. Maintaining it
    /// costs one addition per key and makes the reading free, which is the right way round for
    /// a number whose entire purpose is to be watched.
    bytes: usize,
    /// How many keys this store will *invent*. `None` is no ceiling, which is what every
    /// existing caller gets and what the format has always allowed.
    limit: Option<usize>,
}

impl KeyStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn id(&self, table: TableId, field: FieldId, name: &str) -> Option<RowId> {
        self.ids.get(&((table, field), name.to_string())).copied()
    }

    pub fn name(&self, table: TableId, field: FieldId, row: RowId) -> Option<&str> {
        self.names.get(&((table, field), row)).map(|s| s.as_str())
    }

    /// Returns the existing row when the key is known, otherwise assigns the next one.
    pub fn intern(&mut self, table: TableId, field: FieldId, name: &str) -> Result<RowId> {
        if name.len() > MAX_KEY_LEN {
            return Err(KeyError::TooLong { len: name.len() });
        }
        let scope = (table, field);
        if let Some(id) = self.ids.get(&(scope, name.to_string())) {
            return Ok(*id);
        }
        // Checked here rather than at the caller because this is the only line in the engine
        // that decides a string is worth remembering for ever. A key that already exists costs
        // nothing new and is returned above, so the ceiling never refuses a repeat.
        if let Some(limit) = self.limit {
            if self.ids.len() >= limit {
                return Err(KeyError::TooManyKeys { limit });
            }
        }
        let slot = self.next.entry(scope).or_insert(0);
        let id = *slot;
        *slot += 1;
        self.ids.insert((scope, name.to_string()), id);
        self.names.insert((scope, id), name.to_string());
        self.bytes += entry_bytes(name);
        Ok(id)
    }

    /// Records that `name` means `row`, rather than choosing a row for it.
    ///
    /// The follower half of the schema leader's job. A node that did not assign a row id has
    /// to be told one, and telling it is not the same operation as interning: interning may
    /// invent an id and this may not. What it may do is disagree, and a disagreement is
    /// refused - see [`KeyError::Conflict`].
    ///
    /// Idempotent when the mapping already matches, because a retried batch is the normal
    /// case rather than the exceptional one.
    ///
    /// The counter moves to one past `row` so that a later local [`Self::intern`] - a
    /// single-node path, a leader that lost its cluster config - cannot hand out an id this
    /// field already uses.
    pub fn assign(&mut self, table: TableId, field: FieldId, name: &str, row: RowId) -> Result<()> {
        if name.len() > MAX_KEY_LEN {
            return Err(KeyError::TooLong { len: name.len() });
        }
        let scope = (table, field);
        match self.ids.get(&(scope, name.to_string())) {
            Some(held) if *held == row => return Ok(()),
            Some(held) => {
                return Err(KeyError::Conflict { name_len: name.len(), assigned: row, held: *held })
            }
            None => {}
        }
        // Reached only when the name is new here, so an occupied row means a *different* name
        // already holds it. The equal case returned above.
        if self.names.contains_key(&(scope, row)) {
            return Err(KeyError::RowTaken { row });
        }

        self.ids.insert((scope, name.to_string()), row);
        self.names.insert((scope, row), name.to_string());
        self.bytes += entry_bytes(name);
        let slot = self.next.entry(scope).or_insert(0);
        *slot = (*slot).max(row + 1);
        Ok(())
    }

    /// Batch form: one pass, so a bulk import does not walk the map once per fact.
    pub fn intern_all(
        &mut self,
        table: TableId,
        field: FieldId,
        names: impl IntoIterator<Item = impl AsRef<str>>,
    ) -> Result<Vec<RowId>> {
        names.into_iter().map(|n| self.intern(table, field, n.as_ref())).collect()
    }

    /// Forgets every key of one field, for when the field itself is being dropped.
    ///
    /// Safe only because field ids are never reused: a later field in the same table gets a
    /// fresh id and therefore a fresh scope, so it cannot inherit row ids whose bits are gone.
    pub fn remove_scope(&mut self, table: TableId, field: FieldId) {
        let scope = (table, field);
        self.ids.retain(|(s, name), _| {
            let keep = *s != scope;
            if !keep {
                self.bytes -= entry_bytes(name);
            }
            keep
        });
        self.names.retain(|(s, _), _| *s != scope);
        self.next.remove(&scope);
    }

    /// Forgets every key of every field of one table.
    pub fn remove_table(&mut self, table: TableId) {
        self.ids.retain(|((t, _), name), _| {
            let keep = *t != table;
            if !keep {
                self.bytes -= entry_bytes(name);
            }
            keep
        });
        self.names.retain(|((t, _), _), _| *t != table);
        self.next.retain(|(t, _), _| *t != table);
    }

    pub fn rows(&self, table: TableId, field: FieldId) -> impl Iterator<Item = (RowId, &str)> {
        self.names
            .range(((table, field), 0)..=((table, field), RowId::MAX))
            .map(|((_, r), n)| (*r, n.as_str()))
    }

    pub fn len(&self) -> usize {
        self.ids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    /// Roughly what this store holds in memory, in bytes.
    ///
    /// **Roughly, and deliberately so.** It counts the key bytes and the fixed part of each
    /// entry, twice, because every key is held in both directions. It does *not* count
    /// `BTreeMap` node overhead or the allocator's, which means the true figure is larger by a
    /// factor the standard library does not expose. Under-reporting is the right direction for
    /// a gauge whose job is to be compared against itself over time: it moves exactly when the
    /// dictionary does, and an operator reading it wants the slope rather than the intercept.
    pub fn resident_bytes(&self) -> usize {
        self.bytes
    }

    /// The ceiling on invented keys, or `None` for none.
    pub fn key_limit(&self) -> Option<usize> {
        self.limit
    }

    /// Sets the ceiling. Existing keys are never evicted: a key that has been handed out names
    /// bits that are already written, so forgetting it would make those bits unreadable rather
    /// than free anything. Lowering the limit below what is already held therefore stops new
    /// keys and does nothing else, which is what a ceiling should do.
    pub fn set_key_limit(&mut self, limit: Option<usize>) {
        self.limit = limit;
    }

    pub fn encode(&self) -> Vec<Vec<u8>> {
        self.names
            .iter()
            .map(|(((table, field), row), name)| {
                let mut b = vec![0u8; CATALOG_ENTRY_BYTES];
                b[0] = KIND_ROW_KEY;
                b[2..4].copy_from_slice(&(name.len() as u16).to_le_bytes());
                b[4..8].copy_from_slice(&table.to_le_bytes());
                b[8..12].copy_from_slice(&field.to_le_bytes());
                b[16..24].copy_from_slice(&row.to_le_bytes());
                b[24..24 + name.len()].copy_from_slice(name.as_bytes());
                b
            })
            .collect()
    }

    /// Ignores records belonging to other kinds, so one catalog chain can hold everything.
    pub fn from_entries(entries: &[Vec<u8>]) -> Self {
        let mut s = Self::new();
        for e in entries {
            if e.len() < CATALOG_ENTRY_BYTES || e[0] != KIND_ROW_KEY {
                continue;
            }
            let len = u16::from_le_bytes(e[2..4].try_into().unwrap()) as usize;
            if len > MAX_KEY_LEN {
                continue;
            }
            let table = u32::from_le_bytes(e[4..8].try_into().unwrap());
            let field = u32::from_le_bytes(e[8..12].try_into().unwrap());
            let row = u64::from_le_bytes(e[16..24].try_into().unwrap());
            let Ok(name) = core::str::from_utf8(&e[24..24 + len]) else {
                continue;
            };
            let scope = (table, field);
            s.ids.insert((scope, name.to_string()), row);
            s.names.insert((scope, row), name.to_string());
            s.bytes += entry_bytes(name);
            let slot = s.next.entry(scope).or_insert(0);
            *slot = (*slot).max(row + 1);
        }
        s
    }
}

/// What one key costs this process, counted the same way everywhere.
///
/// The name is stored twice - once as part of a map key, once as a value - and each direction
/// carries a scope and a row id alongside it. Sixteen bytes is `(TableId, FieldId)` plus a
/// `RowId`, which both maps pay.
fn entry_bytes(name: &str) -> usize {
    2 * (name.len() + 16)
}

#[cfg(test)]
mod ceiling_tests {
    use super::*;

    #[test]
    fn a_store_with_no_limit_interns_whatever_it_is_given() {
        let mut s = KeyStore::new();
        for i in 0..64 {
            assert!(s.intern(1, 1, &format!("k{i}")).is_ok());
        }
        assert_eq!(s.len(), 64);
    }

    #[test]
    fn the_ceiling_refuses_a_new_key_and_names_itself() {
        let mut s = KeyStore::new();
        s.set_key_limit(Some(2));
        assert!(s.intern(1, 1, "a").is_ok());
        assert!(s.intern(1, 1, "b").is_ok());
        assert_eq!(s.intern(1, 1, "c"), Err(KeyError::TooManyKeys { limit: 2 }));
    }

    /// The ceiling is on *inventing*, so a key that already exists still resolves. Otherwise a
    /// full dictionary would stop reads of data that is already written.
    #[test]
    fn a_key_that_is_already_known_is_returned_after_the_ceiling_is_reached() {
        let mut s = KeyStore::new();
        s.set_key_limit(Some(1));
        let id = s.intern(1, 1, "a").unwrap();
        assert_eq!(s.intern(1, 1, "a"), Ok(id));
    }

    /// A follower is told what a key means by the schema leader and may not disagree - so the
    /// ceiling must not apply there, or two nodes would hold different dictionaries.
    #[test]
    fn being_told_what_a_key_means_is_never_refused_by_the_ceiling() {
        let mut s = KeyStore::new();
        s.set_key_limit(Some(0));
        assert!(s.assign(1, 1, "from-the-leader", 7).is_ok());
        assert_eq!(s.id(1, 1, "from-the-leader"), Some(7));
    }

    #[test]
    fn the_byte_count_rises_with_keys_and_falls_when_they_are_dropped() {
        let mut s = KeyStore::new();
        assert_eq!(s.resident_bytes(), 0);
        s.intern(1, 1, "hello").unwrap();
        let one = s.resident_bytes();
        assert!(one > 0);
        s.intern(1, 2, "hello").unwrap();
        assert_eq!(s.resident_bytes(), one * 2, "a second scope holds its own copy");

        s.remove_scope(1, 2);
        assert_eq!(s.resident_bytes(), one);
        s.remove_table(1);
        assert_eq!(s.resident_bytes(), 0, "dropping the table gives every byte back");
    }

    /// What is loaded from a file costs the same as what was interned into one.
    #[test]
    fn a_reloaded_store_reports_the_same_bytes() {
        let mut s = KeyStore::new();
        for i in 0..16 {
            s.intern(3, 4, &format!("key-{i}")).unwrap();
        }
        let reloaded = KeyStore::from_entries(&s.encode());
        assert_eq!(reloaded.len(), s.len());
        assert_eq!(reloaded.resident_bytes(), s.resident_bytes());
    }
}
