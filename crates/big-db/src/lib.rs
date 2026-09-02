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

//! Schema, catalog and the handle everything else hangs off.
//!
//! Where names become ids and a fact becomes bits in fragments. Three write paths, because they
//! make different trades: [`DbWrite`] is one transaction committed by hand, [`Ingest`] buffers
//! and commits itself, and [`BulkLoad`] refuses rather than merges into a fragment that already
//! holds data - because merging means reading it back, and not reading it back is why that path
//! exists.
//!
//! Buffering is not an optimisation here. A commit rewrites the catalog and the root records for
//! *every* fragment the database holds, not only the ones it touched, so a commit per record is
//! the engine's worst case by four orders of magnitude.
//!
//! [`copy`] is one primitive wearing three names: backup, full compaction and format migration
//! are the same walk over every page reachable from a consistent set of roots.

#![deny(unsafe_code)]

pub mod catalog;
pub mod copy;
pub mod db;
pub mod error;
pub mod ingest;
pub mod like;
pub mod matches;

pub use catalog::{
    Catalog, DatabaseId, FieldDef, FieldKind, FragmentMeta, TableDef, TableEngine, TableRef,
    DEFAULT_DATABASE, DEFAULT_DATABASE_NAME, EXISTS_FIELD, STANDARD_VIEW,
};
pub use db::{At, Cell, Db, DbRead, DbWrite, KeyStats, QueryLimits};
pub use ingest::Ingest;
pub mod bulk;
pub use big_engine::bitmap::{Container, ContainerKey};
pub use big_keys::KeyError;
pub use bulk::BulkLoad;
pub use db::{FragmentAddr, COLUMN_VIEW, MUTEX_SHADOW_VIEW};
pub use error::{DbError, Result};
pub use matches::Matches;
pub mod float;
pub mod signed;
// `day_view` names the boundary a retention drop actually used, which is the one thing a
// caller passing an instant cannot work out for itself.
pub use big_engine::bitmap::field::{day_view, Granularity, RangeOp};
pub use big_engine::bitmap::{FragmentKey, RowSet};
pub use big_engine::{RecordId, RowId, ShardId};
pub use big_pager::Durability;
