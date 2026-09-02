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

//! The smallest HTTP surface that makes the engine reachable from outside the process.
//!
//! **This decides something the project had left open.** Until now `big` was a library and the
//! README said whether it becomes embedded or a server was undecided. A listener decides it.
//! What is here is deliberately the least server that could work, so that the decision stays
//! cheap to revisit: no framework, no async runtime, no protocol beyond HTTP/1.1 with a
//! `Content-Length` body.
//!
//! **Concurrency is a fixed pool, not a thread per connection.** It used to be a thread per
//! connection with no ceiling, which made the connection count an unbounded multiplier on
//! both memory and threads - a client could hold as many as it could open. Worse, an accepted
//! socket had no timeout of any kind, so a connection that sent one byte and then nothing held
//! a thread for as long as it liked. Both are fixed here: the pool bounds how many requests
//! run at once, the queue bounds how many wait, and anything past that is refused with a `503`
//! immediately rather than queued invisibly. Shedding load is the honest answer; queueing it
//! only moves the failure somewhere harder to see.
//!
//! **Transport security is not here and is not going to be.** Termination belongs to a reverse
//! proxy - see `runbook.md`. A TLS stack would be a larger dependency than the entire engine,
//! and a hand-written one is out of the question. What *is* enforced is the half that keeps
//! that from being an excuse: `big serve` refuses to bind anywhere but loopback without a token
//! file.

#![deny(unsafe_code)]
#![deny(missing_docs)]

pub mod auth;
pub mod json;
pub mod log;
pub mod metrics;
pub mod request;
pub mod response;
pub mod routes;
pub mod status;
mod watchdog;

pub use auth::{Auth, Role};
pub use request::Request;
pub use response::{reason_for, Response};

use big_api::Api;
use big_cluster::Cluster;
use big_pager::PagerMut;
use metrics::ServerMetrics;
use std::io::Write;
use std::net::{TcpListener, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};
use watchdog::Watchdog;

/// Largest request body accepted, so a single client cannot ask the process to allocate
/// without bound.
pub const MAX_BODY: usize = 8 << 20;

/// The same, for the `/internal/` routes one node uses to reach another.
///
/// Larger because the body is not a stranger's: it is what a coordinator made of a request
/// that had already passed [`MAX_BODY`], and the binary encoding of a batch of facts runs to
/// roughly twice the text it was parsed from - a length and a tag per field where the text had
/// a space. A public body that grew into that headroom is still refused; only a peer's is not.
pub const MAX_INTERNAL_BODY: usize = 4 * MAX_BODY;

/// Every ceiling the server enforces on itself.
///
/// All of these have defaults that a loopback server can be started with and forgotten; they
/// exist so that a server facing anything else can be told what it is facing.
#[derive(Clone, Debug)]
pub struct ServerConfig {
    /// Requests handled at once. Past this, connections wait in the queue.
    pub workers: usize,
    /// Connections allowed to wait for a worker. Past this, they are refused with a `503`.
    ///
    /// Small on purpose. A deep queue converts a latency problem into a timeout problem and
    /// hides it from the connection-rejected counter, which is the number an operator would
    /// otherwise use to size the pool.
    pub queue_depth: usize,
    /// How long a client may take to send its request. This is the slow-loris ceiling.
    pub read_timeout: Duration,
    /// How long a client may take to accept its response.
    pub write_timeout: Duration,
    /// How long a kept-alive connection may sit idle before this server closes it.
    ///
    /// Much shorter than [`ServerConfig::read_timeout`], and it has to be: a connection waiting
    /// for its next request is holding a worker from a fixed pool, so the idle window is the
    /// length of time one client can occupy a worker having asked for nothing.
    pub keepalive_idle: Duration,
    /// Requests one connection may carry before it is closed anyway.
    ///
    /// A ceiling rather than a policy. Nothing in the engine leaks per connection; this is here
    /// so that a connection cannot be immortal, which is the property that makes a rolling
    /// restart behind a proxy actually roll.
    pub max_keepalive_requests: usize,
    /// Wall-clock budget for one query. `None` lets a query run to completion.
    pub query_timeout: Option<Duration>,
    /// Who may do what. [`Auth::disabled`] lets every request through, which `big serve` permits
    /// only on a loopback bind.
    pub auth: Auth,
    /// Where `POST /admin/backup` may write. `None` disables the route.
    ///
    /// A directory settled at start-up rather than a path chosen per request. An `admin` token
    /// can already drop every table, so what is withheld here is not power over the data - it
    /// is power over the rest of the filesystem, and that is a much larger thing to hand to
    /// whoever holds a token.
    pub backup_dir: Option<String>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
        Self {
            // Four per core rather than one: a worker spends most of its life on a socket, not
            // in the engine, and the engine fans a read out across its own threads anyway.
            workers: (cores * 4).max(8),
            queue_depth: 64,
            read_timeout: Duration::from_secs(30),
            write_timeout: Duration::from_secs(30),
            keepalive_idle: Duration::from_secs(5),
            max_keepalive_requests: 1_000,
            // No default deadline. A query that used to finish must not start failing because
            // the server was upgraded, so the ceiling is something an operator opts into.
            query_timeout: None,
            auth: Auth::disabled(),
            // Off. A daemon that backed itself up somewhere by default would be a daemon
            // filling a disk nobody chose.
            backup_dir: None,
        }
    }
}

/// Everything a request handler needs, shared by every worker.
struct State<P: PagerMut> {
    /// The database, and whatever other nodes hold the rest of it. A server built from a bare
    /// `Api` gets a cluster of one; nothing here can tell the difference, which is the point.
    cluster: Cluster<P>,
    metrics: ServerMetrics,
    config: ServerConfig,
    /// Connections currently holding a worker on a kept-alive connection.
    ///
    /// The admission control that makes keep-alive safe against a fixed pool. Without it, a
    /// handful of clients that each open a few persistent connections can hold every worker
    /// while sending nothing at all, and the server stops answering anyone - which is a worse
    /// failure than the reconnect keep-alive was avoiding.
    kept_alive: std::sync::atomic::AtomicUsize,
    /// Whether a backup is walking this node's file right now.
    ///
    /// One at a time: two walks would each pin the reclaim horizon for their whole run while
    /// writers kept committing, so the file would grow by everything both of them saw.
    backing_up: AtomicBool,
    /// Set when this server has been asked to stand down.
    ///
    /// A kept-alive connection reads this before waiting for another request. Without it a
    /// peer that keeps sending holds a worker for as long as it likes - a node standing down
    /// while its cluster is still heartbeating at it would drain only after the connection hit
    /// its own request ceiling, which is a shutdown measured in minutes.
    stopping: AtomicBool,
}

impl<P: PagerMut> State<P> {
    /// How many connections may be held open between requests at once.
    ///
    /// Half the pool. The other half is what keeps a saturated server able to accept work from
    /// somebody new, which is the whole reason the pool has a ceiling in the first place.
    fn keepalive_ceiling(&self) -> usize {
        (self.config.workers / 2).max(1)
    }
}

/// A bound listener and the database behind it.
///
/// Binding and serving are separate steps so a caller can learn the port - and refuse to start
/// for its own reasons - before any request is accepted.
pub struct Server<P: PagerMut + Sync + Send + 'static> {
    state: Arc<State<P>>,
    listener: TcpListener,
}

impl<P: PagerMut + Sync + Send + 'static> Server<P> {
    /// Binds with [`ServerConfig::default`], whose worker count follows the machine.
    pub fn bind(api: Api<P>, addr: impl ToSocketAddrs) -> std::io::Result<Self> {
        Self::bind_with(api, addr, ServerConfig::default())
    }

    /// Binds with an explicit configuration, serving one database and no peers.
    pub fn bind_with(
        api: Api<P>,
        addr: impl ToSocketAddrs,
        config: ServerConfig,
    ) -> std::io::Result<Self> {
        Self::bind_cluster(Cluster::solo(api), addr, config)
    }

    /// Binds as one node of a configured cluster.
    ///
    /// The only difference from [`Server::bind_with`] is how many nodes the coordinator knows
    /// about. Every route runs the same code either way.
    pub fn bind_cluster(
        cluster: Cluster<P>,
        addr: impl ToSocketAddrs,
        config: ServerConfig,
    ) -> std::io::Result<Self> {
        Ok(Self {
            state: Arc::new(State {
                cluster,
                metrics: ServerMetrics::new(),
                config,
                kept_alive: std::sync::atomic::AtomicUsize::new(0),
                backing_up: AtomicBool::new(false),
                stopping: AtomicBool::new(false),
            }),
            listener: TcpListener::bind(addr)?,
        })
    }

    /// The address actually bound, which is how a caller finds the port after asking for zero.
    pub fn local_addr(&self) -> std::io::Result<std::net::SocketAddr> {
        self.listener.local_addr()
    }

    /// The configuration in force, including the defaults this server filled in for itself.
    pub fn config(&self) -> &ServerConfig {
        &self.state.config
    }

    /// The database this server was built around, for a caller that has to ask it something
    /// before serving - a startup line that reports the configuration it is running under.
    pub fn api(&self) -> &Api<P> {
        self.state.cluster.local()
    }

    /// The coordinator, for a caller that has to ask which shards this node owns.
    pub fn cluster(&self) -> &Cluster<P> {
        &self.state.cluster
    }

    /// Serves until the listener fails.
    ///
    /// The accepting thread does no work beyond handing a socket over, so it is always
    /// available to accept the next one - which is what makes shedding possible at all. A
    /// server that is saturated must still be able to say so.
    pub fn serve(&self) -> std::io::Result<()> {
        self.accept_loop(None)
    }

    /// Serves until `running` is cleared, then returns so the caller can drop the listener.
    ///
    /// The blocking accept in [`Server::serve`] cannot be interrupted, which is fine for a
    /// daemon that runs until it is killed and useless for anything that has to stop: a test
    /// that takes a node away, and a daemon asked to stand down. The cost is a poll rather
    /// than a block, which is why it is a second entry point and not the only one.
    ///
    /// A port that accepts and never answers is not a stopped node, it is a slow one, and the
    /// two fail very differently - so this returns rather than parking, and dropping the
    /// `Server` afterwards is what actually closes the port.
    pub fn serve_while(&self, running: &AtomicBool) -> std::io::Result<()> {
        self.accept_loop(Some(running))
    }

    fn accept_loop(&self, running: Option<&AtomicBool>) -> std::io::Result<()> {
        let (tx, rx) = mpsc::sync_channel::<TcpStream>(self.state.config.queue_depth);
        let rx = Arc::new(Mutex::new(rx));

        let mut workers = Vec::with_capacity(self.state.config.workers);
        for _ in 0..self.state.config.workers {
            let rx = Arc::clone(&rx);
            let state = Arc::clone(&self.state);
            workers.push(std::thread::spawn(move || {
                loop {
                    // The guard is dropped before the request runs. Holding it across `handle`
                    // would turn the pool into a single worker with extra steps.
                    let job = { rx.lock().unwrap().recv() };
                    let Ok(stream) = job else { return };
                    let _ = handle(&state, stream);
                }
            }));
        }

        log::emit(
            log::Level::Info,
            "listening",
            &[
                ("addr", log::F::S(&self.listener.local_addr()?.to_string())),
                ("workers", log::F::N(self.state.config.workers as u64)),
                ("queue_depth", log::F::N(self.state.config.queue_depth as u64)),
                ("auth", log::F::B(self.state.config.auth.is_enabled())),
            ],
        );

        if running.is_some() {
            self.listener.set_nonblocking(true)?;
        }
        loop {
            if running.is_some_and(|r| !r.load(Ordering::Relaxed)) {
                // Finish what is in flight and take nothing more, so a peer that is still
                // heartbeating cannot hold a worker past this point.
                self.state.stopping.store(true, Ordering::Relaxed);
                // **Before draining the workers, not after.** A node standing down has nothing
                // to say to its peers, and every message it sends now is one they will answer
                // to a socket that is about to close. Stopping first also stops the connections
                // that would otherwise arrive during the drain and take a worker each.
                self.state.cluster.stop();
                break;
            }
            match self.listener.accept() {
                Ok((stream, _)) => {
                    // An accepted socket does not inherit the listener's mode on every
                    // platform, and a handler needs it not to. One syscall removes the
                    // platform from the question.
                    stream.set_nonblocking(false)?;
                    self.state.metrics.connection_accepted();
                    if let Err(mpsc::TrySendError::Full(stream)) = tx.try_send(stream) {
                        self.state.metrics.connection_rejected();
                        shed(stream);
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock && running.is_some() => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                // **One connection failing is not the listener failing.** A client that
                // disconnects between the handshake and the accept produces `ECONNABORTED`
                // here, and treating that as fatal takes the whole server down because one
                // caller changed its mind - which is exactly what a coordinator does to a
                // peer it has given up waiting for.
                Err(e) if transient(&e) => continue,
                Err(e) => {
                    drop(tx);
                    for w in workers {
                        let _ = w.join();
                    }
                    return Err(e);
                }
            }
        }

        drop(tx);
        for w in workers {
            let _ = w.join();
        }
        Ok(())
    }

    /// Serves exactly `n` connections and returns. Lets a test drive the real socket path
    /// without leaving a thread running after it.
    ///
    /// Deliberately not the pool: a test that asserts on `n` responses wants them handled in
    /// the order they arrived and wants the call to return when they are done, and a pool
    /// gives neither.
    pub fn serve_n(&self, n: usize) -> std::io::Result<()> {
        for stream in self.listener.incoming().take(n) {
            let _ = handle(&self.state, stream?);
        }
        Ok(())
    }

    /// The counters, for a caller that embeds the server rather than scraping it.
    pub fn metrics(&self) -> &ServerMetrics {
        &self.state.metrics
    }
}

/// Whether an accept failed for a reason that is about one connection rather than the socket.
///
/// `ECONNABORTED` is a client that went away between the handshake and the accept.
/// `EINTR` is a signal. `EMFILE` and `ENFILE` are running out of descriptors, which is
/// temporary and self-correcting once the requests in flight finish - and which is very much
/// not a reason to stop answering everybody else.
fn transient(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::Interrupted
            | std::io::ErrorKind::ConnectionReset
    ) || matches!(e.raw_os_error(), Some(libc_emfile) if libc_emfile == 24 || libc_emfile == 23)
}

/// Turns a connection away without giving it a worker.
///
/// Written from the accepting thread, which is the only way this can be a fast answer: if
/// saying "too busy" needed a worker, there would be nothing left to say it with.
fn shed(mut stream: TcpStream) {
    let response = Response::failure(503, "server_busy", "every worker is busy; retry shortly")
        .with_header("Retry-After", 1);
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
    let _ = stream.write_all(&response.encode(false));
    log::emit(log::Level::Warn, "connection_shed", &[("status", log::F::N(503))]);
}

/// One connection: answer requests on it until somebody decides it is finished.
///
/// Usually once. A connection is only kept open when the client asked for it *and* the server
/// has workers to spare, because a worker held by an idle connection is a worker not serving
/// anybody. The traffic that asks is the fan-out between nodes, which is also the traffic that
/// pays for it: a coordinator opens one connection per peer per request otherwise.
fn handle<P: PagerMut + Sync>(state: &State<P>, stream: TcpStream) -> std::io::Result<()> {
    let peer = stream.peer_addr().ok().map(|a| a.to_string());

    // Before the first read, so a client that connects and says nothing is on a clock from the
    // start. This is the whole slow-loris fix; everything else here is bookkeeping.
    stream.set_read_timeout(Some(state.config.read_timeout))?;
    stream.set_write_timeout(Some(state.config.write_timeout))?;

    // The socket is read through one buffered reader for the life of the connection. Building
    // one per request would take the front of the next request into a buffer that is then
    // dropped, which nothing notices until a client pipelines.
    let socket = stream.try_clone()?;
    let mut reader = std::io::BufReader::new(stream);

    for n in 0..state.config.max_keepalive_requests {
        // Only from the second request onward: the first is covered by `read_timeout`, which
        // is the ceiling on a client that has connected and not yet said anything.
        let held = if n == 0 {
            None
        } else if state.stopping.load(Ordering::Relaxed) {
            break;
        } else {
            let Some(held) = KeptAlive::admit(state) else { break };
            reader.get_ref().set_read_timeout(Some(state.config.keepalive_idle))?;
            Some(held)
        };

        let again = serve_one(state, &mut reader, &socket, peer.as_deref())?;
        drop(held);
        if !again {
            break;
        }
    }
    Ok(())
}

/// One request on an open connection. `Ok(true)` means the connection may carry another.
fn serve_one<P: PagerMut + Sync>(
    state: &State<P>,
    reader: &mut std::io::BufReader<TcpStream>,
    socket: &TcpStream,
    peer: Option<&str>,
) -> std::io::Result<bool> {
    let started = Instant::now();
    let id = log::next_request_id();

    let (response, method, path, bytes_in, keep) = match Request::read(reader) {
        // Nobody is left to answer. A kept-alive connection ends this way every time, so it is
        // not logged as anything: a line per client that finished politely is a line per
        // client.
        Err(request::RequestError::Closed) => return Ok(false),
        Ok(req) => {
            let bytes_in = req.body.len();
            let (method, path) = (req.method.clone(), req.path.clone());
            let keep = req.wants_keep_alive();
            // A panic in a handler used to kill the thread it ran on. Under a thread per
            // connection that cost one connection; under a pool it costs a worker
            // permanently, and a pool that quietly drains to nothing is a far worse failure
            // than the panic. Caught here rather than in the worker loop so that the client
            // still gets an answer and the log still gets a line.
            let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                answer(state, &req, socket)
            }));
            let response = caught.unwrap_or_else(|_| {
                let mut r =
                    Response::failure(500, "panic", "the server could not complete this request");
                r.detail = Some(format!("a handler panicked serving {} {}", req.method, req.path));
                r
            });
            (response, method, path, bytes_in, keep)
        }
        Err(e) => (e.into_response(), "-".to_string(), "-".to_string(), 0, false),
    };

    // A request this server could not make sense of, or could not complete, takes the
    // connection with it. Reading the next request means trusting that this one ended where
    // the headers said it did, and a `400` is exactly the case where that is in doubt.
    let keep = keep && response.status < 500 && response.status != 400 && response.status != 413;

    let response = response.with_header("X-Request-Id", &id);
    let encoded = response.encode(keep);

    // A cancelled request means the client is already gone, so writing is pointless and
    // failing to write is expected rather than an error worth reporting.
    let write = {
        let mut out = socket;
        out.write_all(&encoded).and_then(|()| out.flush())
    };

    let elapsed = started.elapsed();
    state.metrics.request(response.status, elapsed, bytes_in, response.body.len());
    if response.status == 504 {
        state.metrics.query_timed_out();
    }
    if response.status == 499 {
        state.metrics.query_cancelled();
    }
    if response.status == 401 || response.status == 403 {
        state.metrics.request_unauthorized();
    }

    let level = if response.status >= 500 { log::Level::Error } else { log::Level::Info };
    let mut fields = vec![
        ("id", log::F::S(&id)),
        ("method", log::F::S(&method)),
        ("path", log::F::S(&path)),
        ("status", log::F::N(response.status as u64)),
        ("duration_us", log::F::N(elapsed.as_micros().min(u64::MAX as u128) as u64)),
        ("bytes_in", log::F::N(bytes_in as u64)),
        ("bytes_out", log::F::N(response.body.len() as u64)),
    ];
    if let Some(peer) = peer {
        fields.push(("peer", log::F::S(peer)));
    }
    if let Some(code) = response.code {
        fields.push(("code", log::F::S(code)));
    }
    // The message the client was NOT given, for the one audience that is allowed to see it.
    if let Some(detail) = &response.detail {
        fields.push(("detail", log::F::S(detail)));
    }
    log::emit(level, "request", &fields);

    write?;
    Ok(keep)
}

/// A worker held by a connection that is waiting for its next request.
///
/// A guard rather than a pair of calls, because the count has to come back down however the
/// request ends - including the panic path, which is the one a pair of calls always misses.
struct KeptAlive<'a> {
    count: &'a std::sync::atomic::AtomicUsize,
}

impl<'a> KeptAlive<'a> {
    /// `None` when too many workers are already holding a connection open, which is the point
    /// at which this server would rather have the reconnect than the starvation.
    fn admit<P: PagerMut>(state: &'a State<P>) -> Option<Self> {
        let count = &state.kept_alive;
        let ceiling = state.keepalive_ceiling();
        // A compare-and-swap rather than fetch_add-then-check: the check has to be part of the
        // increment, or every worker can pass a ceiling that none of them has yet crossed.
        let mut held = count.load(Ordering::Relaxed);
        loop {
            if held >= ceiling {
                return None;
            }
            match count.compare_exchange_weak(held, held + 1, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => return Some(Self { count }),
                Err(actual) => held = actual,
            }
        }
    }
}

impl Drop for KeptAlive<'_> {
    fn drop(&mut self) {
        self.count.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Dispatches one request, with a watchdog on the routes that can run long.
fn answer<P: PagerMut + Sync>(state: &State<P>, req: &Request, stream: &TcpStream) -> Response {
    let mut ctx = routes::Ctx {
        cluster: &state.cluster,
        auth: &state.config.auth,
        metrics: &state.metrics,
        query_timeout: state.config.query_timeout,
        cancel: None,
        backup_dir: state.config.backup_dir.as_deref(),
        backup_running: &state.backing_up,
    };

    // Only a query gets a watchdog. Everything else is bounded by the body the client already
    // sent, and a thread per request to watch work that finishes in microseconds would cost
    // more than the work.
    if !routes::may_run_long(req) {
        return routes::dispatch(&ctx, req);
    }

    let cancel = Arc::new(AtomicBool::new(false));
    ctx.cancel = Some(Arc::clone(&cancel));
    let watchdog = Watchdog::spawn(stream, cancel);
    let response = routes::dispatch(&ctx, req);
    watchdog.stop(stream);
    response
}
