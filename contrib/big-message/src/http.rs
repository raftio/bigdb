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

//! One connection to `big serve`, held open across requests.
//!
//! # Why this is not `big_bin::client::http`
//!
//! That client sends `Connection: close` and opens a socket per command, and says why: a
//! short-lived CLI has one exchange to make, and the server's keep-alive holds a worker from a
//! fixed pool. A producer is the opposite case - it makes thousands of exchanges and holds none
//! of them for a human - so it keeps the connection, which the server has always supported:
//! `ServerConfig` allows a thousand requests and five seconds of idle per connection.
//!
//! The parts of that client that were right are kept exactly: `Content-Length` is required, a
//! `Transfer-Encoding` is an error rather than something to handle, and the body is read with
//! `read_exact` so a connection that dies mid-body is an error and not a shorter answer.
//!
//! # The idle-close race, and why it is avoided rather than handled
//!
//! A keep-alive connection can be closed by the server at the very moment a client writes its
//! next request onto it. The write succeeds - it goes into a socket buffer - and the read then
//! ends at once, having read nothing. **A client cannot tell that apart from a server that read
//! the request, ran it, and died before answering.** For an idempotent request the difference
//! does not matter and everybody retries; for an allocating `INSERT` it is the difference
//! between recovering a batch and writing it twice.
//!
//! So this file does not try to tell them apart. It makes the race unreachable instead, by
//! replacing the connection well before the server would: after [`MAX_REQUESTS`] requests, and
//! after [`MAX_IDLE`] of quiet. A connection retired by the client is retired between requests,
//! where a fresh connect is provably safe.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

/// How many requests one connection carries before it is replaced.
///
/// The server's `max_keepalive_requests` is 1000. Stopping at 900 means the connection is always
/// retired by the client, between requests, rather than by the server underneath one.
const MAX_REQUESTS: u32 = 900;

/// How long a connection may sit unused before it is replaced.
///
/// The server's `keepalive_idle` is five seconds. Three leaves two seconds of margin for the
/// two clocks disagreeing about when the last exchange ended - the margin is the point, and it
/// costs one extra connect on a producer that has been quiet anyway.
const MAX_IDLE: Duration = Duration::from_secs(3);

/// What came back.
pub(crate) struct Response {
    pub(crate) status: u16,
    pub(crate) body: String,
    /// Whether the server said it would not take another request on this connection.
    pub(crate) closing: bool,
}

impl Response {
    pub(crate) fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

/// A failure, split by the only question that matters here: did the server see the request?
pub(crate) enum Failure {
    /// It did not, and this is known rather than assumed: either no connection was opened, or
    /// the request was still being written when the write failed - so no complete body reached
    /// the server, and nothing could have been parsed from a partial one.
    ///
    /// Safe to send again.
    NotSent(String),

    /// The request was written in full, and what happened next is not known.
    ///
    /// **Never safe to send again**, because the statement it carried is not idempotent.
    Unknown(String),

    /// Something came back that was not an HTTP response this client can read.
    ///
    /// Its own variant because it says something different about the deployment - a proxy in
    /// the path, most likely - than a connection that failed. The outcome is as unknown as
    /// [`Failure::Unknown`], so it is treated the same way by everything above.
    Protocol(String),
}

/// One server, and the connection currently open to it.
pub(crate) struct Conn {
    addr: String,
    token: Option<String>,
    timeout: Option<Duration>,
    /// `None` until the first request, and after every retirement.
    open: Option<Open>,
}

struct Open {
    reader: BufReader<TcpStream>,
    /// How many requests have gone down this one.
    sent: u32,
    /// When the last response finished, which is what [`MAX_IDLE`] is measured from.
    last: Instant,
}

impl Conn {
    pub(crate) fn new(addr: &str, token: Option<&str>, timeout: Option<Duration>) -> Self {
        Self { addr: addr.to_string(), token: token.map(str::to_string), timeout, open: None }
    }

    /// Sends one request and reads the whole answer.
    ///
    /// `body` is sent verbatim. This never inspects it: a statement is bytes on their way to the
    /// only thing that understands them, and a client that checked SQL would be a second parser
    /// to keep in step with the first.
    pub(crate) fn send(
        &mut self,
        method: &str,
        target: &str,
        body: &str,
    ) -> Result<Response, Failure> {
        self.retire_if_stale();

        // Opening is part of sending rather than a step before it, so that the one place a
        // connection comes into existence is also the one place a failure to open is reported.
        if self.open.is_none() {
            self.open = Some(self.connect()?);
        }

        let request = self.request(method, target, body);
        let open = self.open.as_mut().expect("just opened");

        // **Everything up to here is `NotSent`.** A write that fails part way leaves the server
        // holding fewer bytes than `Content-Length` promised, which it cannot parse as a
        // statement and will never run.
        if let Err(e) = open.reader.get_mut().write_all(request.as_bytes()) {
            self.open = None;
            return Err(Failure::NotSent(format!("could not send to {}: {e}", self.addr)));
        }
        if let Err(e) = open.reader.get_mut().flush() {
            self.open = None;
            return Err(Failure::NotSent(format!("could not send to {}: {e}", self.addr)));
        }

        // **Everything from here is `Unknown`.** The request is on the wire in full.
        let answer = read_response(&mut open.reader);
        match answer {
            Ok(response) => {
                open.sent += 1;
                open.last = Instant::now();
                if response.closing || open.sent >= MAX_REQUESTS {
                    self.open = None;
                }
                Ok(response)
            }
            Err(failure) => {
                self.open = None;
                Err(failure)
            }
        }
    }

    /// Drops a connection the server may be about to close, before a request is written onto it.
    fn retire_if_stale(&mut self) {
        let stale = self
            .open
            .as_ref()
            .is_some_and(|o| o.sent >= MAX_REQUESTS || o.last.elapsed() >= MAX_IDLE);
        if stale {
            self.open = None;
        }
    }

    fn connect(&self) -> Result<Open, Failure> {
        // Resolution can yield several addresses and `TcpStream::connect` tries each, which is
        // what a hostname needs. The per-address connect timeout is only reachable through the
        // single-address call, so the deadline is applied to the socket afterwards and the
        // connect itself uses the OS default - the same trade `big_bin::client::http` makes.
        let stream = TcpStream::connect(self.addr.as_str())
            .map_err(|e| Failure::NotSent(format!("could not connect to {}: {e}", self.addr)))?;
        for set in [TcpStream::set_read_timeout, TcpStream::set_write_timeout] {
            set(&stream, self.timeout).map_err(|e| {
                Failure::NotSent(format!("could not set a timeout on {}: {e}", self.addr))
            })?;
        }
        // Nagle off: a statement is written in one `write_all` and then the client waits for an
        // answer, so there is never a second small write to coalesce with - only a delay to add.
        let _ = stream.set_nodelay(true);
        Ok(Open { reader: BufReader::new(stream), sent: 0, last: Instant::now() })
    }

    fn request(&self, method: &str, target: &str, body: &str) -> String {
        let auth = match &self.token {
            Some(t) => format!("Authorization: Bearer {t}\r\n"),
            None => String::new(),
        };
        format!(
            "{method} {target} HTTP/1.1\r\n\
             Host: {}\r\n\
             User-Agent: big-message\r\n\
             {auth}\
             Content-Length: {}\r\n\
             Connection: keep-alive\r\n\r\n{body}",
            self.addr,
            body.len()
        )
    }
}

/// The status line, the headers, and exactly as many body bytes as were promised.
fn read_response(reader: &mut BufReader<TcpStream>) -> Result<Response, Failure> {
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .map_err(|e| Failure::Unknown(format!("could not read the response: {e}")))?;
    if line.is_empty() {
        return Err(Failure::Unknown("the server closed the connection".to_string()));
    }
    // `HTTP/1.1 200 OK`
    let status: u16 =
        line.split_whitespace().nth(1).and_then(|s| s.parse().ok()).ok_or_else(|| {
            Failure::Protocol(format!("not an HTTP status line: {:?}", line.trim()))
        })?;

    let mut length: Option<usize> = None;
    let mut closing = false;
    loop {
        let mut header = String::new();
        let read = reader
            .read_line(&mut header)
            .map_err(|e| Failure::Unknown(format!("could not read a header: {e}")))?;
        if read == 0 {
            return Err(Failure::Protocol("the headers ended without a blank line".to_string()));
        }
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        let Some((name, value)) = header.split_once(':') else {
            return Err(Failure::Protocol(format!("not a header: {header:?}")));
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("content-length") {
            length = Some(value.parse().map_err(|_| {
                Failure::Protocol(format!("Content-Length is not a number: {value:?}"))
            })?);
        }
        if name.eq_ignore_ascii_case("connection") && value.eq_ignore_ascii_case("close") {
            // The server saying this connection is finished. Believed rather than tested for
            // later: the alternative is finding out by writing a request onto a closed socket,
            // which is the one situation this file exists to stay out of.
            closing = true;
        }
        if name.eq_ignore_ascii_case("transfer-encoding") {
            // `big serve` never sends one. Refused rather than implemented, because implementing
            // a decoder that nothing produces means shipping a path no test can reach.
            return Err(Failure::Protocol(format!(
                "this server sent Transfer-Encoding: {value}, which big does not use"
            )));
        }
    }

    let Some(length) = length else {
        return Err(Failure::Protocol("the response has no Content-Length".to_string()));
    };

    let mut body = vec![0u8; length];
    // `read_exact` rather than `read_to_end`: a connection that dies mid-body must be an error
    // and not a shorter answer.
    reader.read_exact(&mut body).map_err(|e| {
        Failure::Unknown(format!("the response ended after fewer than {length} bytes: {e}"))
    })?;
    let body = String::from_utf8(body)
        .map_err(|_| Failure::Protocol("the response body is not UTF-8".to_string()))?;

    Ok(Response { status, body, closing })
}
