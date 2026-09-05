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

//! What goes back out, for every route and every failure.
//!
//! Separate from the listener because it is the one type both halves of this crate agree on: a
//! handler builds one and knows nothing about sockets, the listener writes one and knows
//! nothing about routes. Two fields carry what the client is *not* told - the stable code and
//! the unredacted detail - so that the log line can report a failure without parsing the body
//! back out of the JSON that was just written.

use crate::json;

/// A status line, a content type, a body, and whatever extra headers the caller added.
///
/// Extra headers exist because three separate things need them and none of them could be
/// expressed by hardcoding: a request id to correlate with the log, a `Retry-After` when load
/// is shed, and a `WWW-Authenticate` challenge on a 401.
pub struct Response {
    /// The status code.
    pub status: u16,
    /// The reason phrase that goes with it, from [`reason_for`].
    pub reason: &'static str,
    /// The encoded body. JSON unless `content_type` says otherwise, and bytes rather than
    /// text because the fan-out between nodes answers with encoded containers.
    pub body: Vec<u8>,
    /// The `Content-Type` header value.
    pub content_type: &'static str,
    /// Extra headers, in the order they were added.
    pub headers: Vec<(String, String)>,
    /// The stable error code, when this response is an error. Carried so the log line can
    /// report it without parsing the body back out of the JSON it just wrote.
    pub code: Option<&'static str>,
    /// The unredacted message, when the body's was redacted. Never encoded - this field
    /// exists so the thing the client was not told still reaches the log.
    pub detail: Option<String>,
}

impl Response {
    /// `200`, with a JSON body the caller has already encoded.
    pub fn ok(body: String) -> Self {
        Self::new(200, "OK", body.into_bytes())
    }

    /// `200`, with a body that is not text at all.
    ///
    /// Only the `/internal/` routes use this: one node answering another with an encoded
    /// `Value` or a page of record ids. Nothing a client can reach produces one.
    pub fn binary(body: Vec<u8>) -> Self {
        Self { content_type: "application/octet-stream", ..Self::new(200, "OK", body) }
    }

    /// A body that is not JSON. Only `/metrics` uses this, and it has to: the Prometheus text
    /// format is a content type of its own and a scraper checks for it.
    pub fn text(content_type: &'static str, body: String) -> Self {
        Self { content_type, ..Self::new(200, "OK", body.into_bytes()) }
    }

    fn new(status: u16, reason: &'static str, body: Vec<u8>) -> Self {
        Self {
            status,
            reason,
            body,
            content_type: "application/json",
            headers: Vec::new(),
            code: None,
            detail: None,
        }
    }

    /// The one way an error becomes a response.
    ///
    /// `code` is the stable, machine-readable half and `message` the human half. Both always
    /// travel together: a client that matches on prose is a client that breaks when the prose
    /// is improved.
    pub fn failure(status: u16, code: &'static str, message: &str) -> Self {
        Self {
            code: Some(code),
            ..Self::new(status, reason_for(status), json::error(code, message).into_bytes())
        }
    }

    pub fn bad_request(message: &str) -> Self {
        Self::failure(400, "bad_request", message)
    }

    /// `404`, code `not_found`.
    pub fn not_found(message: &str) -> Self {
        Self::failure(404, "not_found", message)
    }

    /// `413`, code `request_too_large`. The message says a limit was exceeded and not which
    /// one, because the limits are configuration and a stranger has no business reading it.
    pub fn too_large() -> Self {
        Self::failure(413, "request_too_large", "the request exceeds the limit")
    }

    /// Adds one header, chainably.
    pub fn with_header(mut self, name: &str, value: impl core::fmt::Display) -> Self {
        self.headers.push((name.to_string(), value.to_string()));
        self
    }

    /// The bytes to write to the socket.
    ///
    /// The caller decides whether the connection survives, because that is a property of the
    /// server's load and of what the client asked for, neither of which a response knows.
    /// `detail` is deliberately not encoded: it exists so the part the client was not told
    /// still reaches the log.
    pub fn encode(&self, keep_alive: bool) -> Vec<u8> {
        let Self { status, reason, body, content_type, headers, .. } = self;
        let mut head = format!(
            "HTTP/1.1 {status} {reason}\r\n\
             Content-Type: {content_type}\r\n\
             Content-Length: {}\r\n\
             Connection: {}\r\n",
            body.len(),
            if keep_alive { "keep-alive" } else { "close" }
        );
        for (name, value) in headers {
            // A header value carrying CRLF would let a caller inject headers of its own. The
            // values here are all server-generated today, and this keeps that from being a
            // property anyone has to remember.
            let value: String = value.chars().filter(|c| *c != '\r' && *c != '\n').collect();
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        head.push_str("\r\n");
        let mut out = head.into_bytes();
        out.extend_from_slice(body);
        out
    }
}

/// The reason phrase for a status, for the statuses this server actually emits.
///
/// A phrase is cosmetic - no client parses it - so an unknown status gets a generic one rather
/// than an exhaustive table nobody maintains.
pub fn reason_for(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        // Not in the standard. See `status::db` for why it is emitted anyway.
        499 => "Client Closed Request",
        413 => "Payload Too Large",
        422 => "Unprocessable Content",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        // A peer answered with something this build could not read, or could not answer at
        // all. Distinct from a 503: the cluster was reachable and what came back was not
        // usable, which is a different thing for an operator to go and look at.
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "Error",
    }
}

/// A response whose body is written as it is produced.
///
/// **A second shape rather than a variant of [`Response`].** Everything that already exists —
/// `bigctl`, `clients/go`, `clients/python`, `bigproxy` — reads a `Content-Length` and says so
/// in its own source; three of them refuse a response without one. So the shape they read is
/// left exactly as it was, and this is a separate thing that only a route written for it can
/// produce. No existing route changes, and a client that never asks for one never meets one.
///
/// **`Connection: close`, always.** A chunked body ends when the sender says it does, and the
/// routes that need this are the ones that stay open until somebody goes away. Keeping the
/// connection alive afterwards would mean agreeing with the client about where the body ended,
/// which is exactly the negotiation this shape exists to avoid.
pub struct Streaming {
    status: u16,
    content_type: String,
    headers: Vec<(String, String)>,
}

/// The bytes that end a chunked body. Nothing follows it.
pub const LAST_CHUNK: &[u8] = b"0\r\n\r\n";

impl Streaming {
    /// A `200` of this content type.
    pub fn ok(content_type: &str) -> Self {
        Self { status: 200, content_type: content_type.to_string(), headers: Vec::new() }
    }

    /// Adds one header, chainably. Sanitised exactly as [`Response::encode`] sanitises its own.
    pub fn with_header(mut self, name: &str, value: impl core::fmt::Display) -> Self {
        self.headers.push((name.to_string(), value.to_string()));
        self
    }

    /// The head, to be written before the first chunk.
    pub fn head(&self) -> Vec<u8> {
        let Self { status, content_type, headers } = self;
        let mut head = format!(
            "HTTP/1.1 {status} {}\r\n\
             Content-Type: {content_type}\r\n\
             Transfer-Encoding: chunked\r\n\
             Cache-Control: no-cache\r\n\
             Connection: close\r\n",
            reason_for(*status)
        );
        for (name, value) in headers {
            let value: String = value.chars().filter(|c| *c != '\r' && *c != '\n').collect();
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        head.push_str("\r\n");
        head.into_bytes()
    }
}

/// Wraps a socket and frames whatever is written to it as chunks.
///
/// **An empty write is dropped rather than framed.** A zero-length chunk is how a chunked body
/// says it has ended, so writing one by accident would truncate the response at that point and
/// leave the rest of it as garbage after the terminator.
pub struct Chunked<'w> {
    inner: &'w mut dyn std::io::Write,
    written: u64,
}

impl<'w> Chunked<'w> {
    pub fn new(inner: &'w mut dyn std::io::Write) -> Self {
        Self { inner, written: 0 }
    }

    /// Body bytes actually sent, not counting the framing. What the log and the histogram want.
    pub fn written(&self) -> u64 {
        self.written
    }
}

impl std::io::Write for Chunked<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        self.inner.write_all(format!("{:x}\r\n", buf.len()).as_bytes())?;
        self.inner.write_all(buf)?;
        self.inner.write_all(b"\r\n")?;
        self.written += buf.len() as u64;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}
