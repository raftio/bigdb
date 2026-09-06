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

//! Questions about the catalog rather than about the records.
//!
//! # Why this is not a query
//!
//! A [`crate::Statement`] is calls, probes and a shape, and every cell of that shape names a
//! plan by index. A listing of fields indexes into nothing: no plan produced it, no fan-out
//! merged it, and the answer is already sitting in the catalog every node holds. Folding it
//! into a `Statement` would mean an empty `calls` list under a `Shape` naming plans that do not
//! exist - which is a fiction the reader would then have to keep in mind everywhere.
//!
//! # Why it is not a schema change either
//!
//! Because of what it costs a caller: `POST /sql` raises its role to `admin` for a statement
//! that changes the schema, and describing a table is a read. Being its own variant is how that
//! decision gets made once, in a `match` the edge cannot skip.

use crate::shape::Format;

/// One question about the catalog.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Show {
    pub what: Shown,
    /// `FORMAT`, which changes the bytes and nothing else - the same clause a `SELECT` takes,
    /// so that `SHOW TABLES FORMAT TSV` pipes into a shell like everything else here.
    pub format: Format,
}

/// Which question it is.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Shown {
    /// `DESCRIBE [TABLE] t`, `DESC t`, `SHOW COLUMNS FROM t`: one row per field.
    ///
    /// Three spellings and one meaning, because they are three dialects' words for the same
    /// question and there is nothing to be gained by knowing which one was typed.
    ///
    /// A view answers this too, with the columns it exposes rather than the base table's -
    /// which is what makes a view visible to a client that draws a column tree.
    Columns { database: Option<String>, table: String },
    /// `SHOW TABLES [FROM <database>]`: one row per table **and one per view**, in one database.
    ///
    /// Both, under a `type` column saying which. That is what a JDBC driver asks for and what it
    /// expects back; a listing that hid views would make one invisible to every BI tool while
    /// still being queryable, which is the worst of the two answers.
    Tables { database: Option<String> },
    /// `SHOW VIEWS [FROM <database>]`: one row per view, with the statement it holds.
    ///
    /// Not redundant with `SHOW TABLES`, which says a view exists but not what it means. This is
    /// the listing an operator reads to find the view that is about to break.
    Views { database: Option<String> },
    /// `SHOW DATABASES`, also spelled `SCHEMAS` and `DATASETS`: one row per database.
    ///
    /// The question every JDBC driver and BI tool opens with, which is most of why a database
    /// level exists at all - a client cannot draw a table tree without it.
    Databases,
    /// `SHOW ROLES`: one row per role.
    ///
    /// The reserved `superuser` is in the listing because it is true, not because it is stored -
    /// the same way `default` appears in `SHOW DATABASES` without a record behind it.
    Roles,
    /// `SHOW GRANTS [FOR <role>]`: one row per object a role has been granted anything on.
    ///
    /// `None` means the caller's own role, which is the only form that needs no privilege:
    /// reading what you yourself hold tells you nothing you could not find out by trying.
    Grants { role: Option<String> },
    /// `SHOW CREATE [TABLE | VIEW] t`: one row, holding the statement that would recreate it.
    Create {
        database: Option<String>,
        table: String,
        /// `VIEW` was written, so a table under that name is the wrong object rather than the
        /// answer. Unwritten, the name is looked up as either - which is what somebody typing
        /// `SHOW CREATE x` means, and what makes the bare form useful for exploring.
        view: bool,
    },
    /// `SHOW PROCESSLIST`: the queries running on the node that answers.
    ///
    /// **Node-local, and the answer says so with a `node` column.** A coordinator holds the
    /// ranges it owns; a cluster-wide listing would be a fan-out, and one that showed peers'
    /// *legs* of a fanned-out query as separate rows would be more confusing than useful. The
    /// honest answer is the local one under a name that admits it.
    Processlist,
    /// `SELECT * FROM system.tables`: the catalog, asked about in the language everything else
    /// is asked about in.
    ///
    /// **A `Shown` and not a query, even though it is written as one.** Every cell of a
    /// [`crate::Shape`] names a plan by index, and no plan produces this: the answer is in the
    /// catalog each node already holds. It joins the listings above rather than becoming the
    /// first `Statement` with an empty `calls` list under a shape naming plans that do not
    /// exist.
    ///
    /// It is a *separate variant* from [`Shown::Tables`] rather than a second spelling of it,
    /// and the reason is [`Shown::fill_database`]: a bare `SHOW TABLES` means the request's
    /// database, and `system.tables` means every database. Reusing the variant would make the
    /// question quietly narrower than it reads, which is the kind of wrong answer nobody checks.
    System {
        view: SystemView,
        /// `WHERE database = '<name>'`, the one filter these views take.
        database: Option<String>,
        /// `WHERE table = '<name>'`, accepted on [`SystemView::Columns`] only.
        table: Option<String>,
        /// The columns the select list named, or `None` for `*`.
        ///
        /// `*` here means *every column*, which is the opposite of what it means over a real
        /// table - there it is the record id. That is why the parser decides which kind of
        /// statement this is from the `FROM` before it reads the select list at all.
        columns: Option<Vec<String>>,
    },
}

/// The database name this build's own views live under.
///
/// A constant rather than a literal in three files, because it is read in two places that must
/// agree: the parser forks on it before the select list, and `CREATE DATABASE` refuses it.
pub const SYSTEM_DATABASE: &str = "system";

/// Which view under `system.` was named.
///
/// A closed enum rather than a string, so that a name this build does not have is refused at the
/// name - by [`crate::Refused::SystemTable`], which lists the ones that exist - instead of
/// reaching a layer that would have to answer "no such thing" in some other vocabulary.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SystemView {
    /// Every table and view, in every database.
    Tables,
    /// Every column of every table.
    Columns,
    /// Every database.
    Databases,
    /// **Every fragment**, which is the one thing here no other statement can show.
    ///
    /// `system.tables` repeats `SHOW TABLES`; this does not repeat anything. A fragment is
    /// `(table, field, view, shard)`, and being able to list them is what turns "the query is
    /// slow" into a question with an answer.
    Parts,
}

impl SystemView {
    /// The name written after `system.`, for the message that lists them.
    ///
    /// Here rather than spelled out in `Refused::why` so that a view added to the enum and not
    /// to the sentence is a change in one file rather than a sentence that goes quietly stale.
    pub const NAMES: [&'static str; 4] = ["tables", "columns", "databases", "parts"];

    /// The view a name after `system.` refers to, if it is one.
    #[must_use]
    pub fn of(name: &str) -> Option<Self> {
        Some(match name.to_ascii_lowercase().as_str() {
            "tables" => Self::Tables,
            "columns" => Self::Columns,
            "databases" => Self::Databases,
            "parts" => Self::Parts,
            _ => return None,
        })
    }

    /// The name it was written under.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tables => "tables",
            Self::Columns => "columns",
            Self::Databases => "databases",
            Self::Parts => "parts",
        }
    }
}

impl Shown {
    /// Fills in the database this question did not name. See [`crate::translate_in`].
    ///
    /// `SHOW DATABASES` and a bare `SHOW TABLES` are deliberately untouched: neither is a
    /// question *about* one database. `SHOW TABLES` against a request means that request's
    /// database, which the layer answering it already knows.
    pub fn fill_database(&mut self, database: &str) {
        match self {
            Self::Columns { database: d, .. } | Self::Create { database: d, .. } => {
                d.get_or_insert_with(|| database.to_string());
            }
            Self::Tables { database: d } | Self::Views { database: d } => {
                d.get_or_insert_with(|| database.to_string());
            }
            // Neither is about one database: a role is server-wide, which is what lets one
            // grant reach across two of them.
            Self::Databases | Self::Roles | Self::Grants { .. } | Self::Processlist => {}
            // **Deliberately untouched, and this is the whole reason it is its own variant.** A
            // system view is a question about the server, not about the request's database, so
            // filling one in would silently turn `SELECT * FROM system.tables` into a listing of
            // one database - an answer that looks complete and is not. The `database` field here
            // is a *filter somebody wrote*, never one inherited.
            Self::System { .. } => {}
        }
    }
}
