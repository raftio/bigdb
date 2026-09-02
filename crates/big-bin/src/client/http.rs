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

//! One exchange with `big serve`, written by hand for the same reason the server's reader is.
//!
//! This talks to exactly one server, and that server always answers with a `Content-Length` and
//! never chunks. So this client insists on both: a `Transfer-Encoding` is an error rather than
//! a thing to handle, and a body shorter than its declared length is an error rather than a
//! short answer. **That second one is the whole reason this file is not three lines of
//! `read_to_end`** - a response truncated by a dropped connection would otherwise render as a
//! table with some rows missing, which is the failure mode a person cannot see.
//!
//! No connection reuse. One command is one exchange, the process exits afterwards, and a pool
//! would be state to get wrong for no gain. `bigctl shell` opens a connection per statement for
//! the same reason - the server's keep-alive holds a worker from a fixed pool, and a human
//! typing is the worst possible thing to hold one for.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

/// What came back.
pub struct Response {
    /// The status line's code.
    pub status: u16,
    /// The body, exactly `Content-Length` bytes of it.
    pub body: String,
}

impl Response {
    /// Whether the server answered rather than refused.
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

/// Everything that can go wrong before a body exists.
#[derive(Debug)]
pub enum Error {
    /// Nothing was listening, or the connection died. Its own variant because it is the one
    /// failure that is about the *server not being there* rather than about what it said, and
    /// the exit code differs.
    Unreachable(String),
    /// The connection worked and what came back was not an HTTP response this client can read.
    Protocol(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreachable(m) | Self::Protocol(m) => write!(f, "{m}"),
        }
    }
}

/// Where to send, and what to present.
pub struct Client {
    /// `host:port`.
    pub addr: String,
    /// The bearer token, already read out of its file.
    pub token: Option<String>,
    /// Applied to the connect, the write and the read. `None` waits as long as the server does.
    pub timeout: Option<Duration>,
}

impl Client {
    /// Sends one request and reads the whole answer.
    ///
    /// `body` is sent verbatim. This client never inspects it: a statement is bytes on their way
    /// to the only thing that understands them, and a client that validated SQL would be a
    /// second parser to keep in step with the first.
    pub fn send(&self, method: &str, target: &str, body: &str) -> Result<Response, Error> {
        let mut stream = self.connect()?;

        let auth = match &self.token {
            Some(t) => format!("Authorization: Bearer {t}\r\n"),
            None => String::new(),
        };
        let request = format!(
            "{method} {target} HTTP/1.1\r\n\
             Host: {}\r\n\
             User-Agent: bigctl\r\n\
             {auth}\
             Content-Length: {}\r\n\
             Connection: close\r\n\r\n{body}",
            self.addr,
            body.len()
        );
        stream
            .write_all(request.as_bytes())
            .and_then(|()| stream.flush())
            .map_err(|e| Error::Unreachable(format!("could not send to {}: {e}", self.addr)))?;

        read_response(BufReader::new(stream))
    }

    fn connect(&self) -> Result<TcpStream, Error> {
        // Resolution can yield several addresses; `TcpStream::connect` tries each, which is
        // what a hostname in `--addr` needs. The per-address connect timeout is only reachable
        // through the single-address call, so the deadline below is applied to the socket
        // afterwards and the connect itself uses the OS default.
        let stream = TcpStream::connect(self.addr.as_str())
            .map_err(|e| Error::Unreachable(format!("could not connect to {}: {e}", self.addr)))?;
        for set in [TcpStream::set_read_timeout, TcpStream::set_write_timeout] {
            set(&stream, self.timeout).map_err(|e| {
                Error::Unreachable(format!("could not set a timeout on {}: {e}", self.addr))
            })?;
        }
        Ok(stream)
    }
}

/// The status line, the headers, and exactly as many body bytes as were promised.
fn read_response(mut reader: BufReader<TcpStream>) -> Result<Response, Error> {
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .map_err(|e| Error::Unreachable(format!("could not read the response: {e}")))?;
    if line.is_empty() {
        return Err(Error::Unreachable("the server closed the connection".to_string()));
    }
    // `HTTP/1.1 200 OK`
    let status: u16 =
        line.split_whitespace().nth(1).and_then(|s| s.parse().ok()).ok_or_else(|| {
            Error::Protocol(format!("not an HTTP status line: {:?}", line.trim()))
        })?;

    let mut length: Option<usize> = None;
    loop {
        let mut header = String::new();
        let read = reader
            .read_line(&mut header)
            .map_err(|e| Error::Unreachable(format!("could not read a header: {e}")))?;
        if read == 0 {
            return Err(Error::Protocol("the headers ended without a blank line".to_string()));
        }
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        let Some((name, value)) = header.split_once(':') else {
            return Err(Error::Protocol(format!("not a header: {header:?}")));
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("content-length") {
            length = Some(value.parse().map_err(|_| {
                Error::Protocol(format!("Content-Length is not a number: {value:?}"))
            })?);
        }
        if name.eq_ignore_ascii_case("transfer-encoding") {
            // `big serve` never sends one. Refused rather than implemented, because implementing a
            // decoder that nothing produces means shipping a path no test can reach.
            return Err(Error::Protocol(format!(
                "this server sent Transfer-Encoding: {value}, which big does not use"
            )));
        }
    }

    let Some(length) = length else {
        return Err(Error::Protocol("the response has no Content-Length".to_string()));
    };

    let mut body = vec![0u8; length];
    // `read_exact` rather than `read_to_end`: a connection that dies mid-body must be an error
    // and not a shorter answer.
    reader.read_exact(&mut body).map_err(|e| {
        Error::Unreachable(format!("the response ended after fewer than {length} bytes: {e}"))
    })?;
    let body = String::from_utf8(body)
        .map_err(|_| Error::Protocol("the response body is not UTF-8".to_string()))?;

    Ok(Response { status, body })
}
