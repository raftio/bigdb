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

//! Every node, which of them are in rotation, and which one gets the next request.
//!
//! **Least in flight, not round robin.** Two requests are not the same amount of work even on
//! one node: a query whose ranges are local is a direct call, and one that fans out to two peers
//! is three round trips. Round robin spreads *counts*, which is the wrong quantity — it will
//! keep handing work to a node already occupied by a slow scan. Least in flight spreads
//! occupancy, and it costs nothing because the in-flight ceiling already needed that count.
//!
//! **No stickiness.** There is no per-connection state on a node to be sticky to — authentication
//! is per request and keep-alive is a transport detail. A sticky proxy would pin a long-lived
//! client to a node that is about to stop serving, which is exactly the client failover was for.

use crate::allowlist::Repeatable;
use crate::health::{Health, Policy, Verdict};
use crate::upstream::{Upstream, UpstreamError, UpstreamResponse};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::RwLock;
use std::time::{Duration, Instant};

/// One node and what this proxy currently believes about it.
pub struct Node {
    pub up: Upstream,
    pub health: RwLock<Health>,
}

impl Node {
    pub fn new(up: Upstream, policy: Policy) -> Self {
        Self { up, health: RwLock::new(Health::new(policy)) }
    }

    pub fn in_rotation(&self) -> bool {
        self.health.read().unwrap_or_else(|e| e.into_inner()).in_rotation()
    }
}

/// Why a request found no node to go to.
#[derive(Debug, PartialEq, Eq)]
pub enum NoNode {
    /// Nothing is in rotation. `503`, and never a request sent to a node known to be down.
    NoneInRotation,
    /// Every candidate was tried and none answered.
    AllFailed,
}

/// The nodes behind one address.
pub struct Pool {
    nodes: Vec<Node>,
    /// Breaks ties between equally loaded nodes without a lock. Round robin *within* a tie is
    /// what stops several workers choosing the same idle node in the same instant.
    turn: AtomicUsize,
    max_retries: usize,
}

impl Pool {
    pub fn new(upstreams: Vec<Upstream>, policy: Policy, max_retries: usize) -> Self {
        Self {
            nodes: upstreams.into_iter().map(|u| Node::new(u, policy)).collect(),
            turn: AtomicUsize::new(0),
            max_retries,
        }
    }

    pub fn nodes(&self) -> &[Node] {
        &self.nodes
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    pub fn in_rotation(&self) -> usize {
        self.nodes.iter().filter(|n| n.in_rotation()).count()
    }

    /// The nodes to try, best first, skipping anything out of rotation.
    ///
    /// Returns empty when nothing is in rotation. **It does not fall back to a node known to be
    /// down**: a node answering `serving:false` will answer `503` anyway, so sending it work
    /// trades a clear refusal for a slower one, and a node that is merely *behind* would answer
    /// a count that is quietly wrong.
    pub fn candidates(&self) -> Vec<usize> {
        let live: Vec<(usize, usize)> = (0..self.nodes.len())
            .filter(|i| self.nodes[*i].in_rotation())
            .map(|i| (i, self.nodes[i].up.in_flight()))
            .collect();
        if live.is_empty() {
            return Vec::new();
        }
        order(&live, self.turn.fetch_add(1, Ordering::Relaxed), self.nodes.len())
    }

    /// Send one request, trying another node when it is safe to.
    ///
    /// The retry rule has one shape and two clauses:
    ///
    /// * **The request was never sent.** Connect refused, the node left rotation before the
    ///   write began — zero bytes reached the socket, so the second attempt *is* the first.
    ///   Safe for every route, imports included.
    /// * **The route is `Repeatable::Yes` and the failure carried no answer.** The second answer
    ///   is the first answer, so asking another node costs a round trip and nothing else.
    ///
    /// A `5xx` is neither: it is an *answer*, and asking somebody else the same refused question
    /// turns a clear failure into a confusing one. The single exception is a `503` carrying
    /// `Retry-After` on a repeatable route, which is the daemon saying "this node is busy"
    /// rather than "this request is wrong" — retried once, on a different node.
    pub fn send(
        &self,
        method: &str,
        target: &str,
        header_block: &str,
        body: &[u8],
        budget: Duration,
        repeatable: Repeatable,
    ) -> Result<(usize, UpstreamResponse), (NoNode, Option<UpstreamError>)> {
        let candidates = self.candidates();
        if candidates.is_empty() {
            return Err((NoNode::NoneInRotation, None));
        }

        let mut last: Option<UpstreamError> = None;
        let tries = candidates.len().min(self.max_retries + 1);

        for (attempt, &i) in candidates.iter().take(tries).enumerate() {
            let more = attempt + 1 < tries;
            match self.nodes[i].up.send(method, target, header_block, body, budget) {
                Ok(answer) => {
                    if more && retryable_answer(&answer, repeatable) {
                        continue;
                    }
                    return Ok((i, answer));
                }
                Err(e) => {
                    // A transport failure on real traffic is evidence the poller has not seen
                    // yet. A node that died between two polls should not eat every request in
                    // between waiting for one.
                    if !matches!(e, UpstreamError::Unreadable(_)) {
                        let mut h = self.nodes[i].health.write().unwrap_or_else(|e| e.into_inner());
                        h.saw_failure(Instant::now());
                    }
                    let may_retry = e.not_sent() || repeatable == Repeatable::Yes;
                    // A budget that has run out cannot fund a second attempt.
                    let may_retry = may_retry && !matches!(e, UpstreamError::Timeout);
                    last = Some(e);
                    if !(more && may_retry) {
                        break;
                    }
                }
            }
        }
        Err((NoNode::AllFailed, last))
    }

    /// Fold one probe into a node's health. `true` when the rotation changed.
    pub fn observe(&self, i: usize, verdict: Verdict) -> bool {
        let Some(node) = self.nodes.get(i) else { return false };
        let mut h = node.health.write().unwrap_or_else(|e| e.into_inner());
        h.observe(verdict, Instant::now())
    }

    /// Ask every node `GET /ready` on an interval, until `running` goes false.
    ///
    /// One thread walks the list rather than one thread per node: three to a few dozen nodes,
    /// probed every couple of seconds, is not work that needs a thread each.
    ///
    /// Each probe opens **its own connection** and never takes one from the request pool. A pool
    /// saturated by real traffic must not be able to starve the health check, and a health check
    /// must not be able to occupy a slot a request is queued for.
    pub fn poll_while(
        &self,
        running: &std::sync::atomic::AtomicBool,
        every: Duration,
        budget: Duration,
    ) {
        while running.load(Ordering::Relaxed) {
            for (i, node) in self.nodes.iter().enumerate() {
                if !running.load(Ordering::Relaxed) {
                    return;
                }
                let verdict = probe(node.up.addr(), budget);
                let was = node.in_rotation();
                if self.observe(i, verdict) {
                    let now = node.in_rotation();
                    let why = node
                        .health
                        .read()
                        .unwrap_or_else(|e| e.into_inner())
                        .why()
                        .map(|w| w.to_string())
                        .unwrap_or_default();
                    big_wire::log::emit(
                        if now { big_wire::log::Level::Info } else { big_wire::log::Level::Warn },
                        "rotation",
                        &[
                            ("node", big_wire::log::F::S(node.up.name())),
                            ("addr", big_wire::log::F::S(node.up.addr())),
                            ("in_rotation", big_wire::log::F::B(now)),
                            ("was", big_wire::log::F::B(was)),
                            ("why", big_wire::log::F::S(&why)),
                            ("in_rotation_total", big_wire::log::F::N(self.in_rotation() as u64)),
                        ],
                    );
                }
            }
            // Sleep in slices so a stop is noticed within a slice rather than within a poll.
            let mut left = every;
            while left > Duration::ZERO && running.load(Ordering::Relaxed) {
                let slice = left.min(Duration::from_millis(100));
                std::thread::sleep(slice);
                left -= slice;
            }
        }
    }
}

/// One unauthenticated `GET /ready`, on a connection of its own.
fn probe(addr: &str, budget: Duration) -> Verdict {
    use crate::health::Why;
    use std::io::{BufRead, BufReader, Read, Write};

    let deadline = Instant::now() + budget;
    let Ok(mut addrs) = std::net::ToSocketAddrs::to_socket_addrs(addr) else {
        return Verdict::Down(Why::Unreachable);
    };
    let Some(sock_addr) = addrs.next() else { return Verdict::Down(Why::Unreachable) };
    let Ok(stream) = std::net::TcpStream::connect_timeout(&sock_addr, budget) else {
        return Verdict::Down(Why::Unreachable);
    };
    let left = || deadline.checked_duration_since(Instant::now());
    let Some(remaining) = left() else { return Verdict::Down(Why::Timeout) };
    if stream.set_read_timeout(Some(remaining)).is_err()
        || stream.set_write_timeout(Some(remaining)).is_err()
    {
        return Verdict::Down(Why::Unreachable);
    }

    let mut io = BufReader::new(stream);
    // `Connection: close` rather than keep-alive: a probe connection is not worth pooling, and
    // holding one open would be a file descriptor per node doing nothing between polls.
    let request = format!("GET /ready HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
    if io.get_mut().write_all(request.as_bytes()).is_err() {
        return Verdict::Down(Why::Unreachable);
    }

    let mut line = String::new();
    if io.read_line(&mut line).is_err() {
        return Verdict::Down(Why::Timeout);
    }
    let Some(status) = line.split_whitespace().nth(1).and_then(|s| s.parse::<u16>().ok()) else {
        return Verdict::Down(Why::Unreadable);
    };

    let mut length: Option<usize> = None;
    loop {
        line.clear();
        if io.read_line(&mut line).is_err() {
            return Verdict::Down(Why::Timeout);
        }
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            break;
        }
        if let Some((name, value)) = trimmed.split_once(':') {
            if name.trim().eq_ignore_ascii_case("content-length") {
                length = value.trim().parse().ok();
            }
        }
    }

    // A readiness answer is a couple of hundred bytes; anything claiming more is not one.
    const MAX: usize = 64 << 10;
    let mut body = Vec::new();
    match length {
        Some(n) if n <= MAX => {
            body.resize(n, 0);
            if io.read_exact(&mut body).is_err() {
                return Verdict::Down(Why::Timeout);
            }
        }
        Some(_) => return Verdict::Down(Why::Unreadable),
        None => {
            if io.take(MAX as u64).read_to_end(&mut body).is_err() {
                return Verdict::Down(Why::Timeout);
            }
        }
    }
    Verdict::of(status, &body)
}

/// Order `(index, in_flight)` pairs best-first.
///
/// Pulled out of [`Pool::candidates`] so the rule can be tested against loads that are awkward
/// to produce with real sockets. **Load dominates**; the offset only breaks ties, which is what
/// stops several workers finishing at the same instant from all choosing the same idle node.
fn order(live: &[(usize, usize)], offset: usize, total: usize) -> Vec<usize> {
    let mut live = live.to_vec();
    live.sort_by_key(|(i, load)| (*load, (i + offset) % total.max(1)));
    live.into_iter().map(|(i, _)| i).collect()
}

/// Whether an answer is one worth asking a different node for.
///
/// Only a `503` with `Retry-After`, and only on a repeatable route. The daemon's own comment
/// asks for this — *"a `503` is what tells a proxy to retry"* — and the two codes that produce
/// one, `server_busy` from the shedder and `busy_authenticating` from a throttled password
/// check, both mean "this node is busy" rather than "this request is wrong".
fn retryable_answer(answer: &UpstreamResponse, repeatable: Repeatable) -> bool {
    repeatable == Repeatable::Yes
        && answer.status == 503
        && answer.headers.iter().any(|(n, _)| n == "retry-after")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::health::Why;
    use crate::upstream::ContentType;

    fn pool(n: usize) -> Pool {
        let ups = (0..n)
            .map(|i| Upstream::new(format!("n{i}"), format!("127.0.0.1:{}", 1 + i as u16)))
            .collect();
        Pool::new(ups, Policy { fail: 1, pass: 1, floor: Duration::ZERO }, 2)
    }

    fn eject(pool: &Pool, i: usize) {
        assert!(pool.observe(i, Verdict::Down(Why::NotServing)));
    }

    #[test]
    fn every_node_starts_in_rotation() {
        let p = pool(3);
        assert_eq!(p.in_rotation(), 3);
        assert_eq!(p.candidates().len(), 3);
    }

    #[test]
    fn an_ejected_node_is_not_a_candidate() {
        let p = pool(3);
        eject(&p, 1);
        assert_eq!(p.in_rotation(), 2);
        assert!(!p.candidates().contains(&1));
    }

    /// Fail closed. A node that said it is not serving will answer 503 anyway.
    #[test]
    fn no_node_in_rotation_means_no_candidate_not_a_gamble() {
        let p = pool(2);
        eject(&p, 0);
        eject(&p, 1);
        assert!(p.candidates().is_empty());
        let err = p
            .send("GET", "/schema", "", b"", Duration::from_secs(1), Repeatable::Yes)
            .expect_err("nothing is in rotation");
        assert_eq!(err.0, NoNode::NoneInRotation);
        assert!(err.1.is_none(), "no node was contacted, so there is no error to report");
    }

    #[test]
    fn ties_are_broken_round_robin_so_workers_do_not_collide() {
        let p = pool(3);
        let first: Vec<usize> = (0..6).map(|_| p.candidates()[0]).collect();
        assert!(
            first.windows(2).any(|w| w[0] != w[1]),
            "every worker chose the same node: {first:?}"
        );
    }

    /// The whole reason least-in-flight is worth the code: the idle node wins however the
    /// tie-break rotation happens to be sitting.
    #[test]
    fn the_least_loaded_node_is_chosen_whatever_the_rotation_says() {
        // Node 0 is saturated, node 2 is idle, node 1 is in between.
        let live = [(0usize, 31usize), (1, 4), (2, 0)];
        for offset in 0..12 {
            assert_eq!(order(&live, offset, 3)[0], 2, "offset {offset} chose a busier node");
            assert_eq!(order(&live, offset, 3), vec![2, 1, 0], "offset {offset}");
        }
    }

    /// ...and with equal load it is the rotation that decides, so workers spread out.
    #[test]
    fn equal_load_rotates_instead_of_always_picking_the_first() {
        let live = [(0usize, 7usize), (1, 7), (2, 7)];
        let firsts: Vec<usize> = (0..6).map(|o| order(&live, o, 3)[0]).collect();
        assert_eq!(firsts, vec![0, 2, 1, 0, 2, 1], "the tie-break must move");
    }

    /// A busy node is still a candidate — just the last one. Occupancy orders; it does not
    /// exclude, because a node being busy is not a node being down.
    #[test]
    fn a_saturated_node_is_last_but_still_reachable() {
        let live = [(0usize, 99usize), (1, 0)];
        assert_eq!(order(&live, 0, 2), vec![1, 0]);
    }

    #[test]
    fn a_failure_on_real_traffic_ejects_without_waiting_for_a_probe() {
        let p = pool(2);
        assert_eq!(p.in_rotation(), 2);
        // Port 1 refuses immediately; `fail: 1` means one refusal is enough.
        let _ =
            p.send("GET", "/schema", "Host: x\r\n", b"", Duration::from_secs(2), Repeatable::Yes);
        assert!(p.in_rotation() < 2, "traffic saw what the poller had not");
    }

    #[test]
    fn a_write_is_still_retried_when_the_request_never_left() {
        // Both nodes refuse the connection, so both failures are `NotSent` and an import — a
        // route that is never repeatable — is still allowed to try the second node.
        let p = pool(2);
        let err = p
            .send(
                "POST",
                "/table/t/import",
                "Host: x\r\n",
                b"f",
                Duration::from_secs(2),
                Repeatable::No,
            )
            .expect_err("nothing is listening anywhere");
        assert_eq!(err.0, NoNode::AllFailed);
        assert!(err.1.expect("an error to report").not_sent());
        assert_eq!(p.in_rotation(), 0, "both were tried and both failed");
    }

    #[test]
    fn a_busy_answer_is_retried_only_when_the_route_repeats() {
        let busy = UpstreamResponse {
            status: 503,
            content_type: ContentType::Json,
            body: Vec::new(),
            headers: vec![("retry-after".to_string(), "1".to_string())],
        };
        assert!(retryable_answer(&busy, Repeatable::Yes));
        assert!(!retryable_answer(&busy, Repeatable::No), "a write is never sent twice");
    }

    /// A refusal is an answer. Asking somebody else the same refused question is worse than
    /// reporting it.
    #[test]
    fn an_answer_is_never_retried_just_because_it_failed() {
        for status in [400, 401, 403, 404, 409, 500, 502] {
            let answer = UpstreamResponse {
                status,
                content_type: ContentType::Json,
                body: Vec::new(),
                headers: vec![("retry-after".to_string(), "1".to_string())],
            };
            assert!(!retryable_answer(&answer, Repeatable::Yes), "{status} was retried");
        }
        // ...and even a 503 is not retried without the header that asks for it.
        let bare = UpstreamResponse {
            status: 503,
            content_type: ContentType::Json,
            body: Vec::new(),
            headers: Vec::new(),
        };
        assert!(!retryable_answer(&bare, Repeatable::Yes));
    }

    #[test]
    fn retries_never_exceed_the_nodes_available() {
        let p = Pool::new(
            vec![Upstream::new("only", "127.0.0.1:1")],
            Policy { fail: 9, pass: 1, floor: Duration::ZERO },
            5,
        );
        let err = p
            .send("GET", "/schema", "Host: x\r\n", b"", Duration::from_secs(2), Repeatable::Yes)
            .expect_err("nothing is listening");
        assert_eq!(err.0, NoNode::AllFailed);
    }
}
