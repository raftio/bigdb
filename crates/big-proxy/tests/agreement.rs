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

//! The three things this crate deliberately copies rather than imports, held to the originals.
//!
//! Each copy exists for the same reason: importing it would mean depending on `big-cluster`, and
//! through it on `big-engine`, `big-db`, `big-sql` and argon2 — the whole engine, inside a
//! process that opens no file and verifies no password. `big-wire` was split out to make that
//! claim checkable, and these tests are the other half of the bargain: `big-cluster` is a
//! **dev-dependency**, so the originals are reachable here and nowhere near the shipped binary.
//!
//! A copy nobody checks is a copy that drifts. These fail the moment one does.

use big_proxy::allowlist::Repeatable;
use big_proxy::headers::{CLUSTER_HEADER, WIRE_HEADER};

/// The stamp a node checks before it decodes anything on a `/internal/*` route.
///
/// `headers.rs` strips both from every client request. If the daemon renamed one and this crate
/// went on stripping the old name, a client could set the new one and this proxy would forward
/// it — which is precisely the thing the stripping exists to prevent.
#[test]
fn the_peer_headers_this_proxy_strips_are_the_ones_a_node_checks() {
    assert_eq!(
        WIRE_HEADER,
        big_cluster::WIRE_HEADER,
        "the wire header was renamed and this proxy is still stripping the old name, so a \
         client could now set the new one"
    );
    assert_eq!(
        CLUSTER_HEADER,
        big_cluster::CLUSTER_HEADER,
        "the cluster header was renamed and this proxy is still stripping the old name"
    );
}

/// The retry classification.
///
/// Two variants with the same meanings, so that "repeatable" means one thing in this repository
/// rather than two. The proxy applies it *more* narrowly than `Peer::post` does — it carries a
/// stranger's write rather than its own fan-out leg — but the classification itself is shared.
#[test]
fn repeatable_means_the_same_thing_in_both_crates() {
    use big_cluster::client::Repeatable as Theirs;

    // Exhaustive on both sides: a third variant on either would stop this compiling, which is
    // the point at which somebody has to come and decide what the proxy does with it.
    let pairs = [(Repeatable::Yes, Theirs::Yes), (Repeatable::No, Theirs::No)];
    for (ours, theirs) in pairs {
        let ours_name = match ours {
            Repeatable::Yes => "Yes",
            Repeatable::No => "No",
        };
        let theirs_name = match theirs {
            Theirs::Yes => "Yes",
            Theirs::No => "No",
        };
        assert_eq!(ours_name, theirs_name);
    }
}

/// The ceiling this proxy refuses at is the one the daemon refuses at.
///
/// A proxy that allowed more would accept a body only to have the node reject it a round trip
/// later; one that allowed less would refuse requests the cluster would have taken.
#[test]
fn the_body_ceiling_is_the_daemons() {
    assert_eq!(big_proxy::forward::MAX_BODY, big_http::MAX_BODY);
}

/// And the connection ceilings stay *inside* the daemon's, so this proxy retires a connection
/// before the node decides to close it — the discipline `clients/go/config.go` follows.
#[test]
fn the_connection_ceilings_stay_inside_the_daemons() {
    let daemon = big_http::ServerConfig::default();
    assert!(
        u64::from(big_proxy::upstream::MAX_REQUESTS_PER_CONN)
            < daemon.max_keepalive_requests as u64,
        "the proxy must retire a pooled connection before the node closes it"
    );
    assert!(
        big_proxy::upstream::MAX_IDLE < daemon.keepalive_idle,
        "the proxy must let a pooled connection go idle before the node hangs it up"
    );
}
