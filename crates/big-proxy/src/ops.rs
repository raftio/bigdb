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

//! The three routes this proxy answers about itself, and never forwards.
//!
//! Forwarding `/health` would return one node's `{"status":"ok"}`, chosen at random, which
//! answers neither "is this endpoint usable" nor "is the cluster up". A client pointed here
//! asking `GET /health` is asking the first of those, and this process is the only one that can
//! answer it. It is the same reason the console keeps `/api/health` separate from
//! `/api/bigd/health`: the front door being up and the database being up are different
//! questions, and an orchestrator that restarted one for the other's outage would be applying
//! the wrong cure.
//!
//! `/ready` is a **superset** of the daemon's shape rather than a different one. `status` still
//! reads `ready`, so a client that decodes the daemon's answer draws the right conclusion; the
//! per-node detail arrives in fields it ignores. What is *absent* is as deliberate: `node`,
//! `shards` and `tables` have no single value at this layer, and reporting some node's would be
//! a lie rather than an approximation. Both shipped clients treat every field as optional — the
//! Go one carries `Serving *bool` beside a `HasServing` flag — so absence is a case they were
//! written for.

use crate::metrics::{label, Metrics};
use crate::pool::Pool;
use big_wire::json;
use big_wire::Response;

/// Liveness: can this process answer at all.
///
/// Touches nothing, asks no node. Byte-identical to the daemon's, so a probe written for one
/// works against the other.
pub fn health() -> Response {
    Response::ok("{\"status\":\"ok\"}".to_string())
}

/// Readiness: is there anywhere to send a request.
///
/// `503` when there is not. **It does not probe on the way past** — the answer comes out of what
/// the poller already knows, so a readiness check cannot get slower as the cluster grows, and a
/// probe cannot become the thing that overloads a node.
pub fn ready(pool: &Pool) -> Response {
    let in_rotation = pool.in_rotation();
    let entries: Vec<String> = pool
        .nodes()
        .iter()
        .map(|node| {
            let h = node.health.read().unwrap_or_else(|e| e.into_inner());
            let report = h.report();
            let mut fields = vec![
                format!("\"name\":{}", json::string(node.up.name())),
                format!("\"addr\":{}", json::string(node.up.addr())),
                format!("\"state\":\"{}\"", if h.in_rotation() { "in" } else { "out" }),
                format!("\"in_flight\":{}", node.up.in_flight()),
                format!("\"consecutive_failures\":{}", h.consecutive_failures()),
            ];
            // `null` rather than `false` on a node with no agreement: the daemon sends no
            // `serving` there, and inventing one would report a state that does not exist.
            fields.push(match report.serving {
                Some(s) => format!("\"serving\":{s}"),
                None => "\"serving\":null".to_string(),
            });
            if let Some(n) = &report.node {
                fields.push(format!("\"node\":{}", json::string(n)));
            }
            if let Some(s) = &report.shards {
                fields.push(format!("\"shards\":{}", json::string(s)));
            }
            if let Some(w) = report.wire {
                fields.push(format!("\"wire\":{w}"));
            }
            if let Some(why) = h.why() {
                fields.push(format!("\"why\":{}", json::string(&why.to_string())));
            }
            fields.push(match h.last_ok() {
                Some(t) => format!("\"last_ok_ms\":{}", t.elapsed().as_millis()),
                None => "\"last_ok_ms\":null".to_string(),
            });
            format!("{{{}}}", fields.join(","))
        })
        .collect();

    let body = format!(
        "{{\"status\":{},\"version\":{},\"in_rotation\":{in_rotation},\"total\":{},\
         \"upstreams\":[{}]}}",
        json::string(if in_rotation > 0 { "ready" } else { "unavailable" }),
        json::string(env!("CARGO_PKG_VERSION")),
        pool.nodes().len(),
        entries.join(",")
    );

    if in_rotation == 0 {
        return Response {
            status: 503,
            reason: big_wire::reason_for(503),
            body: body.into_bytes(),
            content_type: "application/json",
            headers: Vec::new(),
            code: Some("no_healthy_upstream"),
            detail: None,
        }
        .with_header("retry-after", 1);
    }
    Response::ok(body)
}

/// Prometheus text about **this proxy**.
///
/// Unauthenticated, unlike the daemon's `/metrics`, which needs `OPERATE`. These counters say
/// how many requests crossed a hop and how long they took; none of them says anything about
/// anyone's data. The difference is worth knowing about, so it is written down here and in
/// `readme.md` rather than left to be discovered.
pub fn metrics(pool: &Pool, m: &Metrics) -> Response {
    let mut out = String::with_capacity(4096);
    m.render(&mut out);

    out.push_str("# HELP big_proxy_upstreams_in_rotation nodes this proxy will send work to\n");
    out.push_str("# TYPE big_proxy_upstreams_in_rotation gauge\n");
    out.push_str(&format!("big_proxy_upstreams_in_rotation {}\n", pool.in_rotation()));

    gauge(&mut out, "big_proxy_upstream_up", "1 when a node is in rotation");
    for node in pool.nodes() {
        let h = node.health.read().unwrap_or_else(|e| e.into_inner());
        out.push_str(&format!(
            "big_proxy_upstream_up{{node=\"{}\"}} {}\n",
            label(node.up.name()),
            u8::from(h.in_rotation())
        ));
    }

    // Three-valued on purpose. `-1` is "this node sends no `serving` field", which is what an
    // unreplicated deployment looks like — and is not the same as `serving:false`.
    gauge(&mut out, "big_proxy_upstream_serving", "1 serving, 0 not, -1 no agreement to report");
    for node in pool.nodes() {
        let h = node.health.read().unwrap_or_else(|e| e.into_inner());
        let value = match h.report().serving {
            Some(true) => "1",
            Some(false) => "0",
            None => "-1",
        };
        out.push_str(&format!(
            "big_proxy_upstream_serving{{node=\"{}\"}} {value}\n",
            label(node.up.name())
        ));
    }

    gauge(&mut out, "big_proxy_upstream_in_flight", "requests in flight to a node");
    for node in pool.nodes() {
        out.push_str(&format!(
            "big_proxy_upstream_in_flight{{node=\"{}\"}} {}\n",
            label(node.up.name()),
            node.up.in_flight()
        ));
    }

    Response::text("text/plain; version=0.0.4; charset=utf-8", out)
}

fn gauge(out: &mut String, name: &str, help: &str) {
    out.push_str(&format!("# HELP {name} {help}\n# TYPE {name} gauge\n"));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::health::{Policy, Verdict, Why};
    use crate::upstream::Upstream;
    use std::time::Duration;

    fn pool(names: &[&str]) -> Pool {
        let ups = names
            .iter()
            .enumerate()
            .map(|(i, n)| Upstream::new(*n, format!("{n}:{}", 7654 + i)))
            .collect();
        Pool::new(ups, Policy { fail: 1, pass: 1, floor: Duration::ZERO }, 2)
    }

    fn body(r: &Response) -> String {
        String::from_utf8_lossy(&r.body).to_string()
    }

    #[test]
    fn health_matches_the_daemons_answer_byte_for_byte() {
        assert_eq!(health().body, br#"{"status":"ok"}"#);
        assert_eq!(health().status, 200);
    }

    /// A client that decodes the daemon's `/ready` reads `status` and gets the right answer.
    #[test]
    fn ready_keeps_the_field_a_client_decodes() {
        let b = body(&ready(&pool(&["a"])));
        assert!(b.starts_with(r#"{"status":"ready""#), "{b}");
    }

    #[test]
    fn ready_reports_every_node_and_counts_the_live_ones() {
        let p = pool(&["a", "b", "a-spare"]);
        p.observe(2, Verdict::Down(Why::NotServing));
        let b = body(&ready(&p));
        assert!(b.contains(r#""in_rotation":2"#), "{b}");
        assert!(b.contains(r#""total":3"#), "{b}");
        assert!(b.contains(r#""name":"a-spare""#), "{b}");
        assert!(b.contains(r#""state":"out""#), "{b}");
        assert!(b.contains(r#""why":"not_serving""#), "{b}");
    }

    /// The distinction the whole poller is built around, carried all the way to the wire.
    #[test]
    fn a_node_with_no_agreement_reports_serving_null_not_false() {
        let p = pool(&["solo"]);
        let solo = r#"{"status":"ready","tables":2,"node":"local","shards":"0..","version":"0.1.0","wire":6}"#;
        p.observe(0, Verdict::of(200, solo.as_bytes()));
        let b = body(&ready(&p));
        assert!(b.contains(r#""serving":null"#), "absent must not become false: {b}");
        assert!(b.contains(r#""state":"in""#), "{b}");
    }

    /// A proxy with nowhere to send a request is not ready, however alive the process is.
    #[test]
    fn no_node_in_rotation_is_a_503_not_an_empty_success() {
        let p = pool(&["a"]);
        p.observe(0, Verdict::Down(Why::NotServing));
        let answer = ready(&p);
        assert_eq!(answer.status, 503);
        assert_eq!(answer.code, Some("no_healthy_upstream"));
        let encoded = String::from_utf8_lossy(&answer.encode(false)).to_string();
        assert!(encoded.contains("retry-after: 1"), "{encoded}");
    }

    /// Liveness and readiness must not fail together, or an orchestrator restarts a healthy
    /// proxy for the cluster's outage.
    #[test]
    fn health_is_still_ok_when_ready_is_not() {
        let p = pool(&["a"]);
        p.observe(0, Verdict::Down(Why::NotServing));
        assert_eq!(ready(&p).status, 503);
        assert_eq!(health().status, 200);
    }

    #[test]
    fn metrics_carry_the_content_type_a_scraper_checks() {
        let answer = metrics(&pool(&["a"]), &Metrics::new());
        assert_eq!(answer.content_type, "text/plain; version=0.0.4; charset=utf-8");
        assert_eq!(answer.status, 200);
    }

    #[test]
    fn metrics_report_serving_three_ways() {
        let p = pool(&["clustered", "solo", "stopped"]);
        let up = r#"{"status":"ready","node":"a","shards":"0..64","wire":6,"serving":true}"#;
        let solo = r#"{"status":"ready","node":"local","shards":"0..","wire":6}"#;
        let stopped = r#"{"status":"ready","node":"c","shards":"","wire":6,"serving":false}"#;
        p.observe(0, Verdict::of(200, up.as_bytes()));
        p.observe(1, Verdict::of(200, solo.as_bytes()));
        p.observe(2, Verdict::of(200, stopped.as_bytes()));

        let text = body(&metrics(&p, &Metrics::new()));
        assert!(text.contains(r#"big_proxy_upstream_serving{node="clustered"} 1"#), "{text}");
        assert!(text.contains(r#"big_proxy_upstream_serving{node="solo"} -1"#), "{text}");
        assert!(text.contains(r#"big_proxy_upstream_serving{node="stopped"} 0"#), "{text}");
        assert!(text.contains("big_proxy_upstreams_in_rotation 2"), "{text}");
    }

    /// The number worth alerting on.
    #[test]
    fn in_rotation_reaches_zero_when_everything_stops_serving() {
        let p = pool(&["a", "b"]);
        p.observe(0, Verdict::Down(Why::NotServing));
        p.observe(1, Verdict::Down(Why::NotServing));
        let text = body(&metrics(&p, &Metrics::new()));
        assert!(text.contains("big_proxy_upstreams_in_rotation 0"), "{text}");
    }
}
