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

//! The one statement on this surface that changes the schema rather than reading it.
//!
//! # Why this is a variant and not a `Statement` with no calls
//!
//! Everything else `big-sql` emits is a query-language call that a planner resolves, fans out
//! and merges. A schema change is none of those: it is decided at one node, applied everywhere,
//! and answered with a number rather than a result set. Folding it into [`crate::Statement`]
//! would mean every caller of a statement having to ask whether this one was really a query.
//!
//! # Why the engine is a string and a field kind is not
//!
//! An engine name is passed on as written: this crate links no storage crate, and a name it has
//! never heard of is the layer above's business. Validating one against the engines a build
//! actually has would put a second copy of that list here to drift from the first.
//!
//! A field kind is the other case. `TEXT` is not a name to pass on - there is no field kind
//! spelled `TEXT` anywhere below - so a column list is only meaningful if this crate decides
//! what a type name means. That decision is a dialect, it is made in [`crate::parse`], and it
//! lands in the closed set [`ColumnKind`] so that the layer applying it matches exhaustively
//! rather than re-parsing a string.

/// A schema change written in SQL.
///
/// Three variants, and each was a decision of its own rather than a family filled in: a table
/// is created, its fields are added and dropped, or the table goes. Everything else `CREATE`
/// and `ALTER` can say in SQL is still refused by name - a database, a view, an index, a
/// column's type - because a surface that grew them by accident would be a surface nobody
/// chose.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Ddl {
    /// `CREATE TABLE [IF NOT EXISTS] <name> [(<column>, ...)] [ENGINE = <engine>]`.
    ///
    /// The column list is optional, and an empty one is not the same statement as an absent
    /// one only in that nobody writes `()`: both create a table with no fields, which is what
    /// the `/table` routes create too. Fields declared here are applied as separate changes by
    /// the layer that owns the schema, exactly the changes `POST /table/{t}/field/{f}` makes -
    /// see [`Column`] for why that is not a transaction.
    CreateTable {
        table: String,
        /// `None` when the statement did not say, which means the server's default rather than
        /// any particular engine.
        engine: Option<String>,
        /// In the order written, which is the order the fields are created in.
        columns: Vec<Column>,
        /// `IF NOT EXISTS`: a table that is already there is left exactly as it is, **fields
        /// included**.
        ///
        /// Not the same statement as a plain `CREATE` that happens to be idempotent. Creating a
        /// table twice with one declaration is already accepted below; creating its *fields*
        /// twice is not, so without this flag the second run of a setup script fails on the
        /// column list rather than on the table.
        if_not_exists: bool,
    },
    /// `ALTER TABLE <name> <change> [, <change>]*`.
    ///
    /// Two changes, because two is what the engine below has: a field is created or a field is
    /// dropped, and there is no operation that alters one in place. See [`Alter`].
    AlterTable {
        table: String,
        /// In the order written, and never empty - `ALTER TABLE t` on its own is a syntax
        /// error rather than a statement that changes nothing.
        changes: Vec<Alter>,
    },
    /// `DROP TABLE [IF EXISTS] <name>`, which is `DELETE /table/{t}` written in SQL.
    ///
    /// **The most destructive statement on this surface**, and it is the same capability the
    /// `admin` token that authorises it already has over the table route - a second spelling
    /// rather than a second power. One table and not a list: two drops are two changes, each
    /// travelling to the schema leader and then to every node, and a list would promise an
    /// atomicity nothing below here has.
    DropTable {
        table: String,
        /// `IF EXISTS`: a table that is not there is a request already satisfied, answered with
        /// nothing dropped rather than with an error.
        if_exists: bool,
    },
}

/// One change an `ALTER TABLE` makes.
///
/// # Why there are only two
///
/// Not a smaller dialect than the engine allows - it is exactly the engine. Below this crate a
/// schema change is one of six things, and the two that concern a field are creating one and
/// dropping one. Nothing renames a field, nothing widens one, and nothing changes a kind: a
/// field's bit planes *are* its depth and its kind decides how every fact in it was routed, so
/// altering either would mean rewriting every fact ever written to it.
///
/// So `MODIFY`, `ALTER COLUMN`, `CHANGE` and `RENAME` are refused by name with what to do
/// instead, rather than accepted and turned into a drop and a create that would silently
/// discard the data. See [`crate::Refused::AlterKind`] and [`crate::Refused::Rename`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Alter {
    /// `ADD [COLUMN] <name> <type>`, which is `POST /table/{t}/field/{f}` written in SQL.
    Add(Column),
    /// `DROP [COLUMN] <name>`, which is `DELETE /table/{t}/field/{f}`.
    ///
    /// **The only destructive statement on this surface**, and it is destructive to exactly one
    /// field: `DROP TABLE` is still refused by name. It reaches nothing the `admin` token that
    /// authorises it could not already reach over the field route - this is a second spelling,
    /// not a second capability.
    Drop(String),
}

/// One field, declared in a column list.
///
/// # Why this is not a transaction
///
/// A table and its fields are still separate changes below this crate: each goes to the schema
/// leader and then to every node, and there is no journal that would let a half-applied batch
/// be undone. What a column list buys is one statement to write and one round of validation
/// before anything is created - every kind, depth and scale here is decided while the statement
/// is being parsed, so the failure a column list can still leave behind is a node that went
/// quiet mid-way, never a `DECIMAL` somebody forgot to give a scale.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Column {
    pub name: String,
    pub kind: ColumnKind,
    /// How many bits a value may occupy. Meaningful for [`ColumnKind::Int`],
    /// [`ColumnKind::Signed`] and [`ColumnKind::Decimal`]; for every other kind it is the
    /// number the field route sends for a field that stores no number, so that two spellings of
    /// one field are one row in `/schema`.
    pub bit_depth: u32,
    /// Digits after the point, and `Some` exactly when the kind is [`ColumnKind::Decimal`].
    pub scale: Option<i8>,
}

/// What a field stores, as a column list may name it.
///
/// A closed set rather than the string an engine name is, and for the opposite reason: an
/// engine name is a spelling this crate has no list of, while a field kind is the whole of what
/// a type name in a column list can mean. Emitting an enum makes the layer that applies it
/// match exhaustively, so a kind added here cannot be quietly dropped there.
///
/// The mapping from SQL type names onto these is [`crate::parse`]'s, and it is the one place in
/// this crate that decides what a word means rather than passing it on: `TEXT` is a `Set`
/// because a keyed field is what a string lands in, and `BIGINT` is 64 bits because that is
/// what `BIGINT` has always been.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ColumnKind {
    Set,
    Mutex,
    Bool,
    Int,
    Signed,
    Decimal,
    TimeQuantum,
}

impl ColumnKind {
    /// The spelling `POST /table/{t}/field/{f}?kind=` takes.
    ///
    /// Here so that a refusal, a doc and a query string cannot disagree about how a kind is
    /// written.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Set => "set",
            Self::Mutex => "mutex",
            Self::Bool => "bool",
            Self::Int => "int",
            Self::Signed => "signed",
            Self::Decimal => "decimal",
            Self::TimeQuantum => "timequantum",
        }
    }
}
