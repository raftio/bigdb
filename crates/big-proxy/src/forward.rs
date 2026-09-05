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

//! One request, from the client's socket to a node and back.
//!
//! The order is the design: **match first, then build, then send.** Nothing that came off the
//! client's socket is used to address the node — the path is rebuilt from the segments that
//! matched a route, the query is rebuilt from the parameters that route declares, and the
//! headers are rebuilt from a two-name allowlist. A request this proxy could not have described
//! in advance is a request it does not make.
//!
//! **Anything a node actually answered passes through untouched**, status and body alike, `401`
//! and `403` included. This proxy adds a status only for things the node did not say. A
//! paraphrase of the daemon's refusal would be a second vocabulary for clients to learn, and a
//! worse one, because it would be written by the process that knows least about why.

use crate::allowlist::{self, Tier};
use crate::headers;
use crate::metrics::Metrics;
use crate::pool::{NoNode, Pool};
use crate::upstream::{ContentType, UpstreamError, UpstreamResponse};
use big_wire::{reason_for, Request, Response};
use std::net::IpAddr;

/// The largest body this proxy will carry, matching the daemon's own ceiling.
///
/// Refusing exactly what the daemon refuses means a client learns one limit rather than two, and
/// learns it a round trip earlier.
pub const MAX_BODY: usize = big_wire::MAX_BODY;

/// What the proxy knows about one hop while it is making it.
pub struct Context<'a> {
    pub client: IpAddr,
    /// The scheme of the leg the client used, not the leg to the node.
    pub proto: &'static str,
    pub request_id: &'a str,
    pub allowed: Tier,
    pub trust_forwarded_for: bool,
    /// `None` in the unit tests, which are about routing rather than about counting.
    pub metrics: Option<&'a Metrics>,
}

/// Forward one already-parsed request to whichever node the pool picks, or explain why not.
pub fn forward(req: &Request, pool: &Pool, cx: &Context<'_>) -> Response {
    let segments = req.segments();
    let Some(hit) = allowlist::match_route(&req.method, &segments, cx.allowed) else {
        // The same sentence the daemon gives an unknown path, and deliberately not a `403`: a
        // refusal that distinguished "not allowed" from "not a route" would answer, for free,
        // the one question somebody probing for the peer surface wants answered.
        if let Some(m) = cx.metrics {
            m.denied();
        }
        return Response::failure(404, "no_such_route", "no such route");
    };

    if req.body.len() > MAX_BODY {
        return Response::failure(
            413,
            "request_too_large",
            &format!("bodies are limited to {MAX_BODY} bytes"),
        );
    }

    let target = hit.target(&allowlist::allowed_query(hit.route, &req.query));

    // **The header block is built per node**, because `Host` names the node it is going to. It
    // is rebuilt on a retry rather than reused, so a second attempt is addressed to the node it
    // is actually being sent to.
    let candidates = pool.candidates();
    let Some(first) = candidates.first().cloned() else {
        if let Some(m) = cx.metrics {
            m.no_upstream_available();
        }
        return no_healthy_upstream();
    };
    let block = headers::upstream_block(
        &req.headers,
        first.up.addr(),
        cx.client,
        cx.proto,
        cx.request_id,
        req.body.len(),
        cx.trust_forwarded_for,
    );

    match pool.send(&req.method, &target, &block, &req.body, hit.route.budget, hit.route.repeatable)
    {
        Ok((answered, answer)) => {
            // Pointer identity, not name: the node that answered is the very object the first
            // candidate was, or it is a different one and this was a retry.
            if !std::sync::Arc::ptr_eq(&answered, &first) {
                if let Some(m) = cx.metrics {
                    m.retried();
                }
            }
            relay(answer)
        }
        Err((NoNode::NoneInRotation, _)) => {
            if let Some(m) = cx.metrics {
                m.no_upstream_available();
            }
            no_healthy_upstream()
        }
        Err((NoNode::AllFailed, e)) => match e {
            Some(e) => refused(&e, first.up.name()),
            None => no_healthy_upstream(),
        },
    }
}

/// Fail closed.
///
/// A node that has said `serving:false` will answer `503` anyway, so sending it work trades a
/// clear refusal for a slower one; a node that is merely *behind* would answer a count that is
/// quietly wrong and cannot be un-sent. Neither is better than saying so.
fn no_healthy_upstream() -> Response {
    Response::failure(
        503,
        "no_healthy_upstream",
        "no node is in rotation. This proxy does not send requests to a node that has said it \
         is not serving",
    )
    .with_header("retry-after", 1)
}

/// Turn what a node said into what the client is told, changing as little as possible.
fn relay(answer: UpstreamResponse) -> Response {
    let body = answer.body;
    let mut out = match answer.content_type {
        ContentType::Json | ContentType::Binary => Response {
            status: answer.status,
            reason: reason_for(answer.status),
            body,
            content_type: answer.content_type.as_str(),
            headers: Vec::new(),
            code: None,
            detail: None,
        },
        ContentType::Prometheus => Response {
            status: answer.status,
            reason: reason_for(answer.status),
            body,
            content_type: ContentType::Prometheus.as_str(),
            headers: Vec::new(),
            code: None,
            detail: None,
        },
    };
    // `www-authenticate` is the one that must survive: a `401` without it is a refusal the
    // client has no way to answer.
    for (name, value) in answer.headers {
        out = out.with_header(&name, value);
    }
    out
}

/// What to tell a client when the node did not answer.
///
/// Every one of these is a thing this proxy observed rather than a thing the cluster said, which
/// is why they carry codes of their own instead of borrowing the daemon's.
fn refused(e: &UpstreamError, node: &str) -> Response {
    match e {
        UpstreamError::Timeout => Response::failure(
            504,
            "upstream_timeout",
            &format!(
                "node {node} did not answer within this route's budget. The work may still be \
                 running there"
            ),
        ),
        UpstreamError::Unreadable(what) => Response::failure(
            502,
            "upstream_unreadable",
            &format!("node {node} answered something this proxy will not relay: {what}"),
        ),
        UpstreamError::NotSent(_) => Response::failure(
            502,
            "upstream_unreachable",
            &format!("node {node} could not be reached; the request was never sent"),
        ),
        UpstreamError::Sent(_) => Response::failure(
            502,
            "upstream_unreachable",
            &format!(
                "node {node} stopped answering after the request was sent; whether it ran is \
                 not known here"
            ),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(method: &str, path: &str, query: &str) -> Request {
        Request {
            method: method.to_string(),
            path: path.to_string(),
            query: query.to_string(),
            body: Vec::new(),
            headers: Vec::new(),
        }
    }

    fn context() -> Context<'static> {
        Context {
            client: "127.0.0.1".parse().unwrap(),
            proto: "http",
            request_id: "test-1",
            allowed: Tier::Ddl,
            trust_forwarded_for: false,
            metrics: None,
        }
    }

    /// Nothing listens on port 1, so this exercises the whole path down to a refused connect
    /// without needing a node.
    fn nowhere() -> Pool {
        Pool::new(
            vec![crate::upstream::Upstream::new("nobody", "127.0.0.1:1")],
            crate::health::Policy::default(),
            2,
        )
    }

    #[test]
    fn an_unknown_route_never_reaches_a_node() {
        let answer = forward(&request("GET", "/nonsense", ""), &nowhere(), &context());
        assert_eq!(answer.status, 404);
        assert_eq!(answer.code, Some("no_such_route"));
    }

    /// The refusal a client gets for the peer surface is the refusal it gets for a typo, and
    /// that is the point: neither one confirms the route exists.
    #[test]
    fn the_peer_surface_is_refused_exactly_like_a_typo() {
        let internal = forward(&request("POST", "/internal/raft", ""), &nowhere(), &context());
        let typo = forward(&request("POST", "/intrenal/raft", ""), &nowhere(), &context());
        assert_eq!(internal.status, typo.status);
        assert_eq!(internal.code, typo.code);
        assert_eq!(internal.body, typo.body);
    }

    #[test]
    fn a_route_above_the_allowed_tier_looks_like_no_route_at_all() {
        let mut cx = context();
        cx.allowed = Tier::Data;
        let answer = forward(&request("GET", "/cluster/topology", ""), &nowhere(), &cx);
        assert_eq!(answer.status, 404);
        assert_eq!(answer.code, Some("no_such_route"));
    }

    #[test]
    fn an_oversized_body_is_refused_before_a_connection_is_opened() {
        let mut req = request("POST", "/table/t/import", "");
        req.body = vec![b'x'; MAX_BODY + 1];
        let answer = forward(&req, &nowhere(), &context());
        assert_eq!(answer.status, 413);
        assert_eq!(answer.code, Some("request_too_large"));
    }

    #[test]
    fn an_unreachable_node_is_a_502_that_says_the_request_was_never_sent() {
        let answer = forward(&request("POST", "/table/t/query", ""), &nowhere(), &context());
        assert_eq!(answer.status, 502);
        assert_eq!(answer.code, Some("upstream_unreachable"));
        let body = String::from_utf8_lossy(&answer.body);
        assert!(body.contains("never sent"), "{body}");
    }

    #[test]
    fn every_refusal_names_the_node() {
        let io = || std::io::Error::from(std::io::ErrorKind::ConnectionReset);
        for e in [
            UpstreamError::NotSent(io()),
            UpstreamError::Sent(io()),
            UpstreamError::Timeout,
            UpstreamError::Unreadable("chunked".to_string()),
        ] {
            let answer = refused(&e, "node-b");
            let body = String::from_utf8_lossy(&answer.body);
            assert!(body.contains("node-b"), "{e} lost the node name: {body}");
            assert!((502..=504).contains(&answer.status), "{e} answered {}", answer.status);
        }
    }

    #[test]
    fn a_node_answer_passes_through_with_its_own_status() {
        for status in [200, 400, 401, 403, 404, 409, 500, 503] {
            let answer = relay(UpstreamResponse {
                status,
                content_type: ContentType::Json,
                body: br#"{"error":"the daemon's own sentence"}"#.to_vec(),
                headers: Vec::new(),
            });
            assert_eq!(answer.status, status);
            assert_eq!(answer.body, br#"{"error":"the daemon's own sentence"}"#);
        }
    }

    #[test]
    fn a_challenge_survives_the_hop() {
        let answer = relay(UpstreamResponse {
            status: 401,
            content_type: ContentType::Json,
            body: b"{}".to_vec(),
            headers: vec![(
                "www-authenticate".to_string(),
                r#"Basic realm="big", charset="UTF-8""#.to_string(),
            )],
        });
        let encoded = String::from_utf8_lossy(&answer.encode(false)).to_string();
        assert!(encoded.contains("www-authenticate: Basic realm=\"big\""), "{encoded}");
    }
}
