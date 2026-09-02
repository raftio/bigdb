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

//! Messages from a running program into a bigdb table.
//!
//! `bigctl import` loads a **file**: it reads `field record value` lines and posts them to
//! `/table/{t}/import`, and every line names the record it writes to. This crate is for the
//! other shape - a program that produces events as it runs, with no file and no ids of its own.
//!
//! # The rule this crate is built around: it never names a record
//!
//! A record id is not a key the data chose. It is the address a bit is written at, and
//! `shard_of(record)` is also which node in a cluster owns it - see `big_engine::RecordId` and
//! `big_sql::insert`, where the reserved column is spelled `_record_id` precisely so that `id`
//! stays a name an ordinary schema may use. It is the engine's own coordinate, and a client
//! that invents one is a client that has started keeping the engine's books.
//!
//! So this crate writes through `POST /sql` with an `INSERT` that names no `_record_id`, and
//! the schema leader allocates - `big_cluster::Cluster::allocate`, one round trip per statement
//! rather than per row. The response is `{"columns":["inserted"],"rows":[[n]]}`: the ids are
//! not in it. A caller of this crate cannot learn a record id, cannot set one, and cannot be
//! made to care what one is.
//!
//! # What that costs, stated plainly
//!
//! `/table/{t}/import` is idempotent because the caller chooses the id: every fact is a `set`
//! at an address, so sending a chunk twice writes the same bits twice, which is writing them
//! once. That is what lets `bigctl import` retry a transport failure and resume from a byte
//! offset - see `docs/ingest-plan.md`, Decisions 4 and 6.
//!
//! **An allocating `INSERT` has none of that.** Sending it twice writes two records. So the
//! retry rule here is the opposite of the loader's, and it is the one thing in this crate worth
//! reading before anything else:
//!
//! > A request is retried only when it can be *proved* never to have reached the server.
//!
//! Once a request has been written in full, a failure is [`Error::Unknown`] - not a retry, not a
//! zero, not a guess. It stops the producer and it is the caller's to resolve. Every duplicate
//! this crate can produce comes through that one variant, which is why it has a name instead of
//! being folded into a general transport error.
//!
//! # What this crate does not do
//!
//! It does not read a schema, and it does not decide what a value means. A [`Value`] says what
//! kind of literal to write; `big_embed::fact::from_literal` on the server decides whether that
//! literal fits the field, and its refusal comes back as the server's own code and sentence.
//! That is the same division `bigctl` keeps, and it is why a field kind added to the engine
//! later needs nothing here.

mod batch;
mod config;
mod error;
mod http;
mod json;
mod producer;
mod sql;
mod value;

pub use config::{
    Config, DEFAULT_IO_TIMEOUT, DEFAULT_LINGER, DEFAULT_MAX_BYTES, DEFAULT_MAX_ROWS,
    DEFAULT_RETRIES,
};
pub use error::Error;
pub use producer::{Flushed, Producer};
pub use value::Value;
