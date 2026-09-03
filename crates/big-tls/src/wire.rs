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

//! One accepted connection, plain or wrapped, behind one pair of `Read`/`Write` impls.

use crate::{ClientTls, Identity, TlsConfig};
use std::io::{self, BufReader, Read, Write};
use std::net::TcpStream;

/// The first byte of a TLS record that starts a handshake. Anything else on a TLS port is a
/// client that spoke in the clear.
const TLS_HANDSHAKE: u8 = 0x16;

/// Why a connection never became a `Wire`.
#[derive(Debug)]
pub enum WireError {
    /// The client spoke plaintext to a TLS port - overwhelmingly `curl http://…`. The socket
    /// comes back with the error so the caller can answer in the language the client was
    /// speaking, which rustls cannot do because by then it is not the one holding the socket.
    Plaintext(TcpStream),
    /// The handshake failed: no shared cipher, an expired certificate, a client that hung up.
    Handshake(String),
    /// A certificate this node's peer CA signed, naming no node on the roster. An operator's
    /// mistake, not an attacker's, most of the time - a node renamed in the cluster file and not
    /// in its certificate.
    UnknownPeer(String),
    /// The socket itself failed.
    Io(io::Error),
}

impl core::fmt::Display for WireError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Plaintext(_) => f.write_str("the request arrived in the clear on a TLS port"),
            Self::Handshake(e) => write!(f, "the TLS handshake failed: {e}"),
            Self::UnknownPeer(fp) => {
                write!(f, "a client certificate ({fp}) that names no node in the cluster file")
            }
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}

impl From<io::Error> for WireError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// One accepted connection, and the raw socket underneath it.
pub struct Wire {
    io: Io,
    /// A duplicated descriptor for the things that are properties of the *socket* rather than of
    /// the session: timeouts, the peer address, and the watchdog's peek. `O_NONBLOCK` and
    /// `SO_RCVTIMEO` live on the open file description both handles share, so setting one here
    /// sets it for the session too - which is the whole reason this field exists.
    ///
    /// **Nothing reads or writes through it.** Under TLS there is no second write handle: bytes
    /// have to go through the session that encrypts them.
    sock: TcpStream,
    identity: Identity,
    /// Set by any I/O error on the TLS path. A rustls session that failed part way through a
    /// record cannot be resynchronised, so keep-alive must not be offered afterwards.
    poisoned: bool,
}

enum Io {
    /// The buffer lives with the connection for the life of it. One per request would take the
    /// front of the next request into a buffer that is then dropped, which nothing notices until
    /// a client pipelines.
    Plain(BufReader<TcpStream>),
    /// Boxed because a `ServerConnection` is a couple of kilobytes, and an unboxed variant would
    /// make every plaintext `Wire` that big too.
    #[cfg(feature = "tls")]
    Tls(Box<BufReader<rustls::StreamOwned<rustls::ServerConnection, TcpStream>>>),
}

impl Wire {
    /// Accepts, and completes a handshake when there is one to complete.
    ///
    /// **The handshake happens here, on a worker thread, not on the accepting one.** A signature
    /// is a millisecond for RSA and a round trip is however long the network is; doing that on
    /// the thread that calls `accept` would cap new connections at a few hundred a second and,
    /// worse, would stop the server being able to shed - and a saturated server that cannot say
    /// so is the failure the whole pool exists to avoid.
    pub fn accept(sock: TcpStream, tls: Option<&TlsConfig>) -> Result<Self, WireError> {
        let dup = sock.try_clone()?;
        let Some(tls) = tls else {
            return Ok(Self {
                io: Io::Plain(BufReader::new(sock)),
                sock: dup,
                identity: Identity::None,
                poisoned: false,
            });
        };

        // One byte, before rustls sees any of them. A client that spoke plaintext gets a
        // sentence it can read instead of rustls's `InvalidMessage`, which is accurate and
        // unhelpful and has cost people hours.
        let mut first = [0u8; 1];
        if sock.peek(&mut first)? == 1 && first[0] != TLS_HANDSHAKE {
            return Err(WireError::Plaintext(sock));
        }

        #[cfg(not(feature = "tls"))]
        {
            let _ = (tls, dup);
            unreachable!("a TlsConfig cannot be constructed without the `tls` feature")
        }
        #[cfg(feature = "tls")]
        {
            let mut conn = rustls::ServerConnection::new(tls.server())
                .map_err(|e| WireError::Handshake(e.to_string()))?;
            let mut sock = sock;
            while conn.is_handshaking() {
                conn.complete_io(&mut sock).map_err(|e| WireError::Handshake(e.to_string()))?;
            }

            // Resolved once, here, rather than per request: a certificate does not change part
            // way through a connection, and asking again per request would be asking webpki the
            // same question on every fan-out message.
            let identity = match conn.peer_certificates() {
                None => Identity::None,
                Some(chain) => match crate::tls::node_identity(Some(chain), tls.roster()) {
                    Some(id) => id,
                    // Signed by our peer CA and naming nobody we know. Refused rather than
                    // demoted to an anonymous client: a certificate that got this far was meant
                    // to be a node, and treating it as a stranger would hide the misconfiguration
                    // behind a 401 nobody can explain.
                    None => {
                        let fp = chain
                            .first()
                            .map_or_else(String::new, |c| crate::tls::fingerprint(c.as_ref()));
                        return Err(WireError::UnknownPeer(fp));
                    }
                },
            };

            Ok(Self {
                io: Io::Tls(Box::new(BufReader::new(rustls::StreamOwned::new(conn, sock)))),
                sock: dup,
                identity,
                poisoned: false,
            })
        }
    }

    /// For timeouts and for the watchdog. Never for reading or writing.
    pub fn socket(&self) -> &TcpStream {
        &self.sock
    }

    /// What the client certificate proved, if there was one.
    pub fn identity(&self) -> &Identity {
        &self.identity
    }

    /// Whether this connection is encrypted, for the log line and for nothing else.
    pub fn is_tls(&self) -> bool {
        match self.io {
            Io::Plain(_) => false,
            #[cfg(feature = "tls")]
            Io::Tls(_) => true,
        }
    }

    /// Whether an I/O error has already broken this session.
    pub fn is_poisoned(&self) -> bool {
        self.poisoned
    }

    /// Whether decrypted bytes are sitting in a buffer that nobody has read.
    ///
    /// The same question `BufReader::buffer().is_empty()` answers one layer up, and it has to be
    /// asked at both layers: rustls holds plaintext the `BufReader` knows nothing about, and a
    /// connection carrying bytes past the end of a request is one where the two sides have lost
    /// track of where a message ends. Keeping it alive would deliver the next response into the
    /// middle of somebody's parser.
    pub fn has_pending_plaintext(&mut self) -> bool {
        match &mut self.io {
            Io::Plain(r) => !r.buffer().is_empty(),
            #[cfg(feature = "tls")]
            Io::Tls(s) => {
                if !s.buffer().is_empty() {
                    return true;
                }
                s.get_mut()
                    .conn
                    .process_new_packets()
                    .map_or(true, |state| state.plaintext_bytes_to_read() > 0)
            }
        }
    }
}

impl Read for Wire {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let r = match &mut self.io {
            Io::Plain(s) => s.read(buf),
            #[cfg(feature = "tls")]
            Io::Tls(s) => s.read(buf),
        };
        if r.is_err() {
            self.poisoned = true;
        }
        r
    }
}

impl Write for Wire {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let r = match &mut self.io {
            Io::Plain(s) => s.get_mut().write(buf),
            #[cfg(feature = "tls")]
            Io::Tls(s) => s.get_mut().write(buf),
        };
        if r.is_err() {
            self.poisoned = true;
        }
        r
    }

    fn flush(&mut self) -> io::Result<()> {
        let r = match &mut self.io {
            Io::Plain(s) => s.get_mut().flush(),
            #[cfg(feature = "tls")]
            Io::Tls(s) => s.get_mut().flush(),
        };
        if r.is_err() {
            self.poisoned = true;
        }
        r
    }
}

/// The outbound half: one connection this node opened to somebody else.
///
/// No `BufReader` inside it, unlike [`Wire`]. The peer client keeps its own around a pooled
/// connection and has to be able to ask that buffer whether it is empty, so wrapping one here
/// would put a second buffer between it and the socket that it could not see into.
pub struct ClientWire {
    io: ClientIo,
    sock: TcpStream,
}

enum ClientIo {
    Plain(TcpStream),
    #[cfg(feature = "tls")]
    Tls(Box<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>),
}

impl ClientWire {
    /// Wraps a connected socket, handshaking if there is a configuration to handshake with.
    ///
    /// `server_name` is what the certificate has to be valid for - the peer's name from the
    /// cluster file, not the address it was reached at. Those differ whenever a node moves, and
    /// the name is the part that was issued a certificate.
    pub fn connect(
        sock: TcpStream,
        tls: Option<&ClientTls>,
        server_name: &str,
    ) -> Result<Self, WireError> {
        let dup = sock.try_clone()?;
        let Some(tls) = tls else {
            return Ok(Self { io: ClientIo::Plain(sock), sock: dup });
        };
        #[cfg(not(feature = "tls"))]
        {
            let _ = (tls, server_name, dup);
            unreachable!("a ClientTls cannot be constructed without the `tls` feature")
        }
        #[cfg(feature = "tls")]
        {
            let name = rustls::pki_types::ServerName::try_from(server_name)
                .map_err(|e| WireError::Handshake(format!("`{server_name}` is not a name: {e}")))?
                .to_owned();
            let mut conn = rustls::ClientConnection::new(tls.config(), name)
                .map_err(|e| WireError::Handshake(e.to_string()))?;
            let mut sock = sock;
            while conn.is_handshaking() {
                conn.complete_io(&mut sock).map_err(|e| WireError::Handshake(e.to_string()))?;
            }
            Ok(Self {
                io: ClientIo::Tls(Box::new(rustls::StreamOwned::new(conn, sock))),
                sock: dup,
            })
        }
    }

    /// For timeouts and for the liveness peek. Never for reading or writing.
    pub fn socket(&self) -> &TcpStream {
        &self.sock
    }

    /// Whether rustls is holding decrypted bytes nobody has read - see
    /// [`Wire::has_pending_plaintext`], which asks the same question of the other direction.
    pub fn has_pending_plaintext(&mut self) -> bool {
        match &mut self.io {
            ClientIo::Plain(_) => false,
            #[cfg(feature = "tls")]
            ClientIo::Tls(s) => {
                s.conn.process_new_packets().map_or(true, |st| st.plaintext_bytes_to_read() > 0)
            }
        }
    }
}

impl Read for ClientWire {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match &mut self.io {
            ClientIo::Plain(s) => s.read(buf),
            #[cfg(feature = "tls")]
            ClientIo::Tls(s) => s.read(buf),
        }
    }
}

impl Write for ClientWire {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match &mut self.io {
            ClientIo::Plain(s) => s.write(buf),
            #[cfg(feature = "tls")]
            ClientIo::Tls(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match &mut self.io {
            ClientIo::Plain(s) => s.flush(),
            #[cfg(feature = "tls")]
            ClientIo::Tls(s) => s.flush(),
        }
    }
}
