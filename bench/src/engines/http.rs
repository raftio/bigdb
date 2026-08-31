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

//! Talking to a peer that runs as a server rather than as a library: the query goes out as the
//! body of an HTTP POST and the answer comes back as text.
//!
//! Blocking, and no TLS. The server is reached over loopback on a machine the harness is already
//! running on, so a TLS stack would be a dependency bought for nothing - and this crate's
//! `deny.toml` is deliberately strict about what gets into the tree. Blocking because the
//! [`Olap`] trait is synchronous, and pulling an async runtime in to satisfy one adapter would
//! put a reactor in the middle of everything being measured.
//!
//! [`Olap`]: crate::olap::Olap

use std::time::{Duration, Instant};

/// One server, reached over loopback.
pub struct Http {
    base: String,
    agent: ureq::Agent,
}

impl Http {
    pub fn new(base: impl Into<String>) -> Self {
        Self { base: base.into(), agent: ureq::Agent::new_with_defaults() }
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    /// POSTs `body` and returns the response, panicking with both the request and the server's
    /// complaint if anything goes wrong.
    ///
    /// Panicking rather than returning a `Result` on purpose: every call site is a benchmark
    /// question, and a question the server refused has no timing worth keeping. Carrying an
    /// error upward would only end in the report printing a number next to a query that never
    /// ran.
    pub fn post(&self, path: &str, body: &str) -> String {
        let url = format!("{}{path}", self.base);
        let preview: String = body.chars().take(200).collect();
        match self.agent.post(&url).send(body) {
            Ok(mut r) => r
                .body_mut()
                .read_to_string()
                .unwrap_or_else(|e| panic!("{url}: could not read the response body: {e}")),
            Err(e) => panic!("{url} rejected `{preview}`: {e}"),
        }
    }

    /// The cost of asking the server nothing.
    ///
    /// The floor under every other timing this engine reports, and the reason the analytical
    /// report keeps servers in their own table: an in-process engine pays none of it, so a
    /// column that mixed the two would be measuring the socket as much as the engine. Printed
    /// separately so a reader can subtract it rather than being asked to trust that it is small.
    pub fn round_trip(&self, path: &str, body: &str) -> Duration {
        // Once to connect and warm whatever the server caches, then the median of three, the
        // same shape every other timing in this harness uses.
        let _ = self.post(path, body);
        let mut times: Vec<Duration> = (0..3)
            .map(|_| {
                let t0 = Instant::now();
                let _ = self.post(path, body);
                t0.elapsed()
            })
            .collect();
        times.sort_unstable();
        times[1]
    }
}

/// Whether a server is listening and willing to answer, so the report can say "not run" with a
/// reason instead of taking the whole run down with it.
///
/// A missing server is an ordinary situation - the analytical peers are started by hand, and
/// the storage half of the report is run with Docker stopped - and it is not the same event as
/// a server that answered wrongly. The first prints a note; the second still fails loudly.
pub fn responds(base: &str, path: &str, body: &str) -> bool {
    ureq::Agent::new_with_defaults().post(format!("{base}{path}")).send(body).is_ok()
}

/// Reads a server's address from the environment, so a run against a remote box or a
/// non-default port needs no rebuild.
pub fn endpoint(var: &str, default: &str) -> String {
    std::env::var(var).unwrap_or_else(|_| default.to_string())
}
