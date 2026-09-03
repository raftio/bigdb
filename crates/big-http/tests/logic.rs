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

//! The same corpus, over a socket.
//!
//! # Why one corpus and two ways of running it
//!
//! `big-cluster/tests/logic` is a few hundred statements and the answers they come to. Nothing
//! in those files is about HTTP - which is exactly what makes them worth running through it. The
//! route adds a decoder, a role check, a renderer and a status map, and a bug in any of them is
//! a bug in every one of those statements at once.
//!
//! This is the shape CockroachDB's logic tests have: one set of files, several *configurations*
//! that run them. The files stay a description of what the engine answers, and each
//! configuration is a claim that some other path answers the same.
//!
//! # Why this one checks against the engine rather than against the files
//!
//! The expected blocks in those files are a table of aligned columns, and this route answers in
//! JSON, TSV or CSV. Re-deriving one from the other would mean writing a JSON reader in a test,
//! and what it would then be checking is that reader.
//!
//! So the oracle here is the engine itself. Every statement goes to the server *and* to a local
//! cluster in the same order, so both hold the same data at every step, and what is asserted is
//! that the bytes coming back are the bytes the engine's own renderer produces for that answer.
//! A refusal is compared the same way: the same stable code, and a status that says whether it
//! is worth retrying.
//!
//! What this cannot claim is that the answers are *right* - that is the corpus's job, one layer
//! down, where they are written out and read. This one claims the route does not change them.

mod common;
use common::{send, spawn};

use big_cluster::{Cluster, ClusterError};
use big_embed::{Api, MemPager, QueryOptions};
use big_testfile::Case;

#[test]
fn the_corpus_answers_the_same_over_a_socket() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../big-cluster/tests/logic");
    // The server answers a fixed number of requests and then stops, so the count has to be known
    // before the first one. Counted from the files rather than guessed at, with room for the
    // handful of setup requests the harness makes.
    let mut state: Option<(std::path::PathBuf, Server)> = None;
    big_testfile::check(dir, move |case| {
        let fresh = state.as_ref().is_none_or(|(path, _)| path != &case.file);
        if fresh {
            state = Some((case.file.clone(), Server::for_file(&case.file)));
        }
        state.as_mut().unwrap().1.check(case)
    });
}

/// One file's worth of engine, in both places at once.
struct Server {
    addr: std::net::SocketAddr,
    /// The same statements, run locally, which is what the route's answer is compared against.
    local: Cluster<MemPager>,
}

impl Server {
    fn for_file(path: &std::path::Path) -> Self {
        let text = std::fs::read_to_string(path).unwrap();
        // Every non-blank, non-comment line that is not inside an expected block begins either a
        // directive or a statement; one request per directive is the bound, and the slack covers
        // the harness's own.
        let cases = text
            .lines()
            .filter(|l| l.trim_start().starts_with(|c: char| c.is_alphabetic()))
            .count();
        Self { addr: spawn(cases + 16), local: Cluster::solo(Api::in_memory().unwrap()) }
    }

    /// Runs one case both ways and answers with what disagreed, or with nothing.
    ///
    /// Nothing on agreement, deliberately: these files carry their expected blocks for the other
    /// configuration, so this one has to print nothing when it is happy or every case in every
    /// file would fail against a block written for a different question.
    fn check(&mut self, case: &Case) -> String {
        let sql = case.input.trim();
        if !matches!(case.directive.as_str(), "statement" | "exec" | "query" | "same" | "error") {
            return format!("unknown directive `{}`", case.directive);
        }

        let want = self.local.sql(sql, &QueryOptions::default());
        let (status, body) = send(self.addr, "POST", "/sql", sql);

        match want {
            Ok((set, format)) => {
                let rendered = big_http::json::result_set(format, &set);
                if status != 200 {
                    return format!("the engine answered and the route refused: {status} {body}");
                }
                if body != rendered {
                    return format!(
                        "the route rendered it differently\n  route:  {body}\n  engine: {rendered}"
                    );
                }
                String::new()
            }
            Err(e) => {
                if status == 200 {
                    return format!("the engine refused and the route answered: {body}");
                }
                let code = code_of(&e);
                if !body.contains(&format!("\"code\":\"{code}\"")) {
                    return format!("the route reported a different refusal\n  route:  {body}\n  engine: {code}");
                }
                // A status is what a proxy and a retry policy act on without reading the body,
                // so "refused forever" and "try again" must not look alike. Everything in this
                // corpus is refused for what it says, which is never worth retrying.
                if (500..600).contains(&status) {
                    return format!("`{code}` came back as {status}, which invites a retry");
                }
                String::new()
            }
        }
    }
}

/// The stable code a failure carries, reaching past the cluster's own `internal` the same way
/// the corpus's own runner does.
fn code_of(e: &ClusterError) -> String {
    match e {
        ClusterError::Local(e) => e.code().to_string(),
        other => other.code().to_string(),
    }
}
