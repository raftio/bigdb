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

//! A Redis stream into a bigdb table.
//!
//! `XREADGROUP` → send → flush → `XACK`, with the acknowledgement naming exactly the entries the
//! flush covered. See [`sink`] for why that order is the whole design.
//!
//! # What it promises
//!
//! **At-least-once.** A crash between bigdb taking a batch and Redis being told repeats that
//! batch, and because the server allocates the record ids, a repeat is new records rather than
//! the same ones written again. That is the trade `big-message` documents and this inherits;
//! `--dedup-field` closes the restart half of it, and nothing closes the rest.
//!
//! # What it will not do
//!
//! Guess a mapping. A stream entry's fields and a table's columns are two vocabularies chosen by
//! two different people, and matching them by name because they happen to agree is a rule that
//! works until the day it silently does not. The mapping is declared or the sink does not start.

pub mod resp;
pub mod sink;
pub mod stream;

pub use sink::{Config, Kind, Mapping, Report, Sink, Stopped};
pub use stream::{Entry, Redis};
