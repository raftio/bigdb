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
//! **Transport security is here now, and this paragraph used to say it never would be.** The old
//! argument was that a TLS stack is a larger dependency than the entire engine, and that
//! terminating at a reverse proxy protects a bearer token well enough to be worth it. Both
//! halves of that were true. What changed is not the cost - it is what was being protected. A
//! token belongs to this database and to nothing else; a password is a thing a person also uses
//! somewhere else, so sending one in the clear risks something that was never ours to risk. So
//! [`ServerConfig::tls`] exists, behind a cargo feature that keeps the old tree available to
//! anybody who still wants the proxy - and `big serve` still refuses to bind anywhere but
//! loopback without both a users file and a certificate.

#![deny(unsafe_code)]
#![deny(missing_docs)]

pub mod auth;
pub mod json;
pub mod metrics;
pub mod routes;
pub mod status;
mod steward;
mod watchdog;
mod writer;

/// Re-exported from [`big_wire`], which is where the HTTP/1.1 framing lives now.
///
/// Kept at these paths so `big_http::Request`, `big_http::log` and the rest go on resolving: the
/// split was for `big-proxy`'s dependency graph, not for anybody's imports. The one thing that
/// did move is `Response::from_error`, which classifies an engine error and so could not follow
/// the parser out — it is [`status::response_for`] now.
pub use big_wire::{log, request, response};

pub use auth::Auth;
pub use big_wire::{reason_for, Request, Response, MAX_BODY, MAX_INTERNAL_BODY};

use big_cluster::Cluster;
use big_embed::Api;
use big_pager::PagerMut;
use big_tls::{TlsConfig, Wire, WireError};
use metrics::ServerMetrics;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};
use watchdog::Watchdog;

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
    /// The certificate this listener presents, and the CA a peer's client certificate must chain
    /// to. `None` serves in the clear, which `big serve` permits only on a loopback bind.
    ///
    /// When the cluster may reshape itself, and how hard. Off by default.
    pub balance: big_cluster::balance::Policy,
    /// Whether this node may give trailing free pages back to the filesystem while it serves.
    ///
    /// Off by default, like everything else that acts without being asked. What it changes is
    /// that a file which has churned can get smaller without stopping the daemon; what it
    /// costs is the write lock for the length of one truncation, and only once a quarter of
    /// the file is reclaimable.
    pub reclaim: bool,
    /// An ordinary `Option` with no `#[cfg]` on it: `TlsConfig` is uninhabited in a build with
    /// the feature off, so this is provably `None` there and every construction site in the tree
    /// - tests included - compiles either way without knowing which build it is in.
    pub tls: Option<TlsConfig>,
    /// Where `POST /admin/backup` may write. `None` disables the route.
    ///
    /// A directory settled at start-up rather than a path chosen per request. An `admin` token
    /// can already drop every table, so what is withheld here is not power over the data - it
    /// is power over the rest of the filesystem, and that is a much larger thing to hand to
    /// whoever holds a token.
    pub backup_dir: Option<String>,
    /// How many `GET /watch` subscriptions to hold at once. `0`, the default, turns the route
    /// off: a subscriber holds a worker for as long as it stays connected, so an uncapped
    /// version of this route is a way to take the pool away from every other request.
    pub watch_max: usize,
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
            // In the clear. Same reasoning as `auth`, and the same safety net: a loopback-only
            // server is the default, and `big serve` refuses to bind anywhere else without one.
            tls: None,
            balance: big_cluster::balance::Policy::default(),
            reclaim: false,
            // Off. A daemon that backed itself up somewhere by default would be a daemon
            // filling a disk nobody chose.
            backup_dir: None,
            // Off, like everything that lets a client hold something open.
            watch_max: 0,
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
    /// Live `GET /watch` subscriptions, against `config.watch_max`.
    watching: std::sync::atomic::AtomicUsize,
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
    ///
    /// The listener comes first here, where every other path builds the coordinator first.
    /// That is what lets the solo node report the address it is *actually* on rather than the
    /// one it was asked for - the two differ whenever the caller asked for port zero, and the
    /// difference is the whole value of the report. See [`Cluster::solo`].
    pub fn bind_with(
        api: Api<P>,
        addr: impl ToSocketAddrs,
        config: ServerConfig,
    ) -> std::io::Result<Self> {
        let listener = TcpListener::bind(addr)?;
        let local = listener.local_addr()?.to_string();
        Self::assembled(Cluster::solo(api, &local), listener, config)
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
        Self::assembled(cluster, TcpListener::bind(addr)?, config)
    }

    /// The two `bind` paths once they each have a listener, so that what a server *is* is
    /// written once however it was reached.
    fn assembled(
        cluster: Cluster<P>,
        listener: TcpListener,
        config: ServerConfig,
    ) -> std::io::Result<Self> {
        // Once, here, because the ceiling on concurrent password verifications is a fact about
        // the worker pool and `Auth` cannot know the pool from where it is built.
        config.auth.size_for(config.workers);
        // **The agreement is the roster, and this is where the two meet.** The listener decides
        // which peer certificates to admit and the agreement decides who is in the cluster; a
        // node that joined would otherwise present a certificate this listener refuses, because
        // it names somebody the file it read has never heard of.
        if let Some(tls) = config.tls.clone() {
            cluster.follow_roster(tls);
        }
        Ok(Self {
            state: Arc::new(State {
                cluster,
                metrics: ServerMetrics::new(),
                config,
                kept_alive: std::sync::atomic::AtomicUsize::new(0),
                watching: std::sync::atomic::AtomicUsize::new(0),
                backing_up: AtomicBool::new(false),
                stopping: AtomicBool::new(false),
            }),
            listener,
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

        // **A lane of its own for saying "too busy", but only when saying it costs a handshake.**
        // Shedding used to happen on the accepting thread, which was right: the answer is thirty
        // bytes and writing them costs nothing. Under TLS it is a handshake first, and a
        // handshake on the accepting thread stops it accepting - precisely the failure shedding
        // exists to avoid. So a plaintext listener keeps the old path exactly, and a TLS one
        // hands the socket to one thread whose only job is handshake, `503`, close.
        //
        // Deeper than the worker queue on purpose. This queue holds connections that are already
        // being turned away, and every slot in it is a client that gets a `503` instead of a
        // reset - so the memory buys something an operator can see.
        let sheds_inline = self.state.config.tls.is_none();
        let (shed_tx, shed_rx) =
            mpsc::sync_channel::<TcpStream>((self.state.config.queue_depth * 4).max(64));
        let shedder = {
            let state = Arc::clone(&self.state);
            std::thread::spawn(move || {
                while let Ok(stream) = shed_rx.recv() {
                    shed(&state, stream);
                }
            })
        };

        // The node's own slow work - balancing, and whatever else is switched on - on a thread
        // that is neither the agreement's nor a worker's. See `steward.rs` for why it can be
        // neither. Stopped the way the workers are: `stopping` is set on every way out of this
        // loop, and it is joined after them.
        let steward = {
            let state = Arc::clone(&self.state);
            std::thread::Builder::new()
                .name("big-steward".to_string())
                .spawn(move || steward::run(&state))
                .expect("one steward thread")
        };

        // The clock behind `?ack=queued`, and only when something can use it. A node that
        // never answers a write early has nothing for this to commit, so it does not get a
        // thread it would only wake to find an empty queue. See `writer.rs`.
        let scribe = self.state.cluster.local().takes_async_writes().then(|| {
            let state = Arc::clone(&self.state);
            std::thread::Builder::new()
                .name("big-writer".to_string())
                .spawn(move || writer::run(&state))
                .expect("one writer thread")
        });

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
                // The three of these are one security posture, so they are on one line. An
                // operator reading a log after an incident should not have to find out from
                // three different places whether the port was authenticated, encrypted, and
                // checking its peers.
                ("auth", log::F::B(self.state.config.auth.is_enabled())),
                ("tls", log::F::B(self.state.config.tls.is_some())),
                (
                    "peer_ca",
                    log::F::B(self.state.config.tls.as_ref().is_some_and(TlsConfig::checks_peers)),
                ),
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
                        if sheds_inline {
                            // No handshake to do, so this is the fast answer it always was and
                            // every refused connection is told why.
                            shed(&self.state, stream);
                        } else if let Err(mpsc::TrySendError::Full(_)) = shed_tx.try_send(stream) {
                            // Past what the shedding thread can keep up with, the connection is
                            // closed in silence. An unspoken refusal beats an accept loop that
                            // has stopped accepting, and the counter says how often it happened.
                            self.state.metrics.connection_shed_unspoken();
                        }
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
                    // A listener that has failed is a server that is stopping, whatever the
                    // caller asked for; the steward has to hear that too, or it outlives the
                    // port it was serving.
                    self.state.stopping.store(true, Ordering::Relaxed);
                    drop(tx);
                    drop(shed_tx);
                    for w in workers {
                        let _ = w.join();
                    }
                    let _ = shedder.join();
                    let _ = steward.join();
                    if let Some(scribe) = scribe {
                        let _ = scribe.join();
                    }
                    // After the workers, so nothing can still be submitting into the drain.
                    writer::stop(&self.state);
                    return Err(e);
                }
            }
        }

        drop(tx);
        drop(shed_tx);
        for w in workers {
            let _ = w.join();
        }
        let _ = shedder.join();
        let _ = steward.join();
        if let Some(scribe) = scribe {
            let _ = scribe.join();
        }
        // **After the workers are joined, and that is the whole of it.** Only then can no
        // thread submit another write, so what this drains is a queue that cannot grow under
        // it. Placed earlier, it would leave behind exactly the acknowledged writes it exists
        // to save.
        writer::stop(&self.state);
        Ok(())
    }

    /// Serves exactly `n` connections and returns. Lets a test drive the real socket path
    /// without leaving a thread running after it.
    ///
    /// Deliberately not the pool: a test that asserts on `n` responses wants them handled in
    /// the order they arrived and wants the call to return when they are done, and a pool
    /// gives neither.
    ///
    /// This does handshake on the accepting thread, which the pooled accept loop goes out of
    /// its way not to do. That is correct here and must not be "fixed": there is one thread, the
    /// connections are counted, and handling them in order is the entire point.
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

/// Closes a socket in a way that does not throw away what was just written to it.
///
/// **Dropping a socket that still has unread bytes in its receive queue sends an `RST`, not a
/// `FIN`** - and an `RST` discards whatever is still in the send queue. So a client that spoke
/// plaintext to a TLS port would get "connection reset by peer" instead of the sentence
/// explaining what it did wrong, which is the exact failure the sentence exists to prevent. The
/// request that arrived was peeked and never read, so there is always something in that queue.
///
/// Draining is bounded twice over - by a short timeout and by a byte ceiling - because the
/// client on the other end of this is by definition one that is not following the protocol, and
/// "read until they stop" is not a promise worth making to it.
fn close_politely(mut sock: TcpStream) {
    const DRAIN_BUDGET: Duration = Duration::from_millis(250);
    const DRAIN_CEILING: usize = 64 << 10;

    let _ = sock.flush();
    let _ = sock.shutdown(std::net::Shutdown::Write);
    let _ = sock.set_read_timeout(Some(DRAIN_BUDGET));
    let mut seen = 0usize;
    let mut buf = [0u8; 4096];
    while seen < DRAIN_CEILING {
        match sock.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => seen += n,
        }
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
/// On its own thread rather than on the accepting one, which is where this used to live. Under
/// TLS the `503` has to go through a handshake to be legible at all - a client waiting for a
/// ServerHello that is handed `HTTP/1.1 503` reports a protocol error and never sees the
/// `Retry-After` - and a handshake on the accepting thread stops it accepting.
///
/// A short budget of its own, deliberately shorter than a served connection's: this handshake
/// exists only to carry a refusal, and a client too slow to complete it is a client that can
/// have the refusal by way of a closed socket instead.
fn shed<P: PagerMut + Sync>(state: &State<P>, stream: TcpStream) {
    const BUDGET: Duration = Duration::from_secs(2);
    let _ = stream.set_read_timeout(Some(BUDGET));
    let _ = stream.set_write_timeout(Some(BUDGET));

    let response = Response::failure(503, "server_busy", "every worker is busy; retry shortly")
        .with_header("Retry-After", 1);
    match Wire::accept(stream, state.config.tls.as_ref()) {
        Ok(mut wire) => {
            let _ = wire.write_all(&response.encode(false)).and_then(|()| wire.flush());
        }
        // Plaintext on a TLS port, while saturated. The client gets the same `503` it would
        // have got on a plain port: it is already talking in the clear, and telling it about
        // TLS is less use than telling it to come back.
        Err(WireError::Plaintext(mut sock)) => {
            let _ = sock.write_all(&response.encode(false));
            close_politely(sock);
        }
        Err(_) => state.metrics.handshake_failed(),
    }
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

    // Before the first read *and before the handshake*, so a client that connects and says
    // nothing is on a clock from the start - a socket that never sends a ClientHello is the same
    // slow loris as one that never sends a request line. `Wire` inherits both of these, because
    // a timeout lives on the open file description rather than on the handle.
    stream.set_read_timeout(Some(state.config.read_timeout))?;
    stream.set_write_timeout(Some(state.config.write_timeout))?;

    let mut wire = match Wire::accept(stream, state.config.tls.as_ref()) {
        Ok(w) => w,
        // `curl http://…` against a TLS port. Answered in the language the client was actually
        // speaking, because rustls's own answer for this is `InvalidMessage` and nobody has ever
        // read that and known what to do.
        Err(WireError::Plaintext(mut sock)) => {
            let response = Response::failure(
                400,
                "plaintext_on_a_tls_port",
                "this port speaks TLS and this request arrived in the clear; use https://",
            );
            let _ = sock.write_all(&response.encode(false));
            state.metrics.handshake_failed();
            close_politely(sock);
            return Ok(());
        }
        // One connection failing to start is not the listener failing, and is not this worker
        // failing either. Logged rather than returned, because the caller discards the error and
        // an operator chasing a certificate problem needs to see which peer and why.
        Err(e) => {
            state.metrics.handshake_failed();
            log::emit(
                log::Level::Warn,
                "handshake_failed",
                &[
                    ("peer", log::F::S(peer.as_deref().unwrap_or("-"))),
                    ("why", log::F::S(&e.to_string())),
                ],
            );
            return Ok(());
        }
    };

    for n in 0..state.config.max_keepalive_requests {
        // Only from the second request onward: the first is covered by `read_timeout`, which
        // is the ceiling on a client that has connected and not yet said anything.
        let held = if n == 0 {
            None
        } else if state.stopping.load(Ordering::Relaxed) {
            break;
        } else {
            let Some(held) = KeptAlive::admit(state) else { break };
            // Through `socket()`, which is the *only* way to reach the descriptor. Reaching
            // through the session instead - a `get_ref()` on the TLS arm - would return the
            // rustls stream and set a timeout on nothing, and would compile.
            wire.socket().set_read_timeout(Some(state.config.keepalive_idle))?;
            Some(held)
        };

        let again = serve_one(state, &mut wire, peer.as_deref())?;
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
    wire: &mut Wire,
    peer: Option<&str>,
) -> std::io::Result<bool> {
    let started = Instant::now();
    let id = log::next_request_id();
    // Who the request turned out to be, filled in once the route has decided. Declared out here
    // so that the log line below can reach it whether or not a handler ever ran.
    let mut who: Option<(&'static str, String)> = None;
    // Set when the answer turns out to be a stream. Declared out here for the same reason
    // `who` is: the match below borrows the request, and this outlives it.
    let mut stream: Option<routes::Watch> = None;
    // When the request was actually in hand. **Not `started`**, which begins before
    // `Request::read` and therefore runs while a worker sits blocked waiting for a client to
    // send anything - on a pooled connection that idle wait can be seconds, and reporting it as
    // time this server spent working produced a "server" figure larger than the client's own
    // round trip. The log below keeps using `started`, because the whole slot is what an
    // operator wants to see; the header wants the work.
    let mut in_hand: Option<Instant> = None;

    let (response, method, path, bytes_in, keep) = match Request::read(wire) {
        // Nobody is left to answer. A kept-alive connection ends this way every time, so it is
        // not logged as anything: a line per client that finished politely is a line per
        // client.
        Err(request::RequestError::Closed) => return Ok(false),
        Ok(req) => {
            in_hand = Some(Instant::now());
            let bytes_in = req.body.len();
            let (method, path) = (req.method.clone(), req.path.clone());
            let keep = req.wants_keep_alive();
            // A panic in a handler used to kill the thread it ran on. Under a thread per
            // connection that cost one connection; under a pool it costs a worker
            // permanently, and a pool that quietly drains to nothing is a far worse failure
            // than the panic. Caught here rather than in the worker loop so that the client
            // still gets an answer and the log still gets a line.
            let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                answer(state, &req, &*wire)
            }));
            let answered = caught.unwrap_or_else(|_| {
                let mut r =
                    Response::failure(500, "panic", "the server could not complete this request");
                r.detail = Some(format!("a handler panicked serving {} {}", req.method, req.path));
                r.into()
            });
            who = answered.who;
            stream = answered.stream;
            (answered.response, method, path, bytes_in, keep)
        }
        Err(e) => (e.into_response(), "-".to_string(), "-".to_string(), 0, false),
    };

    // **A subscription is answered here rather than by a handler**, because it is the only
    // answer that needs the socket: it writes its head, then keeps writing until the reader
    // leaves. Everything below this point that talks about a body is skipped, and the
    // connection ends with the stream - a chunked body ends where the sender says it does, and
    // agreeing with the client about that afterwards is the negotiation this shape avoids.
    if let Some(watch) = stream {
        let head = watch.head.head();
        if wire.write_all(&head).and_then(|()| wire.flush()).is_err() {
            return Ok(false);
        }
        let sent = {
            let mut chunked = big_wire::Chunked::new(wire);
            routes::watch::run(state, &watch, &mut chunked);
            chunked.written()
        };
        let _ = wire.write_all(big_wire::LAST_CHUNK).and_then(|()| wire.flush());

        let elapsed = started.elapsed();
        state.metrics.request(200, elapsed, bytes_in, sent as usize);
        let mut fields = vec![
            ("id", log::F::S(&id)),
            ("method", log::F::S(&method)),
            ("path", log::F::S(&path)),
            ("status", log::F::N(200)),
            ("duration_us", log::F::N(elapsed.as_micros().min(u64::MAX as u128) as u64)),
            ("bytes_in", log::F::N(bytes_in as u64)),
            ("bytes_out", log::F::N(sent)),
        ];
        if let Some(peer) = peer {
            fields.push(("peer", log::F::S(peer)));
        }
        if let Some((kind, name)) = &who {
            fields.push((kind, log::F::S(name)));
        }
        log::emit(log::Level::Info, "subscription", &fields);
        return Ok(false);
    }

    // A request this server could not make sense of, or could not complete, takes the
    // connection with it. Reading the next request means trusting that this one ended where
    // the headers said it did, and a `400` is exactly the case where that is in doubt.
    //
    // The last two clauses are what TLS adds. A session that hit an I/O error cannot be
    // resynchronised, and one holding decrypted bytes nobody read is one where the two sides
    // have lost track of where a message ends - keeping either alive would deliver the next
    // response into the middle of somebody's parser.
    let keep = keep
        && response.status < 500
        && response.status != 400
        && response.status != 413
        && !wire.is_poisoned()
        && !wire.has_pending_plaintext();

    // **How long this server took, measured from the request being in hand to just before the
    // bytes go out.**
    //
    // A client timing a request from the outside cannot separate its own network from this
    // server's work, and over a tunnel or a long link the network is most of what it measures.
    // Sending this costs one header and makes the difference between "the query was slow" and
    // "getting to the query was slow" answerable from the other end.
    //
    // Both ends of the measurement matter: it starts at `in_hand` rather than `started` so that
    // a worker waiting for a client to speak is not counted, and it stops before the write
    // rather than after so that a slow client reading the answer is not either. `elapsed` below
    // keeps measuring the whole slot, for the log and the latency histogram, which is the
    // number an operator wants.
    let served_us = in_hand.unwrap_or(started).elapsed().as_micros().min(u64::MAX as u128) as u64;
    let response = response.with_header("X-Request-Id", &id).with_header("X-Served-Us", served_us);
    let encoded = response.encode(keep);

    // A cancelled request means the client is already gone, so writing is pointless and
    // failing to write is expected rather than an error worth reporting.
    //
    // Through the `Wire` rather than a second handle on the socket: under TLS there is no
    // second handle, because the bytes have to go through the session that encrypts them.
    let write = wire.write_all(&encoded).and_then(|()| wire.flush());

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
    // **The first time this server has ever logged who asked.** A line saying a table was
    // dropped, without saying by whom, is half a line - which was tolerable while a credential
    // was an anonymous string and is not once it belongs to a person.
    if let Some((kind, name)) = &who {
        fields.push((kind, log::F::S(name)));
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
///
/// **Nothing here may touch the wire between `Watchdog::spawn` and `watchdog.stop`.** The
/// watchdog duplicates the descriptor and makes it non-blocking, and `O_NONBLOCK` lives on the
/// open file description that both handles share - so for the length of that window the session
/// is non-blocking too. That was already true and already load-bearing; TLS makes it sharper,
/// because a `WouldBlock` reaching rustls part way through a record poisons the session rather
/// than merely confusing a write. This function is pure compute after the body has been read,
/// which is what keeps it true. A future change that streams a response has to move the
/// watchdog, not work around it.
fn answer<P: PagerMut + Sync>(state: &State<P>, req: &Request, wire: &Wire) -> routes::Answered {
    let mut ctx = routes::Ctx {
        cluster: &state.cluster,
        auth: &state.config.auth,
        identity: wire.identity(),
        metrics: &state.metrics,
        query_timeout: state.config.query_timeout,
        cancel: None,
        backup_dir: state.config.backup_dir.as_deref(),
        backup_running: &state.backing_up,
        watch_max: state.config.watch_max,
        watching: &state.watching,
        balance: state.config.balance,
    };

    // Only a query gets a watchdog. Everything else is bounded by the body the client already
    // sent, and a thread per request to watch work that finishes in microseconds would cost
    // more than the work.
    if !routes::may_run_long(req) {
        return routes::dispatch(&ctx, req);
    }

    let cancel = Arc::new(AtomicBool::new(false));
    ctx.cancel = Some(Arc::clone(&cancel));
    let watchdog = Watchdog::spawn(wire.socket(), cancel);
    let response = routes::dispatch(&ctx, req);
    watchdog.stop(wire.socket());
    response
}
