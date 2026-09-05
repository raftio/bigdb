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

//! Following the cluster's membership, for a proxy that was told to.
//!
//! The list of upstreams a proxy starts with names the cluster as it was when the process
//! started. A node admitted after that is one no request can ever reach - so a front door in
//! front of a cluster that grows is a front door that has to be restarted to see the growth.
//! This is what removes that restart.
//!
//! **Off unless asked for**, like every other thing in this repository that changes a shape by
//! itself. An operator who wrote down four addresses and got five upstreams has a deployment
//! whose shape they cannot predict from what they wrote, and that is worth more than the
//! restart it saves.
//!
//! **The answer comes from one node, chosen without telling anybody.** That is the honest
//! weakness of reading membership through a proxy: `/cluster/topology` is one node's view, and
//! this process is not in the agreement that would settle it. Two things keep it safe. Only a
//! node already in rotation is asked, so the view comes from something that is serving; and
//! nothing here decides that a node is *ready* - it decides only that a node exists, and the
//! health check keeps its say over which of them get requests.

use crate::health::Policy;
use crate::pool::Pool;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// What this proxy needs in order to follow the membership.
#[derive(Clone)]
pub struct Discovery {
    /// How often the membership is read. The health interval by default: the two questions are
    /// asked of the same nodes at the same rate, and a second knob would be a second thing to
    /// get wrong.
    pub every: Duration,
    /// How long one read may take.
    pub budget: Duration,
    /// `Authorization: Basic ...`, already encoded. `/cluster/topology` needs `Operate`, so a
    /// cluster with a users file needs this and one without needs nothing.
    pub authorization: Option<String>,
    /// What an adopted node is reached with, which has to be what every other upstream is
    /// reached with: a node discovered over plaintext in a TLS cluster would be a hole.
    pub tls: Option<Arc<big_tls::ClientTls>>,
    /// The health policy an adopted node starts under.
    pub policy: Policy,
}

/// Reads the membership until `running` goes false, adopting what it finds.
pub fn follow_while(
    pool: &Pool,
    discovery: &Discovery,
    metrics: Option<&crate::metrics::Metrics>,
    running: &AtomicBool,
) {
    // A refusal is not something to repeat every two seconds into a log nobody can then read.
    // The message is worth exactly one line, and it names the flag that fixes it.
    let mut complained = false;
    while running.load(Ordering::Relaxed) {
        sleep_in_slices(discovery.every, running);
        if !running.load(Ordering::Relaxed) {
            return;
        }
        match read_members(pool, discovery) {
            Ok(members) if members.is_empty() => {}
            Ok(members) => {
                complained = false;
                let (added, removed) =
                    pool.adopt(&members, discovery.policy, discovery.tls.as_ref());
                for name in &added {
                    if let Some(m) = metrics {
                        m.upstream_discovered();
                    }
                    big_wire::log::emit(
                        big_wire::log::Level::Info,
                        "upstream_discovered",
                        &[
                            ("node", big_wire::log::F::S(name)),
                            ("upstreams", big_wire::log::F::N(pool.nodes().len() as u64)),
                        ],
                    );
                }
                for name in &removed {
                    if let Some(m) = metrics {
                        m.upstream_removed();
                    }
                    big_wire::log::emit(
                        big_wire::log::Level::Warn,
                        "upstream_removed",
                        &[
                            ("node", big_wire::log::F::S(name)),
                            ("upstreams", big_wire::log::F::N(pool.nodes().len() as u64)),
                        ],
                    );
                }
            }
            Err(why) => {
                if !complained {
                    complained = true;
                    big_wire::log::emit(
                        big_wire::log::Level::Warn,
                        "discovery_failed",
                        &[("why", big_wire::log::F::S(&why))],
                    );
                }
            }
        }
    }
}

/// Asks one node in rotation who is in the cluster.
fn read_members(pool: &Pool, discovery: &Discovery) -> Result<Vec<(String, String)>, String> {
    let candidates = pool.candidates();
    let Some(node) = candidates.first() else {
        return Err("no node is in rotation to ask".to_string());
    };
    let mut block = String::new();
    if let Some(auth) = &discovery.authorization {
        block.push_str(&format!("Authorization: {auth}\r\n"));
    }
    let answer = node
        .up
        .send("GET", "/cluster/topology", &block, &[], discovery.budget)
        .map_err(|e| format!("{}: {e}", node.up.name()))?;
    if answer.status == 401 || answer.status == 403 {
        return Err(format!(
            "{} refused the membership request ({}). Reading it needs the Operate privilege: \
             pass --discover-credentials with a file holding one `user:password` line for a \
             role that has it",
            node.up.name(),
            answer.status
        ));
    }
    if answer.status != 200 {
        return Err(format!("{} answered {}", node.up.name(), answer.status));
    }
    let text = std::str::from_utf8(&answer.body).map_err(|_| "not text".to_string())?;
    Ok(members_of(text))
}

/// Pulls `name` and `addr` out of the `members` array of a topology answer.
///
/// Scanned rather than parsed, exactly as [`crate::health::Verdict::of`] reads `/ready`: two
/// fields of one shape, written by this repository's own encoder, in a workspace that has
/// twice declined to add a JSON dependency.
///
/// **`ranges` is deliberately not read.** Which node holds which shard is the agreement's
/// business and this process is not in it; a proxy that started routing by range would be
/// making a decision it cannot be told it got wrong.
pub fn members_of(text: &str) -> Vec<(String, String)> {
    let Some(start) = text.find("\"members\":[") else { return Vec::new() };
    let rest = &text[start + "\"members\":[".len()..];
    let end = rest.find(']').unwrap_or(rest.len());
    let mut out = Vec::new();
    for entry in rest[..end].split('{').skip(1) {
        if let (Some(name), Some(addr)) = (field(entry, "name"), field(entry, "addr")) {
            out.push((name, addr));
        }
    }
    out
}

fn field(text: &str, key: &str) -> Option<String> {
    let at = text.find(&format!("\"{key}\":"))? + key.len() + 3;
    let rest = text[at..].trim_start().strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

fn sleep_in_slices(total: Duration, running: &AtomicBool) {
    let mut left = total;
    while left > Duration::ZERO && running.load(Ordering::Relaxed) {
        let slice = left.min(Duration::from_millis(100));
        std::thread::sleep(slice);
        left -= slice;
    }
}
