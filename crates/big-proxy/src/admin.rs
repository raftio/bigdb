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

//! Seeding an upstream into a running proxy.
//!
//! **What this exists for.** A proxy started with no upstreams is a coherent thing - it answers
//! `503`, exactly as it does when every upstream it was given is down - and it lets the front
//! door be brought up before the machines behind it. What was missing was any way to fill it
//! afterwards, which made an empty start a process nobody could rescue without a restart.
//!
//! **Why a second listener rather than a route on the first.** This proxy authenticates
//! nothing. It forwards the client's `Authorization` byte for byte and never parses it, which
//! is what keeps it a process that has never seen a password. A route that could add an
//! upstream is a route that could point every client's traffic at a machine of the caller's
//! choosing - so on the public port it would be the one unauthenticated way to take the whole
//! deployment over. Rather than give this process a credential to check, and a first password
//! to hold, the port itself is the check: **loopback only, refused anywhere else.** Reaching it
//! means already being on the machine.
//!
//! **And the seed still has to prove which cluster it is in.** Being on the machine is not the
//! same as being right. With `--cluster-id`, a seeded address is asked who it is before it is
//! adopted, and a node that answers with another cluster's name - or does not answer - is
//! refused. That is not authentication and is not offered as any: it is what stops a typo
//! pointing this proxy at the wrong cluster, which is the mistake actually made at three in the
//! morning.

use crate::health::Policy;
use crate::pool::Pool;
use crate::upstream::Upstream;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// The seeding listener and what it needs to judge a seed.
///
/// `Debug` by address alone: what it holds beyond that is a socket and a policy, and neither
/// reads usefully in a failure message.
pub struct Admin {
    listener: TcpListener,
    policy: Policy,
    tls: Option<Arc<big_tls::ClientTls>>,
    /// What a seed must call its cluster, when the operator said. `None` adopts what it is
    /// given, which is the right default only because the port is already loopback.
    cluster_id: Option<String>,
    budget: Duration,
}

/// A bind address that is not loopback, refused with the reason rather than the address.
#[derive(Debug)]
pub struct NotLoopback(pub String);

impl std::fmt::Display for NotLoopback {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "--admin-addr {} is not a loopback address. This port can point every client's \
             traffic at a machine of the caller's choosing, and this proxy checks no credential \
             of its own - so reaching it has to mean already being on the machine. Bind \
             127.0.0.1, and put a tunnel in front of it if it has to be reached from elsewhere",
            self.0
        )
    }
}

impl std::fmt::Debug for Admin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.listener.local_addr() {
            Ok(a) => write!(f, "Admin({a})"),
            Err(_) => write!(f, "Admin(unbound)"),
        }
    }
}

impl Admin {
    /// Binds the seeding port, refusing anything but loopback.
    pub fn bind(
        addr: &str,
        policy: Policy,
        tls: Option<Arc<big_tls::ClientTls>>,
        cluster_id: Option<String>,
        budget: Duration,
    ) -> Result<Self, String> {
        // Resolved and checked before it is bound: every address the name resolves to has to be
        // loopback, or a name that happens to have one loopback record among several would open
        // the port on the others.
        let resolved: Vec<_> = std::net::ToSocketAddrs::to_socket_addrs(addr)
            .map_err(|e| format!("--admin-addr {addr}: {e}"))?
            .collect();
        if resolved.is_empty() {
            return Err(format!("--admin-addr {addr} resolves to nothing"));
        }
        if let Some(open) = resolved.iter().find(|a| !a.ip().is_loopback()) {
            return Err(NotLoopback(open.to_string()).to_string());
        }
        let listener = TcpListener::bind(addr).map_err(|e| format!("--admin-addr {addr}: {e}"))?;
        Ok(Self { listener, policy, tls, cluster_id, budget })
    }

    pub fn local_addr(&self) -> std::io::Result<std::net::SocketAddr> {
        self.listener.local_addr()
    }

    /// Serves the seeding port until `running` goes false.
    ///
    /// One connection at a time and no pool: this port takes an operator's occasional request,
    /// not traffic, and a thread each would be machinery for a load that does not exist.
    pub fn serve_while(&self, pool: &Pool, running: &AtomicBool) {
        // So a stop is noticed without a request arriving to notice it on.
        let _ = self.listener.set_nonblocking(false);
        for stream in self.listener.incoming() {
            if !running.load(Ordering::Relaxed) {
                return;
            }
            match stream {
                Ok(stream) => self.answer(stream, pool),
                Err(_) => continue,
            }
        }
    }

    fn answer(&self, mut stream: TcpStream, pool: &Pool) {
        let _ = stream.set_read_timeout(Some(self.budget));
        let mut line = String::new();
        if BufReader::new(&stream).read_line(&mut line).is_err() {
            return;
        }
        let mut parts = line.split_whitespace();
        let (method, target) = (parts.next().unwrap_or_default(), parts.next().unwrap_or_default());
        let (status, body) = self.act(method, target, pool);
        let _ = stream.write_all(
            format!(
                "HTTP/1.1 {status} \r\nContent-Type: application/json\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        );
    }

    fn act(&self, method: &str, target: &str, pool: &Pool) -> (u16, String) {
        let (path, query) = target.split_once('?').unwrap_or((target, ""));
        let name = param(query, "name");
        match (method, path) {
            ("POST", "/admin/upstream") => match (name, param(query, "addr")) {
                (Some(name), Some(addr)) => self.seed(pool, &name, &addr),
                _ => (400, refusal("bad_request", "name and addr are both required")),
            },
            ("DELETE", "/admin/upstream") => match name {
                Some(name) => match pool.forget(&name) {
                    true => (200, format!("{{\"removed\":{}}}", json_str(&name))),
                    false => (404, refusal("no_such_upstream", "this proxy has no such upstream")),
                },
                None => (400, refusal("bad_request", "name is required")),
            },
            ("GET", "/admin/upstream") => {
                let names: Vec<String> =
                    pool.nodes().iter().map(|n| json_str(n.up.name())).collect();
                (200, format!("{{\"upstreams\":[{}]}}", names.join(",")))
            }
            _ => (404, refusal("no_such_route", "POST, DELETE or GET /admin/upstream")),
        }
    }

    /// Adopts one address, after it has said which cluster it belongs to.
    fn seed(&self, pool: &Pool, name: &str, addr: &str) -> (u16, String) {
        let up = match &self.tls {
            Some(tls) => Upstream::secured(name, addr, Arc::clone(tls)),
            None => Upstream::new(name, addr),
        };
        if let Some(want) = &self.cluster_id {
            match asked_its_cluster(&up, self.budget) {
                Some(said) if &said == want => {}
                Some(said) => {
                    return (
                        409,
                        refusal(
                            "cluster_mismatch",
                            &format!("`{name}` says it is in cluster `{said}`, not `{want}`"),
                        ),
                    )
                }
                None => {
                    return (
                        503,
                        refusal(
                            "unreachable",
                            &format!(
                                "`{name}` did not say which cluster it is in, so it was not \
                                 adopted. It has to be answering to be seeded"
                            ),
                        ),
                    )
                }
            }
        }
        // Out of rotation like anything discovered, for the same reason: existing is not
        // serving, and the health check is what tells them apart.
        let added = pool.adopt_one(up, self.policy);
        big_wire::log::emit(
            big_wire::log::Level::Info,
            "upstream_seeded",
            &[
                ("node", big_wire::log::F::S(name)),
                ("addr", big_wire::log::F::S(addr)),
                ("added", big_wire::log::F::B(added)),
            ],
        );
        (200, format!("{{\"seeded\":{},\"added\":{added}}}", json_str(name)))
    }
}

/// Asks an address which cluster it is in, through the route that says so.
fn asked_its_cluster(up: &Upstream, budget: Duration) -> Option<String> {
    let answer = up.send("GET", "/cluster/topology", "", &[], budget).ok()?;
    if answer.status != 200 {
        return None;
    }
    let text = std::str::from_utf8(&answer.body).ok()?;
    let at = text.find("\"cluster_id\":")? + "\"cluster_id\":".len();
    let rest = text[at..].trim_start().strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

fn param(query: &str, key: &str) -> Option<String> {
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(k, _)| *k == key)
        .map(|(_, v)| crate::allowlist::decode(v))
        .filter(|v| !v.is_empty())
}

fn refusal(code: &str, message: &str) -> String {
    format!("{{\"code\":{},\"message\":{}}}", json_str(code), json_str(message))
}

fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
