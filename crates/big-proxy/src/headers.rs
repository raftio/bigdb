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

//! Which headers cross the hop, in each direction.
//!
//! **The forward set is an allowlist, like the route table and for the same reason.** The daemon
//! reads exactly four header names — `content-length`, `authorization`, `connection`, and the two
//! cluster stamps — so anything else a client sends is surface with no destination. Dropping it
//! costs nothing and means a header the daemon grows a meaning for tomorrow cannot already be
//! arriving from strangers today.
//!
//! Two headers are stripped with more force than the rest: [`WIRE_HEADER`] and
//! [`CLUSTER_HEADER`] are the stamp a node checks before it will decode a peer message. Forwarding a client-supplied one would be this process vouching for a claim it has
//! no way to check. They are removed unconditionally, on every route, including the ones where
//! it obviously cannot matter — a rule with an exception is a rule somebody has to reason about.

use std::net::IpAddr;

/// The header carrying a peer's wire version, spelled the same as `WIRE_HEADER`.
///
/// Two string constants rather than a dependency on `big-cluster`, which would drag the whole
/// engine into a process that touches no data — see `big-wire`. `tests/agreement.rs` asserts
/// these still match the names a node actually checks, so the duplicate cannot drift silently.
pub const WIRE_HEADER: &str = "x-big-wire";

/// The header carrying a peer's cluster fingerprint, spelled the same as
/// `CLUSTER_HEADER`.
pub const CLUSTER_HEADER: &str = "x-big-cluster";

/// Headers that describe *this* connection rather than the message, per RFC 9110 §7.6.1.
///
/// `Connection` is here twice over: it is hop-by-hop itself, and it *names* others that are.
/// [`is_hop_by_hop`] handles the second half.
const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// The only client headers that reach a node.
///
/// `authorization` is the entire authentication story: forwarded byte for byte, never parsed
/// here, never logged, never cached. The daemon stays the only process that has seen a password.
const FORWARD: &[&str] = &["authorization", "content-type"];

/// Response headers worth carrying back.
///
/// `www-authenticate` is not optional: without it a `401` is a refusal a client cannot act on.
const KEEP_FROM_UPSTREAM: &[&str] = &["www-authenticate", "retry-after"];

/// Whether this header belongs to the connection rather than the message.
///
/// `connection_value` is the request's own `Connection` header, whose listed field names are
/// hop-by-hop for this hop even though they are not on the fixed list.
pub fn is_hop_by_hop(name: &str, connection_value: Option<&str>) -> bool {
    if HOP_BY_HOP.contains(&name) {
        return true;
    }
    connection_value
        .is_some_and(|v| v.split(',').any(|token| token.trim().eq_ignore_ascii_case(name)))
}

/// Whether a header the client sent is one this proxy carries upstream.
pub fn forwards(name: &str, connection_value: Option<&str>) -> bool {
    FORWARD.contains(&name) && !is_hop_by_hop(name, connection_value)
}

/// Whether a header the node sent is one this proxy carries back to the client.
pub fn keeps_from_upstream(name: &str) -> bool {
    KEEP_FROM_UPSTREAM.contains(&name)
}

/// What this proxy calls itself in a `Via` header.
pub const VIA: &str = "1.1 bigproxy";

/// The header block for one upstream request, already `\r\n`-terminated.
///
/// `client` is the peer address of the downstream connection. `forwarded_for` is what the client
/// claimed, and is only consulted when an operator has said to trust it: without
/// `--trust-forwarded-for` the value is **rebuilt**, not extended, because a client that can
/// append to it is a client choosing what the logs say.
#[allow(clippy::too_many_arguments)]
pub fn upstream_block(
    request: &[(String, String)],
    host: &str,
    client: IpAddr,
    proto: &str,
    request_id: &str,
    body_len: usize,
    trust_forwarded_for: bool,
) -> String {
    let connection = value_of(request, "connection");
    let mut out = String::with_capacity(256);

    out.push_str(&format!("Host: {host}\r\n"));
    out.push_str(&format!("Content-Length: {body_len}\r\n"));
    out.push_str("Connection: keep-alive\r\n");
    out.push_str(&format!("Via: {VIA}\r\n"));
    out.push_str(&format!("X-Request-Id: {}\r\n", sanitise(request_id)));
    out.push_str(&format!("X-Forwarded-Proto: {proto}\r\n"));

    let forwarded_for = match trust_forwarded_for.then(|| value_of(request, "x-forwarded-for")) {
        Some(Some(claimed)) => format!("{}, {client}", sanitise(&claimed)),
        _ => client.to_string(),
    };
    out.push_str(&format!("X-Forwarded-For: {forwarded_for}\r\n"));

    for (name, value) in request {
        if forwards(name, connection.as_deref()) {
            out.push_str(&format!("{name}: {}\r\n", sanitise(value)));
        }
    }
    out
}

fn value_of(headers: &[(String, String)], name: &str) -> Option<String> {
    // The **first** occurrence, matching `Request::header`. A duplicate `Authorization` must not
    // let a client show this proxy one credential and the daemon another.
    headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.clone())
}

/// Strip anything that could end a header line early.
///
/// `Response::encode` already does this on the way back, so this is the same guard facing the
/// other way: a `\r` in a value a client controls is a request-splitting attempt, and a value
/// that arrives with one is a value whose sender was not being honest.
fn sanitise(value: &str) -> String {
    value.chars().filter(|c| *c != '\r' && *c != '\n' && *c != '\0').collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(n, v)| (n.to_string(), v.to_string())).collect()
    }

    fn block(headers: &[(&str, &str)]) -> String {
        upstream_block(
            &h(headers),
            "node-a:7654",
            "10.0.0.7".parse().unwrap(),
            "http",
            "req-1",
            0,
            false,
        )
    }

    #[test]
    fn authorization_crosses_untouched() {
        let out = block(&[("authorization", "Basic b3BzOmh1bnRlcjI=")]);
        assert!(out.contains("authorization: Basic b3BzOmh1bnRlcjI=\r\n"), "{out}");
    }

    /// The rule that keeps `/internal/*` closed at the message as well as at the route table.
    #[test]
    fn a_client_cannot_forge_a_peer_stamp() {
        let out = block(&[
            (WIRE_HEADER, "6"),
            (CLUSTER_HEADER, "deadbeef"),
            ("authorization", "Basic x"),
        ]);
        assert!(!out.to_lowercase().contains(WIRE_HEADER), "{out}");
        assert!(!out.to_lowercase().contains(CLUSTER_HEADER), "{out}");
        assert!(out.contains("authorization: Basic x"), "the rest still goes: {out}");
    }

    #[test]
    fn hop_by_hop_headers_do_not_cross() {
        let out = block(&[("te", "trailers"), ("upgrade", "websocket"), ("keep-alive", "600")]);
        assert!(!out.contains("te: trailers"), "{out}");
        assert!(!out.contains("upgrade:"), "{out}");
        assert!(!out.contains("keep-alive: 600"), "{out}");
    }

    /// `Connection` does not only describe itself; it names other headers as hop-by-hop.
    #[test]
    fn connection_names_more_hop_by_hop_headers() {
        assert!(is_hop_by_hop("x-secret", Some("keep-alive, X-Secret")));
        assert!(!is_hop_by_hop("x-secret", Some("keep-alive")));
        assert!(!is_hop_by_hop("x-secret", None));
    }

    /// Everything outside the forward set is dropped, not carried.
    #[test]
    fn unlisted_client_headers_are_dropped() {
        let out = block(&[
            ("cookie", "session=abc"),
            ("user-agent", "curl/8"),
            ("referer", "http://elsewhere"),
            ("origin", "http://elsewhere"),
            ("accept", "application/json"),
        ]);
        for absent in ["cookie", "user-agent", "referer", "origin", "accept:"] {
            assert!(!out.contains(absent), "{absent} crossed: {out}");
        }
    }

    #[test]
    fn forwarded_for_is_rebuilt_not_appended() {
        let out = block(&[("x-forwarded-for", "1.2.3.4")]);
        assert!(out.contains("X-Forwarded-For: 10.0.0.7\r\n"), "{out}");
        assert!(!out.contains("1.2.3.4"), "the client chose what the log says: {out}");
    }

    #[test]
    fn forwarded_for_is_appended_when_an_operator_asks() {
        let out = upstream_block(
            &h(&[("x-forwarded-for", "1.2.3.4")]),
            "node-a:7654",
            "10.0.0.7".parse().unwrap(),
            "https",
            "req-1",
            0,
            true,
        );
        assert!(out.contains("X-Forwarded-For: 1.2.3.4, 10.0.0.7\r\n"), "{out}");
    }

    /// A value cannot end its own line and start a header of its own.
    ///
    /// `Request::read` splits on `\r\n` before this runs, so a client cannot actually deliver
    /// such a value through the parser — this guards the values the proxy composes itself, and
    /// costs one filter to keep true for both.
    #[test]
    fn a_header_value_cannot_start_a_header_of_its_own() {
        let out = block(&[("authorization", "Basic x\r\nX-Big-Wire: 6")]);
        let starts_a_line =
            out.split("\r\n").any(|line| line.to_lowercase().starts_with(WIRE_HEADER));
        assert!(!starts_a_line, "request splitting: {out}");
        // Folded onto the one line it belongs to, where the daemon reads it as a credential that
        // does not verify — an honest `401` rather than a header nobody sent.
        assert!(out.contains("authorization: Basic xX-Big-Wire: 6\r\n"), "{out}");
    }

    #[test]
    fn content_length_is_the_proxys_own_count() {
        let out = upstream_block(
            &h(&[("content-length", "999999")]),
            "n:1",
            "127.0.0.1".parse().unwrap(),
            "http",
            "r",
            42,
            false,
        );
        assert!(out.contains("Content-Length: 42\r\n"), "{out}");
        assert!(!out.contains("999999"), "the client's count crossed: {out}");
    }

    #[test]
    fn only_the_two_response_headers_come_back() {
        assert!(keeps_from_upstream("www-authenticate"));
        assert!(keeps_from_upstream("retry-after"));
        assert!(!keeps_from_upstream("content-length"));
        assert!(!keeps_from_upstream("transfer-encoding"));
        assert!(!keeps_from_upstream("connection"));
    }
}
