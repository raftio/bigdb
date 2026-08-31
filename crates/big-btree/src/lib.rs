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

//! A copy-on-write b-tree whose leaves hold roaring containers.
//!
//! Generic over `Pager` for reads, so the whole thing is testable against an in-memory pager
//! without a file anywhere in sight.

#![deny(unsafe_code)]

pub mod error;
pub mod item;
pub mod read;
pub mod walk;
pub mod write;

pub use error::{BTreeError, Result};
pub use item::LeafItem;
pub use read::{collect, count, find, find_many, leaf_for, scan, Found, MAX_DEPTH};
pub use walk::{copy_tree, free_tree, scrub_tree, visit_tree, Scrubbed};
pub use write::{build, make_item, put, put_container, put_containers, put_many, remove, CAPS};
