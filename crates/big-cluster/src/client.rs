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

//! Talking to a peer.
//!
//! The other half of the server one directory across, and deliberately no more general than
//! the requests this crate makes: one `POST`, a `Content-Length` body, a status and a body
//! back. No redirects, no chunked encoding, no cookies.
//!
//! **There is TLS here now, and this paragraph used to say there was not.** A peer used to
//! present the same `admin` bearer token every other node presented, so one leaked string was
//! the whole cluster and nothing could say which node was speaking - a gap `docs/clustering.md`
//! recorded and could not close, because a shared secret has no way to carry an identity. A
//! client certificate does. So the credential moved from the request to the connection: there is
//! no `Authorization` header on a peer request at all, and what proves this node is a node is
//! the key it holds. A reverse proxy could never have offered that.
//!
//! **Connections are reused, and both halves of that are here.** A request asks for
//! `Connection: keep-alive` and a connection that comes back alive goes into a small per-peer
//! pool. The peer may refuse - it holds a worker from a fixed pool while a connection waits,
//! so it closes rather than starve - and a refusal costs nothing but the connect that would
//! have happened anyway.
//!
//! Two things are pooled, and they are not the same thing. **Sockets**, so a fan-out does not
//! pay a handshake per leg. And **permission to have a request in flight at all**: a
//! coordinator whose own worker pool is saturated would otherwise open one connection per
//! worker per peer, and the peer would shed most of them - a round trip spent learning the
//! peer is busy.
//!
//! **A reused socket can be closed underneath us**, between the peer deciding it has waited
//! long enough and this node deciding to send. It is checked for before the write and retried
//! after - but only for a request that is safe to send twice. A query, a listing and an intern
//! are; an import, a delete and a schema change are not, and those fail rather than risk a
//! second application of the first attempt.
//!
//! **A deadline covers the whole exchange**, not each syscall: connect, write and read each
//! get what is left of it, so a peer that is slow three times cannot take three times as long
//! as the caller allowed.

use big_tls::ClientWire;
use std::io::{BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

/// Most bytes accepted in one answer.
///
/// This is not a size a legitimate answer is expected to approach - an owner enforces its own
/// memory ceilings before it encodes anything - it is the point past which a peer that has
/// stopped making sense stops being able to make the coordinator allocate.
pub const MAX_RESPONSE: usize = 256 << 20;

/// How long a peer may say **nothing at all** before the exchange is given up on.
///
/// Not a deadline. A deadline bounds the whole exchange and belongs to the request that set
/// one - a query with a budget. This bounds *silence*, and it exists because the alternative is
/// worse than it sounds: a peer that accepts a connection and then stops - a wedged process, a
/// dropped route, a machine that went away without closing anything - would otherwise hold a
/// coordinator's worker until somebody restarted it, and a write has no deadline of its own to
/// fall back on.
///
/// Generous, because a peer that is answering slowly is answering: a large batch that takes a
/// minute of commit is not silence, it is work.
pub const IDLE: Duration = Duration::from_secs(60);

/// Requests in flight to one peer at once.
///
/// Not a socket count for its own sake: past what the peer can serve, extra connections are
/// answered with a `503` from its accept loop, which is a round trip spent learning the peer
/// is busy. Waiting here instead spends the same time without the round trip.
pub const DEFAULT_MAX_IN_FLIGHT: usize = 32;

#[derive(Debug)]
pub enum ClientError {
    /// The address did not resolve, or nothing was listening.
    Unreachable(std::io::Error),
    /// The connection failed part way through.
    Io(std::io::Error),
    /// The peer answered and the handshake did not complete: a certificate this node's CA did
    /// not sign, an expired one, or a name that does not match the cluster file.
    ///
    /// Its own variant because this is an operator's mistake rather than a network's, and
    /// reporting it as [`ClientError::Unreachable`] sends somebody to look at a firewall.
    Tls(String),
    /// The budget ran out: waiting for a slot, connecting, writing or reading.
    Timeout,
    /// The bytes coming back were not a response this client understands.
    Malformed(&'static str),
    /// A response past [`MAX_RESPONSE`].
    TooLarge,
}

impl core::fmt::Display for ClientError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Unreachable(e) => write!(f, "could not connect: {e}"),
            Self::Io(e) => write!(f, "the connection failed: {e}"),
            Self::Tls(e) => write!(f, "the peer's certificate was not usable: {e}"),
            Self::Timeout => write!(f, "ran out of time"),
            Self::Malformed(what) => write!(f, "the answer was not usable: {what}"),
            Self::TooLarge => write!(f, "the answer was larger than {MAX_RESPONSE} bytes"),
        }
    }
}

impl core::error::Error for ClientError {}

/// Whether a request may be sent a second time after a connection failed with no answer.
///
/// Not a property of the transport. Sending a fact twice writes the same bit and is invisible;
/// sending a `delete` twice reports how many records the *second* one removed, which is a
/// number the caller would then be told and would be wrong. So the caller says, per request,
/// and the default is no.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Repeatable {
    /// Safe to send again: the second answer is the first answer.
    Yes,
    /// Not safe: failing is the correct outcome.
    No,
}

/// What a peer said.
#[derive(Debug)]
pub struct PeerResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

impl PeerResponse {
    pub fn is_ok(&self) -> bool {
        self.status == 200
    }

    /// The stable error code out of a JSON error body, if there is one.
    ///
    /// Read with a scan rather than a parser: the shape is `{"error":"...","code":"..."}`,
    /// written by [`big_http`]'s own encoder one crate up, and a JSON parser for one field of
    /// one shape would be a dependency carried for an error path.
    ///
    /// [`big_http`]: https://docs.rs/big-http
    pub fn code(&self) -> Option<String> {
        let body = core::str::from_utf8(&self.body).ok()?;
        let at = body.find("\"code\":\"")? + "\"code\":\"".len();
        let rest = &body[at..];
        let end = rest.find('"')?;
        Some(rest[..end].to_string())
    }

    /// The human half of the same body, for the message a coordinator passes on.
    pub fn message(&self) -> Option<String> {
        let body = core::str::from_utf8(&self.body).ok()?;
        let at = body.find("\"error\":\"")? + "\"error\":\"".len();
        let rest = &body[at..];
        let end = rest.find('"')?;
        Some(rest[..end].to_string())
    }
}

/// One peer, and the ceiling on how much of it this node uses at once.
pub struct Peer {
    addr: String,
    /// The name this peer's certificate has to be valid for.
    ///
    /// The *name* from the cluster file, not the address it is reached at. Those differ whenever
    /// a node moves, and the name is the half that was issued a certificate.
    name: String,
    /// What this node presents when a peer asks, and what it trusts in return. Shared by every
    /// peer: one configuration, and - the part that matters for a fan-out - one TLS session
    /// cache, so a reconnect resumes rather than handshaking from nothing.
    tls: Option<std::sync::Arc<big_tls::ClientTls>>,
    /// Sent on every request: what this build speaks, and which cluster file it read. A peer
    /// that disagrees about either refuses before it decodes anything.
    stamp: String,
    /// In-flight count and the condvar that wakes a waiter when one finishes.
    slots: (Mutex<usize>, Condvar),
    max_in_flight: usize,
    /// Connections this node has finished with and the peer has not closed.
    ///
    /// A stack rather than a queue: the most recently used connection is the one most likely
    /// to still be open, because it is the one whose idle window started last.
    idle: Mutex<Vec<Conn>>,
}

/// One open connection, and the buffer its answers are read through.
///
/// The reader lives with the socket rather than being built per request. A buffered reader
/// created for one response and dropped would take the front of the next one with it, which is
/// invisible until the connection is reused and then loses a response for no reason anyone
/// could find.
struct Conn {
    io: BufReader<ClientWire>,
}

impl Conn {
    /// The raw descriptor, for timeouts and for the liveness peek. **Never for reading or
    /// writing**: under TLS the bytes have to go through the session that encrypts them, and a
    /// write here would put plaintext on an encrypted socket.
    fn stream(&self) -> &TcpStream {
        self.io.get_ref().socket()
    }

    /// Whether this connection still looks usable, without sending anything.
    ///
    /// A peek of zero bytes is end of stream: the peer closed while this sat in the pool, which
    /// is the normal end of an idle keep-alive connection. Bytes actually waiting mean the two
    /// sides have lost track of where a message ends, which is worse than a closed connection
    /// and gets the same treatment.
    /// **Three buffers, not two.** The `BufReader`'s own buffer, then the plaintext rustls is
    /// holding that the `BufReader` knows nothing about, and only then the socket. Asking the
    /// first and the third and skipping the second is the bug that survives review and comes
    /// back as a response delivered into the middle of the next one's parser.
    fn is_live(&mut self) -> bool {
        if !self.io.buffer().is_empty() {
            return false;
        }
        if self.io.get_mut().has_pending_plaintext() {
            return false;
        }
        let stream = self.stream();
        if stream.set_nonblocking(true).is_err() {
            return false;
        }
        let mut byte = [0u8; 1];
        let live = matches!(
            stream.peek(&mut byte),
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock
        );
        stream.set_nonblocking(false).is_ok() && live
    }
}

/// How this node reaches the others.
///
/// A trait rather than the peer table itself, and the reason is testability rather than taste.
/// Everything this crate decides - which copy serves a range, what a coordinator does when one
/// refuses, whether a repair moved a fragment - was reachable only by starting real servers on
/// real ports and arranging for one of them to misbehave. That made the interesting cases the
/// expensive ones, which is backwards: a peer that answers slowly, or refuses, or returns bytes
/// from a different build, is exactly what this logic exists to handle.
///
/// The production implementation is [`HttpPeers`] and there is no second one in the binary. A
/// test supplies its own and never opens a socket.
pub trait Peers: Send + Sync {
    /// Sends one body to one node and waits for its answer.
    ///
    /// `node` indexes the config's node list. This node's own index is never passed - a
    /// coordinator runs its own share in-process - and an implementation may treat it as a bug.
    fn post(
        &self,
        node: usize,
        path: &str,
        body: &[u8],
        budget: Option<Duration>,
        repeatable: Repeatable,
    ) -> Result<PeerResponse, ClientError>;

    /// How many nodes the table has, this one included.
    fn len(&self) -> usize;

    /// Whether the table has no nodes at all, which a cluster of one does not reach.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Makes sure this table can reach every node in `addrs`.
    ///
    /// **What a node joining at runtime needs.** The default does nothing, which is right for
    /// a table that was handed a fixed list and for the fakes a test drives - neither can grow
    /// and neither is ever asked to.
    fn extend(&self, addrs: &[(String, String)]) {
        let _ = addrs;
    }

    /// Where a node is, for a report that has to name it. `None` for this node's own index,
    /// and for a slot no peer has been made for yet.
    ///
    /// Owned rather than borrowed, because the table can grow while the cluster runs and a
    /// borrow would hold its lock for as long as the caller kept the string.
    fn addr(&self, node: usize) -> Option<String>;
}

/// The peer table a running node uses: one pooled client per *other* node.
///
/// `None` at this node's own index, because this node is reached by calling it.
pub struct HttpPeers {
    /// One slot per node, `None` at this node's own index and at any slot not filled yet.
    ///
    /// **Behind a lock because a node can join while the cluster runs.** The table used to be
    /// built once from the cluster file and indexed directly, so a `NodeId` the file did not
    /// contain was a panic on the request path. It grows now, and the lock is taken for the
    /// length of a clone of one `Arc` - never across the request itself.
    slots: std::sync::RwLock<Vec<Option<std::sync::Arc<Peer>>>>,
    this: usize,
    tls: Option<std::sync::Arc<big_tls::ClientTls>>,
    fingerprint: u64,
}

impl HttpPeers {
    /// One client per node except this one.
    ///
    /// Names as well as addresses, because a certificate is issued to a name: the address is
    /// where a node is reached and the name is what it is, and a node that moves keeps the
    /// second. The `Arc` is shared rather than cloned per peer so that all of them use one TLS
    /// session cache.
    pub fn new(
        names: impl IntoIterator<Item = String>,
        addrs: impl IntoIterator<Item = String>,
        this: usize,
        tls: Option<std::sync::Arc<big_tls::ClientTls>>,
        fingerprint: u64,
    ) -> Self {
        let slots = names
            .into_iter()
            .zip(addrs)
            .enumerate()
            .map(|(i, (name, addr))| {
                (i != this)
                    .then(|| std::sync::Arc::new(Peer::new(name, addr, tls.clone(), fingerprint)))
            })
            .collect();
        Self { slots: std::sync::RwLock::new(slots), this, tls, fingerprint }
    }

    /// Makes sure there is a client for every node in `addrs`, adding any that are new.
    ///
    /// **Called when the agreement says the cluster has changed.** A node that joined has an
    /// index nothing in this table has ever seen, and the first message sent to it is a
    /// heartbeat - so the table has to be able to grow without a restart, which is the whole
    /// difference between membership as a file and membership as a decision.
    ///
    /// Existing peers are left alone. Replacing one would drop a connection pool that is
    /// working, and an address that changed is a different question - a node keeps its name.
    pub fn extend_to(&self, addrs: &[(String, String)]) {
        let mut slots = self.slots.write().expect("no panic holds this lock");
        if slots.len() < addrs.len() {
            slots.resize_with(addrs.len(), || None);
        }
        for (i, (name, addr)) in addrs.iter().enumerate() {
            if i == self.this || slots[i].is_some() {
                continue;
            }
            slots[i] = Some(std::sync::Arc::new(Peer::new(
                name.clone(),
                addr.clone(),
                self.tls.clone(),
                self.fingerprint,
            )));
        }
    }

    /// The client for one node, or `None` for this node and for a slot nothing has filled.
    fn peer(&self, node: usize) -> Option<std::sync::Arc<Peer>> {
        self.slots.read().expect("no panic holds this lock").get(node)?.clone()
    }
}

impl Peers for HttpPeers {
    fn post(
        &self,
        node: usize,
        path: &str,
        body: &[u8],
        budget: Option<Duration>,
        repeatable: Repeatable,
    ) -> Result<PeerResponse, ClientError> {
        // **An index this table has never seen is a refusal, not a panic.** A node can join
        // while this one is serving, and a coordinator that learned about it a heartbeat before
        // this table did would otherwise take the whole process down.
        let Some(peer) = self.peer(node) else {
            return Err(ClientError::Unreachable(std::io::Error::other(format!(
                "node {node} is not in this node's peer table yet"
            ))));
        };
        peer.post(path, body, budget, repeatable)
    }

    fn extend(&self, addrs: &[(String, String)]) {
        self.extend_to(addrs);
    }

    fn len(&self) -> usize {
        self.slots.read().expect("no panic holds this lock").len()
    }

    fn addr(&self, node: usize) -> Option<String> {
        Some(self.peer(node)?.addr().to_string())
    }
}

impl Peer {
    pub fn new(
        name: impl Into<String>,
        addr: impl Into<String>,
        tls: Option<std::sync::Arc<big_tls::ClientTls>>,
        fingerprint: u64,
    ) -> Self {
        Self::with_ceiling(name, addr, tls, fingerprint, DEFAULT_MAX_IN_FLIGHT)
    }

    pub fn with_ceiling(
        name: impl Into<String>,
        addr: impl Into<String>,
        tls: Option<std::sync::Arc<big_tls::ClientTls>>,
        fingerprint: u64,
        max_in_flight: usize,
    ) -> Self {
        Self {
            addr: addr.into(),
            name: name.into(),
            tls,
            stamp: format!(
                "{}: {}\r\n{}: {fingerprint:x}\r\n",
                crate::WIRE_HEADER,
                crate::WIRE_VERSION,
                crate::CLUSTER_HEADER
            ),
            slots: (Mutex::new(0), Condvar::new()),
            max_in_flight: max_in_flight.max(1),
            idle: Mutex::new(Vec::new()),
        }
    }

    pub fn addr(&self) -> &str {
        &self.addr
    }

    /// One request, entirely within `budget`.
    ///
    /// `None` is no budget at all, which is what a query without a deadline gets. It is not a
    /// hang: the request still fails when the socket does.
    pub fn post(
        &self,
        path: &str,
        body: &[u8],
        budget: Option<Duration>,
        repeatable: Repeatable,
    ) -> Result<PeerResponse, ClientError> {
        let deadline = budget.map(|b| Instant::now() + b);
        let _slot = self.slot(deadline)?;
        let request = self.encode(path, body);

        // A connection out of the pool may have been closed by the peer since it went in. The
        // check before sending catches almost all of those; the retry catches the rest, and
        // only for a request whose second attempt cannot be told from its first.
        if let Some(conn) = self.take_idle() {
            match self.exchange(conn, &request, deadline) {
                Ok(response) => return Ok(response),
                Err(e) if repeatable == Repeatable::No => return Err(e),
                // Anything else and the connection is not the suspect: a timeout is a timeout
                // whether the socket was new or not, and retrying would spend a budget that
                // has already run out.
                Err(ClientError::Timeout) => return Err(ClientError::Timeout),
                Err(_) => {}
            }
        }

        let conn = Conn { io: BufReader::new(self.connect(deadline)?) };
        self.exchange(conn, &request, deadline)
    }

    /// Sends on one connection and reads one answer, returning the connection to the pool if
    /// both sides are still willing.
    fn exchange(
        &self,
        mut conn: Conn,
        request: &[u8],
        deadline: Option<Instant>,
    ) -> Result<PeerResponse, ClientError> {
        set_timeouts(conn.stream(), deadline)?;
        // Written in one call so a peer's read never sees a header block arrive in pieces,
        // which is the shape that makes a slow-loris check fire against its own coordinator.
        //
        // Through the session rather than through a second handle on the socket. There is no
        // second handle under TLS, and there was never a reason for one here beyond its being
        // available.
        let out = conn.io.get_mut();
        out.write_all(request).map_err(ClientError::Io)?;
        out.flush().map_err(ClientError::Io)?;

        set_timeouts(conn.stream(), deadline)?;
        let (response, reusable) = read_response(&mut conn.io)?;
        if reusable {
            self.put_idle(conn);
        }
        Ok(response)
    }

    /// The request bytes, which do not depend on which connection carries them.
    fn encode(&self, path: &str, body: &[u8]) -> Vec<u8> {
        let mut request = Vec::with_capacity(body.len() + 256);
        request.extend_from_slice(
            format!(
                "POST {path} HTTP/1.1\r\n\
                 Host: {}\r\n\
                 Content-Type: application/octet-stream\r\n\
                 Content-Length: {}\r\n\
                 Connection: keep-alive\r\n",
                self.addr,
                body.len()
            )
            .as_bytes(),
        );
        request.extend_from_slice(self.stamp.as_bytes());
        // **No `Authorization` line, and that is the change.** What proves this node is a node is
        // the client certificate it presented during the handshake - the credential is the
        // connection now, not the request. A header could be replayed by anything that read one;
        // a private key cannot be.
        request.extend_from_slice(b"\r\n");
        request.extend_from_slice(body);
        request
    }

    /// The most recently returned connection that still looks open.
    fn take_idle(&self) -> Option<Conn> {
        let mut idle = self.idle.lock().expect("no panic holds this lock");
        while let Some(mut conn) = idle.pop() {
            if conn.is_live() {
                return Some(conn);
            }
        }
        None
    }

    /// Keeps a connection for the next request, up to the number that could be in flight at
    /// once. Past that they are connections nobody is going to reach for.
    fn put_idle(&self, conn: Conn) {
        let mut idle = self.idle.lock().expect("no panic holds this lock");
        if idle.len() < self.max_in_flight {
            idle.push(conn);
        }
    }

    /// Blocks until this node may have another request in flight to this peer.
    ///
    /// The guard decrements on drop, so a request that fails half way through still gives its
    /// slot back - which is the case that would otherwise wedge a peer permanently.
    fn slot(&self, deadline: Option<Instant>) -> Result<Slot<'_>, ClientError> {
        let (lock, cv) = &self.slots;
        let mut n = lock.lock().expect("no panic holds this lock");
        while *n >= self.max_in_flight {
            match deadline {
                Some(d) => {
                    let left = d.checked_duration_since(Instant::now()).ok_or(ClientError::Timeout);
                    let (guard, timed_out) =
                        cv.wait_timeout(n, left?).expect("no panic holds this lock");
                    n = guard;
                    if timed_out.timed_out() && *n >= self.max_in_flight {
                        return Err(ClientError::Timeout);
                    }
                }
                None => n = cv.wait(n).expect("no panic holds this lock"),
            }
        }
        *n += 1;
        Ok(Slot { peer: self })
    }

    fn connect(&self, deadline: Option<Instant>) -> Result<ClientWire, ClientError> {
        let mut last = None;
        // Resolution is not covered by the deadline: there is no resolver call in the standard
        // library that takes one, and pretending otherwise by checking the clock afterwards
        // would report a timeout for work that had already finished.
        let addrs = self.addr.to_socket_addrs().map_err(ClientError::Unreachable)?;
        for addr in addrs {
            // Connecting gets the same treatment: `TcpStream::connect` with no timeout waits
            // as long as the operating system's, which on an unreachable host is minutes.
            let left = match deadline {
                Some(d) => d.checked_duration_since(Instant::now()).ok_or(ClientError::Timeout)?,
                None => IDLE,
            };
            let result = TcpStream::connect_timeout(&addr, left);
            match result {
                Ok(s) => {
                    // A fan-out sends one small message and waits. Nagle would hold it back
                    // for an ack that is not coming until the peer has answered.
                    let _ = s.set_nodelay(true);
                    // The handshake gets whatever is left of the budget, which the timeouts
                    // set here are what bound: a peer that accepts and then says nothing during
                    // a handshake is the same hang as one that says nothing during a response.
                    set_timeouts(&s, deadline)?;
                    return ClientWire::connect(s, self.tls.as_deref(), &self.name)
                        .map_err(|e| ClientError::Tls(e.to_string()));
                }
                Err(e) => last = Some(e),
            }
        }
        Err(ClientError::Unreachable(last.unwrap_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "the address resolved to nothing")
        })))
    }
}

/// Holds one in-flight slot, and gives it back however the request ends.
struct Slot<'a> {
    peer: &'a Peer,
}

impl Drop for Slot<'_> {
    fn drop(&mut self) {
        let (lock, cv) = &self.peer.slots;
        let mut n = lock.lock().expect("no panic holds this lock");
        *n -= 1;
        cv.notify_one();
    }
}

/// What is left of the budget, on both directions of the socket.
///
/// Re-applied before the read as well as before the write, so the two halves share one budget
/// instead of each getting a whole one.
///
/// **A request with no budget still gets [`IDLE`].** No budget means no deadline, not no
/// timeout: a socket with neither is a worker a silent peer can keep for as long as it likes,
/// and the requests that carry no budget are the writes, which is exactly where that hurts.
fn set_timeouts(stream: &TcpStream, deadline: Option<Instant>) -> Result<(), ClientError> {
    let left = match deadline {
        None => IDLE,
        Some(d) => d.checked_duration_since(Instant::now()).ok_or(ClientError::Timeout)?,
    };
    stream.set_read_timeout(Some(left)).map_err(ClientError::Io)?;
    stream.set_write_timeout(Some(left)).map_err(ClientError::Io)?;
    Ok(())
}

/// Status line, headers, then exactly `Content-Length` bytes.
///
/// Also answers whether the connection may carry another request: only when the peer said
/// `Connection: keep-alive` *and* the body had a declared length, because a body that ends
/// when the stream does has taken the connection with it.
fn read_response(io: &mut BufReader<ClientWire>) -> Result<(PeerResponse, bool), ClientError> {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    // One byte at a time, through the buffer that lives with the connection: the body is read
    // from the same reader, so nothing can be swallowed between the two.
    while !head.ends_with(b"\r\n\r\n") {
        match io.read(&mut byte) {
            Ok(0) => {
                return Err(ClientError::Malformed("the peer closed before its headers ended"))
            }
            Ok(_) => head.push(byte[0]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) if timed_out(&e) => return Err(ClientError::Timeout),
            Err(e) => return Err(ClientError::Io(e)),
        }
        if head.len() > 16 << 10 {
            return Err(ClientError::Malformed("the header block never ended"));
        }
    }

    let head =
        String::from_utf8(head).map_err(|_| ClientError::Malformed("headers are not text"))?;
    let mut lines = head.lines();
    let status: u16 = lines
        .next()
        .and_then(|l| l.split(' ').nth(1))
        .and_then(|s| s.parse().ok())
        .ok_or(ClientError::Malformed("no status line"))?;

    let mut length = None;
    let mut keep_alive = false;
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                length = value
                    .trim()
                    .parse::<usize>()
                    .map(Some)
                    .map_err(|_| ClientError::Malformed("bad content-length"))?;
            }
            if name.eq_ignore_ascii_case("connection") {
                keep_alive = value.trim().eq_ignore_ascii_case("keep-alive");
            }
        }
    }

    let body = match length {
        Some(n) if n > MAX_RESPONSE => return Err(ClientError::TooLarge),
        Some(n) => {
            let mut body = vec![0u8; n];
            read_exact(io, &mut body)?;
            body
        }
        None => {
            keep_alive = false;
            let mut body = Vec::new();
            // `take` rather than an unbounded read, so a peer that never stops sending is
            // refused rather than being allowed to fill this process.
            io.take(MAX_RESPONSE as u64 + 1).read_to_end(&mut body).map_err(|e| {
                if timed_out(&e) {
                    ClientError::Timeout
                } else {
                    ClientError::Io(e)
                }
            })?;
            if body.len() > MAX_RESPONSE {
                return Err(ClientError::TooLarge);
            }
            body
        }
    };

    Ok((PeerResponse { status, body }, keep_alive))
}

fn read_exact(io: &mut BufReader<ClientWire>, buf: &mut [u8]) -> Result<(), ClientError> {
    let mut at = 0;
    while at < buf.len() {
        match io.read(&mut buf[at..]) {
            Ok(0) => return Err(ClientError::Malformed("the peer closed mid-body")),
            Ok(n) => at += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) if timed_out(&e) => return Err(ClientError::Timeout),
            Err(e) => return Err(ClientError::Io(e)),
        }
    }
    Ok(())
}

/// A socket timeout is `WouldBlock` on some platforms and `TimedOut` on others, and the caller
/// needs one answer.
fn timed_out(e: &std::io::Error) -> bool {
    matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut)
}
