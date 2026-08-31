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

//! What every engine uses and no engine owns.
//!
//! The split this module exists to make is between *an* engine and *the* engine: [`coords`] is
//! how a record is addressed, and [`engine`] is how an engine says what it is. Neither belongs to
//! [`crate::bitmap`] or [`crate::columnar`] - a fact that the columnar block number and the
//! cluster's shard ownership both derive from the same arithmetic is exactly why it is here.
//!
//! # Adding an engine
//!
//! A module beside [`crate::bitmap`], a unit struct implementing [`engine::Engine`], and one line
//! in [`engine::ENGINES`]. **Nothing in `big-db` changes.**
//!
//! That last sentence is the point of [`engine::Engine::place`]. The write path used to ask
//! `has_bitmap()` and `has_columns()` at nine separate setters and do the buffering itself, so
//! the *description* of an engine lived here and the *decision* lived up there - and a fourth
//! engine meant finding all nine. Now `big-db` owns the buffers and hands them over as a
//! [`engine::Sink`]; the engine says what to put in them.
//!
//! Everything else follows from the list. [`engine::TableEngine::from_u8`], `parse`, `all` and
//! the error message that names the engines all read [`engine::ENGINES`] rather than repeating
//! it - including the conformance suite in `crates/big-db/tests/engines.rs`, which iterates
//! [`engine::TableEngine::all`] and starts exercising a new engine without being told to.
//!
//! # What is left outside, and why
//!
//! Three questions in `big-db` still ask an engine what it keeps rather than telling it what to
//! do, and they are all *read routing* rather than policy: whether a point read comes from a
//! column or from bit planes, whether a predicate is answered by an index or by a scan, and
//! whether a bulk load can write everything the table stores. Those are the planner asking a
//! description, which is what [`engine::Engine::has_bitmap`] and
//! [`engine::Engine::has_columns`] are for - a new engine answers them, it does not edit them.
//!
//! What none of this buys is a genuinely new *format*. Those two capabilities describe the two
//! kinds of tree this build knows how to maintain; an engine storing something that is neither
//! needs code in `big-db` as well. The registry buys the identity, the naming and the routing.

pub mod coords;
pub mod engine;
pub mod field_kind;

pub use coords::*;
pub use engine::{Engine, TableEngine, ENGINES};
pub use field_kind::FieldKind;
