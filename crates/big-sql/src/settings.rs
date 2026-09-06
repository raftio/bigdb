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

//! `SETTINGS max_execution_time = 30`: what one statement may spend, written on the statement.
//!
//! **A clause rather than a session, and that is the whole design.** `USE` is refused here
//! because this surface remembers nothing between statements - one statement is one request,
//! answered and forgotten - and a `SET` that stuck a limit to a connection would need exactly
//! the memory `USE` was refused for wanting. So the limit travels with the statement that is
//! bounded by it, which is also the only way a limit survives a client that reconnects, a proxy
//! that pools connections, or a retry on another node.
//!
//! # Why a struct and not a map
//!
//! A map of name to value would accept `max_execution_tim = 30` and answer a query that runs
//! forever, having been told to stop. The typed struct makes the accepted set closed at compile
//! time, so an unrecognised key is refused by name at the key - the argument
//! [`crate::error::Refused::Round`] already makes about typed values, applied to typed names.
//! It is also what lets `EXPLAIN` print the settings back: a printer over three `Option`s cannot
//! disagree with the parser about which keys exist.
//!
//! # Every value is a ceiling the caller may lower and never raise
//!
//! What is written here is not what the statement gets; it is the most it may ask for. The
//! operator's configured limits still apply, and `big-embed` takes the minimum of the two. A
//! statement asking for more than the server allows is not a wrong statement - the author of a
//! query has no way to know what the server was configured with - so it is clamped rather than
//! refused, and `EXPLAIN` prints the number that will actually be enforced.

/// The limits a statement may write on itself.
///
/// Four, because these are the four that exist end to end: two are `big_db::QueryLimits`, one is
/// the deadline `DbRead` checks at every fragment, and the fourth bounds the half of a delete
/// that no deadline can. A key naming a knob nothing reads would be a promise the engine does
/// not keep, which is why `max_threads`, `max_block_size` and `join_algorithm` are refused
/// rather than accepted and ignored.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Settings {
    /// Seconds the statement may run before `DbRead::checkpoint` aborts it.
    ///
    /// A read only. The selection half of a write is bounded by this; the applying half is one
    /// transaction and nothing interrupts one, which is why a write is bounded by a count
    /// instead.
    pub max_execution_time: Option<u64>,
    /// Bytes of bitmap the statement may hold at once.
    pub max_memory_usage: Option<u64>,
    /// Records the statement may read back.
    ///
    /// The bound an unlimited projection meets: `SELECT *` with no `LIMIT`, and any `ORDER BY`
    /// over one, reads every record the `WHERE` matched.
    pub max_result_rows: Option<u64>,
    /// Records one `DELETE` may clear.
    ///
    /// The one key here that bounds a *write*. It exists because the other two cannot: the
    /// clearing half of a delete is a single transaction and neither a deadline nor a memory
    /// ceiling interrupts one, so what bounds it is a count taken before it starts.
    pub max_delete_records: Option<u64>,
}

impl Settings {
    /// Whether the statement wrote any of them.
    ///
    /// Used by the printers to leave the line out entirely rather than print a row of `None`s, and
    /// by the parser to decide whether a wrapper is needed at all - a statement with no
    /// `SETTINGS` is the statement it was before this clause existed, byte for byte in every
    /// test that pins one.
    #[must_use]
    pub fn is_empty(self) -> bool {
        self.max_execution_time.is_none()
            && self.max_memory_usage.is_none()
            && self.max_result_rows.is_none()
            && self.max_delete_records.is_none()
    }

    /// The keys this dialect reads, for the message that lists them.
    ///
    /// Here rather than spelled out in `Refused::why` so that a key added to the struct and not
    /// to the sentence is a change in one file rather than a sentence that quietly goes stale.
    pub const KEYS: [&'static str; 4] =
        ["max_execution_time", "max_memory_usage", "max_result_rows", "max_delete_records"];
}
