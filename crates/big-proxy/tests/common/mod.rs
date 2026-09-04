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

//! Real daemons and a real proxy on loopback ports.
//!
//! Everything here is in-process: `Server::bind` with an in-memory `Api` is a whole daemon, and
//! `Proxy::bind` is the whole front door. What is *not* faked is the part that matters — every
//! request crosses a socket, so the pooling, the timeouts and the failover are the ones that
//! ship rather than ones a test invented.

#![allow(dead_code)]

use big_embed::Api;
use big_http::{Server, ServerConfig};
use big_proxy::allowlist::Tier;
use big_proxy::health::Policy;
use big_proxy::listen::{Config, HealthConfig, Proxy};
use big_proxy::pool::Pool;
use big_proxy::upstream::Upstream;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// A daemon that can be stopped and started again on the same port.
pub struct Daemon {
    pub addr: SocketAddr,
    running: Arc<AtomicBool>,
}

impl Daemon {
    /// Stop answering, and wait until the port is actually closed.
    ///
    /// The wait matters: a test that stopped a node and immediately asserted on failover would
    /// be racing the listener's own shutdown rather than testing the proxy.
    pub fn stop(&self) {
        self.running.store(false, Ordering::Relaxed);
        // `serve_while` only notices between accepts, so one connection wakes it.
        let _ = TcpStream::connect_timeout(&self.addr, Duration::from_millis(200));
        wait_for(Duration::from_secs(5), || TcpStream::connect(self.addr).is_err());
    }

    /// Bring a daemon back up on the same port.
    pub fn restart(&self) -> Daemon {
        on_port(self.addr)
    }
}

/// A daemon on a free loopback port.
pub fn daemon() -> Daemon {
    let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("a free port");
    let addr = probe.local_addr().expect("a bound listener has an address");
    drop(probe);
    on_port(addr)
}

/// A daemon on this exact port, with a database of its own.
///
/// A restart gets a fresh in-memory database rather than the one that was there. That is fine
/// for what these tests ask — whether a request reaches *a* node — and it keeps `Daemon` from
/// having to own storage that outlives the process using it.
fn on_port(addr: SocketAddr) -> Daemon {
    let server = loop {
        let api = Api::in_memory().expect("an in-memory database");
        // The port was free a moment ago; on a busy machine the kernel may still be holding it.
        match Server::bind_with(api, addr, ServerConfig::default()) {
            Ok(s) => break s,
            Err(_) => std::thread::sleep(Duration::from_millis(20)),
        }
    };
    let running = Arc::new(AtomicBool::new(true));
    let flag = Arc::clone(&running);
    std::thread::spawn(move || {
        let _ = server.serve_while(&flag);
    });
    wait_for(Duration::from_secs(5), || TcpStream::connect(addr).is_ok());
    Daemon { addr, running }
}

/// Stops the proxy when it goes out of scope, so a failing test does not leave a thread behind.
pub struct Stop(Arc<AtomicBool>);

impl Drop for Stop {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Relaxed);
    }
}

/// A proxy in front of these nodes, polling fast so a test does not wait on production cadence.
pub fn proxy_in_front(nodes: &[(&str, SocketAddr)], allowed: Tier) -> (SocketAddr, Stop) {
    let ups: Vec<Upstream> =
        nodes.iter().map(|(name, addr)| Upstream::new(*name, addr.to_string())).collect();
    // `floor: ZERO` because the anti-flap floor is tested directly in `health.rs`, and here it
    // would only make the test wait for behaviour it is not asserting.
    let pool = Pool::new(ups, Policy { fail: 2, pass: 2, floor: Duration::ZERO }, 2);
    let config = Config {
        allowed,
        health: HealthConfig {
            every: Duration::from_millis(100),
            budget: Duration::from_millis(300),
        },
        ..Default::default()
    };

    let proxy = Proxy::bind("127.0.0.1:0", pool, config).expect("a free port");
    let addr = proxy.local_addr().expect("a bound listener has an address");
    let running = Arc::new(AtomicBool::new(true));
    let flag = Arc::clone(&running);
    std::thread::spawn(move || {
        let _ = proxy.serve_while(&flag);
    });
    wait_for(Duration::from_secs(5), || TcpStream::connect(addr).is_ok());
    (addr, Stop(running))
}

pub struct Reply {
    pub status: u16,
    pub body: String,
}

/// One request on a connection of its own, so a test never depends on keep-alive state.
pub fn send(addr: SocketAddr, method: &str, target: &str, body: &str) -> Reply {
    let Ok(mut stream) = TcpStream::connect_timeout(&addr, Duration::from_secs(2)) else {
        return Reply { status: 0, body: "could not connect".to_string() };
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(20)));
    let request = format!(
        "{method} {target} HTTP/1.1\r\nHost: test\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    );
    if stream.write_all(request.as_bytes()).is_err() {
        return Reply { status: 0, body: "could not write".to_string() };
    }

    let mut raw = Vec::new();
    if stream.read_to_end(&mut raw).is_err() && raw.is_empty() {
        return Reply { status: 0, body: "could not read".to_string() };
    }
    let raw = String::from_utf8_lossy(&raw).to_string();
    let status = raw.split_whitespace().nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    let body = raw.split_once("\r\n\r\n").map(|(_, b)| b.to_string()).unwrap_or_default();
    Reply { status, body }
}

/// Poll `check` until it is true or `limit` runs out.
///
/// Failover is asynchronous by nature — a poller has an interval and a streak to satisfy — so a
/// test that asserted immediately would be asserting on timing rather than on behaviour.
pub fn wait_for(limit: Duration, mut check: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if check() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    check()
}
