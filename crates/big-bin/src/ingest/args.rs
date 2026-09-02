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

//! What a load is made of: its two verbs, where its lines come from, and its knobs.
//!
//! The parsing itself is in [`crate::client::args`], which is the only parser `bigctl` has.
//! It used to be two, one per binary, agreeing by hand that `--addr`, `--token-file` and
//! `--timeout` meant the same thing on both. Agreement by hand is the arrangement that
//! eventually stops agreeing.

/// How much of a chunk a request carries by default.
///
/// The server's ceiling is `big_http::MAX_BODY`, 8 MiB. This sits below it rather than on it: a
/// request that is refused with `413` costs the whole chunk, and the megabyte of headroom is
/// cheap insurance against a proxy in front of `big serve` with a smaller idea of large. It is a
/// throughput knob and not decoration - the server commits once per request, so doubling this
/// halves the number of commits. `--chunk-bytes` raises it for a deployment that measured.
pub const DEFAULT_CHUNK_BYTES: usize = 7 << 20;

/// The second ceiling, generous on purpose: the byte ceiling is meant to be the one that binds,
/// and this is here so that a file of two-byte lines cannot put four million facts in one
/// commit and call it a chunk.
pub const DEFAULT_CHUNK_LINES: usize = 1_000_000;

/// How many times a *transport* failure is tried again. Never a refusal - see `run`.
pub const DEFAULT_RETRIES: u32 = 3;

/// Requests in flight by default: one, which is a strictly sequential load.
///
/// **Two, because one leaves both ends idle and the second is worth 1.6x.**
///
/// With a single request outstanding the load is strictly alternating: the client waits while
/// the server parses and commits, then the server waits while the client reads and sends. A
/// second request lets the server parse one body while it commits the one before, which is the
/// only overlap available - the engine has one writer, so this does not make writes concurrent.
///
/// Two million records over HTTP, bitmap engine, `durability full`, two passes:
///
/// | in flight | | | records/s |
/// |---|---|---|---|
/// | 1 | 9.4s | 10.3s | ~203,000 |
/// | **2** | 6.3s | **5.8s** | **~331,000** |
/// | 3 | 5.8s | 5.8s | ~345,000 |
/// | 4 | 6.6s | 5.7s | ~327,000 |
/// | 6 | 6.4s | 5.7s | ~331,000 |
///
/// A third is inside the noise and a fourth is nothing at all, which is what the single writer
/// predicts: past the one body being parsed ahead, another request can only queue. So the
/// default is the first step and not the largest number that still helps.
///
/// **What it costs is how much a resume repeats.** A checkpoint is one offset meaning
/// "everything before this is written", so it may only advance across a contiguous run of
/// acknowledged chunks. With one request outstanding a killed run has sent exactly what it
/// acknowledged plus one; with two it has sent one more, and the resumed run sends it again.
/// Nothing is lost - a fact is a bit set at a record id the caller chose, so a resend writes
/// what the first send wrote - and the repeat is bounded by one `--chunk-bytes`. That is a
/// smaller thing to explain to an operator than why a load takes half again as long as it need.
pub const DEFAULT_IN_FLIGHT: usize = 2;

/// Past this, an in-flight window is holding more memory than any overlap it can buy.
pub const MAX_IN_FLIGHT: usize = 8;

/// The two routes this binary sends to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Verb {
    Import,
    Delete,
}

impl Verb {
    /// The last segment of the route, which is also the word on the command line.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Import => "import",
            Self::Delete => "delete",
        }
    }

    /// What one line of this route's body is, for the summary a person reads.
    pub fn noun(self) -> &'static str {
        match self {
            Self::Import => "imported",
            Self::Delete => "deleted",
        }
    }
}

/// Where the lines come from.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Input {
    /// A path, which can be seeked and therefore resumed.
    Path(String),
    /// `-`. What makes `bigctl` the far end of a pipe, at the cost of `--resume`.
    Stdin,
}

/// The knobs that only a load has, already resolved.
///
/// Separate from the client's [`crate::client::Options`] because these are the *subcommand's*,
/// not the run's: `--addr` means the same thing to every command, and `--chunk-bytes` means
/// nothing to any command but these two. `only()` in the parser is what keeps that true.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Load {
    pub chunk_bytes: usize,
    pub chunk_lines: usize,
    pub resume: Option<String>,
    pub retries: u32,
    /// Requests allowed to be waiting on the server at once.
    ///
    /// One is the loop this used to be: send, wait, send. The server parses a body before it
    /// can commit it, and those are different resources - so a second request in flight lets
    /// the parse of one overlap the commit of the last. It cannot make the *writes* concurrent,
    /// because the engine has one writer by design.
    pub in_flight: usize,
    /// `None` means "decide from whether stderr is a terminal".
    pub progress: Option<bool>,
    pub dry_run: bool,
}

impl Default for Load {
    fn default() -> Self {
        Self {
            chunk_bytes: DEFAULT_CHUNK_BYTES,
            chunk_lines: DEFAULT_CHUNK_LINES,
            resume: None,
            retries: DEFAULT_RETRIES,
            in_flight: DEFAULT_IN_FLIGHT,
            progress: None,
            dry_run: false,
        }
    }
}
