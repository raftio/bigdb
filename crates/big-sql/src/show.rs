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
    Columns { table: String },
    /// `SHOW TABLES`: one row per table.
    Tables,
    /// `SHOW CREATE [TABLE] t`: one row, holding the statement that would recreate it.
    Create { table: String },
}
