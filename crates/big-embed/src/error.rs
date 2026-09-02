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

//! Everything a caller of this facade can be told.

// Both are carried inside `ApiError` variants, so a caller that matches on one has to be able
// to name what it holds. Neither crate is published; this re-export is the only way to it.
pub use big_db::DbError;
pub use big_exec::ExecError;
pub use big_sql::SqlError;

/// Everything a caller of this facade can be told.
///
/// The facade adds no failures of its own: a request failed on the way to storage, inside it,
/// or before either, because the statement was refused. [`ApiError::code`] is the stable
/// identifier; `Display` is the sentence.
///
/// **`Sql` is not a third kind of failure, it is a third *surface*.** A statement that names a
/// field which does not exist fails the same way in both languages and carries the same code -
/// see [`SqlError::Plan`]. What the variant records is which surface the text came through,
/// which is what lets a refusal that only SQL has (`sql_no_joins`) exist at all without the
/// query language growing an error it can never produce.
#[derive(Debug)]
pub enum ApiError {
    /// The query was not understood, or storage could not answer it.
    Query(ExecError),
    /// The SQL statement was refused, or did not describe anything real.
    Sql(SqlError),
    /// Schema or ingest failed.
    Db(DbError),
    /// A written value does not fit the field it was written to.
    ///
    /// Its own variant because it is neither a planning failure nor a storage one: the schema
    /// is fine, the statement is well formed, and the value is simply not something that field
    /// holds - `'GB'` into an integer, or three digits after the point on a decimal that keeps
    /// two. The sentence is built by `fact::ValueError::why`, which is where both write paths
    /// meet, so an import line and a SQL `INSERT` report the same mistake the same way.
    Value(String),
}

impl From<ExecError> for ApiError {
    fn from(e: ExecError) -> Self {
        Self::Query(e)
    }
}

impl From<SqlError> for ApiError {
    fn from(e: SqlError) -> Self {
        Self::Sql(e)
    }
}

impl From<DbError> for ApiError {
    fn from(e: DbError) -> Self {
        Self::Db(e)
    }
}

impl core::fmt::Display for ApiError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Query(e) => write!(f, "{e}"),
            Self::Sql(e) => write!(f, "{e}"),
            Self::Db(e) => write!(f, "{e}"),
            Self::Value(why) => write!(f, "{why}"),
        }
    }
}

/// Delegated, like `Display`. The facade adds no failures of its own.
impl ApiError {
    /// A stable, machine-readable identifier for this failure.
    ///
    /// Delegated to whichever error is inside, so the code a client sees does not change when
    /// a failure moves between layers.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Query(e) => e.code(),
            Self::Sql(e) => e.code(),
            Self::Db(e) => e.code(),
            // The code an import line with the same mistake already carries.
            Self::Value(_) => "malformed_line",
        }
    }
}

impl core::error::Error for ApiError {}

/// This crate's `Result`.
pub type Result<T> = core::result::Result<T, ApiError>;
