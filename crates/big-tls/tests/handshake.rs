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

//! A real socket, a real handshake, and the four answers `Wire::accept` can give.
//!
//! Certificates are generated per run rather than checked in. A fixture certificate expires, and
//! a test that starts failing in 2036 is a landmine left for somebody who will have no idea what
//! it was guarding.

#![cfg(feature = "tls")]

use big_tls::{ClientTls, ClientWire, Identity, TlsConfig, Wire, WireError};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};

/// A certificate authority, and the leaves it will sign.
struct Ca {
    cert: rcgen::Certificate,
    key: rcgen::KeyPair,
    dir: tempfile::TempDir,
}

impl Ca {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let key = rcgen::KeyPair::generate().unwrap();
        let mut params = rcgen::CertificateParams::new(Vec::new()).unwrap();
        params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        params.distinguished_name.push(rcgen::DnType::CommonName, "big test ca");
        let cert = params.self_signed(&key).unwrap();
        Self { cert, key, dir }
    }

    /// The CA certificate on disk, for whoever has to trust it.
    fn pem(&self) -> PathBuf {
        self.write("ca.pem", &self.cert.pem(), 0o644)
    }

    /// A leaf certificate valid for `names`, and its key. Both at the modes the loader demands:
    /// a certificate is public, a key is not.
    fn issue(&self, stem: &str, names: &[&str]) -> (PathBuf, PathBuf) {
        let key = rcgen::KeyPair::generate().unwrap();
        let params =
            rcgen::CertificateParams::new(names.iter().map(|s| s.to_string()).collect::<Vec<_>>())
                .unwrap();
        let cert = params.signed_by(&key, &self.cert, &self.key).unwrap();
        (
            self.write(&format!("{stem}.pem"), &cert.pem(), 0o644),
            self.write(&format!("{stem}.key"), &key.serialize_pem(), 0o600),
        )
    }

    fn write(&self, name: &str, body: &str, mode: u32) -> PathBuf {
        let path = self.dir.path().join(name);
        std::fs::write(&path, body).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        }
        path
    }
}

/// The client thread, joined when the test is done talking to it.
///
/// **Not joined by `exchange` itself**, which was the first shape this took and deadlocks: a
/// client blocked reading the reply cannot finish until the server writes it, and the server
/// cannot write it until `exchange` returns. Every test here has to be able to do its half
/// before the other side is waited on.
struct Client(Option<std::thread::JoinHandle<()>>);

impl Drop for Client {
    fn drop(&mut self) {
        if let Some(h) = self.0.take() {
            // A panic on the client thread is a real failure and is re-raised here, where it
            // names the test, rather than being printed loose at process exit. Skipped while
            // already unwinding, because panicking in a `Drop` during a panic aborts.
            let joined = h.join();
            if !std::thread::panicking() {
                joined.expect("the client thread");
            }
        }
    }
}

/// Accepts one connection on a fresh port and hands back what `Wire::accept` made of it.
///
/// The read timeout is what turns a handshake that will never complete into a failing test
/// rather than a hanging one.
fn exchange(
    tls: &TlsConfig,
    client: impl FnOnce(TcpStream) + Send + 'static,
) -> (Result<Wire, WireError>, Client) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = std::thread::spawn(move || client(TcpStream::connect(addr).unwrap()));
    let (sock, _) = listener.accept().unwrap();
    sock.set_read_timeout(Some(std::time::Duration::from_secs(10))).unwrap();
    sock.set_write_timeout(Some(std::time::Duration::from_secs(10))).unwrap();
    (Wire::accept(sock, Some(tls)), Client(Some(handle)))
}

fn client_tls(ca: &Path, identity: Option<(&Path, &Path)>) -> ClientTls {
    ClientTls::new(Some(ca), identity).unwrap()
}

#[test]
fn a_plaintext_wire_carries_bytes_both_ways() {
    // The configuration every existing test in the tree runs under, asserted here so that the
    // TLS work below cannot quietly become the only path that is exercised.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = std::thread::spawn(move || {
        let mut sock = TcpStream::connect(addr).unwrap();
        sock.write_all(b"ping").unwrap();
        let mut back = [0u8; 4];
        sock.read_exact(&mut back).unwrap();
        assert_eq!(&back, b"pong");
    });

    let (sock, _) = listener.accept().unwrap();
    let mut wire = Wire::accept(sock, None).unwrap();
    assert!(!wire.is_tls());
    assert_eq!(wire.identity(), &Identity::None);

    let mut buf = [0u8; 4];
    wire.read_exact(&mut buf).unwrap();
    assert_eq!(&buf, b"ping");
    wire.write_all(b"pong").unwrap();
    wire.flush().unwrap();
    handle.join().unwrap();
}

#[test]
fn a_handshake_completes_and_the_wire_carries_bytes() {
    let ca = Ca::new();
    let (cert, key) = ca.issue("server", &["localhost"]);
    let ca_pem = ca.pem();
    let tls = TlsConfig::load(&cert, &key, None, Vec::new()).unwrap();

    let (wire, _client) = exchange(&tls, move |sock| {
        let mut w = ClientWire::connect(sock, Some(&client_tls(&ca_pem, None)), "localhost")
            .expect("the client handshake");
        w.write_all(b"ping").unwrap();
        w.flush().unwrap();
        let mut back = [0u8; 4];
        w.read_exact(&mut back).unwrap();
        assert_eq!(&back, b"pong");
    });
    let mut wire = wire.expect("the server handshake");

    assert!(wire.is_tls());
    let mut buf = [0u8; 4];
    wire.read_exact(&mut buf).unwrap();
    assert_eq!(&buf, b"ping");
    wire.write_all(b"pong").unwrap();
    wire.flush().unwrap();
}

#[test]
fn a_client_with_no_certificate_is_anonymous_and_still_served() {
    // Client authentication is optional, not required. Required would refuse every `bigctl` and
    // every `curl`, which between them are every client this database has.
    let ca = Ca::new();
    let (cert, key) = ca.issue("server", &["localhost"]);
    let ca_pem = ca.pem();
    let peer_ca = ca.pem();
    let tls = TlsConfig::load(&cert, &key, Some(&peer_ca), vec!["node-a".to_string()]).unwrap();

    let (wire, _client) = exchange(&tls, move |sock| {
        let mut w = ClientWire::connect(sock, Some(&client_tls(&ca_pem, None)), "localhost")
            .expect("a client with no certificate is still allowed to connect");
        let _ = w.write_all(b"ping");
        let _ = w.flush();
    });
    let wire = wire.expect("the server handshake");

    assert_eq!(wire.identity(), &Identity::None, "no certificate proves no node");
}

#[test]
fn a_peer_certificate_names_the_node_it_was_issued_for() {
    let ca = Ca::new();
    let (cert, key) = ca.issue("server", &["localhost"]);
    let (peer_cert, peer_key) = ca.issue("node-a", &["node-a"]);
    let ca_pem = ca.pem();
    let peer_ca = ca.pem();
    let roster = vec!["node-a".to_string(), "node-b".to_string()];
    let tls = TlsConfig::load(&cert, &key, Some(&peer_ca), roster).unwrap();
    assert!(tls.checks_peers());

    let (wire, _client) = exchange(&tls, move |sock| {
        let identity = Some((peer_cert.as_path(), peer_key.as_path()));
        let mut w = ClientWire::connect(sock, Some(&client_tls(&ca_pem, identity)), "localhost")
            .expect("the peer handshake");
        let _ = w.write_all(b"ping");
        let _ = w.flush();
    });
    let wire = wire.expect("the server handshake");

    assert_eq!(
        wire.identity(),
        &Identity::Node("node-a".to_string()),
        "the roster entry the certificate is valid for is the node it is"
    );
}

#[test]
fn a_certificate_naming_no_node_on_the_roster_is_refused() {
    // Signed by the peer CA, so the chain verifies - and naming nobody in the cluster file, so
    // there is no node it could be. Refused at accept rather than demoted to an anonymous
    // client: a certificate that got this far was meant to be a node, and hiding the
    // misconfiguration behind a later 401 would send the operator to look at the users file.
    let ca = Ca::new();
    let (cert, key) = ca.issue("server", &["localhost"]);
    let (peer_cert, peer_key) = ca.issue("node-z", &["node-z"]);
    let ca_pem = ca.pem();
    let peer_ca = ca.pem();
    let tls = TlsConfig::load(&cert, &key, Some(&peer_ca), vec!["node-a".to_string()]).unwrap();

    let (refused, _client) = exchange(&tls, move |sock| {
        let identity = Some((peer_cert.as_path(), peer_key.as_path()));
        if let Ok(mut w) =
            ClientWire::connect(sock, Some(&client_tls(&ca_pem, identity)), "localhost")
        {
            let _ = w.write_all(b"ping");
            let _ = w.flush();
        }
    });

    match refused {
        Err(WireError::UnknownPeer(fingerprint)) => {
            assert!(!fingerprint.is_empty(), "the log line needs something to grep for");
        }
        other => panic!("expected UnknownPeer, got {other:?}", other = other.map(|_| "a wire")),
    }
}

#[test]
fn a_certificate_from_another_ca_never_gets_that_far() {
    let ca = Ca::new();
    let other = Ca::new();
    let (cert, key) = ca.issue("server", &["localhost"]);
    let (peer_cert, peer_key) = other.issue("node-a", &["node-a"]);
    let ca_pem = ca.pem();
    let peer_ca = ca.pem();
    let tls = TlsConfig::load(&cert, &key, Some(&peer_ca), vec!["node-a".to_string()]).unwrap();

    let (refused, _client) = exchange(&tls, move |sock| {
        let identity = Some((peer_cert.as_path(), peer_key.as_path()));
        let _ = ClientWire::connect(sock, Some(&client_tls(&ca_pem, identity)), "localhost");
    });

    assert!(
        matches!(refused, Err(WireError::Handshake(_))),
        "a certificate the peer CA did not sign fails in the handshake, not in the roster"
    );
}

#[test]
fn plaintext_on_a_tls_port_comes_back_with_its_socket() {
    // `curl http://…` against a TLS port. rustls's own answer is `InvalidMessage`, which is
    // accurate and unreadable; the socket comes back so the caller can answer in the language
    // the client was actually speaking.
    let ca = Ca::new();
    let (cert, key) = ca.issue("server", &["localhost"]);
    let tls = TlsConfig::load(&cert, &key, None, Vec::new()).unwrap();

    let (refused, _client) = exchange(&tls, |mut sock| {
        // Written and then left. Reading for the answer here would block until the server drops
        // the socket, and the server is holding it inside the error this test is about.
        sock.write_all(b"GET /health HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
    });

    match refused {
        Err(WireError::Plaintext(sock)) => {
            assert!(sock.peer_addr().is_ok(), "the socket comes back usable");
        }
        other => panic!("expected Plaintext, got {:?}", other.map(|_| "a wire")),
    }
}

#[test]
fn a_wire_notices_bytes_the_reader_has_not_taken() {
    // The keep-alive question, asked of the TLS path. A session holding decrypted bytes nobody
    // read is one where the two sides disagree about where a message ended, and keeping it open
    // would deliver the next response into the middle of somebody's parser.
    let ca = Ca::new();
    let (cert, key) = ca.issue("server", &["localhost"]);
    let ca_pem = ca.pem();
    let tls = TlsConfig::load(&cert, &key, None, Vec::new()).unwrap();

    let (wire, _client) = exchange(&tls, move |sock| {
        let mut w = ClientWire::connect(sock, Some(&client_tls(&ca_pem, None)), "localhost")
            .expect("the client handshake");
        w.write_all(b"first!second").unwrap();
        w.flush().unwrap();
        // Held open so the server's peek sees a live socket rather than an EOF.
        std::thread::sleep(std::time::Duration::from_millis(300));
    });
    let mut wire = wire.expect("the server handshake");

    let mut buf = [0u8; 6];
    wire.read_exact(&mut buf).unwrap();
    assert_eq!(&buf, b"first!");
    assert!(wire.has_pending_plaintext(), "`second` is still sitting in a buffer");
}

#[test]
fn a_key_file_anyone_can_read_is_refused_before_the_port_opens() {
    let ca = Ca::new();
    let (cert, key) = ca.issue("server", &["localhost"]);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o644)).unwrap();
        let e = TlsConfig::load(&cert, &key, None, Vec::new()).unwrap_err();
        assert!(e.to_string().contains("chmod 600"), "{e}");
        assert!(e.to_string().contains("a private key"), "{e}");
    }
}

#[test]
fn swapping_the_certificate_and_key_files_says_which_way_round_they_go() {
    // The mistake somebody makes at 2am. Both files exist, both are PEM, and the message has to
    // name which flag got which file or it is no help at all.
    //
    // In the ordinary case the *mode* check catches it first, and that is the right order: a
    // certificate is world-readable because a certificate is public, so pointing `--tls-key` at
    // one trips the rule about private keys before anything looks at a PEM label. That message
    // names the file too, so it is no worse an answer - it is a different correct one.
    let ca = Ca::new();
    let (cert, key) = ca.issue("server", &["localhost"]);
    let e = TlsConfig::load(&key, &cert, None, Vec::new()).unwrap_err();
    assert!(e.to_string().contains("chmod 600"), "{e}");
    assert!(e.to_string().ends_with("server.pem`"), "the message names the swapped file: {e}");

    // With the mode rule satisfied, the labels are what is left to notice - both directions.
    let tight_cert = ca.write("copy-of-cert.pem", &std::fs::read_to_string(&cert).unwrap(), 0o600);
    let e = TlsConfig::load(&cert, &tight_cert, None, Vec::new()).unwrap_err();
    assert!(e.to_string().contains("is this the certificate file?"), "{e}");

    let e = TlsConfig::load(&key, &key, None, Vec::new()).unwrap_err();
    assert!(e.to_string().contains("is this the key file?"), "{e}");
}

#[test]
fn a_fully_read_request_leaves_nothing_pending() {
    // The other half of `a_wire_notices_bytes_the_reader_has_not_taken`, and the half that
    // decides whether keep-alive works at all: a `true` here would close every connection after
    // one request, which presents as latency rather than as a failure.
    let ca = Ca::new();
    let (cert, key) = ca.issue("server", &["localhost"]);
    let ca_pem = ca.pem();
    let tls = TlsConfig::load(&cert, &key, None, Vec::new()).unwrap();

    let (wire, _client) = exchange(&tls, move |sock| {
        let mut w = ClientWire::connect(sock, Some(&client_tls(&ca_pem, None)), "localhost")
            .expect("the client handshake");
        w.write_all(b"first!").unwrap();
        w.flush().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(300));
    });
    let mut wire = wire.expect("the server handshake");

    let mut buf = [0u8; 6];
    wire.read_exact(&mut buf).unwrap();
    assert_eq!(&buf, b"first!");
    assert!(
        !wire.has_pending_plaintext(),
        "everything sent has been read, so the connection is reusable"
    );
}
