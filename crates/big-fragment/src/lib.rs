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

//! Fragment addressing and row access.
//!
//! A **fragment** is one b-tree; a **row** is a contiguous span of its container keys. Because
//! a row occupies a known contiguous range, reading one is a range scan rather than a lookup
//! per chunk, and unioning two is a merge of two scans.
//!
//! A fragment is addressed by [`FragmentKey`] - table, field, view, shard - and each of those
//! four exists because something partitions on it independently. In particular **shard** is what
//! bounds how much of the tree a write touches and what lets a read fan out across threads.

#![deny(unsafe_code)]

pub mod coords;
pub mod read;
pub mod rowset;
pub mod write;

pub use big_container::Container;
pub use big_page::{ContainerKey, FragmentKey};
pub use coords::*;
pub use read::FragmentRead;
pub use rowset::RowSet;
pub use write::FragmentWrite;
