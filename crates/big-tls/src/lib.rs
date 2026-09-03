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

//! The socket a request arrives on, plain or TLS, and what it proves about the caller.
//!
//! **Why this is a crate and not a module.** `big-http` serves connections and `big-cluster`
//! opens them, and `big-cluster` sits underneath `big-http` - so it cannot borrow a type defined
//! there. The alternative was the same enum in both, which means the same feature flag, the same
//! PEM loader, the same private-key mode check and the same "is this pooled connection still
//! usable" rule written twice. Those are four things that must not drift, so they are here once.
//!
//! **Why the feature is off by default.** With `tls` off this crate has no dependencies at all,
//! [`Wire`] is a `TcpStream` behind one `match`, and `cargo tree -p big-http
//! --no-default-features` still fits on a few lines. That is a property CI asserts rather than a
//! claim the readme makes. `big-bin` turns the feature on, because a shipped daemon that cannot
//! speak TLS is not much of a daemon.
//!
//! **What changed the project's mind about TLS.** The four module docs that argued against it
//! were arguing against it *for bearer tokens*, and they were right: a token belongs to this
//! database, it is one string, and terminating TLS at a reverse proxy protected it well enough
//! that carrying a TLS stack was the larger cost. Passwords are not like that. A password is a
//! thing a person also uses somewhere else, so sending one in the clear risks something that is
//! not ours to risk - and mutual TLS additionally replaces the cluster's single shared admin
//! secret with a per-node identity, which is a capability the old design could not express at
//! any price. The dependency argument did not lose; the thing being weighed against it changed.

#![deny(unsafe_code)]
#![deny(missing_docs)]

pub mod base64;
pub mod mode;
pub mod pem;

mod config;
mod wire;

#[cfg(feature = "tls")]
mod tls;

pub use config::{ClientTls, Identity, TlsConfig, NO_TLS_IN_THIS_BUILD};
pub use wire::{ClientWire, Wire, WireError};

/// Whether this build can speak TLS at all.
///
/// For the line a daemon prints about itself on the way up. An operator reading a log after an
/// incident should not have to work out which binary they were running from the absence of
/// something.
pub const fn built_with_tls() -> bool {
    cfg!(feature = "tls")
}
