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

//! Everything that touches rustls, in one file behind one `#[cfg]`.
//!
//! The rest of the crate names these functions and never names a rustls type, so that turning
//! the feature off deletes this module and nothing else has to know.

use crate::pem;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use std::io;
use std::path::Path;
use std::sync::{Arc, OnceLock};

/// TLS 1.3 and nothing else.
///
/// Both ends of every connection here are ours or a current curl, and a protocol version that is
/// not compiled in is a protocol version that cannot be downgraded to. The cost is clients older
/// than roughly 2017, which for a database port is the right side of the trade.
pub(crate) const VERSIONS: &[&rustls::SupportedProtocolVersion] = &[&rustls::version::TLS13];

/// The one provider this process uses.
///
/// Deliberately **not** `CryptoProvider::install_default()`. That is a process-global singleton
/// which errors on a second call, so two tests in one binary - or a library embedded twice -
/// would fight over it. Passing the provider explicitly costs one `Arc` clone and has no such
/// failure mode.
pub(crate) fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    static PROVIDER: OnceLock<Arc<rustls::crypto::CryptoProvider>> = OnceLock::new();
    Arc::clone(PROVIDER.get_or_init(|| Arc::new(rustls::crypto::ring::default_provider())))
}

pub(crate) fn bad(e: impl core::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e.to_string())
}

/// Every `CERTIFICATE` block in a PEM file, leaf first, as the chain to present.
pub(crate) fn certificates(path: &Path) -> io::Result<Vec<CertificateDer<'static>>> {
    let blocks = pem::parse_file(path)?;
    let chain: Vec<_> = blocks
        .into_iter()
        .filter(|b| b.label == "CERTIFICATE" || b.label == "X509 CERTIFICATE")
        .map(|b| CertificateDer::from(b.der))
        .collect();
    if chain.is_empty() {
        return Err(bad(format!(
            "{}: no CERTIFICATE block; is this the key file?",
            path.display()
        )));
    }
    Ok(chain)
}

/// The first private key in a PEM file, in whichever of the three spellings it was written.
///
/// All three are accepted because all three are what the tool an operator reached for produces:
/// `PRIVATE KEY` from openssl's modern output and from most CA tooling, `RSA PRIVATE KEY` from
/// its older output, `EC PRIVATE KEY` from `openssl ecparam`. Refusing two of them would be
/// refusing the file that is already on the disk.
pub(crate) fn private_key(path: &Path) -> io::Result<PrivateKeyDer<'static>> {
    for block in pem::parse_file(path)? {
        let key = match block.label.as_str() {
            "PRIVATE KEY" => PrivateKeyDer::try_from(block.der),
            "RSA PRIVATE KEY" => {
                PrivateKeyDer::try_from(block.der).map(|k: PrivateKeyDer<'static>| k)
            }
            "EC PRIVATE KEY" => PrivateKeyDer::try_from(block.der),
            _ => continue,
        };
        return key.map_err(|e| bad(format!("{}: {e}", path.display())));
    }
    Err(bad(format!("{}: no PRIVATE KEY block; is this the certificate file?", path.display())))
}

/// A trust store from one PEM file of CA certificates, or an empty one.
///
/// Empty is not a mistake and is not softened into "use the platform roots". A peer signed by a
/// public CA is not a peer, it is anybody, so the empty store fails closed and says so at the
/// handshake rather than trusting the internet.
pub(crate) fn roots(ca: Option<&Path>) -> io::Result<rustls::RootCertStore> {
    let mut store = rustls::RootCertStore::empty();
    if let Some(path) = ca {
        for cert in certificates(path)? {
            store.add(cert).map_err(|e| bad(format!("{}: {e}", path.display())))?;
        }
    }
    Ok(store)
}

/// The verifier that decides whether a client certificate was signed by the peer CA.
///
/// **Optional, not required.** A listener that required a client certificate would refuse every
/// `bigctl` and every `curl`, which is every client this database has. A connection with no
/// certificate is `Identity::None` and the route table takes it from there.
pub(crate) fn peer_verifier(
    ca: &Path,
) -> io::Result<Arc<dyn rustls::server::danger::ClientCertVerifier>> {
    let store = Arc::new(roots(Some(ca))?);
    rustls::server::WebPkiClientVerifier::builder_with_provider(store, provider())
        .allow_unauthenticated()
        .build()
        .map_err(|e| bad(format!("{}: {e}", ca.display())))
}

/// Which node of the roster this client certificate names, if any.
///
/// **Match, do not parse.** rustls verifies chains; it has no accessor for a subject name, and
/// writing an X.509 name parser to get one would be writing the most notorious parser in
/// security to answer a question that can be asked the other way round. This server already
/// knows every node's name from the cluster file, so it asks webpki whether the presented leaf
/// is valid for each of them and takes the one that matches.
///
/// The chain itself was already verified by [`peer_verifier`] before this runs; this is only the
/// name. A certificate the peer CA signed that matches nobody on the roster returns `None`, and
/// the caller refuses the connection - which is what makes the cluster file the roster.
pub(crate) fn node_identity(
    peer_certificates: Option<&[CertificateDer<'static>]>,
    roster: &[String],
) -> Option<crate::Identity> {
    let leaf = peer_certificates?.first()?;
    let cert = webpki::EndEntityCert::try_from(leaf).ok()?;
    for name in roster {
        let Ok(ServerName::DnsName(dns)) = ServerName::try_from(name.as_str()) else { continue };
        if cert.verify_is_valid_for_subject_name(&ServerName::DnsName(dns)).is_ok() {
            return Some(crate::Identity::Node(name.clone()));
        }
    }
    None
}

/// A short, greppable fingerprint of a certificate, for the log line.
///
/// Eight bytes of the DER's own hash rendered as hex. Not a security boundary - it is so that an
/// operator comparing two nodes can see they were issued different certificates without owning
/// a parser.
pub(crate) fn fingerprint(der: &[u8]) -> String {
    // FNV-1a. A cryptographic hash would be better and is not available without reaching into
    // rustls's internals; what this has to survive is a typo, not an adversary.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in der {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x1000_0000_01b3);
    }
    format!("{h:016x}")
}

/// A server-certificate verifier that verifies nothing.
///
/// Reached only through [`crate::ClientTls::insecure`], whose callers announce it on every run.
pub(crate) fn trust_anything() -> Arc<dyn rustls::client::danger::ServerCertVerifier> {
    Arc::new(TrustAnything(provider()))
}

#[derive(Debug)]
struct TrustAnything(Arc<rustls::crypto::CryptoProvider>);

impl rustls::client::danger::ServerCertVerifier for TrustAnything {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    /// Still checked, even here. Skipping the *name* is what an operator asked for; accepting a
    /// signature that does not verify would mean the handshake proved nothing at all, which is
    /// not the same request and is never what anybody wants.
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}
