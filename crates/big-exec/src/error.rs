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

//! Where a query can fail once it is well formed.

use big_db::DbError;
use big_plan::PlanError;

#[derive(Debug)]
pub enum ExecError {
    /// The text was not a query, or did not describe anything real.
    Plan(PlanError),
    /// The query was fine; storage could not answer it.
    Db(DbError),
    /// A pair grouping whose outer column holds more values than the plan allowed.
    ///
    /// **Refused rather than truncated.** Grouping by two columns is one pass over the inner
    /// one per value of the outer, so the number of those values is what it costs - and cutting
    /// the list to fit would answer with fewer groups than exist, which nothing in the answer
    /// could show.
    /// More calendar buckets than the plan allowed.
    ///
    /// **Refused rather than truncated**, for the reason `TooManyGroups` gives: an answer with
    /// fewer buckets than the data spans is a different answer, and nothing in it could show
    /// which ones were dropped. The remedy is a different one, though, which is why this is not
    /// that variant: a coarser boundary, or a narrower `WHERE`.
    TooManyBuckets {
        /// The column being bucketed.
        field: String,
        /// How many the plan allowed.
        limit: usize,
    },
    TooManyGroups {
        /// The outer column.
        field: String,
        /// How many values it holds among the records selected.
        found: usize,
        /// How many the plan allowed.
        limit: usize,
    },
}

impl From<PlanError> for ExecError {
    fn from(e: PlanError) -> Self {
        Self::Plan(e)
    }
}

impl From<DbError> for ExecError {
    fn from(e: DbError) -> Self {
        Self::Db(e)
    }
}

impl core::fmt::Display for ExecError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Plan(e) => write!(f, "{e}"),
            Self::Db(e) => write!(f, "{e}"),
            Self::TooManyBuckets { field, limit } => write!(
                f,
                "grouping `{field}` by a calendar boundary is one range read per bucket, and the \
                 values here span more than the {limit} this allows. Group by a coarser \
                 boundary, or narrow the range with a `WHERE`"
            ),
            Self::TooManyGroups { field, found, limit } => write!(
                f,
                "grouping by `{field}` and a second column is one pass over the second per \
                 value of `{field}`, and `{field}` holds {found} of them here against a limit \
                 of {limit}. Narrow the query, or group by a column with fewer values"
            ),
        }
    }
}

/// Delegated in both arms: this enum only records which half of the query failed, and that is
/// not a distinction a client acts on.
impl ExecError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Plan(e) => e.code(),
            Self::Db(e) => e.code(),
            Self::TooManyBuckets { .. } => "too_many_buckets",
            Self::TooManyGroups { .. } => "too_many_groups",
        }
    }
}

impl core::error::Error for ExecError {}

pub type Result<T> = core::result::Result<T, ExecError>;
