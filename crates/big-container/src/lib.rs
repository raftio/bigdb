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

//! Roaring containers: the set of up to 65,536 integers everything above is built out of.
//!
//! Three representations - array, bitmap, run - and which one a container uses is a consequence
//! of its own contents rather than a choice the caller makes. Nothing here knows it is
//! persisted: `big-page` decides how a container is laid out in bytes and `big-btree` decides
//! which one answers a given key.

#![deny(unsafe_code)]

pub mod builder;
pub mod container;
pub mod delta;
mod interval;
pub mod ops;
pub mod optimize;

pub use container::{Container, ContainerRef, ContainerType, Iter, OpResult};
pub use delta::{DeltaEntry, DELTA_ENTRY_BYTES, MAX_DELTA};
pub use interval::Interval;
pub use ops::cardinality::and_cardinality;
pub use ops::{and, andnot, apply, fold, or, xor, SetOp};
pub use optimize::{optimize, run_count, to_runs, Caps};

/// Number of u64 words in a dense bitmap container.
pub const BITMAP_WORDS: usize = 1024;
/// A dense container occupies a whole 8192-byte page.
pub const BITMAP_BYTES: usize = BITMAP_WORDS * 8;
/// The *economic* threshold: past this an array stops being smaller than a bitmap.
/// The *physical* ceiling lives in `big-page`.
pub const ARRAY_BREAK_EVEN: usize = BITMAP_BYTES / 2;

const _: () = assert!(BITMAP_BYTES == 8192);
