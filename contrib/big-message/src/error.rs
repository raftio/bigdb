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

//! What can go wrong, split by **what the caller can do about it**.
//!
//! The split that matters is not client-side versus server-side. It is whether the write may
//! have landed: [`Error::Unknown`] is the only variant that leaves that question open, and it is
//! the only one that can produce a duplicate record. Everything else either never reached the
//! server ([`Error::Connect`]) or was refused by it without writing ([`Error::Refused`]).

use core::fmt;

/// A refusal, or a failure, from one flush.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Error {
    /// The request never reached the server, and this is known rather than assumed: the
    /// connection could not be opened, or the request was still being written when it failed.
    ///
    /// Safe to retry, and the producer retries it itself before reporting one.
    Connect(String),

    /// The request was written in full and the outcome is not known.
    ///
    /// **This is the whole price of letting the server allocate record ids.** A read timeout, a
    /// connection reset, an end of file where a response should have been - each of them looks
    /// identical whether the server died before running the statement or after committing it.
    /// The loader can retry its equivalent because every chunk it sends is idempotent; an
    /// allocating `INSERT` is not, so retrying here would write the batch twice as often as it
    /// would recover it.
    ///
    /// The producer stops. What to do next is a question about the caller's data - whether a
    /// duplicate matters, whether there is a key to check against - and this crate does not
    /// have enough to answer it.
    Unknown(String),

    /// The server understood the statement and refused it, in its own words.
    ///
    /// Never retried: a statement the server rejected will be rejected identically the second
    /// time, and sending it again only delays the sentence somebody needs to read.
    Refused { status: u16, code: String, message: String },

    /// One message renders to more bytes than a whole request may carry, so no batching can
    /// make it sendable.
    ///
    /// Names which message, because a producer that has sent a million of them needs to know
    /// which one rather than that one exists.
    MessageTooLarge { at: usize, len: usize, cap: usize },

    /// A value or a name this crate will not put into a statement.
    ///
    /// Caught here rather than left to the server for the ones where the server's answer would
    /// arrive without the context to place it: a `NaN` in the four-hundredth message of a batch
    /// comes back as one refusal for the whole statement.
    Value(String),

    /// The bytes on the connection were not an HTTP response this crate can read.
    Protocol(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Connect(why) => write!(f, "{why}"),
            Self::Unknown(why) => {
                write!(f, "{why}; the batch may or may not have been written")
            }
            Self::Refused { status, code, message } => {
                write!(f, "{status} {code}: {message}")
            }
            Self::MessageTooLarge { at, len, cap } => {
                write!(f, "message {at} is {len} bytes, past the {cap} a request may carry")
            }
            Self::Value(why) => write!(f, "{why}"),
            Self::Protocol(why) => write!(f, "{why}"),
        }
    }
}

impl std::error::Error for Error {}
