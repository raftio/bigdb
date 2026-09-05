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

#![deny(unsafe_code)]

//! One request off a socket, one response back, and the log line between.
//!
//! This is the half of `big-http` that has nothing to do with a database: the HTTP/1.1 parser
//! and its ceilings, the response encoder, the structured logger, and the two JSON primitives
//! everything else is built from. `big-http` re-exports all of it, so nothing that already
//! depended on it had to change.
//!
//! **It links no engine, and CI asserts that.** The reason is `big-proxy`, which speaks HTTP to
//! clients and to nodes and touches no data at all. While the parser lived beside the router, a
//! proxy that wanted it linked the engine, the SQL planner and argon2 as well — and "this
//! process never verifies a password" stayed a claim in a readme rather than a fact about the
//! dependency graph. `cargo tree -p big-proxy -e normal` is now the proof.
//!
//! Its one dependency is `big-tls`, for the base64 decoder an `Authorization: Basic` header
//! needs. With the `tls` feature off that crate has no dependencies either, so the whole tree
//! under this one is two crates and the standard library.
//!
//! Splitting it out cost one thing, and it is worth naming: `Response::from_error` classifies an
//! engine error and so cannot live here. It is `big_http::status::response_for` instead — a name
//! rather than a link, because `big-http` depends on this crate and a link the other way would
//! be a dependency this crate must not have.

pub mod json;
pub mod log;
pub mod request;
pub mod response;

pub use request::{Basic, Request, RequestError};
pub use response::{reason_for, Chunked, Response, Streaming, LAST_CHUNK};

/// Largest request body accepted, so a single client cannot ask the process to allocate
/// without bound.
pub const MAX_BODY: usize = 8 << 20;

/// The same, for the `/internal/` routes one node uses to reach another.
///
/// Larger because the body is not a stranger's: it is what a coordinator made of a request
/// that had already passed [`MAX_BODY`], and the binary encoding of a batch of facts runs to
/// roughly twice the text it was parsed from - a length and a tag per field where the text had
/// a space. A public body that grew into that headroom is still refused; only a peer's is not.
pub const MAX_INTERNAL_BODY: usize = 4 * MAX_BODY;
