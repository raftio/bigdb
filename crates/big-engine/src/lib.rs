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
//! Everything that decides *what a table writes for every fact it takes* lives here, and one
//! module per engine:
//!
//! - [`bitmap`] - fragments of roaring containers, plus the field conventions ([`bitmap::field`])
//!   that decide which rows a value turns on. Answers *which records* without reading values.
//! - [`columnar`] - column segments, a field's values in record order, block-encoded. Answers
//!   *what a record holds* without reconstructing it from bit planes.
//! - [`hybrid`] - both, over the same facts. The default, and a composition rather than a third
//!   implementation.
//!
//! Below all three sit the parts none of them owns: [`coords`], the shard and record arithmetic
//! the cluster partitions on too, and the four crates this one is built over - `big-container`,
//! `big-page`, `big-pager`, `big-btree`. Those stay outside because they are not engines. Every
//! engine here uses all of them.
//!
//! # The registry
//!
//! [`engine`] holds the [`Engine`] trait and [`ENGINES`], the list of the ones this build has.
//! [`TableEngine`] is a handle into that list - the value the catalog stores, the cluster wire
//! carries and the write path branches on. Adding an engine is a module and one line in
//! [`ENGINES`]; see [`engine`] for what that does and does not buy.

#![deny(unsafe_code)]

pub mod bitmap;
pub mod columnar;
pub mod coords;
pub mod engine;
pub mod hybrid;

pub use engine::{Engine, TableEngine, ENGINES};

// The addressing every engine and the cluster share, at the root rather than inside one of them.
pub use coords::*;

// The vocabulary of a fragment's address, which is `big-page`'s byte layout given meaning here.
// Re-exported at the root because it is what `big-db`, `big-cluster` and both engines all speak.
pub use big_container::Container;
pub use big_page::{ContainerKey, FragmentKey};

pub use bitmap::{FragmentRead, FragmentWrite, RowSet};
pub use columnar::{ColumnRead, ColumnWrite};
