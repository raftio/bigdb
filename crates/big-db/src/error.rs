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

use crate::catalog::{FieldId, TableEngine, TableId};
use big_engine::bitmap::field::FieldError;
use big_keys::KeyError;
use big_pager::StoreError;

#[derive(Debug)]
pub enum DbError {
    Store(StoreError),
    Field(FieldError),
    Key(KeyError),
    Tree(big_btree::BTreeError),
    /// A column segment could not be read or written. See [`big_engine::columnar::ColumnError`].
    Column(big_engine::columnar::ColumnError),
    UnknownTable(String),
    /// A database named in a qualified table reference, or as a request's default, that no
    /// `CREATE DATABASE` ever made.
    ///
    /// Distinct from [`DbError::UnknownTable`] on purpose: `sales.orders` failing because there
    /// is no `sales` and failing because `sales` holds no `orders` call for different fixes,
    /// and answering both with "no such table" hides which one happened.
    UnknownDatabase(String),
    /// A `DROP DATABASE` without `CASCADE`, aimed at one that still holds tables.
    DatabaseNotEmpty {
        database: String,
        tables: usize,
    },
    /// `DROP DATABASE default`. Every table is in some database and this is the one that is
    /// always there, so it is the one database that cannot go.
    DropDefaultDatabase,
    UnknownField {
        table: String,
        field: String,
    },
    /// The operation does not match how the field stores its values.
    WrongFieldKind {
        field: String,
        expected: &'static str,
    },
    /// A catalog entry is one fixed-width record, so a name has a hard ceiling. Refused rather
    /// than truncated: cutting a name can split a character, and an entry that fails to decode
    /// disappears silently the next time the file is opened.
    NameTooLong {
        name: String,
        max: usize,
    },
    /// Renaming onto a name already in use would leave the other holder unreachable by name.
    NameTaken(String),
    /// A name holding a `.`, which is the separator in a qualified `database.table`. Refused
    /// rather than accepted, because a table called `a.b` and table `b` in database `a` would
    /// be the same string everywhere a table travels as one - see
    /// [`crate::catalog::TableRef::parse`].
    NameSeparator(String),
    /// A backup was aimed at a path that already holds something. Never overwritten: the
    /// caller who typed the wrong name is the one who needed the old file.
    BackupDestinationExists(std::path::PathBuf),
    /// A copy was aimed at a pager that already has pages in it. Initialising over them would
    /// leave the old contents unreachable but still on disk.
    BackupDestinationNotEmpty,
    /// A read asked for more memory than its ceiling allows. Refused rather than attempted:
    /// the alternative is the kernel choosing which process dies, and it does not choose the
    /// one that asked.
    QueryTooLarge {
        limit: usize,
        needed: usize,
        unit: &'static str,
    },
    /// The field exists with a different definition. Returning the old one would hand back a
    /// field the caller does not think they asked for.
    FieldRedefined {
        table: String,
        field: String,
    },
    /// The table exists under a different storage engine. Returning the old one would hand back
    /// a table whose answers cost what the caller did not ask for.
    TableRedefined {
        table: String,
        existing: &'static str,
        asked: &'static str,
    },
    /// A statement named a storage engine that does not exist.
    ///
    /// Distinct from [`DbError::UnknownTableEngine`], which is a byte on disk from a newer
    /// build: this one is a name a caller typed, and the fix is to type another.
    UnknownEngineName(String),
    /// A table record names a storage engine this build has no code for. Refused rather than
    /// read as bitmap-only, which would make its columns unreachable and its emptiness look
    /// like an answer.
    UnknownTableEngine {
        table: u32,
        engine: u8,
    },
    /// A read passed its deadline and was abandoned. Nothing was written, so there is nothing
    /// to undo: a read transaction that stops early simply stops.
    QueryTimeout {
        elapsed_ms: u64,
        limit_ms: u64,
    },
    /// A read was abandoned because whoever asked for it went away. Distinct from a timeout
    /// because it is not a symptom of anything being slow - it is the correct outcome, and an
    /// alert that cannot tell the two apart will fire on a client pressing ctrl-c.
    QueryCancelled,
    /// The catalog names a field kind this build has never heard of.
    ///
    /// Which means the file was written by a newer big. It used to be **skipped**: the field
    /// vanished, its data stayed on disk unreachable, and every query naming it answered
    /// "unknown field" as though the schema had never had it. That is the same class of mistake
    /// the meta page had - a file from another build being indistinguishable from one that is
    /// simply missing something - and it calls for the same answer, which is to say so.
    UnknownFieldKind {
        table: TableId,
        field: FieldId,
        kind: u8,
    },
    /// A bulk load was aimed at a table that already holds data in that shard. Refused rather
    /// than merged: merging would mean reading the fragment back, which is the one thing a bulk
    /// load exists not to do, and the alternative - overwriting - would strand what was there.
    BulkLoadNotEmpty {
        table: String,
        shard: u64,
    },
    /// The table's storage engine cannot answer this question at all.
    ///
    /// Not "nothing matched". A time window reads the per-day views a time quantum field writes,
    /// and those are an index construct - a segment records which keys a record holds and never
    /// when it held them. Answering empty would be indistinguishable from a window that
    /// genuinely matched nothing, and the two call for opposite actions: one is a query to
    /// rewrite, the other is a table to rebuild under a different engine.
    EngineCannotAnswer {
        table: String,
        what: &'static str,
        engine: &'static str,
        instead: &'static str,
    },
    /// A signed value does not fit the range its field declared. Refused rather than wrapped:
    /// a number that reads back as a different number is worse than a write that failed.
    SignedValueOutOfRange {
        value: i64,
        min: i64,
        max: i64,
    },
}

impl From<std::io::Error> for DbError {
    fn from(e: std::io::Error) -> Self {
        Self::Store(StoreError::Io(e))
    }
}

impl From<StoreError> for DbError {
    fn from(e: StoreError) -> Self {
        Self::Store(e)
    }
}
impl From<FieldError> for DbError {
    fn from(e: FieldError) -> Self {
        Self::Field(e)
    }
}
impl From<KeyError> for DbError {
    fn from(e: KeyError) -> Self {
        Self::Key(e)
    }
}
impl From<big_btree::BTreeError> for DbError {
    fn from(e: big_btree::BTreeError) -> Self {
        Self::Tree(e)
    }
}
impl From<big_engine::columnar::ColumnError> for DbError {
    fn from(e: big_engine::columnar::ColumnError) -> Self {
        // A segment error that is really a tree error keeps its own shape, so a damaged page
        // under a segment reports the same code as the same damage under a fragment.
        match e {
            big_engine::columnar::ColumnError::Tree(t) => Self::Tree(t),
            other => Self::Column(other),
        }
    }
}

/// One sentence per variant, addressed to whoever has to fix it.
///
/// `Display` used to be `{self:?}`, which meant every message an operator ever saw was a Rust
/// debug dump. The names in these messages are the ones the caller typed, so a mistyped field
/// is readable as a mistyped field rather than as a struct literal.
impl core::fmt::Display for DbError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Store(e) => write!(f, "storage: {e}"),
            Self::Field(e) => write!(f, "field: {e}"),
            Self::Key(e) => write!(f, "row key: {e}"),
            Self::Tree(e) => write!(f, "tree: {e}"),
            Self::Column(e) => write!(f, "segment: {e}"),
            Self::UnknownTable(t) => write!(f, "no table named `{t}`"),
            Self::UnknownDatabase(d) => write!(f, "no database named `{d}`"),
            Self::NameSeparator(n) => write!(
                f,
                "`{n}` cannot be a name here: `.` separates a database from a table, so a name \
                 holding one would be indistinguishable from a qualified pair"
            ),
            Self::DatabaseNotEmpty { database, tables } => write!(
                f,
                "database `{database}` still holds {tables} table{}; write \
                 `DROP DATABASE {database} CASCADE` to drop them with it",
                if *tables == 1 { "" } else { "s" }
            ),
            Self::DropDefaultDatabase => write!(
                f,
                "the `default` database cannot be dropped: every table is in some database, \
                 and this is the one that is always there"
            ),
            Self::UnknownField { table, field } => {
                write!(f, "table `{table}` has no field named `{field}`")
            }
            Self::WrongFieldKind { field, expected } => {
                write!(f, "field `{field}` is not a {expected} field")
            }
            Self::NameTooLong { name, max } => {
                write!(f, "name `{name}` is longer than the {max}-byte limit")
            }
            Self::NameTaken(n) => write!(f, "the name `{n}` is already in use"),
            Self::BackupDestinationExists(p) => {
                write!(f, "{} already exists; backups never overwrite", p.display())
            }
            Self::BackupDestinationNotEmpty => {
                write!(f, "the copy destination already holds pages and would be overwritten")
            }
            Self::QueryTooLarge { limit, needed, unit } => write!(
                f,
                "query needs at least {needed} {unit} but the limit is {limit}; \
                 narrow the predicate or raise the limit"
            ),
            Self::FieldRedefined { table, field } => {
                write!(f, "field `{field}` on `{table}` already exists with a different definition")
            }
            Self::TableRedefined { table, existing, asked } => write!(
                f,
                "table `{table}` already exists with the `{existing}` engine, not `{asked}`; \
                 an engine is fixed at creation"
            ),
            // The list is built from the registry rather than written out here, so an engine
            // added to `big_engine::ENGINES` names itself in this message without anyone
            // remembering to come and add it.
            Self::UnknownEngineName(name) => {
                write!(f, "no storage engine named `{name}`; it is one of {}", TableEngine::names())
            }
            Self::UnknownTableEngine { table, engine } => write!(
                f,
                "table {table} names storage engine {engine}, which this build does not know; \
                 the file was written by a newer version"
            ),
            Self::QueryTimeout { elapsed_ms, limit_ms } => write!(
                f,
                "query ran for {elapsed_ms}ms and was stopped at the {limit_ms}ms limit; \
                 narrow the predicate or raise the timeout"
            ),
            Self::QueryCancelled => write!(f, "the client went away before the query finished"),
            Self::EngineCannotAnswer { table, what, engine, instead } => write!(
                f,
                "table `{table}` uses the `{engine}` engine, which cannot answer {what}; \
                 {instead}"
            ),
            Self::BulkLoadNotEmpty { table, shard } => write!(
                f,
                "`{table}` already holds records in shard {shard}; a bulk load only builds \
                 fragments that are empty - use an ordinary write or an ingest to add to it"
            ),
            Self::SignedValueOutOfRange { value, min, max } => {
                write!(f, "{value} is outside the field's range of {min}..={max}")
            }
            Self::UnknownFieldKind { table, field, kind } => write!(
                f,
                "field {field} of table {table} has kind {kind}, which this build does not know; \
                 the file was written by a newer big"
            ),
        }
    }
}

/// A stable, machine-readable name for the failure, to sit beside the sentence.
///
/// The wrapping variants delegate so that a code always names the thing that actually went
/// wrong. Flattening them to `storage` would mean a checksum mismatch and a full disk arrived
/// as the same code, and those call for opposite actions.
impl DbError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Store(e) => e.code(),
            Self::Field(e) => e.code(),
            Self::Key(e) => e.code(),
            Self::Tree(e) => e.code(),
            Self::Column(e) => e.code(),
            Self::UnknownTable(_) => "unknown_table",
            Self::UnknownDatabase(_) => "unknown_database",
            Self::NameSeparator(_) => "name_separator",
            Self::DatabaseNotEmpty { .. } => "database_not_empty",
            Self::DropDefaultDatabase => "drop_default_database",
            Self::UnknownField { .. } => "unknown_field",
            Self::WrongFieldKind { .. } => "wrong_field_kind",
            Self::NameTooLong { .. } => "name_too_long",
            Self::NameTaken(_) => "name_taken",
            Self::BackupDestinationExists(_) => "backup_destination_exists",
            Self::BackupDestinationNotEmpty => "backup_destination_not_empty",
            Self::QueryTooLarge { .. } => "query_too_large",
            Self::FieldRedefined { .. } => "field_redefined",
            Self::TableRedefined { .. } => "table_redefined",
            Self::UnknownEngineName(_) => "unknown_engine_name",
            Self::UnknownTableEngine { .. } => "unknown_table_engine",
            Self::QueryTimeout { .. } => "query_timeout",
            Self::QueryCancelled => "query_cancelled",
            Self::EngineCannotAnswer { .. } => "engine_cannot_answer",
            Self::UnknownFieldKind { .. } => "unknown_field_kind",
            Self::SignedValueOutOfRange { .. } => "value_out_of_range",
            Self::BulkLoadNotEmpty { .. } => "bulk_load_not_empty",
        }
    }
}

impl core::error::Error for DbError {}

pub type Result<T> = core::result::Result<T, DbError>;
