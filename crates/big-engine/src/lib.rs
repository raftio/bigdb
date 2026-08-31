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

//! The storage engines, and the registry that names them.
//!
//! Everything that decides *what a table writes for every fact it takes* lives here, and the
//! layout says which is which: [`base`] is what no engine owns, and every other module is one
//! engine.
//!
//! - [`bitmap`] - fragments of roaring containers, plus the field conventions
//!   ([`bitmap::field`]) that decide which rows a value turns on. Answers *which records*
//!   without reading values.
//! - [`columnar`] - column segments, a field's values in record order, block-encoded. Answers
//!   *what a record holds* without reconstructing it from bit planes.
//! - [`hybrid`] - both, over the same facts. The default, and a composition rather than a third
//!   implementation.
//!
//! [`base`] holds the two things every engine needs and none of them owns: [`base::coords`], the
//! shard and record arithmetic the cluster partitions on too, and [`base::engine`], where an
//! engine says what it is. **See [`base`] for how to add one.**
//!
//! Below all of it sit four crates this one is built over - `big-container`, `big-page`,
//! `big-pager`, `big-btree`. Those stay outside because they are not engines: every engine here
//! uses all of them.

#![deny(unsafe_code)]

pub mod base;
pub mod bitmap;
pub mod columnar;
pub mod hybrid;

pub use base::engine::{Engine, TableEngine, ENGINES};
pub use base::field_kind::FieldKind;

// The addressing every engine and the cluster share, at the root as well as under `base`, because
// `big-cluster` partitions on a shard id without caring which engine produced it.
pub use base::coords;
pub use base::coords::*;

// The vocabulary of a fragment's address, which is `big-page`'s byte layout given meaning here.
// Re-exported at the root because it is what `big-db`, `big-cluster` and both engines all speak.
pub use big_container::Container;
pub use big_page::{ContainerKey, FragmentKey};

pub use bitmap::{FragmentRead, FragmentWrite, RowSet};
pub use columnar::{ColumnRead, ColumnWrite};
