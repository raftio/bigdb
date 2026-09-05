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

//! What this proxy counts about itself.
//!
//! `big_http::ServerMetrics` cannot be reused: `render` takes a `&PagerMetrics`, because the
//! daemon's exposition is about a database. The *approach* is reused — atomics rather than a
//! lock, and histogram buckets that are not cumulative until they are summed at render time, so
//! the hot path is one `fetch_add` and never a comparison against a ladder.
//!
//! **These are about the proxy, not about the cluster.** The nodes' own `/metrics` are not
//! forwarded, because a scraper that reached a different node on each scrape would draw a graph
//! whose every point came from somewhere else. Scrape the nodes directly; scrape this for the
//! front door.
//!
//! The two numbers worth an alert are `big_proxy_upstreams_in_rotation` reaching zero, which is
//! an outage, and `big_proxy_route_denied_total` climbing, which is somebody knocking on doors
//! that are not there.

use std::sync::atomic::{AtomicU64, Ordering};

/// Upper bounds in microseconds. The same ladder `big-http` uses, so a latency graph of the
/// proxy and one of the daemon have the same shape and can be read side by side.
const BUCKETS_US: [u64; 8] = [1_000, 5_000, 10_000, 50_000, 100_000, 500_000, 1_000_000, 5_000_000];

#[derive(Default)]
pub struct Metrics {
    pub requests: AtomicU64,
    pub responses_2xx: AtomicU64,
    pub responses_4xx: AtomicU64,
    pub responses_5xx: AtomicU64,
    pub route_denied: AtomicU64,
    pub no_upstream: AtomicU64,
    pub retries: AtomicU64,
    /// Nodes this proxy adopted from the cluster's membership, and nodes it dropped.
    pub discovered: AtomicU64,
    pub undiscovered: AtomicU64,
    pub bytes_in: AtomicU64,
    pub bytes_out: AtomicU64,
    /// Non-cumulative: each request lands in exactly one bucket, plus `overflow`.
    buckets: [AtomicU64; BUCKETS_US.len()],
    overflow: AtomicU64,
    duration_us: AtomicU64,
}

impl Metrics {
    pub fn new() -> Self {
        Self::default()
    }

    /// One finished request.
    pub fn request(
        &self,
        status: u16,
        elapsed: std::time::Duration,
        bytes_in: usize,
        bytes_out: usize,
    ) {
        self.requests.fetch_add(1, Ordering::Relaxed);
        match status {
            200..=299 => &self.responses_2xx,
            400..=499 => &self.responses_4xx,
            500..=599 => &self.responses_5xx,
            // 1xx and 3xx are neither, and this proxy produces neither. Counted in `requests`
            // and in no class, rather than being filed under one they do not belong to.
            _ => return,
        }
        .fetch_add(1, Ordering::Relaxed);

        self.bytes_in.fetch_add(bytes_in as u64, Ordering::Relaxed);
        self.bytes_out.fetch_add(bytes_out as u64, Ordering::Relaxed);

        let us = elapsed.as_micros() as u64;
        self.duration_us.fetch_add(us, Ordering::Relaxed);
        match BUCKETS_US.iter().position(|b| us <= *b) {
            Some(i) => self.buckets[i].fetch_add(1, Ordering::Relaxed),
            None => self.overflow.fetch_add(1, Ordering::Relaxed),
        };
    }

    pub fn denied(&self) {
        self.route_denied.fetch_add(1, Ordering::Relaxed);
    }

    pub fn no_upstream_available(&self) {
        self.no_upstream.fetch_add(1, Ordering::Relaxed);
    }

    pub fn retried(&self) {
        self.retries.fetch_add(1, Ordering::Relaxed);
    }

    pub fn upstream_discovered(&self) {
        self.discovered.fetch_add(1, Ordering::Relaxed);
    }

    pub fn upstream_removed(&self) {
        self.undiscovered.fetch_add(1, Ordering::Relaxed);
    }

    /// Prometheus text, without the per-node gauges — those come from the pool, which knows.
    pub fn render(&self, out: &mut String) {
        let g = |a: &AtomicU64| a.load(Ordering::Relaxed);

        counter(
            out,
            "big_proxy_requests_total",
            "requests this proxy has answered",
            g(&self.requests),
        );
        out.push_str("# HELP big_proxy_responses_total answers by status class\n");
        out.push_str("# TYPE big_proxy_responses_total counter\n");
        for (class, value) in [
            ("2xx", g(&self.responses_2xx)),
            ("4xx", g(&self.responses_4xx)),
            ("5xx", g(&self.responses_5xx)),
        ] {
            out.push_str(&format!("big_proxy_responses_total{{class=\"{class}\"}} {value}\n"));
        }

        counter(
            out,
            "big_proxy_route_denied_total",
            "requests refused because no route in the allowlist matched",
            g(&self.route_denied),
        );
        counter(
            out,
            "big_proxy_no_upstream_total",
            "requests that found no node in rotation",
            g(&self.no_upstream),
        );
        counter(out, "big_proxy_retries_total", "requests sent to a second node", g(&self.retries));
        // Two counters rather than a gauge of the current count: the count is already on
        // `/ready`, and what an operator cannot reconstruct from it is *when the set moved*.
        counter(
            out,
            "big_proxy_upstreams_discovered_total",
            "nodes added to this proxy from the cluster's membership",
            g(&self.discovered),
        );
        counter(
            out,
            "big_proxy_upstreams_removed_total",
            "nodes dropped from this proxy because the cluster no longer holds them",
            g(&self.undiscovered),
        );
        counter(
            out,
            "big_proxy_request_bytes_total",
            "request body bytes forwarded",
            g(&self.bytes_in),
        );
        counter(
            out,
            "big_proxy_response_bytes_total",
            "response body bytes relayed",
            g(&self.bytes_out),
        );

        out.push_str("# HELP big_proxy_request_duration_seconds time from accept to answer\n");
        out.push_str("# TYPE big_proxy_request_duration_seconds histogram\n");
        let mut running = 0u64;
        for (i, bound) in BUCKETS_US.iter().enumerate() {
            running += g(&self.buckets[i]);
            let seconds = *bound as f64 / 1e6;
            out.push_str(&format!(
                "big_proxy_request_duration_seconds_bucket{{le=\"{seconds}\"}} {running}\n"
            ));
        }
        running += g(&self.overflow);
        out.push_str(&format!(
            "big_proxy_request_duration_seconds_bucket{{le=\"+Inf\"}} {running}\n"
        ));
        out.push_str(&format!(
            "big_proxy_request_duration_seconds_sum {}\n",
            g(&self.duration_us) as f64 / 1e6
        ));
        out.push_str(&format!("big_proxy_request_duration_seconds_count {running}\n"));
    }
}

fn counter(out: &mut String, name: &str, help: &str, value: u64) {
    out.push_str(&format!("# HELP {name} {help}\n# TYPE {name} counter\n{name} {value}\n"));
}

/// Escape a label value: a node name comes from a config file, and a `"` or a `\` in one would
/// produce a line no scraper can parse.
pub fn label(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn a_request_lands_in_one_bucket_and_every_bucket_above_it() {
        let m = Metrics::new();
        m.request(200, Duration::from_micros(3_000), 10, 20);
        let mut out = String::new();
        m.render(&mut out);
        // 3ms is over the 1ms bound and under the 5ms one.
        assert!(out.contains("le=\"0.001\"} 0"), "{out}");
        assert!(out.contains("le=\"0.005\"} 1"), "{out}");
        assert!(out.contains("le=\"+Inf\"} 1"), "{out}");
        assert!(out.contains("big_proxy_request_duration_seconds_count 1"), "{out}");
    }

    #[test]
    fn buckets_are_cumulative_by_the_time_a_scraper_sees_them() {
        let m = Metrics::new();
        m.request(200, Duration::from_micros(500), 0, 0);
        m.request(200, Duration::from_micros(2_000_000), 0, 0);
        let mut out = String::new();
        m.render(&mut out);
        assert!(out.contains("le=\"0.001\"} 1"), "{out}");
        assert!(out.contains("le=\"1\"} 1"), "{out}");
        assert!(out.contains("le=\"5\"} 2"), "{out}");
        assert!(out.contains("le=\"+Inf\"} 2"), "{out}");
    }

    #[test]
    fn something_slower_than_the_last_bucket_still_counts() {
        let m = Metrics::new();
        m.request(200, Duration::from_secs(60), 0, 0);
        let mut out = String::new();
        m.render(&mut out);
        assert!(out.contains("le=\"5\"} 0"), "{out}");
        assert!(out.contains("le=\"+Inf\"} 1"), "the overflow was lost: {out}");
    }

    #[test]
    fn statuses_are_counted_by_class() {
        let m = Metrics::new();
        for s in [200, 201, 404, 401, 500, 503, 502] {
            m.request(s, Duration::ZERO, 0, 0);
        }
        let mut out = String::new();
        m.render(&mut out);
        assert!(out.contains(r#"big_proxy_responses_total{class="2xx"} 2"#), "{out}");
        assert!(out.contains(r#"big_proxy_responses_total{class="4xx"} 2"#), "{out}");
        assert!(out.contains(r#"big_proxy_responses_total{class="5xx"} 3"#), "{out}");
        assert!(out.contains("big_proxy_requests_total 7"), "{out}");
    }

    #[test]
    fn every_series_carries_its_help_and_type() {
        let mut out = String::new();
        Metrics::new().render(&mut out);
        let counters = out.lines().filter(|l| l.starts_with("# TYPE")).count();
        let helps = out.lines().filter(|l| l.starts_with("# HELP")).count();
        assert_eq!(counters, helps, "a series without HELP is one a scraper cannot describe");
        assert!(counters >= 8, "{out}");
    }

    /// A node name comes from a file somebody edits.
    #[test]
    fn a_label_cannot_break_out_of_its_quotes() {
        assert_eq!(label(r#"a"b"#), r#"a\"b"#);
        assert_eq!(label(r"a\b"), r"a\\b");
        assert_eq!(label("a\nb"), "a\\nb");
        assert_eq!(label("a-spare"), "a-spare");
    }
}
