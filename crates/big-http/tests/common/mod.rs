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

//! One server, one request, one answer - the three lines every test over HTTP starts with.
//!
//! These were copied into four test files, which meant four places to notice that a response
//! without a body is not a response without a status, and four chances to fix it in three of
//! them. The fixtures below them stay per file: what a test stocks a table with is what the
//! test is about, and sharing that would make every file read against a schema written for
//! somebody else.

// Each test binary uses its own subset, and an unused helper in one of them is not a defect.
#![allow(dead_code)]

use big_embed::Api;
use big_http::Server;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};

/// A server on a free port, serving exactly `requests` and then stopping.
///
/// Counted rather than stopped by hand so that a test which sends one request too many fails on
/// the request rather than by hanging.
pub fn spawn(requests: usize) -> SocketAddr {
    let api = Api::in_memory().unwrap();
    let server = Server::bind(api, "127.0.0.1:0").unwrap();
    let addr = server.local_addr().unwrap();
    std::thread::spawn(move || {
        let _ = server.serve_n(requests);
    });
    addr
}

/// The same, with authentication on, and a chance to arrange roles and grants first.
pub fn spawn_with_auth(
    requests: usize,
    users: &str,
    setup: impl FnOnce(&Api<big_pager::MemPager>),
) -> SocketAddr {
    let api = Api::in_memory().unwrap();
    // Before the server takes it, because arranging the grants *through* the surface being
    // tested would make every test here assume the thing it is checking.
    setup(&api);
    let config = big_http::ServerConfig {
        auth: big_http::Auth::parse(users, "test").unwrap(),
        ..Default::default()
    };
    let server = Server::bind_with(api, "127.0.0.1:0", config).unwrap();
    let addr = server.local_addr().unwrap();
    std::thread::spawn(move || {
        let _ = server.serve_n(requests);
    });
    addr
}

/// One request on its own connection, carrying a username and password.
pub fn send_as(
    addr: SocketAddr,
    user: &str,
    password: &str,
    method: &str,
    target: &str,
    body: &str,
) -> (u16, String) {
    let mut stream = TcpStream::connect(addr).unwrap();
    let credential = base64(format!("{user}:{password}").as_bytes());
    let request = format!(
        "{method} {target} HTTP/1.1\r\nHost: localhost\r\nAuthorization: Basic {credential}\r\n\
         Content-Length: {}\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).unwrap();
    stream.flush().unwrap();
    let mut raw = String::new();
    stream.read_to_string(&mut raw).unwrap();
    let status = raw.split(' ').nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    let body = raw.split_once("\r\n\r\n").map(|(_, b)| b.to_string()).unwrap_or_default();
    (status, body)
}

/// Standard base64, written out rather than pulled in: this crate has no dependencies and a test
/// helper is not a reason to give it one.
fn base64(input: &[u8]) -> String {
    const SET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in input.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = u32::from(b[0]) << 16 | u32::from(b[1]) << 8 | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(SET[(n >> (18 - 6 * i)) as usize & 63] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// One request on its own connection, and the status and body that come back.
pub fn send(addr: SocketAddr, method: &str, target: &str, body: &str) -> (u16, String) {
    let (status, _, body) = send_full(addr, method, target, body);
    (status, body)
}

/// The same, keeping the head as well, for a test that is about a header.
pub fn send_full(
    addr: SocketAddr,
    method: &str,
    target: &str,
    body: &str,
) -> (u16, String, String) {
    let mut stream = TcpStream::connect(addr).unwrap();
    let request = format!(
        "{method} {target} HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).unwrap();
    stream.flush().unwrap();

    let mut raw = String::new();
    stream.read_to_string(&mut raw).unwrap();
    let status = raw
        .split(' ')
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("no status in {raw:?}"));
    let head = raw.split("\r\n\r\n").next().unwrap_or_default().to_string();
    let body = raw.split("\r\n\r\n").nth(1).unwrap_or_default().to_string();
    (status, head, body)
}
