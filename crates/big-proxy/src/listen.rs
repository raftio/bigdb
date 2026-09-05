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

//! The listener: one thread accepting, a fixed pool answering, and a bounded queue between.
//!
//! `big_http::Server` cannot be reused here — it is generic over the pager and every constructor
//! takes an `Api`, because it *is* the engine's edge. What is reused is everything underneath
//! it: [`big_wire::Request`] parses and bounds the request, [`big_wire::Response`] encodes the
//! answer, and `big_wire::log` writes the same structured line the daemon writes, so one
//! `X-Request-Id` reads across both processes.
//!
//! The ceilings are copied rather than chosen. A queue deeper than the daemon's would convert a
//! latency problem into a timeout problem one layer earlier, and a keep-alive limit higher than
//! the daemon's would stop a rolling restart from rolling.

use crate::forward::{self, Context};
use crate::ops;
use crate::pool::Pool;
use big_tls::{TlsConfig, Wire};
use big_wire::request::RequestError;
use big_wire::{log, Request, Response};
use std::io::Write;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

/// Connections allowed to wait for a worker. Past this, `503`.
///
/// The daemon's number, for the daemon's reason: a deep queue converts a latency problem into a
/// timeout problem, and a client that is told to come back is better served than one left
/// holding a socket.
pub const QUEUE_DEPTH: usize = 64;

/// How long a client may take to send a request.
pub const READ_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a client may hold an idle keep-alive connection.
pub const KEEPALIVE_IDLE: Duration = Duration::from_secs(5);

/// Requests on one client connection before it is closed.
pub const MAX_KEEPALIVE_REQUESTS: u32 = 1000;

/// How often each node is asked `GET /ready`, and how long it has to answer.
#[derive(Clone, Copy, Debug)]
pub struct HealthConfig {
    pub every: Duration,
    pub budget: Duration,
}

impl Default for HealthConfig {
    fn default() -> Self {
        // Two seconds between probes, one second to answer. Faster costs a connection per node
        // per second for a state that changes rarely; slower leaves a dead node in rotation for
        // longer than the passive ejection in `Pool::send` can cover on a quiet endpoint.
        Self { every: Duration::from_secs(2), budget: Duration::from_secs(1) }
    }
}

/// Everything the listener needs and nothing it does not.
pub struct Config {
    pub workers: usize,
    pub queue_depth: usize,
    pub read_timeout: Duration,
    pub allowed: crate::allowlist::Tier,
    pub trust_forwarded_for: bool,
    pub health: HealthConfig,
    /// What the *downstream* leg was, for `X-Forwarded-Proto`. Not the leg to the node.
    pub proto: &'static str,
    /// Following the cluster's membership, when the operator asked for it. `None` is the list
    /// this proxy was started with and nothing else - see [`crate::discover`].
    pub discovery: Option<crate::discover::Discovery>,
    /// A second listener where upstreams can be seeded while this runs. See [`crate::admin`].
    pub admin: Option<crate::admin::Admin>,
    /// The listener's own certificate, when it has one.
    ///
    /// Built with `peer_ca: None`, which takes rustls' `with_no_client_auth()` branch: this
    /// listener never asks for and never accepts a client certificate. It is a front door for
    /// people, and the identity it cares about arrives in an `Authorization` header.
    pub tls: Option<Arc<TlsConfig>>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            workers: std::thread::available_parallelism().map_or(8, |n| (n.get() * 4).max(8)),
            queue_depth: QUEUE_DEPTH,
            read_timeout: READ_TIMEOUT,
            allowed: crate::allowlist::Tier::Ddl,
            trust_forwarded_for: false,
            health: HealthConfig::default(),
            discovery: None,
            admin: None,
            proto: "http",
            tls: None,
        }
    }
}

pub struct Proxy {
    listener: TcpListener,
    pool: Arc<Pool>,
    config: Config,
    metrics: crate::metrics::Metrics,
    served: AtomicUsize,
}

impl Proxy {
    pub fn bind(
        addr: impl std::net::ToSocketAddrs,
        pool: Pool,
        config: Config,
    ) -> std::io::Result<Self> {
        Ok(Self {
            listener: TcpListener::bind(addr)?,
            pool: Arc::new(pool),
            config,
            metrics: crate::metrics::Metrics::new(),
            served: AtomicUsize::new(0),
        })
    }

    pub fn pool(&self) -> &Arc<Pool> {
        &self.pool
    }

    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    pub fn serve(&self) -> std::io::Result<()> {
        self.serve_while(&AtomicBool::new(true))
    }

    /// Accept and answer until `running` goes false.
    ///
    /// One thread does nothing but accept and hand off, because a thread that accepts and also
    /// works is a thread that stops accepting whenever the work is slow.
    pub fn serve_while(&self, running: &AtomicBool) -> std::io::Result<()> {
        let (tx, rx) = mpsc::sync_channel::<TcpStream>(self.config.queue_depth);
        let rx = Arc::new(std::sync::Mutex::new(rx));

        std::thread::scope(|scope| {
            for _ in 0..self.config.workers {
                let rx = Arc::clone(&rx);
                let pool = Arc::clone(&self.pool);
                scope.spawn(move || loop {
                    let stream = {
                        let guard = rx.lock().unwrap_or_else(|e| e.into_inner());
                        guard.recv()
                    };
                    let Ok(stream) = stream else { return };
                    self.converse(stream, &pool);
                });
            }

            // The health poller, in the same scope as the workers so it stops when they do.
            // One thread walks every node: a few dozen nodes probed every couple of seconds is
            // not work that needs a thread each.
            let pool = Arc::clone(&self.pool);
            let health = self.config.health;
            scope.spawn(move || pool.poll_while(running, health.every, health.budget));

            // Membership, on a thread of its own and in the same scope. Separate from the
            // health poller because they answer different questions at different costs: one is
            // an unauthenticated probe of every node, the other one authenticated read from a
            // single node, and folding them together would make each wait for the other.
            // The seeding port, on a listener of its own. Separate from the one above because
            // it is a different audience on a different address: this one is for whoever is on
            // the machine, and the check that keeps it that way is the bind.
            if let Some(admin) = &self.config.admin {
                let pool = Arc::clone(&self.pool);
                scope.spawn(move || admin.serve_while(&pool, running));
            }

            if let Some(discovery) = self.config.discovery.clone() {
                let pool = Arc::clone(&self.pool);
                let metrics = &self.metrics;
                scope.spawn(move || {
                    crate::discover::follow_while(&pool, &discovery, Some(metrics), running)
                });
            }

            while running.load(Ordering::Relaxed) {
                match self.listener.accept() {
                    Ok((stream, _)) => {
                        if let Err(mpsc::TrySendError::Full(stream)) = tx.try_send(stream) {
                            shed(stream);
                        }
                    }
                    // One connection failing is not the listener failing. A listener that exited
                    // on `ECONNABORTED` would take the process down for a client that hung up
                    // between the SYN and the accept.
                    Err(e) if transient(&e) => continue,
                    Err(e) => return Err(e),
                }
            }
            drop(tx);
            Ok(())
        })
    }

    /// How many requests this listener has answered, for the tests that need to wait for one.
    pub fn served(&self) -> usize {
        self.served.load(Ordering::Relaxed)
    }

    /// One client connection, for as long as it keeps asking.
    fn converse(&self, stream: TcpStream, pool: &Pool) {
        let client = match stream.peer_addr() {
            Ok(peer) => peer.ip(),
            // A connection whose peer cannot be named is one that has already gone.
            Err(_) => return,
        };
        let _ = stream.set_nodelay(true);
        // The handshake happens on a worker, never on the accepting thread: a slow or hostile
        // client would otherwise stop this process accepting anything at all.
        let _ = stream.set_read_timeout(Some(self.config.read_timeout));
        let Ok(wire) = Wire::accept(stream, self.config.tls.as_deref()) else {
            // A failed handshake is one client, and it is common enough — a scanner, a browser
            // sent to the wrong port — that it is not worth a line each.
            return;
        };
        let mut reader = std::io::BufReader::new(wire);

        for served in 0..MAX_KEEPALIVE_REQUESTS {
            // The first request has the full read timeout; a connection already kept alive gets
            // the shorter idle one, so a client holding a socket open contributes nothing.
            let budget = if served == 0 { self.config.read_timeout } else { KEEPALIVE_IDLE };
            if reader.get_ref().socket().set_read_timeout(Some(budget)).is_err() {
                return;
            }

            let (answer, keep_alive) = match Request::read(&mut reader) {
                Ok(req) => {
                    let keep_alive = req.wants_keep_alive();
                    (self.answer(&req, pool, client), keep_alive)
                }
                // A client that sent nothing and went away is not an error to report.
                Err(RequestError::Closed) => return,
                Err(e) => (e.into_response(), false),
            };

            let last = served + 1 >= MAX_KEEPALIVE_REQUESTS;
            let keep_alive = keep_alive && !last;
            let encoded = answer.encode(keep_alive);
            if reader.get_mut().write_all(&encoded).is_err() {
                return;
            }
            self.served.fetch_add(1, Ordering::Relaxed);
            if !keep_alive {
                close_politely(reader.get_ref().socket());
                return;
            }
        }
    }

    fn answer(&self, req: &Request, pool: &Pool, client: std::net::IpAddr) -> Response {
        let id = log::next_request_id();
        let started = std::time::Instant::now();

        let answer = match (req.method.as_str(), req.path.as_str()) {
            // Answered here, never forwarded — see `ops`.
            ("GET", "/health") => ops::health(),
            ("GET", "/ready") => ops::ready(pool),
            ("GET", "/metrics") => ops::metrics(pool, &self.metrics),
            _ => forward::forward(
                req,
                pool,
                &Context {
                    client,
                    proto: self.config.proto,
                    request_id: &id,
                    allowed: self.config.allowed,
                    trust_forwarded_for: self.config.trust_forwarded_for,
                    metrics: Some(&self.metrics),
                },
            ),
        };

        let elapsed = started.elapsed();
        self.metrics.request(answer.status, elapsed, req.body.len(), answer.body.len());

        log::emit(
            if answer.status >= 500 { log::Level::Error } else { log::Level::Info },
            "request",
            &[
                ("id", log::F::S(&id)),
                ("method", log::F::S(&req.method)),
                ("path", log::F::S(&req.path)),
                ("status", log::F::N(answer.status as u64)),
                ("duration_us", log::F::N(elapsed.as_micros() as u64)),
                ("peer", log::F::S(&client.to_string())),
            ],
        );
        answer
    }
}

/// Tell a client the queue is full, then close without discarding what was said.
fn shed(mut stream: TcpStream) {
    let answer = Response::failure(503, "server_busy", "this proxy is at capacity")
        .with_header("retry-after", 1);
    let _ = stream.set_write_timeout(Some(Duration::from_secs(1)));
    let _ = stream.write_all(&answer.encode(false));
    close_politely(&stream);
}

/// Drain what the client already sent before closing.
///
/// Dropping a socket with unread bytes in it sends `RST`, and an `RST` discards the response
/// that was just written — so the client sees a connection reset instead of the `503` explaining
/// why.
fn close_politely(stream: &TcpStream) {
    let _ = stream.set_read_timeout(Some(Duration::from_millis(100)));
    let mut sink = [0u8; 1024];
    for _ in 0..8 {
        match std::io::Read::read(&mut &*stream, &mut sink) {
            Ok(0) | Err(_) => break,
            Ok(_) => continue,
        }
    }
    let _ = stream.shutdown(std::net::Shutdown::Both);
}

/// Whether this accept error is one connection failing rather than the listener failing.
fn transient(e: &std::io::Error) -> bool {
    use std::io::ErrorKind::*;
    matches!(e.kind(), ConnectionAborted | Interrupted | ConnectionReset | WouldBlock)
        // Out of file descriptors is the process being at its limit, not the socket being
        // broken: the next accept after one closes will work.
        || matches!(e.raw_os_error(), Some(libc_emfile) if libc_emfile == 24 || libc_emfile == 23)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hung_up_client_does_not_stop_the_listener() {
        for kind in [
            std::io::ErrorKind::ConnectionAborted,
            std::io::ErrorKind::Interrupted,
            std::io::ErrorKind::ConnectionReset,
        ] {
            assert!(transient(&std::io::Error::from(kind)), "{kind:?} took the listener down");
        }
    }

    #[test]
    fn a_real_failure_is_not_transient() {
        assert!(!transient(&std::io::Error::from(std::io::ErrorKind::AddrInUse)));
        assert!(!transient(&std::io::Error::from(std::io::ErrorKind::PermissionDenied)));
    }

    /// Out of descriptors is the process at its limit; one close and the next accept works.
    #[test]
    fn running_out_of_descriptors_is_transient() {
        assert!(transient(&std::io::Error::from_raw_os_error(24)));
        assert!(transient(&std::io::Error::from_raw_os_error(23)));
    }

    /// Checked at compile time rather than at test time: a ceiling that drifted above the
    /// daemon's should not be something a test run has the option of not reaching.
    #[test]
    fn the_ceilings_stay_inside_the_daemons() {
        const _: () = assert!(MAX_KEEPALIVE_REQUESTS <= 1000);
        const _: () = assert!(crate::upstream::MAX_REQUESTS_PER_CONN < MAX_KEEPALIVE_REQUESTS);
        assert!(KEEPALIVE_IDLE <= Duration::from_secs(5));
        assert!(crate::upstream::MAX_IDLE < KEEPALIVE_IDLE);
    }

    #[test]
    fn the_default_pool_is_never_empty() {
        assert!(Config::default().workers >= 8);
        assert_eq!(Config::default().queue_depth, QUEUE_DEPTH);
    }
}
