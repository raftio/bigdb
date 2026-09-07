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
//! what a type name means. That decision is a dialect, it is made in [`mod@crate::parse`], and it
//! lands in the closed set [`ColumnKind`] so that the layer applying it matches exhaustively
//! rather than re-parsing a string.

/// How deep a view may be nested inside another before the expansion is refused.
///
/// **A bound on the statement a `FROM` becomes, not a taste in schemas.** A view is expanded by
/// substitution, so a view over a view over a view is one statement with three filters ANDed
/// together and three rounds of column renaming. Eight is past anything a schema arrives at on
/// purpose and short of anything that makes an expansion expensive.
///
/// A cycle cannot be built - `CREATE VIEW` requires what it names to exist already - so this is
/// the belt to that braces, and it also bounds the recursion for a file some other tool wrote.
pub const MAX_VIEW_DEPTH: usize = 8;

/// A schema change written in SQL.
///
/// Each variant was a decision of its own rather than a family filled in: a database, a table,
/// its fields, a view. Everything else `CREATE` and `ALTER` can say in SQL is still refused by
/// name - an index, a materialised view, a column's type - because a surface that grew them by
/// accident would be a surface nobody chose.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Ddl {
    /// `CREATE DATABASE [IF NOT EXISTS] <name>`, also spelled `SCHEMA` and `DATASET`.
    ///
    /// A database here is a **namespace**, and the natural place to hang retention and access
    /// on later - not a storage boundary. Nothing below the catalog knows there is one: a
    /// `TableId` is unique across every database, so no fragment, row key or root record
    /// mentions one. Which is why cross-database questions are free, and why this statement
    /// writes a single catalog record.
    CreateDatabase { name: String, if_not_exists: bool },
    /// `DROP DATABASE [IF EXISTS] <name> [CASCADE | RESTRICT]`.
    ///
    /// `RESTRICT` is the default and the absence of `CASCADE` means it: a database still
    /// holding tables is refused rather than emptied. `DROP DATABASE` is one word away from
    /// being the most expensive typo on this surface, and Postgres and BigQuery both make you
    /// say the word.
    DropDatabase {
        name: String,
        if_exists: bool,
        /// `CASCADE` was written: drop every table in it too.
        cascade: bool,
    },
    /// `CREATE TABLE [IF NOT EXISTS] <name> [(<column>, ...)] [ENGINE = <engine>]`.
    ///
    /// The column list is optional, and an empty one is not the same statement as an absent
    /// one only in that nobody writes `()`: both create a table with no fields, which is what
    /// the `/table` routes create too. Fields declared here are applied as separate changes by
    /// the layer that owns the schema, exactly the changes `POST /table/{t}/field/{f}` makes -
    /// see [`Column`] for why that is not a transaction.
    CreateTable {
        /// `sales` in `sales.orders`. `None` means the request's default database,
        /// which is what an unqualified name asks for.
        database: Option<String>,
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
        /// `sales` in `sales.orders`. `None` means the request's default database,
        /// which is what an unqualified name asks for.
        database: Option<String>,
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
        /// `sales` in `sales.orders`. `None` means the request's default database,
        /// which is what an unqualified name asks for.
        database: Option<String>,
        table: String,
        /// `IF EXISTS`: a table that is not there is a request already satisfied, answered with
        /// nothing dropped rather than with an error.
        if_exists: bool,
    },
    /// `TRUNCATE TABLE [IF EXISTS] <name>`: the records go, the table stays.
    ///
    /// **A schema change rather than a write, and that is not a filing decision.** It costs the
    /// number of *fragments*, not the number of records - the pages are freed by key, the way a
    /// `DROP TABLE` frees them - so it travels the leader-then-fan-out path every other schema
    /// change takes rather than the shard fan-out a delete would. It is also what a bulk load
    /// needs in order to reload a table: `big_db::bulk` refuses to write into occupied
    /// fragments, and until this existed the only way to clear them was to drop the table and
    /// lose its declaration with them.
    TruncateTable {
        database: Option<String>,
        table: String,
        /// `IF EXISTS`, so a setup script that empties a table before filling it is re-runnable
        /// against a database where it has not been created yet. The same argument
        /// [`Ddl::DropTable`] takes it for.
        if_exists: bool,
    },
    /// `CREATE [OR REPLACE] VIEW [IF NOT EXISTS] <name> AS <select>`.
    ///
    /// # What a body may be, and why it is that little
    ///
    /// A filter and a projection over one table, and nothing else. A view here is **inlined**
    /// into the statement that reads it - the base table replaces the view's name, the two
    /// `WHERE`s are ANDed, and the outer statement's columns are renamed through the body's
    /// select list. There is no subquery below this crate to nest one in, so a body that groups
    /// or aggregates has nothing to become: its answer would have to exist before the outer
    /// statement ran, which is the materialised half.
    ///
    /// The parser enforces that ([`crate::Refused::ViewBody`]), so it costs no schema and every
    /// case is reachable by the corpus.
    CreateView {
        /// `sales` in `sales.v`. `None` means the request's default database.
        database: Option<String>,
        name: String,
        /// The `SELECT`, sliced from the statement exactly as it was written.
        ///
        /// **Text, not the parsed body.** It is re-parsed at every read, which is what keeps one
        /// place deciding what a name means - and it is what `SHOW CREATE VIEW` answers with.
        /// Storing the tree would also mean writing a `SELECT` renderer, which is a second
        /// dialect to keep in step with the parser.
        body: String,
        /// `OR REPLACE`: a name already holding a different statement is overwritten rather than
        /// refused.
        or_replace: bool,
        /// `IF NOT EXISTS`: a name already holding a statement is left exactly as it is.
        ///
        /// Not the same as `OR REPLACE` and not a spelling of it - they are opposite answers to
        /// the same situation. Writing both is refused where they are parsed.
        if_not_exists: bool,
    },
    /// `DROP VIEW [IF EXISTS] <name>`.
    ///
    /// Cheap in a way `DROP TABLE` is not: a view owns no pages, so this forgets a name and a
    /// string and frees nothing. It is still `admin`, because what it breaks is every statement
    /// that was reading through it.
    DropView {
        /// `sales` in `sales.v`. `None` means the request's default database.
        database: Option<String>,
        name: String,
        /// `IF EXISTS`: a view that is not there is a request already satisfied.
        if_exists: bool,
    },
}

impl Ddl {
    /// Fills in the database a table this statement names did not carry. See
    /// [`crate::translate_in`].
    ///
    /// A `CREATE DATABASE` names a database rather than being in one, so it is untouched -
    /// creating `sales` from a request against `ops` creates `sales`.
    pub fn fill_database(&mut self, database: &str) {
        match self {
            Self::CreateTable { database: d, .. }
            | Self::AlterTable { database: d, .. }
            | Self::DropTable { database: d, .. }
            | Self::TruncateTable { database: d, .. }
            // A view is *in* a database the way a table is, so it takes the request's. What its
            // body resolves in is a different question with a different answer - the view's own
            // database, applied where the body is parsed rather than here.
            | Self::CreateView { database: d, .. }
            | Self::DropView { database: d, .. } => {
                d.get_or_insert_with(|| database.to_string());
            }
            Self::CreateDatabase { .. } | Self::DropDatabase { .. } => {}
        }
    }
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
    /// `DROP DAYS BEFORE '<date>' ON <column>`: retention for one time-quantum column.
    ///
    /// **An `ALTER` because it changes what the table's *index* holds without changing what the
    /// table stores** - and because `ALTER` is where clauses already compose, so it is validated
    /// beside the adds and drops against the same simulated post-statement schema.
    ///
    /// It is imperative rather than a declarative `TTL` clause, and that is a decision rather
    /// than a stage: nothing here would keep a standing promise. There is no scheduler in this
    /// workspace, `big-embed` publishes that it spawns no threads, and a table plus a promise
    /// nothing keeps is the object [`crate::Refused::MaterializedView`] already refuses by name.
    /// Run it from cron.
    DropDaysBefore {
        /// The time-quantum column whose index is being trimmed.
        column: String,
        /// The date to keep from, as written. The day it falls in is **kept**.
        before: String,
    },
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
/// The mapping from SQL type names onto these is [`mod@crate::parse`]'s, and it is the one place in
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
    Float32,
    Float64,
    Date,
    DateTime,
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
            Self::Float32 => "float32",
            Self::Float64 => "float64",
            Self::Date => "date",
            Self::DateTime => "datetime",
        }
    }
}
