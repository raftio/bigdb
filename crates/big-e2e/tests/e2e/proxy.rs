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

//! Real `big serve` processes behind a real `bigproxy`, driven by a real `bigctl`.
//!
//! **Ignored by default, for the reason `cluster.rs` gives.** `big-proxy`'s own integration
//! tests already cover failover in-process, where the poller's interval is a number a test
//! chooses. What this adds is the part that cannot be held still: separate operating system
//! processes, a socket between each pair, and a rotation decided by wall-clock probes. A timing
//! test in the default run is one that eventually fails for reasons nobody changed.
//!
//! What it proves that the in-process tests cannot: that `bigctl` — a client that knows nothing
//! about this component — works against the proxy exactly as it works against a node, and goes
//! on working when the node it was reaching stops.
//!
//! Run it deliberately: `cargo test -p big-e2e -- --ignored proxy::`.

use crate::common::*;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

/// A running `bigproxy`, killed and reaped whenever the test ends.
struct Proxy {
    child: Child,
    addr: SocketAddr,
}

impl Proxy {
    /// A proxy in front of these nodes, probing fast so a test does not wait on production
    /// cadence.
    fn in_front(nodes: &[(&str, SocketAddr)]) -> Self {
        let addr: SocketAddr = format!("127.0.0.1:{}", reserved_port()).parse().expect("loopback");
        let mut cmd = Command::new(bin("bigproxy"));
        cmd.arg(addr.to_string());
        for (name, node) in nodes {
            cmd.args(["--upstream", &format!("{name}={node}")]);
        }
        cmd.args(["--health-interval", "200", "--health-timeout", "500"]);
        cmd.env("BIG_LOG", "warn");
        let child = cmd.spawn().expect("bigproxy is built and on the path this harness uses");

        let proxy = Self { child, addr };
        assert!(
            proxy.wait_until(Duration::from_secs(10), |p| p.get("/health").is_some()),
            "the proxy never came up"
        );
        proxy
    }

    fn wait_until(&self, limit: Duration, mut check: impl FnMut(&Self) -> bool) -> bool {
        let deadline = Instant::now() + limit;
        while Instant::now() < deadline {
            if check(self) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        check(self)
    }

    /// A bare `GET`, for the routes that are never authenticated.
    fn get(&self, target: &str) -> Option<(u16, String)> {
        let mut stream = TcpStream::connect_timeout(&self.addr, Duration::from_millis(500)).ok()?;
        stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
        let request = format!(
            "GET {target} HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\
             Connection: close\r\n\r\n"
        );
        stream.write_all(request.as_bytes()).ok()?;
        stream.flush().ok()?;
        let mut raw = String::new();
        stream.read_to_string(&mut raw).ok()?;
        let status: u16 = raw.split(' ').nth(1)?.parse().ok()?;
        Some((status, raw.split("\r\n\r\n").nth(1).unwrap_or_default().to_string()))
    }

    fn post(&self, target: &str) -> Option<(u16, String)> {
        let mut stream = TcpStream::connect_timeout(&self.addr, Duration::from_millis(500)).ok()?;
        stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
        let request = format!(
            "POST {target} HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\
             Connection: close\r\n\r\n"
        );
        stream.write_all(request.as_bytes()).ok()?;
        stream.flush().ok()?;
        let mut raw = String::new();
        stream.read_to_string(&mut raw).ok()?;
        let status: u16 = raw.split(' ').nth(1)?.parse().ok()?;
        Some((status, raw.split("\r\n\r\n").nth(1).unwrap_or_default().to_string()))
    }

    /// One `bigctl` subcommand, pointed at the proxy rather than at a node.
    ///
    /// This is the whole claim in one line: `bigctl` has no flag for a proxy and no idea one
    /// exists. It takes an address, and the address happens to be this.
    fn bigctl(&self, args: &[&str]) -> Run {
        let addr = self.addr.to_string();
        let mut all = vec!["--addr", addr.as_str()];
        all.extend_from_slice(args);
        run("bigctl", &all)
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
#[ignore = "spawns processes and waits on wall-clock probes; run with --ignored"]
fn bigctl_works_through_the_proxy_without_knowing_it_is_there() {
    let node = Daemon::start();
    let proxy = Proxy::in_front(&[("a", node.addr)]);

    proxy.bigctl(&["create", "table", "events"]).expect(0);

    let schema = proxy.bigctl(&["schema"]).expect(0);
    assert!(schema.said("events"), "{}", schema.out);
}

/// The claim the component exists to make.
#[test]
#[ignore = "spawns processes and waits on wall-clock probes; run with --ignored"]
fn stopping_the_node_a_client_was_reaching_does_not_stop_the_client() {
    let workspace = Workspace::new();
    let (port_a, port_b) = (reserved_port(), reserved_port());
    let (addr_a, addr_b) = (format!("127.0.0.1:{port_a}"), format!("127.0.0.1:{port_b}"));
    let a = workspace.daemon_at("a.big", &addr_a, &[]);
    let b = workspace.daemon_at("b.big", &addr_b, &[]);

    let proxy = Proxy::in_front(&[("a", a.addr), ("b", b.addr)]);
    proxy.bigctl(&["schema"]).expect(0);

    // Two separate databases rather than a cluster, deliberately: this test is about the front
    // door choosing a live node, not about the agreement choosing a primary. What it asserts is
    // that a client keeps getting answers, not which file answered.
    a.stop();

    // Wait for the rotation to notice, rather than asserting the instant after the kill: the
    // poller has an interval and an anti-flap floor, and a test that asserted immediately would
    // be asserting on timing rather than on behaviour.
    let noticed = proxy.wait_until(
        Duration::from_secs(30),
        |p| matches!(p.get("/ready"), Some((200, ref body)) if body.contains("\"in_rotation\":1")),
    );
    let (_, body) = proxy.get("/ready").expect("the proxy answers about itself");
    assert!(noticed, "the stopped node never left rotation: {body}");

    // And through all of it — before, during and after — a client kept getting answers.
    for _ in 0..3 {
        proxy.bigctl(&["schema"]).expect(0);
    }
}

/// Fail closed, and stay alive while doing it.
#[test]
#[ignore = "spawns processes and waits on wall-clock probes; run with --ignored"]
fn losing_every_node_is_a_503_and_not_a_dead_proxy() {
    let node = Daemon::start();
    let proxy = Proxy::in_front(&[("a", node.addr)]);
    proxy.bigctl(&["schema"]).expect(0);

    node.stop();

    // `/ready` is a readiness report rather than an error envelope, so what a probe reads is
    // the status and what an operator reads is `status` and `in_rotation`. The
    // `no_healthy_upstream` code lives on the response for the log line, not in this body.
    let refused = proxy.wait_until(
        Duration::from_secs(30),
        |p| matches!(p.get("/ready"), Some((503, ref body)) if body.contains("\"in_rotation\":0")),
    );
    let seen = proxy.get("/ready").map(|(s, b)| format!("{s}: {b}")).unwrap_or_default();
    assert!(refused, "a proxy with nowhere to send a request must say so; saw {seen}");

    // Liveness and readiness are different questions. An orchestrator that restarted this
    // process for the database's outage would be applying the wrong cure.
    let (status, _) = proxy.get("/health").expect("the process is still answering");
    assert_eq!(status, 200, "the proxy must stay alive while the cluster is down");
}

/// The peer surface is not reachable through the front door, and says nothing about existing.
#[test]
#[ignore = "spawns processes and waits on wall-clock probes; run with --ignored"]
fn the_peer_surface_is_refused_exactly_like_a_typo() {
    let node = Daemon::start();
    let proxy = Proxy::in_front(&[("a", node.addr)]);

    let (internal, internal_body) = proxy.post("/internal/query").expect("an answer");
    let (typo, typo_body) = proxy.post("/intrenal/query").expect("an answer");
    assert_eq!(internal, 404, "{internal_body}");
    assert_eq!(internal, typo, "the peer surface answered differently from a typo");
    assert_eq!(internal_body, typo_body, "and so is distinguishable from one");
    assert!(internal_body.contains("no_such_route"), "{internal_body}");
}
