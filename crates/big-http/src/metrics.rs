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

//! What the server counts about itself, and the Prometheus text format that gets it out.
//!
//! The pager has collected free pages, the reclaim horizon and the live reader count since it
//! was written ([`big_pager::Metrics`]) and nothing ever read them out, which made them a
//! debugger's convenience rather than an operator's instrument. This module is the reading-out
//! half, plus the counters the pager could not have: it does not know what a request is.
//!
//! **No client library.** The exposition format is a line per sample; a crate to write it
//! would be a dependency carried for one function, which is the same argument that keeps the
//! JSON writer hand-written.
//!
//! **Statuses are bucketed by class, not by code.** `2xx/4xx/5xx` answers every question an
//! alert asks ("is it serving", "are clients wrong", "are we broken") and cannot grow a label
//! per status that some future route invents.

use big_pager::{IoStats, Metrics as PagerMetrics};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Upper bounds of the latency histogram, in microseconds.
///
/// Spread over four orders of magnitude because that is the real range: a `/health` is tens of
/// microseconds and a wide scan is seconds, and a histogram that cannot separate them cannot
/// be used to set a timeout.
const BUCKETS_US: [u64; 8] = [1_000, 5_000, 10_000, 50_000, 100_000, 500_000, 1_000_000, 5_000_000];

/// The server's own counters - the half of `/metrics` that is about the edge rather than the
/// engine. Atomics rather than a lock, because every request touches several of them and none
/// of them has to agree with any other at any instant.
#[derive(Default)]
pub struct ServerMetrics {
    connections_accepted: AtomicU64,
    connections_rejected: AtomicU64,
    requests_total: AtomicU64,
    responses_2xx: AtomicU64,
    responses_4xx: AtomicU64,
    responses_5xx: AtomicU64,
    request_bytes: AtomicU64,
    response_bytes: AtomicU64,
    queries_timed_out: AtomicU64,
    queries_cancelled: AtomicU64,
    unauthorized: AtomicU64,
    /// Non-cumulative: each slot counts the requests that fell in that band. The exposition
    /// format wants them cumulative, and `render` is where that sum happens - keeping them
    /// separate here means recording a request touches exactly one counter.
    duration_buckets: [AtomicU64; BUCKETS_US.len()],
    duration_overflow: AtomicU64,
    duration_sum_us: AtomicU64,
}

impl ServerMetrics {
    /// All counters at zero.
    pub fn new() -> Self {
        Self::default()
    }

    /// One connection taken off the listener.
    pub fn connection_accepted(&self) {
        self.connections_accepted.fetch_add(1, Ordering::Relaxed);
    }

    /// A connection turned away because every worker was busy and the queue was full.
    ///
    /// The single most important number here: it is the one that says the cap is the thing
    /// limiting throughput, rather than the engine.
    pub fn connection_rejected(&self) {
        self.connections_rejected.fetch_add(1, Ordering::Relaxed);
    }

    /// One query that hit its wall-clock budget.
    pub fn query_timed_out(&self) {
        self.queries_timed_out.fetch_add(1, Ordering::Relaxed);
    }

    /// One query abandoned because the client hung up.
    pub fn query_cancelled(&self) {
        self.queries_cancelled.fetch_add(1, Ordering::Relaxed);
    }

    /// One request refused with a `401` or a `403`.
    pub fn request_unauthorized(&self) {
        self.unauthorized.fetch_add(1, Ordering::Relaxed);
    }

    /// One completed request: its status class, its latency bucket, and the bytes each way.
    pub fn request(&self, status: u16, elapsed: Duration, bytes_in: usize, bytes_out: usize) {
        self.requests_total.fetch_add(1, Ordering::Relaxed);
        match status {
            200..=399 => &self.responses_2xx,
            400..=499 => &self.responses_4xx,
            _ => &self.responses_5xx,
        }
        .fetch_add(1, Ordering::Relaxed);

        self.request_bytes.fetch_add(bytes_in as u64, Ordering::Relaxed);
        self.response_bytes.fetch_add(bytes_out as u64, Ordering::Relaxed);

        let us = elapsed.as_micros().min(u64::MAX as u128) as u64;
        self.duration_sum_us.fetch_add(us, Ordering::Relaxed);
        match BUCKETS_US.iter().position(|b| us <= *b) {
            Some(i) => &self.duration_buckets[i],
            None => &self.duration_overflow,
        }
        .fetch_add(1, Ordering::Relaxed);
    }

    /// The whole exposition, server counters and pager gauges together.
    ///
    /// One scrape rather than two endpoints because they are read together: "the file is
    /// growing" and "requests are slow" are usually the same incident.
    pub fn render(&self, pager: &PagerMetrics) -> String {
        let mut out = String::with_capacity(2048);
        let g = |o: &AtomicU64| o.load(Ordering::Relaxed);

        counter(
            &mut out,
            "big_http_connections_accepted_total",
            "Connections accepted by the listener.",
            g(&self.connections_accepted),
        );
        counter(
            &mut out,
            "big_http_connections_rejected_total",
            "Connections shed because every worker was busy and the queue was full.",
            g(&self.connections_rejected),
        );
        counter(
            &mut out,
            "big_http_requests_total",
            "Requests that produced a response.",
            g(&self.requests_total),
        );
        counter(
            &mut out,
            "big_http_request_bytes_total",
            "Request body bytes read.",
            g(&self.request_bytes),
        );
        counter(
            &mut out,
            "big_http_response_bytes_total",
            "Response body bytes written.",
            g(&self.response_bytes),
        );
        counter(
            &mut out,
            "big_http_queries_timed_out_total",
            "Queries abandoned because they passed their deadline.",
            g(&self.queries_timed_out),
        );
        counter(
            &mut out,
            "big_http_queries_cancelled_total",
            "Queries abandoned because the client went away.",
            g(&self.queries_cancelled),
        );
        counter(
            &mut out,
            "big_http_unauthorized_total",
            "Requests refused for a missing or insufficient credential.",
            g(&self.unauthorized),
        );

        out.push_str("# HELP big_http_responses_total Responses by status class.\n");
        out.push_str("# TYPE big_http_responses_total counter\n");
        for (class, v) in [
            ("2xx", g(&self.responses_2xx)),
            ("4xx", g(&self.responses_4xx)),
            ("5xx", g(&self.responses_5xx)),
        ] {
            out.push_str(&format!("big_http_responses_total{{class=\"{class}\"}} {v}\n"));
        }

        // Buckets are cumulative in this format: each `le` counts everything at or below it.
        out.push_str("# HELP big_http_request_duration_seconds Time to produce a response.\n");
        out.push_str("# TYPE big_http_request_duration_seconds histogram\n");
        let mut running = 0u64;
        for (i, bound) in BUCKETS_US.iter().enumerate() {
            running += g(&self.duration_buckets[i]);
            let le = *bound as f64 / 1e6;
            out.push_str(&format!(
                "big_http_request_duration_seconds_bucket{{le=\"{le}\"}} {running}\n"
            ));
        }
        running += g(&self.duration_overflow);
        out.push_str(&format!(
            "big_http_request_duration_seconds_bucket{{le=\"+Inf\"}} {running}\n"
        ));
        out.push_str(&format!(
            "big_http_request_duration_seconds_sum {}\n",
            g(&self.duration_sum_us) as f64 / 1e6
        ));
        out.push_str(&format!("big_http_request_duration_seconds_count {running}\n"));

        render_pager(&mut out, pager);
        out
    }
}

/// The gauges the pager has always collected, finally readable from outside the process.
fn render_pager(out: &mut String, m: &PagerMetrics) {
    gauge(out, "big_page_count", "Pages in the file, live and free.", m.page_count);
    gauge(
        out,
        "big_free_pages_reusable",
        "Free pages the next allocation may take. Falling to zero while the file grows is the \
         signal that something is holding the reclaim horizon.",
        m.free_pages_reusable,
    );
    gauge(
        out,
        "big_pages_pending_reclaim_reader",
        "Pages that are free but pinned by a live reader. Climbing means a long-running query.",
        m.pages_pending_reclaim_reader,
    );
    gauge(
        out,
        "big_pages_pending_reclaim_retention",
        "Pages that are free but pinned by a snapshot. Kept apart from the reader count \
         because they call for a different fix.",
        m.pages_pending_reclaim_retention,
    );
    gauge(out, "big_live_readers", "Open read transactions.", m.live_readers as u64);
    gauge(out, "big_snapshots", "Retained snapshots.", m.snapshots as u64);
    gauge(out, "big_fragments", "Fragments the catalog holds a root for.", m.fragments as u64);
    gauge(
        out,
        "big_txn_id",
        "The committed transaction id. Monotonic; its rate is the write rate.",
        m.txn_id,
    );

    // One series per level, exactly one of them 1. The alternative - a single gauge holding
    // 0, 1 or 2 - would need whoever reads it to remember which number meant which, and the
    // number an alert wants to fire on is "not full", which is a label match rather than a
    // comparison.
    out.push_str(
        "# HELP big_durability What a commit promises before reporting success.\n         # TYPE big_durability gauge\n",
    );
    for level in ["full", "barrier", "none"] {
        let on = u8::from(level == m.durability.label());
        out.push_str(&format!("big_durability{{level=\"{level}\"}} {on}\n"));
    }

    // Emitted only when a reader exists. Zero is a real transaction id, so publishing zero for
    // "nobody is reading" would be publishing a wrong answer rather than no answer.
    if let Some(id) = m.oldest_reader_txn_id {
        gauge(out, "big_oldest_reader_txn_id",
            "The transaction id the oldest live reader is pinned to. Absent when no reader is open.",
            id);
    }

    // Omitted entirely when the backend keeps no count, for the same reason as the line above:
    // a backend that does not measure and a backend that did no I/O are different facts, and a
    // block of zeroes says the second when it means the first.
    if let Some(io) = &m.io {
        render_io(out, io);
    }
}

/// What the storage backend did to the disk. Counters, not gauges: every one of these is read
/// as a rate, and the panel an operator wants is pages-per-second next to flushes-per-second.
///
/// **Everything here carries a `backend` label**, and it costs no cardinality: one process
/// opens one backend, so the label is fixed for its lifetime. It is here because these numbers
/// only mean something once you know what produced them - `reads` on a mapped backend counts
/// what the engine asked for and says nothing about what reached the disk, and a second backend
/// would answer the same series a different way.
fn render_io(out: &mut String, io: &IoStats) {
    let b = io.backend;
    io_counter(
        out,
        "big_storage_reads_total",
        b,
        "Pages the storage backend handed to the engine since it was opened. Pages asked for, \
         not disk reads: on a mapped backend the read that reaches the disk is a page fault \
         this process is never told about. Against the write rate it is the read/write mix.",
        io.reads,
    );
    io_counter(
        out,
        "big_storage_writes_total",
        b,
        "Pages written. Divided by the rate of big_txn_id this is write amplification: pages \
         per commit, which under copy-on-write is the number that decides how fast the file \
         grows.",
        io.writes,
    );
    io_counter(out, "big_storage_write_bytes_total", b, "Bytes written.", io.write_bytes);
    io_counter(
        out,
        "big_storage_grows_total",
        b,
        "Calls that extended the file. Rising while big_free_pages_reusable is non-zero means \
         pages are being pinned faster than they can be reused.",
        io.grows,
    );
    io_counter(
        out,
        "big_storage_truncates_total",
        b,
        "Calls that shortened the file.",
        io.truncates,
    );
    io_counter(
        out,
        "big_storage_syncs_total",
        b,
        "Flushes the engine asked for: two per commit unless durability is off, and none at \
         all when it is. Its ratio to big_txn_id is what the durability setting is actually \
         doing, as opposed to what it is set to. Barrier counts the same two as full - it is a \
         weaker kind of flush, not fewer of them, and the difference between them is a duration \
         rather than a count.",
        io.syncs,
    );

    // Seconds and a float, which is what the exposition format wants for a time counter, and
    // the pair `_seconds_total / _total` is a mean flush that any dashboard can divide out.
    out.push_str(
        "# HELP big_storage_sync_seconds_total Wall-clock spent inside those flushes. Over \
         big_storage_syncs_total it is the mean flush, and a commit that got slow almost always \
         got slow here.\n# TYPE big_storage_sync_seconds_total counter\n",
    );
    out.push_str(&format!(
        "big_storage_sync_seconds_total{{backend=\"{b}\"}} {}\n",
        io.sync_nanos as f64 / 1e9
    ));
}

/// A counter carrying the one label this section uses.
fn io_counter(out: &mut String, name: &str, backend: &str, help: &str, value: u64) {
    let help: String = help.chars().map(|c| if c == '\n' { ' ' } else { c }).collect();
    out.push_str(&format!(
        "# HELP {name} {help}\n# TYPE {name} counter\n{name}{{backend=\"{backend}\"}} {value}\n"
    ));
}

/// The row-key dictionary, which is the one thing here that grows with cardinality.
///
/// Separate from `render_pager` because it does not come from the pager: the dictionary is
/// catalog state held in memory, and the file it was loaded from says nothing about what it
/// costs to hold. It is exported at all because it is the only unbounded allocation in a
/// process whose whole read path is otherwise a borrow out of a mapping - a database with a
/// high-cardinality keyed column runs out of memory long before it runs out of disk, and
/// without this gauge the first symptom is the OOM killer.
pub fn render_keys(out: &mut String, k: &big_embed::KeyStats) {
    gauge(out, "big_row_keys", "Distinct row keys held in memory.", k.count as u64);
    gauge(
        out,
        "big_row_key_bytes",
        "Approximately what those keys occupy. Counts the keys in both directions and not \
         the maps' own overhead, so the true figure is larger: watch the slope, not the value.",
        k.resident_bytes as u64,
    );
    // Zero rather than absent when there is no ceiling. A series that disappears makes every
    // ratio against it disappear too, and "unlimited" is exactly when an operator most wants
    // the headroom query to keep evaluating.
    gauge(
        out,
        "big_row_key_limit",
        "The ceiling on inventing new row keys, or 0 when there is none.",
        k.limit.unwrap_or(0) as u64,
    );
}

/// What this node can say about the cluster it is part of.
///
/// A second entry point rather than more fields on `ServerMetrics`, because none of this is
/// the server's: it is the coordinator's, and a server built from a bare `Api` has a cluster of
/// one to report on. Rendered into the same document because an operator scrapes one endpoint.
///
/// **`big_cluster_copies_behind` is the number to alert on.** It is redundancy this cluster has
/// lost and will not get back until somebody runs `POST /repair`; everything else here recovers
/// on its own.
pub fn render_cluster(out: &mut String, c: &big_cluster::counters::Snapshot) {
    gauge(
        out,
        "big_cluster_nodes",
        "Nodes in the cluster file, this one included.",
        c.nodes as u64,
    );
    gauge(
        out,
        "big_cluster_copies_behind",
        "Copies the agreement has marked behind. Redundancy lost until a repair is run.",
        c.behind as u64,
    );
    gauge(
        out,
        "big_cluster_serving",
        "1 when this node may answer for the range it holds. 0 when it has lost touch with the \
         agreement and is refusing rather than risk a second node answering too.",
        c.serving as u64,
    );
    gauge(
        out,
        "big_cluster_agreement_term",
        "The agreement's term. Climbing steadily means elections are being held, which means \
         nodes are not hearing each other.",
        c.term,
    );
    gauge(
        out,
        "big_cluster_agreement_leader",
        "1 when this node currently leads the agreement.",
        c.leader as u64,
    );
    counter(
        out,
        "big_cluster_peer_requests_total",
        "Requests this node has made to another.",
        c.counts.sent,
    );
    counter(
        out,
        "big_cluster_peer_unreachable_total",
        "Peer requests that found nobody there. A machine to go and look at.",
        c.counts.unreachable,
    );
    counter(
        out,
        "big_cluster_peer_refused_total",
        "Peer requests answered with a refusal. Usually two builds or two configurations that \
         disagree, rather than a machine that is down.",
        c.counts.refused,
    );
}

fn counter(out: &mut String, name: &str, help: &str, value: u64) {
    sample(out, name, "counter", help, value);
}

fn gauge(out: &mut String, name: &str, help: &str, value: u64) {
    sample(out, name, "gauge", help, value);
}

fn sample(out: &mut String, name: &str, kind: &str, help: &str, value: u64) {
    // A newline inside HELP would terminate the line and make everything after it garbage.
    let help: String = help.chars().map(|c| if c == '\n' { ' ' } else { c }).collect();
    out.push_str(&format!("# HELP {name} {help}\n# TYPE {name} {kind}\n{name} {value}\n"));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pager_metrics() -> PagerMetrics {
        PagerMetrics { page_count: 42, live_readers: 2, ..Default::default() }
    }

    #[test]
    fn a_backend_that_counts_nothing_publishes_no_io_series() {
        let text = ServerMetrics::new().render(&pager_metrics());
        // Not "zero reads" - no reads series at all. Zeroes would read as an idle database.
        assert!(!text.contains("big_storage_"), "io series published for a backend with no counts");
    }

    #[test]
    fn io_series_are_labelled_by_backend() {
        let io = IoStats {
            backend: "mmap",
            reads: 9,
            writes: 3,
            write_bytes: 3 * 8192,
            grows: 1,
            truncates: 0,
            syncs: 2,
            sync_nanos: 1_500_000_000,
        };
        let text = ServerMetrics::new().render(&PagerMetrics { io: Some(io), ..pager_metrics() });

        assert!(text.contains("big_storage_reads_total{backend=\"mmap\"} 9"));
        assert!(text.contains("big_storage_writes_total{backend=\"mmap\"} 3"));
        assert!(text.contains("big_storage_write_bytes_total{backend=\"mmap\"} 24576"));
        assert!(text.contains("big_storage_grows_total{backend=\"mmap\"} 1"));
        assert!(text.contains("big_storage_syncs_total{backend=\"mmap\"} 2"));
        // Nanoseconds in, seconds out: the exposition format has no other unit for time.
        assert!(text.contains("big_storage_sync_seconds_total{backend=\"mmap\"} 1.5"));
    }

    #[test]
    fn every_sample_declares_a_type() {
        let m = ServerMetrics::new();
        let io = IoStats { backend: "mmap", ..Default::default() };
        let text = m.render(&PagerMetrics { io: Some(io), ..pager_metrics() });
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("# HELP ") {
                let name = rest.split(' ').next().unwrap();
                assert!(
                    text.contains(&format!("# TYPE {name} ")),
                    "{name} has HELP but no TYPE, which makes it unusable to a scraper"
                );
            }
        }
    }

    #[test]
    fn buckets_are_cumulative_and_end_at_the_count() {
        let m = ServerMetrics::new();
        m.request(200, Duration::from_micros(500), 0, 0); // first bucket
        m.request(200, Duration::from_millis(20), 0, 0); // le=0.05
        m.request(500, Duration::from_secs(30), 0, 0); // overflow
        let text = m.render(&pager_metrics());

        assert!(text.contains("big_http_request_duration_seconds_bucket{le=\"0.001\"} 1"));
        assert!(text.contains("big_http_request_duration_seconds_bucket{le=\"0.05\"} 2"));
        assert!(text.contains("big_http_request_duration_seconds_bucket{le=\"+Inf\"} 3"));
        assert!(text.contains("big_http_request_duration_seconds_count 3"));
    }

    #[test]
    fn statuses_land_in_their_class() {
        let m = ServerMetrics::new();
        m.request(200, Duration::ZERO, 0, 0);
        m.request(404, Duration::ZERO, 0, 0);
        m.request(422, Duration::ZERO, 0, 0);
        m.request(503, Duration::ZERO, 0, 0);
        let text = m.render(&pager_metrics());
        assert!(text.contains("big_http_responses_total{class=\"2xx\"} 1"));
        assert!(text.contains("big_http_responses_total{class=\"4xx\"} 2"));
        assert!(text.contains("big_http_responses_total{class=\"5xx\"} 1"));
    }

    #[test]
    fn no_reader_means_no_oldest_reader_sample() {
        let m = ServerMetrics::new();
        let quiet = m.render(&PagerMetrics { oldest_reader_txn_id: None, ..Default::default() });
        assert!(!quiet.contains("big_oldest_reader_txn_id"));

        let busy = m.render(&PagerMetrics { oldest_reader_txn_id: Some(0), ..Default::default() });
        assert!(busy.contains("big_oldest_reader_txn_id 0"));
    }

    #[test]
    fn pager_gauges_reach_the_output() {
        let m = ServerMetrics::new();
        let text = m.render(&pager_metrics());
        assert!(text.contains("big_page_count 42"));
        assert!(text.contains("big_live_readers 2"));
    }
}
