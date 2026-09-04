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

//! The test that keeps the route table honest.
//!
//! `src/allowlist.rs` mirrors `resolve()` in `big-http`, and a mirror drifts. This drives every
//! entry in the table against a **real daemon** and asserts none of them comes back
//! `no_such_route` — so a route the daemon renames or removes fails here rather than becoming a
//! hole a client falls into at three in the morning.
//!
//! It deliberately cannot prove the converse. A route the daemon *grows* that the proxy has not
//! learned is invisible to this test, and that is the right way round: the allowlist is a
//! deliberate subset, and its whole value is that additions are opt-in. The failure worth
//! catching is the table pointing at something that is no longer there.
//!
//! What it asserts is only that the route **resolves**. Whether it then succeeds depends on
//! state this test has no reason to set up — a table that does not exist, a cluster verb on a
//! solo node, a backup directory that was never configured. All of those are answers, and an
//! answer means the route was found.

use big_embed::Api;
use big_http::{Server, ServerConfig};
use big_proxy::allowlist::{Seg, Tier, ROUTES};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

/// A daemon on a loopback port, with authentication off, answering for the whole test.
fn daemon() -> SocketAddr {
    let api = Api::in_memory().expect("an in-memory database");
    let server = Server::bind_with(api, "127.0.0.1:0", ServerConfig::default())
        .expect("a loopback port is available");
    let addr = server.local_addr().expect("a bound listener has an address");
    std::thread::spawn(move || {
        let _ = server.serve();
    });
    addr
}

struct Reply {
    status: u16,
    body: String,
}

fn send(addr: SocketAddr, method: &str, target: &str) -> Reply {
    send_with(addr, method, target, "")
}

/// `extra` goes in verbatim, already `\r\n`-terminated, for the tests that forge a peer stamp.
fn send_with(addr: SocketAddr, method: &str, target: &str, extra: &str) -> Reply {
    let mut stream = TcpStream::connect(addr).expect("the daemon is listening");
    stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let request = format!(
        "{method} {target} HTTP/1.1\r\nHost: test\r\nContent-Length: 0\r\n\
         Connection: close\r\n{extra}\r\n"
    );
    stream.write_all(request.as_bytes()).expect("the request goes out");

    let mut raw = String::new();
    stream.read_to_string(&mut raw).expect("the daemon answers");
    let status = raw
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("no status line in {raw:?}"));
    let body = raw.split_once("\r\n\r\n").map(|(_, b)| b.to_string()).unwrap_or_default();
    Reply { status, body }
}

/// A concrete path for a route pattern, with `n` standing in for every captured name.
fn canonical(segments: &[Seg]) -> String {
    segments
        .iter()
        .map(|s| match s {
            Seg::Lit(l) => format!("/{l}"),
            Seg::Name => "/n".to_string(),
        })
        .collect()
}

#[test]
fn every_allowlisted_route_exists_on_the_daemon() {
    let addr = daemon();
    let mut checked = 0;

    for route in ROUTES {
        let target = canonical(route.segments);
        let reply = send(addr, route.method, &target);

        assert!(
            !reply.body.contains("no_such_route"),
            "{} {target} is in the proxy's allowlist but the daemon does not have it \
             (answered {}: {})\n\n\
             Either `resolve()` in big-http/src/routes/mod.rs dropped or renamed this route \
             and src/allowlist.rs still points at it, or the entry was never right.",
            route.method,
            reply.status,
            reply.body.trim()
        );
        checked += 1;
    }

    assert_eq!(checked, ROUTES.len(), "every route is exercised");
    assert!(ROUTES.len() >= 20, "the table lost entries: {} left", ROUTES.len());
}

const PEER_ROUTES: &[&str] = &["/internal/query", "/internal/raft", "/internal/schema"];

/// The other half of the same claim: the routes the table omits are real, and are refused.
///
/// The proxy would never build these requests — `match_route` returns `None` and the forwarder
/// stops there. This asserts the routes exist on the daemon, so that omitting them is guarding
/// something rather than describing paths that do not exist anyway.
#[test]
fn the_internal_routes_this_table_omits_are_real_and_refused() {
    let addr = daemon();

    for path in PEER_ROUTES {
        let reply = send(addr, "POST", path);
        assert!(
            !reply.body.contains("no_such_route"),
            "{path} answered no_such_route, so omitting it from the allowlist guards nothing \
             — has the peer surface moved?"
        );
        // A caller with no peer stamp is refused before anything is decoded. This is the first
        // of the three things standing between a client and the peer surface; the allowlist is
        // the second and the missing client certificate is the third.
        assert_eq!(reply.status, 409, "{path} answered {}", reply.body.trim());
        assert!(reply.body.contains("wire_version"), "{path} answered {}", reply.body.trim());
    }

    for path in PEER_ROUTES {
        let segments: Vec<_> =
            path.split('/').filter(|s| !s.is_empty()).map(std::borrow::Cow::Borrowed).collect();
        assert!(
            big_proxy::allowlist::match_route("POST", &segments, Tier::Ops).is_none(),
            "{path} is forwardable and must not be"
        );
    }
}

/// Why `forward.rs` strips `x-big-wire` and `x-big-cluster` from every client request.
///
/// A client that knows the header names and the current wire version gets one step further into
/// `mismatched()` and is stopped by the next check. That is the daemon holding its own line —
/// but it is a line the proxy must not help anyone reach, because forwarding a client-supplied
/// stamp would be this process vouching for a claim it cannot check.
#[test]
fn a_forged_peer_stamp_gets_no_further() {
    let addr = daemon();
    let stamp = format!(
        "{}: {}\r\n{}: deadbeef\r\n",
        big_cluster::WIRE_HEADER,
        big_cluster::WIRE_VERSION,
        big_cluster::CLUSTER_HEADER
    );

    for path in PEER_ROUTES {
        let reply = send_with(addr, "POST", path, &stamp);
        assert_eq!(reply.status, 409, "{path} answered {}", reply.body.trim());
        assert!(
            reply.body.contains("cluster_mismatch"),
            "{path} got past the wire check and then answered {}",
            reply.body.trim()
        );
    }
}
