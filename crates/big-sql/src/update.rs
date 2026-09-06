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

//! `UPDATE t SET c = v WHERE ...`: a new fact written where an old one was.
//!
//! # There is no row to change, and this is not a workaround for that
//!
//! A fact here is a bit at `(row, record)`. Changing a record's value means writing the new fact
//! and clearing the old one - which is exactly what the engine already does for the kinds where
//! one record holds one value: `Bsi::set` emits the set *and* the clear in one tree pass, and
//! `MutexField::put` reads the old row, clears it and sets the new. So an update on those columns
//! is an ordinary write, and this statement is the spelling for it rather than a mechanism.
//!
//! # Which columns, and why the others are refused rather than emulated
//!
//! A `SET` column holds **every** value a record was ever given - that is what makes it a set -
//! and a `TIMEQUANTUM` column writes a copy into the view for each moment. Writing a new value to
//! either *adds* it, and there is no per-value unset below this layer to remove the old one. So
//! those are refused by name, with the two-statement spelling that does work. Emulating it by
//! deleting the record and rewriting it would be a different statement with a different failure
//! mode, chosen silently on the client's behalf.
//!
//! # Literals only
//!
//! `SET amount = amount + 1` needs each record's current value read back, arithmetic done on it
//! and the result written - a read-modify-write per record, which is not a plan this engine has.
//! It is refused at the token that is not a literal.

/// `UPDATE t SET c = v [, ...] WHERE ...`, lowered.
///
/// The parse-tree half is [`crate::ast::Update`]; the split is the one [`crate::delete::Delete`]
/// makes and for the same reason.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Update {
    pub database: Option<String>,
    pub table: String,
    /// The columns to write, and the value each takes, in the order written.
    pub assignments: Vec<(String, big_plan::Literal)>,
    /// The records to write them to, as the call that selects them.
    ///
    /// The same call a `DELETE` with the same `WHERE` carries, produced by the same lowering -
    /// so the three statements that take a `WHERE` cannot come to disagree about what one selects.
    pub rows: big_plan::ast::Call,
    /// Every other table the filter reads. See [`crate::delete::Delete::reads`].
    pub reads: Vec<String>,
}

impl Update {
    /// The table this reads and writes, qualified.
    #[must_use]
    pub fn qualified(&self) -> String {
        match &self.database {
            Some(d) => format!("{d}.{}", self.table),
            None => self.table.clone(),
        }
    }
}
