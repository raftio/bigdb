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

//! The two binaries, and the halves they are made of.
//!
//! [`serve`] and [`offline`] are `big`: one holds the file open and answers over HTTP, the
//! other takes the exclusive lock and works on the file directly. [`client`] and [`ingest`] are
//! `bigctl`: one subcommand is one route, except `import` and `delete`, which are one *file*
//! cut into as many requests as it takes.
//!
//! The logic lives here rather than under `src/bin/` so that the tests drive exactly what a
//! user drives - `main` is only the real streams, the real environment, and an exit code.
//!
//! **What the client may not do**, now that the dependency graph no longer forbids it: parse a
//! statement, validate one, or answer anything offline. A statement travels as bytes and an
//! error comes back as the server's own code and the server's own sentence, so `sql_no_joins`
//! means on the command line exactly what it means over HTTP, because it *is* the same string.
//! A second surface drifts from the first, and the second one always loses.

#![deny(unsafe_code)]

pub mod client;
pub mod ingest;
pub mod offline;
pub mod serve;

use std::io::{BufRead, Write};

/// Exit codes, which are part of the surface: a script branches on them.
///
/// Unchanged by the rename from `bigc` to `bigctl` - see `docs/versioning.md`, which promises
/// exactly these four and nothing about the name in front of them.
pub mod exit {
    /// The server answered.
    pub const OK: i32 = 0;
    /// The server refused. The code and the sentence are on stderr.
    pub const REFUSED: i32 = 1;
    /// The command line was wrong, or an input could not be read.
    pub const USAGE: i32 = 2;
    /// Nothing was listening, or the exchange did not complete.
    pub const UNREACHABLE: i32 = 3;
}

/// The streams a run works over, so that a test can supply its own.
///
/// **Two terminal flags, not one.** They used to be one field each in two crates that meant
/// different things by it, and merging them into a single `tty` would have been the kind of
/// tidying that silently changes behaviour: the format is chosen by where the *answer* goes,
/// and the progress bar by where the *noise* goes. A run whose table is piped to a file still
/// has a person watching its progress, and that person is looking at stderr.
pub struct Io<'a> {
    pub input: &'a mut dyn BufRead,
    pub out: &'a mut dyn Write,
    pub err: &'a mut dyn Write,
    /// Whether `out` is a terminal, which decides the default format and whether the shell
    /// prints a prompt. Passed in rather than asked, because `out` may not be a terminal *or* a
    /// pipe - in a test it is a `Vec<u8>`.
    pub out_tty: bool,
    /// Whether `err` is a terminal, which decides whether a load redraws one progress line or
    /// writes plain ones.
    pub err_tty: bool,
}
