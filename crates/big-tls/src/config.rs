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

//! What a listener presents, what a caller trusts, and what a certificate proved.

use std::io;
use std::path::Path;

#[cfg(feature = "tls")]
use std::sync::Arc;

/// What the *connection* proved, before a single header was read.
///
/// Not a role. A role is a statement about a person's authority over data; this is a statement
/// about which process is speaking. Keeping them separate is what stops a leaked node key from
/// being an admin credential on the public routes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Identity {
    /// No client certificate. Every ordinary client connection, and every plaintext one.
    #[default]
    None,
    /// Another node of this cluster, named by the certificate it presented.
    Node(String),
}

impl Identity {
    /// The node this connection proved itself to be, if it proved anything.
    pub fn node(&self) -> Option<&str> {
        match self {
            Self::None => None,
            Self::Node(name) => Some(name),
        }
    }
}

/// What a listener presents, and what it will accept from a peer.
///
/// The whole type is uninhabited when the `tls` feature is off, which is what lets
/// `Option<TlsConfig>` be an ordinary field on `ServerConfig` with no `#[cfg]` at any of the
/// places that construct one. The alternative - a `#[cfg]` on the field itself - would put one
/// at every construction site in the tree, tests included.
#[derive(Clone)]
pub struct TlsConfig {
    #[cfg(feature = "tls")]
    inner: Arc<ServerInner>,
    #[cfg(not(feature = "tls"))]
    never: core::convert::Infallible,
}

#[cfg(feature = "tls")]
struct ServerInner {
    server: Arc<rustls::ServerConfig>,
    /// The node names a client certificate is allowed to name. A certificate this node's peer CA
    /// signed that matches none of them is refused.
    ///
    /// **Replaceable while the listener runs.** The cluster file used to be the roster for the
    /// life of the process, which was exact while membership was a file - and impossible once a
    /// node can join, because the joining node's certificate names somebody the file has never
    /// heard of. The agreement is the roster now; the file is only what it starts as.
    ///
    /// A lock rather than an `Arc` swap because it is read once per accepted connection, which
    /// is nowhere near often enough to be worth anything cleverer.
    roster: std::sync::RwLock<Vec<String>>,
}

/// Redacted, like `Auth`. Nothing here is a secret that printing would leak - the private key is
/// inside rustls and has no `Debug` - but a derived one would dump a certificate chain into
/// whatever printed it, and a wall of base64 in a panic message helps nobody.
impl core::fmt::Debug for TlsConfig {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        #[cfg(feature = "tls")]
        {
            write!(f, "TlsConfig({} peers in the roster)", self.roster().len())
        }
        #[cfg(not(feature = "tls"))]
        {
            let _ = f;
            match self.never {}
        }
    }
}

/// The message a build with no TLS gives when it is asked for some.
///
/// Not "unknown option", and not silence. The flag is real, the operator spelled it correctly,
/// and the thing that is wrong is the binary - so the binary is what the message is about.
pub const NO_TLS_IN_THIS_BUILD: &str =
    "this build has no TLS: it was compiled with the `tls` feature off. Rebuild with \
     `--features tls`, or terminate TLS at a reverse proxy and pass --insecure-no-tls";

impl TlsConfig {
    /// The certificate chain this listener presents, its key, and optionally the CA that a
    /// peer's client certificate must chain to.
    ///
    /// `roster` is the node names a client certificate may name; an empty roster means no peer
    /// can authenticate, which is exactly what a solo node wants.
    ///
    /// The key file is held to the same rule as the users file - see [`crate::mode`]. A private
    /// key anyone can read is not a private key, and there is no reason for that to be two
    /// policies.
    pub fn load(
        cert: &Path,
        key: &Path,
        peer_ca: Option<&Path>,
        roster: Vec<String>,
    ) -> io::Result<Self> {
        crate::mode::check_permissions(key, "a private key")?;
        #[cfg(not(feature = "tls"))]
        {
            let _ = (cert, peer_ca, roster);
            Err(io::Error::new(io::ErrorKind::Unsupported, NO_TLS_IN_THIS_BUILD))
        }
        #[cfg(feature = "tls")]
        {
            let chain = super::tls::certificates(cert)?;
            let key = super::tls::private_key(key)?;
            let builder = rustls::ServerConfig::builder_with_provider(super::tls::provider())
                .with_protocol_versions(super::tls::VERSIONS)
                .map_err(super::tls::bad)?;
            let builder = match peer_ca {
                None => builder.with_no_client_auth(),
                Some(ca) => builder.with_client_cert_verifier(super::tls::peer_verifier(ca)?),
            };
            let server = builder.with_single_cert(chain, key).map_err(super::tls::bad)?;
            Ok(Self {
                inner: Arc::new(ServerInner {
                    server: Arc::new(server),
                    roster: std::sync::RwLock::new(roster),
                }),
            })
        }
    }

    /// Whether this listener will look at a client certificate at all.
    pub fn checks_peers(&self) -> bool {
        #[cfg(feature = "tls")]
        {
            !self.roster().is_empty()
        }
        #[cfg(not(feature = "tls"))]
        match self.never {}
    }

    /// Replaces the names a peer certificate may claim.
    ///
    /// **Called when the agreement says the cluster has changed.** Without it a node that
    /// joined could never connect: its certificate is signed by the right CA and names a node
    /// this listener has never heard of, which is exactly what the roster refuses.
    ///
    /// A node that has *left* is dropped from the roster by the same call, which is what makes
    /// removing one mean anything - a certificate is not revoked by editing a file nobody
    /// re-reads.
    pub fn set_roster(&self, names: Vec<String>) {
        #[cfg(feature = "tls")]
        {
            *self.inner.roster.write().expect("no panic holds this lock") = names;
        }
        #[cfg(not(feature = "tls"))]
        {
            let _ = names;
            match self.never {}
        }
    }

    #[cfg(feature = "tls")]
    pub(crate) fn server(&self) -> Arc<rustls::ServerConfig> {
        Arc::clone(&self.inner.server)
    }

    #[cfg(feature = "tls")]
    pub(crate) fn roster(&self) -> Vec<String> {
        self.inner.roster.read().expect("no panic holds this lock").clone()
    }
}

/// What an outbound connection trusts, and what it presents when asked.
///
/// One of these is shared by every peer a node talks to: one configuration, and - the part that
/// matters for the fan-out - one TLS session cache, so a reconnect to any peer resumes rather
/// than handshaking from nothing.
#[derive(Clone)]
pub struct ClientTls {
    #[cfg(feature = "tls")]
    inner: Arc<rustls::ClientConfig>,
    #[cfg(not(feature = "tls"))]
    never: core::convert::Infallible,
}

impl core::fmt::Debug for ClientTls {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        #[cfg(feature = "tls")]
        {
            f.write_str("ClientTls(..)")
        }
        #[cfg(not(feature = "tls"))]
        {
            let _ = f;
            match self.never {}
        }
    }
}

impl ClientTls {
    /// Trust `ca`, and present `identity` - a certificate chain and its key - when a server asks
    /// for one.
    ///
    /// `ca` of `None` means the trust roots are empty, which fails every verification. That is
    /// deliberate: there is no platform root store in this build, because a database peer signed
    /// by a public CA is not a peer, it is anybody.
    pub fn new(ca: Option<&Path>, identity: Option<(&Path, &Path)>) -> io::Result<Self> {
        if let Some((_, key)) = identity {
            crate::mode::check_permissions(key, "a private key")?;
        }
        #[cfg(not(feature = "tls"))]
        {
            let _ = ca;
            Err(io::Error::new(io::ErrorKind::Unsupported, NO_TLS_IN_THIS_BUILD))
        }
        #[cfg(feature = "tls")]
        {
            let builder = rustls::ClientConfig::builder_with_provider(super::tls::provider())
                .with_protocol_versions(super::tls::VERSIONS)
                .map_err(super::tls::bad)?
                .with_root_certificates(super::tls::roots(ca)?);
            let config = match identity {
                None => builder.with_no_client_auth(),
                Some((cert, key)) => builder
                    .with_client_auth_cert(
                        super::tls::certificates(cert)?,
                        super::tls::private_key(key)?,
                    )
                    .map_err(super::tls::bad)?,
            };
            Ok(Self { inner: Arc::new(config) })
        }
    }

    /// Verify nothing about the server's certificate.
    ///
    /// Exists because self-signed certificates during bring-up are real, and an operator who
    /// cannot get past that will reach for something worse. Every caller of this prints a line
    /// saying what it means, on every run.
    pub fn insecure(identity: Option<(&Path, &Path)>) -> io::Result<Self> {
        #[cfg(not(feature = "tls"))]
        {
            let _ = identity;
            Err(io::Error::new(io::ErrorKind::Unsupported, NO_TLS_IN_THIS_BUILD))
        }
        #[cfg(feature = "tls")]
        {
            let builder = rustls::ClientConfig::builder_with_provider(super::tls::provider())
                .with_protocol_versions(super::tls::VERSIONS)
                .map_err(super::tls::bad)?
                .dangerous()
                .with_custom_certificate_verifier(super::tls::trust_anything());
            let config = match identity {
                None => builder.with_no_client_auth(),
                Some((cert, key)) => builder
                    .with_client_auth_cert(
                        super::tls::certificates(cert)?,
                        super::tls::private_key(key)?,
                    )
                    .map_err(super::tls::bad)?,
            };
            Ok(Self { inner: Arc::new(config) })
        }
    }

    #[cfg(feature = "tls")]
    pub(crate) fn config(&self) -> Arc<rustls::ClientConfig> {
        Arc::clone(&self.inner)
    }
}
