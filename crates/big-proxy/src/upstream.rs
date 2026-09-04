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

//! One node, and the pooled connections to it.
//!
//! This is `big-cluster`'s `Peer` in shape and not in substance, because `Peer` cannot carry a
//! proxied request: it hardcodes `POST`, has nowhere to put an `Authorization` header, and
//! stamps every request with the cluster's wire version and fingerprint. A proxy is not a peer,
//! and a request that claimed otherwise would be this process asserting something it cannot
//! check. What is borrowed is everything that took work to get right — the in-flight ceiling
//! with a guard that comes back down however the request ends, the "is this pooled connection
//! still alive" check with one retry for the race it cannot close, and re-applying the deadline
//! before every blocking call so three slow steps cannot cost three budgets.
//!
//! **Every ceiling here sits one notch inside the daemon's**, which is the same discipline
//! `clients/go/config.go` follows: retire a connection at 900 requests where the server closes
//! at 1000, go idle at 3s where the server closes at 5s. A client that retires first never has a
//! request in flight on a connection the server has already decided to close.

use big_tls::{ClientTls, ClientWire};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// Requests on one pooled connection before it is retired.
///
/// The daemon closes at 1000. Retiring first is what makes a rolling restart behind this proxy
/// actually roll, rather than each side waiting for the other to hang up.
pub const MAX_REQUESTS_PER_CONN: u32 = 900;

/// How long a pooled connection may sit unused. The daemon's keep-alive idle is 5s.
pub const MAX_IDLE: Duration = Duration::from_secs(3);

/// Requests in flight to one node at once, from `big_cluster::client::DEFAULT_MAX_IN_FLIGHT`.
pub const MAX_IN_FLIGHT: usize = 32;

/// The largest response body this proxy will hold for one request.
///
/// A listing or a query answer, not a fan-out payload: the peer surface is not proxied, so the
/// 256 MiB `big-cluster` allows for encoded containers is not a size that can arrive here.
pub const MAX_RESPONSE: usize = 64 << 20;

/// Why a request to a node did not produce an answer.
///
/// The split between [`NotSent`](UpstreamError::NotSent) and [`Sent`](UpstreamError::Sent) is
/// the whole retry policy. Nothing reached the socket in the first case, so a second attempt on
/// another node *is* the first attempt and is safe even for an import. In the second the bytes
/// are gone and the outcome is unknown, which is a thing to report rather than to guess at.
#[derive(Debug)]
pub enum UpstreamError {
    /// Connect failed, or the write had not begun. Safe to try elsewhere, for any route.
    NotSent(std::io::Error),
    /// Bytes left this process. Only a `Repeatable::Yes` route may be sent again.
    Sent(std::io::Error),
    /// The budget ran out. Never retried: a budget that has run out cannot fund a second try.
    Timeout,
    /// The node answered something this proxy will not relay.
    Unreadable(String),
}

impl UpstreamError {
    /// Whether the request is known not to have reached the node.
    pub fn not_sent(&self) -> bool {
        matches!(self, UpstreamError::NotSent(_))
    }
}

impl std::fmt::Display for UpstreamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UpstreamError::NotSent(e) => write!(f, "not sent: {e}"),
            UpstreamError::Sent(e) => write!(f, "sent, no answer: {e}"),
            UpstreamError::Timeout => write!(f, "timed out"),
            UpstreamError::Unreadable(w) => write!(f, "unreadable: {w}"),
        }
    }
}

/// The three content types the daemon produces, and nothing else.
///
/// `Response.content_type` is a `&'static str`, so an upstream value cannot be carried without
/// leaking it. Mapping is not a workaround for that: refusing the unknown case is what makes
/// "this proxy relays only what this cluster produces" a fact rather than a hope.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ContentType {
    Json,
    Prometheus,
    Binary,
}

impl ContentType {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "application/json" => Some(ContentType::Json),
            "text/plain; version=0.0.4; charset=utf-8" => Some(ContentType::Prometheus),
            "application/octet-stream" => Some(ContentType::Binary),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ContentType::Json => "application/json",
            ContentType::Prometheus => "text/plain; version=0.0.4; charset=utf-8",
            ContentType::Binary => "application/octet-stream",
        }
    }
}

/// What a node answered.
pub struct UpstreamResponse {
    pub status: u16,
    pub content_type: ContentType,
    pub body: Vec<u8>,
    /// Only the headers [`crate::headers::keeps_from_upstream`] names.
    pub headers: Vec<(String, String)>,
}

/// Written out rather than derived: a body is up to [`MAX_RESPONSE`] of somebody's query result,
/// and a derived `Debug` would put all of it into any panic message that touched a response.
impl std::fmt::Debug for UpstreamResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UpstreamResponse")
            .field("status", &self.status)
            .field("content_type", &self.content_type)
            .field("body_len", &self.body.len())
            .field("headers", &self.headers)
            .finish()
    }
}

struct Conn {
    io: BufReader<ClientWire>,
    served: u32,
    idle_since: Instant,
}

impl Conn {
    /// Whether this connection is worth trying before the request is written to it.
    ///
    /// Catches almost every connection the node has closed since it went into the pool. The
    /// remainder is a race no check can close, and [`Upstream::send`] handles that with one
    /// retry rather than by looking harder.
    fn live(&self) -> bool {
        if self.served >= MAX_REQUESTS_PER_CONN || self.idle_since.elapsed() >= MAX_IDLE {
            return false;
        }
        let sock = self.io.get_ref().socket();
        let Ok(previous) = sock.read_timeout() else { return false };
        if sock.set_read_timeout(Some(Duration::from_millis(1))).is_err() {
            return false;
        }
        let mut probe = [0u8; 1];
        let alive = match sock.peek(&mut probe) {
            // Readable at a moment nothing was asked for means the peer said something, and the
            // only thing it can be saying is goodbye.
            Ok(0) => false,
            Ok(_) => false,
            Err(e) => {
                matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut)
            }
        };
        sock.set_read_timeout(previous).is_ok() && alive
    }
}

/// A guard that gives the in-flight slot back however the request ends, panic included.
struct Slot<'a>(&'a Upstream);

impl Drop for Slot<'_> {
    fn drop(&mut self) {
        let (lock, cvar) = &self.0.slots;
        let mut n = lock.lock().unwrap_or_else(|e| e.into_inner());
        *n -= 1;
        cvar.notify_one();
    }
}

/// One node this proxy forwards to.
pub struct Upstream {
    name: String,
    addr: String,
    /// `None` is a plaintext hop. When it is `Some`, the handshake uses [`Upstream::name`] as
    /// the server name — not the address — because `certs.sh` issues
    /// `subjectAltName = DNS:<node>` and a node reached at a new address keeps its name.
    ///
    /// **The identity inside it is always `None`.** This proxy presents no client certificate,
    /// which is what leaves it as `Identity::None` at a node and so refused by every
    /// `Guard::Node` route. There is no constructor here that takes one.
    tls: Option<Arc<ClientTls>>,
    max_in_flight: usize,
    slots: (Mutex<usize>, Condvar),
    idle: Mutex<Vec<Conn>>,
}

impl Upstream {
    pub fn new(name: impl Into<String>, addr: impl Into<String>) -> Self {
        Self::with_ceiling(name, addr, None, MAX_IN_FLIGHT)
    }

    /// The same, over TLS. `tls` carries the CA the node's certificate must chain to.
    pub fn secured(name: impl Into<String>, addr: impl Into<String>, tls: Arc<ClientTls>) -> Self {
        Self::with_ceiling(name, addr, Some(tls), MAX_IN_FLIGHT)
    }

    pub fn with_ceiling(
        name: impl Into<String>,
        addr: impl Into<String>,
        tls: Option<Arc<ClientTls>>,
        max_in_flight: usize,
    ) -> Self {
        Self {
            name: name.into(),
            addr: addr.into(),
            tls,
            max_in_flight: max_in_flight.max(1),
            slots: (Mutex::new(0), Condvar::new()),
            idle: Mutex::new(Vec::new()),
        }
    }

    pub fn is_tls(&self) -> bool {
        self.tls.is_some()
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn addr(&self) -> &str {
        &self.addr
    }

    /// How many requests are in flight to this node right now.
    ///
    /// The number [`crate::pool`] selects on, and the reason least-in-flight costs nothing: the
    /// ceiling already needed this count.
    pub fn in_flight(&self) -> usize {
        *self.slots.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// One request, entirely within `budget`.
    ///
    /// `header_block` is already `\r\n`-terminated and already filtered — see
    /// [`crate::headers::upstream_block`]. This function adds the request line and the blank
    /// line and nothing else, because deciding what crosses the hop is not the socket's job.
    pub fn send(
        &self,
        method: &str,
        target: &str,
        header_block: &str,
        body: &[u8],
        budget: Duration,
    ) -> Result<UpstreamResponse, UpstreamError> {
        let deadline = Instant::now() + budget;
        let _slot = self.slot(deadline)?;

        let mut request = Vec::with_capacity(header_block.len() + body.len() + 64);
        request.extend_from_slice(format!("{method} {target} HTTP/1.1\r\n").as_bytes());
        request.extend_from_slice(header_block.as_bytes());
        request.extend_from_slice(b"\r\n");
        request.extend_from_slice(body);

        // A pooled connection may have been closed since it went in. `live()` catches almost all
        // of those before anything is written; this retry catches the rest, and only because a
        // second attempt down a *fresh* socket cannot be told from the first — the node never
        // heard the one that failed.
        if let Some(conn) = self.take_idle() {
            match self.exchange(conn, &request, deadline) {
                Ok(response) => return Ok(response),
                // A timeout is a timeout whether the socket was new or not, and retrying would
                // spend a budget that has already run out.
                Err(UpstreamError::Timeout) => return Err(UpstreamError::Timeout),
                Err(UpstreamError::Unreadable(w)) => return Err(UpstreamError::Unreadable(w)),
                Err(_) => {}
            }
        }

        let wire = self.connect(deadline)?;
        self.exchange(
            Conn { io: BufReader::new(wire), served: 0, idle_since: Instant::now() },
            &request,
            deadline,
        )
    }

    /// Wait for one of this node's in-flight slots.
    fn slot(&self, deadline: Instant) -> Result<Slot<'_>, UpstreamError> {
        let (lock, cvar) = &self.slots;
        let mut n = lock.lock().unwrap_or_else(|e| e.into_inner());
        while *n >= self.max_in_flight {
            let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                return Err(UpstreamError::Timeout);
            };
            let (guard, wait) = cvar.wait_timeout(n, left).unwrap_or_else(|e| e.into_inner());
            n = guard;
            if wait.timed_out() && *n >= self.max_in_flight {
                return Err(UpstreamError::Timeout);
            }
        }
        *n += 1;
        Ok(Slot(self))
    }

    fn connect(&self, deadline: Instant) -> Result<ClientWire, UpstreamError> {
        let Some(left) = deadline.checked_duration_since(Instant::now()) else {
            return Err(UpstreamError::Timeout);
        };
        let mut last = std::io::Error::other(format!("{} resolved to no address", self.addr));
        let addrs =
            std::net::ToSocketAddrs::to_socket_addrs(&self.addr).map_err(UpstreamError::NotSent)?;
        for addr in addrs {
            match TcpStream::connect_timeout(&addr, left) {
                Ok(stream) => {
                    // Nagle would hold a small request waiting for more of it, and there is no
                    // more of it: the whole thing is written in one call.
                    let _ = stream.set_nodelay(true);
                    // The handshake has to finish inside the budget too, so the timeouts go on
                    // before it starts rather than after.
                    let _ = stream.set_read_timeout(Some(left));
                    let _ = stream.set_write_timeout(Some(left));
                    // **The server name is the node's name, not its address.** A certificate
                    // carries `subjectAltName = DNS:<node>`; handshaking against the address
                    // produces a failure that reads exactly like a network problem.
                    return ClientWire::connect(stream, self.tls.as_deref(), &self.name).map_err(
                        |e| {
                            // A failed handshake is a failed *connect*: nothing of the request
                            // reached the node, so another node may still be tried for it.
                            UpstreamError::NotSent(std::io::Error::other(e.to_string()))
                        },
                    );
                }
                Err(e) => last = e,
            }
        }
        Err(UpstreamError::NotSent(last))
    }

    fn take_idle(&self) -> Option<Conn> {
        let mut idle = self.idle.lock().unwrap_or_else(|e| e.into_inner());
        while let Some(conn) = idle.pop() {
            if conn.live() {
                return Some(conn);
            }
        }
        None
    }

    fn give_back(&self, conn: Conn) {
        if conn.served >= MAX_REQUESTS_PER_CONN {
            return;
        }
        let mut idle = self.idle.lock().unwrap_or_else(|e| e.into_inner());
        // A pool deeper than the in-flight ceiling holds connections nothing can be waiting for.
        if idle.len() < self.max_in_flight {
            idle.push(conn);
        }
    }

    fn exchange(
        &self,
        mut conn: Conn,
        request: &[u8],
        deadline: Instant,
    ) -> Result<UpstreamResponse, UpstreamError> {
        set_deadline(conn.io.get_ref().socket(), deadline)?;
        // Everything before this point is recoverable on another node; everything after is not.
        if let Err(e) = conn.io.get_mut().write_all(request) {
            return Err(timeout_or(e, UpstreamError::Sent));
        }
        if let Err(e) = conn.io.get_mut().flush() {
            return Err(timeout_or(e, UpstreamError::Sent));
        }

        set_deadline(conn.io.get_ref().socket(), deadline)?;
        let response = read_response(&mut conn.io)?;
        conn.served += 1;
        conn.idle_since = Instant::now();
        self.give_back(conn);
        Ok(response)
    }
}

/// Re-apply the remaining budget to the socket before each blocking step.
///
/// One deadline covers connect, write and read; setting the *remaining* time each time is what
/// stops three slow steps from costing three budgets.
fn set_deadline(sock: &TcpStream, deadline: Instant) -> Result<(), UpstreamError> {
    let Some(left) = deadline.checked_duration_since(Instant::now()) else {
        return Err(UpstreamError::Timeout);
    };
    // A zero `Duration` means "no timeout" to the socket API, which is the opposite of what a
    // budget with nothing left should mean.
    let left = left.max(Duration::from_millis(1));
    sock.set_read_timeout(Some(left)).map_err(UpstreamError::NotSent)?;
    sock.set_write_timeout(Some(left)).map_err(UpstreamError::NotSent)?;
    Ok(())
}

fn timeout_or(e: std::io::Error, otherwise: fn(std::io::Error) -> UpstreamError) -> UpstreamError {
    match e.kind() {
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => UpstreamError::Timeout,
        _ => otherwise(e),
    }
}

/// Read one HTTP/1.1 response: status line, headers, then exactly `Content-Length` bytes.
///
/// **`Transfer-Encoding` is refused rather than decoded.** The daemon always sends
/// `Content-Length` — `Response::encode` has no other mode — so a chunked answer means something
/// that is not this database is on the other end. `bigctl` made the same call for the same
/// reason: implementing a decoder nothing produces means shipping a path no test can reach.
fn read_response(io: &mut BufReader<ClientWire>) -> Result<UpstreamResponse, UpstreamError> {
    let mut line = String::new();
    read_line(io, &mut line)?;
    let status: u16 = line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| UpstreamError::Unreadable(format!("no status in {:?}", line.trim())))?;

    let mut length: Option<usize> = None;
    let mut content_type = None;
    let mut headers = Vec::new();
    loop {
        line.clear();
        read_line(io, &mut line)?;
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(UpstreamError::Unreadable(format!("no colon in header {line:?}")));
        };
        let (name, value) = (name.trim().to_ascii_lowercase(), value.trim());
        match name.as_str() {
            "content-length" => {
                length =
                    Some(value.parse().map_err(|_| {
                        UpstreamError::Unreadable(format!("content-length {value:?}"))
                    })?)
            }
            "content-type" => content_type = ContentType::parse(value),
            "transfer-encoding" => {
                return Err(UpstreamError::Unreadable(
                    "the node answered with Transfer-Encoding, which this database never sends"
                        .to_string(),
                ))
            }
            _ if crate::headers::keeps_from_upstream(&name) => {
                headers.push((name, value.to_string()))
            }
            _ => {}
        }
    }

    let length = length
        .ok_or_else(|| UpstreamError::Unreadable("the node sent no Content-Length".to_string()))?;
    if length > MAX_RESPONSE {
        return Err(UpstreamError::Unreadable(format!("{length} byte answer over the ceiling")));
    }
    let mut body = vec![0u8; length];
    io.read_exact(&mut body).map_err(|e| timeout_or(e, UpstreamError::Sent))?;

    // A body with no content type is a body this proxy has no honest way to label.
    let content_type = content_type.ok_or_else(|| {
        UpstreamError::Unreadable("the node sent a content type this proxy does not relay".into())
    })?;

    Ok(UpstreamResponse { status, content_type, body, headers })
}

/// One header line, bounded so a node that never sends a newline cannot grow this without limit.
fn read_line(io: &mut BufReader<ClientWire>, out: &mut String) -> Result<(), UpstreamError> {
    const MAX_LINE: usize = 16 << 10;
    let mut taken = io.take(MAX_LINE as u64);
    match taken.read_line(out) {
        Ok(0) => Err(UpstreamError::Sent(std::io::Error::from(std::io::ErrorKind::UnexpectedEof))),
        Ok(_) if out.ends_with('\n') => Ok(()),
        Ok(_) => Err(UpstreamError::Unreadable("a header line over the ceiling".to_string())),
        Err(e) => Err(timeout_or(e, UpstreamError::Sent)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_three_content_types_round_trip() {
        for ct in [ContentType::Json, ContentType::Prometheus, ContentType::Binary] {
            assert_eq!(ContentType::parse(ct.as_str()), Some(ct));
        }
    }

    #[test]
    fn an_unknown_content_type_is_refused_not_guessed() {
        assert_eq!(ContentType::parse("text/html"), None);
        assert_eq!(ContentType::parse("application/json; charset=utf-8"), None);
    }

    /// The distinction the retry policy is built on.
    #[test]
    fn not_sent_is_the_only_error_safe_for_any_route() {
        let io = || std::io::Error::from(std::io::ErrorKind::ConnectionRefused);
        assert!(UpstreamError::NotSent(io()).not_sent());
        assert!(!UpstreamError::Sent(io()).not_sent());
        assert!(!UpstreamError::Timeout.not_sent());
        assert!(!UpstreamError::Unreadable(String::new()).not_sent());
    }

    #[test]
    fn a_refused_connection_is_not_sent() {
        // Port 1 on loopback: nothing listens, and the refusal is immediate.
        let up = Upstream::new("nobody", "127.0.0.1:1");
        let err = up
            .send("GET", "/schema", "Host: x\r\n", b"", Duration::from_secs(2))
            .expect_err("nothing is listening");
        assert!(err.not_sent(), "a refused connect must be retryable anywhere: {err}");
    }

    #[test]
    fn the_in_flight_count_comes_back_down() {
        let up = Upstream::new("nobody", "127.0.0.1:1");
        assert_eq!(up.in_flight(), 0);
        let _ = up.send("GET", "/schema", "Host: x\r\n", b"", Duration::from_secs(2));
        assert_eq!(up.in_flight(), 0, "the slot guard did not release");
    }

    /// The property that keeps `/internal/*` closed at the connection as well as at the table.
    #[test]
    fn a_plain_upstream_carries_no_identity_and_there_is_no_way_to_give_it_one() {
        let up = Upstream::new("a", "a:7654");
        assert!(!up.is_tls());
        // `Upstream::secured` takes a `ClientTls` and nothing else. The only constructor of one
        // that this crate ever calls passes `identity: None` — see `main.rs`. There is no
        // `--upstream-cert`, and adding one would be adding peer access.
    }

    #[test]
    fn a_ceiling_of_zero_still_admits_one() {
        let up = Upstream::with_ceiling("n", "127.0.0.1:1", None, 0);
        assert_eq!(up.max_in_flight, 1);
    }
}
