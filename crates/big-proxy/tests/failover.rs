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

//! The claim the whole crate exists to make: stop the node a client was reaching, and the
//! client keeps working.
//!
//! Driven over real sockets with real daemons, because failover is a property of the listener,
//! the poller and the pool together. A test that called `Pool::send` directly would assert
//! something about a function rather than about a deployment.

mod common;

use big_proxy::allowlist::Tier;
use common::{daemon, proxy_in_front, send, wait_for};
use std::time::Duration;

#[test]
fn a_query_reaches_a_node_and_comes_back() {
    let a = daemon();
    let (proxy, _stop) = proxy_in_front(&[("a", a.addr)], Tier::Ddl);

    let reply = send(proxy, "GET", "/schema", "");
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert!(reply.body.contains("\"tables\""), "{}", reply.body);
}

/// The point of the component.
#[test]
fn stopping_the_node_a_client_was_reaching_does_not_stop_the_client() {
    let a = daemon();
    let b = daemon();
    let (proxy, _stop) = proxy_in_front(&[("a", a.addr), ("b", b.addr)], Tier::Ddl);

    assert_eq!(send(proxy, "GET", "/schema", "").status, 200);

    // Stop one of them. Which one does not matter — the client never chose.
    a.stop();

    // The passive ejection in `Pool::send` catches this on the first request that tries `a`,
    // and the poller catches it within an interval either way. Every request still succeeds.
    let ok = wait_for(Duration::from_secs(10), || {
        (0..4).all(|_| send(proxy, "GET", "/schema", "").status == 200)
    });
    assert!(ok, "requests kept failing after one of two nodes stopped");

    let ready = send(proxy, "GET", "/ready", "");
    assert_eq!(ready.status, 200, "{}", ready.body);
    assert!(ready.body.contains("\"in_rotation\":1"), "{}", ready.body);
    assert!(ready.body.contains("\"state\":\"out\""), "{}", ready.body);
}

/// Fail closed. Not a hang, not a `502`, and not a request sent to a node known to be down.
#[test]
fn losing_every_node_is_a_503_that_says_so() {
    let a = daemon();
    let b = daemon();
    let (proxy, _stop) = proxy_in_front(&[("a", a.addr), ("b", b.addr)], Tier::Ddl);
    assert_eq!(send(proxy, "GET", "/schema", "").status, 200);

    a.stop();
    b.stop();

    let ok = wait_for(Duration::from_secs(10), || {
        let r = send(proxy, "GET", "/schema", "");
        r.status == 503 && r.body.contains("no_healthy_upstream")
    });
    assert!(ok, "a proxy with nowhere to send a request must say so");

    // And it says so about itself too, while staying alive: liveness and readiness are
    // different questions, and an orchestrator must not restart this process for the cluster's
    // outage.
    let ready = send(proxy, "GET", "/ready", "");
    assert_eq!(ready.status, 503, "{}", ready.body);
    assert!(ready.body.contains("\"in_rotation\":0"), "{}", ready.body);
    assert_eq!(send(proxy, "GET", "/health", "").status, 200, "the process is still fine");
}

/// A node coming back is put back, after the success streak the policy asks for.
#[test]
fn a_node_that_comes_back_is_used_again() {
    let a = daemon();
    let b = daemon();
    let (proxy, _stop) = proxy_in_front(&[("a", a.addr), ("b", b.addr)], Tier::Ddl);

    a.stop();
    let out = wait_for(Duration::from_secs(10), || {
        send(proxy, "GET", "/ready", "").body.contains("\"in_rotation\":1")
    });
    assert!(out, "the stopped node never left rotation");

    let back = a.restart();
    let in_again = wait_for(Duration::from_secs(15), || {
        send(proxy, "GET", "/ready", "").body.contains("\"in_rotation\":2")
    });
    assert!(in_again, "the node came back and was not readmitted");
    drop(back);
}

/// Through the proxy, the peer surface is refused exactly as a typo is.
#[test]
fn the_peer_surface_is_not_reachable_through_the_proxy() {
    let a = daemon();
    let (proxy, _stop) = proxy_in_front(&[("a", a.addr)], Tier::Ops);

    for path in ["/internal/query", "/internal/raft", "/internal/fragment/put"] {
        let reply = send(proxy, "POST", path, "");
        assert_eq!(reply.status, 404, "{path} answered {}", reply.body);
        assert!(reply.body.contains("no_such_route"), "{path}: {}", reply.body);
    }

    // The daemon has those routes; it is this proxy that will not carry them.
    let direct = send(a.addr, "POST", "/internal/query", "");
    assert_ne!(direct.status, 404, "the peer surface moved and this test stopped guarding it");
}

/// `/metrics` is about the proxy, and the number worth alerting on is in it.
#[test]
fn metrics_report_the_rotation() {
    let a = daemon();
    let b = daemon();
    let (proxy, _stop) = proxy_in_front(&[("a", a.addr), ("b", b.addr)], Tier::Ddl);
    send(proxy, "GET", "/schema", "");

    let m = send(proxy, "GET", "/metrics", "");
    assert_eq!(m.status, 200);
    assert!(m.body.contains("big_proxy_upstreams_in_rotation 2"), "{}", m.body);
    assert!(m.body.contains("big_proxy_requests_total"), "{}", m.body);
    assert!(m.body.contains(r#"big_proxy_upstream_up{node="a"} 1"#), "{}", m.body);

    a.stop();
    let dropped = wait_for(Duration::from_secs(10), || {
        send(proxy, "GET", "/metrics", "").body.contains("big_proxy_upstreams_in_rotation 1")
    });
    assert!(dropped, "the gauge did not follow the rotation");
}
