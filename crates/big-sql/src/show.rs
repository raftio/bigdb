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
    /// `SHOW CREATE [TABLE | VIEW] t`: one row, holding the statement that would recreate it.
    Create {
        database: Option<String>,
        table: String,
        /// `VIEW` was written, so a table under that name is the wrong object rather than the
        /// answer. Unwritten, the name is looked up as either - which is what somebody typing
        /// `SHOW CREATE x` means, and what makes the bare form useful for exploring.
        view: bool,
    },
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
            Self::Databases => {}
        }
    }
}
