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

//! The real listener, over TLS.
//!
//! `big-tls`'s own tests prove a handshake completes; these prove the *server* still works on the
//! other side of one - that a request goes in and a response comes out, that keep-alive survives
//! encryption, and that a client which forgets the `s` in `https` is told so rather than being
//! handed a rustls error nobody can read.

#![cfg(feature = "tls")]

use big_embed::Api;
use big_http::{Server, ServerConfig};
use big_tls::{ClientTls, ClientWire, TlsConfig};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::time::Duration;

/// A self-signed certificate for `localhost`, and the file to trust it from.
///
/// Generated per run rather than checked in, for the reason a fixture certificate cannot be:
/// it expires, and a test that starts failing in 2036 is a landmine.
///
/// **Named, not addressed.** The suites bind `127.0.0.1:0` and the client connects to that port,
/// but the *name* it verifies against is `localhost` - so a DNS SAN is enough and an IP SAN is
/// not needed. Passing the address as the name is what would require one.
struct Certs {
    dir: tempfile::TempDir,
    cert: PathBuf,
    key: PathBuf,
}

impl Certs {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let issued = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        let cert = dir.path().join("cert.pem");
        let key = dir.path().join("key.pem");
        std::fs::write(&cert, issued.cert.pem()).unwrap();
        std::fs::write(&key, issued.key_pair.serialize_pem()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        Self { dir, cert, key }
    }

    fn server(&self) -> TlsConfig {
        TlsConfig::load(&self.cert, &self.key, None, None, Vec::new()).unwrap()
    }

    /// A client that trusts this certificate and nothing else.
    fn client(&self) -> ClientTls {
        ClientTls::new(Some(&self.cert), None).unwrap()
    }
}

/// A server on a loopback port that answers `requests` and then stops.
fn spawn(requests: usize, config: ServerConfig) -> SocketAddr {
    let api = Api::in_memory().unwrap();
    api.create_table("tx").unwrap();
    api.create_field("tx", "amount", big_db::catalog::FieldKind::Int, 32).unwrap();
    let facts: Vec<_> = (1..=8)
        .map(|r| big_embed::Fact::Int { field: "amount", record: r, value: r * 10 })
        .collect();
    api.import("tx", &facts).unwrap();

    let server = Server::bind_with(api, "127.0.0.1:0", config).unwrap();
    let addr = server.local_addr().unwrap();
    std::thread::spawn(move || {
        let _ = server.serve_n(requests);
    });
    addr
}

/// One request over TLS, reading until the connection closes.
fn get(addr: SocketAddr, tls: &ClientTls, target: &str) -> (u16, String) {
    let sock = TcpStream::connect(addr).unwrap();
    sock.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut wire = ClientWire::connect(sock, Some(tls), "localhost").expect("the handshake");
    let request = format!("GET {target} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    wire.write_all(request.as_bytes()).unwrap();
    wire.flush().unwrap();

    let mut raw = Vec::new();
    // `UnexpectedEof` is what rustls returns when the peer drops the connection without a
    // `close_notify`, which is what this server does. The bytes already read are still the
    // response, so it is not an error worth failing on.
    let _ = wire.read_to_end(&mut raw);
    let raw = String::from_utf8_lossy(&raw).into_owned();
    let status = raw
        .split(' ')
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("no status in {raw:?}"));
    (status, raw)
}

#[test]
fn a_request_over_tls_is_answered_like_any_other() {
    let certs = Certs::new();
    let addr = spawn(1, ServerConfig { tls: Some(certs.server()), ..Default::default() });
    let (status, raw) = get(addr, &certs.client(), "/health");
    assert_eq!(status, 200, "{raw}");
    assert!(raw.contains("X-Request-Id"), "the ordinary response headers are still there: {raw}");
}

#[test]
fn the_engine_is_reachable_through_the_session() {
    // Not just the probes: a route that actually reads the database, so that what is being
    // asserted is the whole path rather than a string this server could have written anywhere.
    let certs = Certs::new();
    let addr = spawn(1, ServerConfig { tls: Some(certs.server()), ..Default::default() });
    let (status, raw) = get(addr, &certs.client(), "/schema");
    assert_eq!(status, 200, "{raw}");
    assert!(raw.contains("amount"), "the schema came back through the session: {raw}");
}

#[test]
fn one_session_carries_more_than_one_request() {
    // Keep-alive over TLS. Worth its own test because a handshake is expensive enough that
    // losing reuse would be a real regression, and because the `keep` decision grew two clauses
    // when the wire did - either of which, wrong, would close every connection after one request
    // and be invisible except as latency.
    let certs = Certs::new();
    let addr = spawn(1, ServerConfig { tls: Some(certs.server()), ..Default::default() });
    let sock = TcpStream::connect(addr).unwrap();
    sock.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let client = certs.client();
    let mut wire = ClientWire::connect(sock, Some(&client), "localhost").expect("the handshake");

    for n in 0..3 {
        // Asked for explicitly. This server does not assume it from the HTTP version - see
        // `Request::wants_keep_alive` - so a test that wants a reused connection has to say so.
        wire.write_all(
            b"GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: keep-alive\r\n\r\n",
        )
        .unwrap();
        wire.flush().unwrap();
        let mut buf = [0u8; 1024];
        let read = wire.read(&mut buf).unwrap();
        let text = String::from_utf8_lossy(&buf[..read]).into_owned();
        assert!(text.starts_with("HTTP/1.1 200"), "request {n} on the same session: {text}");
    }
}

#[test]
fn plaintext_on_a_tls_port_is_told_so_in_words_it_can_read() {
    // `curl http://…` against a TLS port, which is the single most likely first mistake after
    // this change ships. rustls's own answer is `InvalidMessage`; this one is an HTTP response,
    // because the client is speaking HTTP and can read one.
    let certs = Certs::new();
    let addr = spawn(1, ServerConfig { tls: Some(certs.server()), ..Default::default() });

    let mut sock = TcpStream::connect(addr).unwrap();
    sock.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    sock.write_all(b"GET /health HTTP/1.1\r\nHost: localhost\r\n\r\n").unwrap();
    let mut raw = String::new();
    sock.read_to_string(&mut raw).unwrap();

    assert!(raw.starts_with("HTTP/1.1 400"), "{raw}");
    assert!(raw.contains("plaintext_on_a_tls_port"), "{raw}");
    assert!(raw.contains("https://"), "it says what to do instead: {raw}");
}

#[test]
fn a_client_that_does_not_trust_the_certificate_gets_no_session() {
    let certs = Certs::new();
    let addr = spawn(1, ServerConfig { tls: Some(certs.server()), ..Default::default() });

    // Trusting nothing at all, which is what an empty CA file amounts to. Deliberately not
    // softened into "use the platform roots": a database peer signed by a public CA is not a
    // peer, it is anybody.
    let stranger = ClientTls::new(None, None).unwrap();
    let sock = TcpStream::connect(addr).unwrap();
    sock.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    assert!(
        ClientWire::connect(sock, Some(&stranger), "localhost").is_err(),
        "an untrusted certificate must not produce a session"
    );
}

#[test]
fn a_plaintext_listener_is_still_a_plaintext_listener() {
    // The other half of the same question, and the one that matters most: `tls: None` has to
    // behave exactly as it did before any of this existed.
    let certs = Certs::new();
    drop(certs);
    let addr = spawn(1, ServerConfig::default());
    let mut sock = TcpStream::connect(addr).unwrap();
    sock.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    sock.write_all(b"GET /health HTTP/1.1\r\nHost: localhost\r\n\r\n").unwrap();
    let mut raw = String::new();
    sock.read_to_string(&mut raw).unwrap();
    assert!(raw.starts_with("HTTP/1.1 200"), "{raw}");
}

#[test]
fn the_directory_outlives_the_certificates_it_holds() {
    // Not a behaviour test: a compile-time reminder that `Certs` owns its `TempDir`, because a
    // version of this file that returned only the paths deleted them before the server read
    // them and failed in a way that looked like a certificate problem.
    let certs = Certs::new();
    assert!(certs.dir.path().exists());
    assert!(certs.cert.exists() && certs.key.exists());
}
