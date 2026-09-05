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

//! The parts of the server an operator deals with: probes, metrics, credentials, and the two
//! ceilings that stop one client from taking the process down.
//!
//! Driven over a real socket, like `server.rs`, and for the same reason: a connection cap and
//! a read timeout are properties of the listener, and a test that called the router directly
//! would assert nothing about either.

use big_embed::Api;
use big_http::{Auth, Server, ServerConfig};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

fn config() -> ServerConfig {
    ServerConfig { ..Default::default() }
}

/// A server on a loopback port that answers `requests` and then stops.
fn spawn(requests: usize, config: ServerConfig) -> SocketAddr {
    let api = Api::in_memory().unwrap();
    api.create_table("tx").unwrap();
    api.create_field("tx", "amount", big_db::catalog::FieldKind::Int, 32).unwrap();
    // Enough facts that a scan has fragments to visit. A query over an empty table does no
    // work, and a test that asserts on a scan being interrupted has to give it a scan.
    let facts: Vec<_> = (1..=32)
        .map(|r| big_embed::Fact::Int { field: "amount", record: r, value: r * 10 })
        .collect();
    api.import("tx", &facts).unwrap();

    let server = Server::bind_with(api, "127.0.0.1:0", config).unwrap();
    let addr = server.local_addr().unwrap();
    std::thread::spawn(move || {
        let _ = server.serve_n(requests);
    });
    addr
}

/// The same, with group commit switched on before the server takes the database.
fn spawn_coalescing(requests: usize, jobs: usize) -> SocketAddr {
    let api = Api::in_memory().unwrap();
    api.create_table("tx").unwrap();
    api.create_field("tx", "amount", big_db::catalog::FieldKind::Int, 32).unwrap();
    api.configure_group_commit(big_embed::GroupConfig {
        enabled: true,
        max_jobs: jobs,
        ..big_embed::GroupConfig::default()
    });

    let server = Server::bind_with(api, "127.0.0.1:0", config()).unwrap();
    let addr = server.local_addr().unwrap();
    std::thread::spawn(move || {
        let _ = server.serve_n(requests);
    });
    addr
}

/// The same, offering `?ack=queued`.
fn spawn_early(requests: usize) -> SocketAddr {
    let api = Api::in_memory().unwrap();
    api.create_table("tx").unwrap();
    api.create_field("tx", "amount", big_db::catalog::FieldKind::Int, 32).unwrap();
    api.configure_group_commit(big_embed::GroupConfig {
        enabled: true,
        async_writes: true,
        ..big_embed::GroupConfig::default()
    });

    let server = Server::bind_with(api, "127.0.0.1:0", config()).unwrap();
    let addr = server.local_addr().unwrap();
    std::thread::spawn(move || {
        let _ = server.serve_n(requests);
    });
    addr
}

/// The same, with `GET /watch` configured.
fn spawn_watching(requests: usize) -> SocketAddr {
    let api = Api::in_memory().unwrap();
    api.create_table("tx").unwrap();
    api.create_field("tx", "amount", big_db::catalog::FieldKind::Int, 32).unwrap();

    let server =
        Server::bind_with(api, "127.0.0.1:0", ServerConfig { watch_max: 4, ..config() }).unwrap();
    let addr = server.local_addr().unwrap();
    std::thread::spawn(move || {
        let _ = server.serve_n(requests);
    });
    addr
}

/// The same, on the real worker pool rather than the inline path, for the tests that are
/// about the pool itself.
fn spawn_pooled(config: ServerConfig) -> SocketAddr {
    let api = Api::in_memory().unwrap();
    let server = Server::bind_with(api, "127.0.0.1:0", config).unwrap();
    let addr = server.local_addr().unwrap();
    std::thread::spawn(move || {
        let _ = server.serve();
    });
    addr
}

struct Reply {
    status: u16,
    headers: String,
    body: String,
}

fn send_with(
    addr: SocketAddr,
    method: &str,
    target: &str,
    body: &str,
    user: Option<&str>,
) -> Reply {
    let mut stream = TcpStream::connect(addr).unwrap();
    // The password is the username - see `users_file` for why the fixture is arranged that way.
    let auth = match user {
        Some(u) => format!(
            "Authorization: Basic {}\r\n",
            big_tls::base64::encode(format!("{u}:{u}").as_bytes())
        ),
        None => String::new(),
    };
    let request = format!(
        "{method} {target} HTTP/1.1\r\nHost: localhost\r\n{auth}Content-Length: {}\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).unwrap();
    stream.flush().unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();

    let mut raw = String::new();
    stream.read_to_string(&mut raw).unwrap();
    let status = raw
        .split(' ')
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("no status in {raw:?}"));
    let (headers, body) = raw.split_once("\r\n\r\n").unwrap_or((&raw, ""));
    Reply { status, headers: headers.to_string(), body: body.to_string() }
}

fn send(addr: SocketAddr, method: &str, target: &str, body: &str) -> Reply {
    send_with(addr, method, target, body, None)
}

// ---------------------------------------------------------------------------
// Probes
// ---------------------------------------------------------------------------

#[test]
fn health_answers_without_touching_the_database() {
    let addr = spawn(1, config());
    let r = send(addr, "GET", "/health", "");
    assert_eq!(r.status, 200);
    assert!(r.body.contains(r#""status":"ok""#), "{}", r.body);
}

#[test]
fn ready_reports_what_the_engine_can_see() {
    let addr = spawn(1, config());
    let r = send(addr, "GET", "/ready", "");
    assert_eq!(r.status, 200);
    assert!(r.body.contains(r#""status":"ready""#), "{}", r.body);
    // Readiness is more than a constant: it went and asked the engine.
    assert!(r.body.contains("\"txn_id\":"), "{}", r.body);
    assert!(r.body.contains("\"tables\":1"), "{}", r.body);
}

/// A load balancer carries no credential, so requiring one would mean putting a token into
/// every environment that has a probe - which is more places than the data it guards.
#[test]
fn the_probes_stay_open_when_authentication_is_on() {
    let (_dir, path) = users_file("secret admin\n");
    let addr = spawn(3, ServerConfig { auth: Auth::from_file(&path).unwrap(), ..config() });

    assert_eq!(send(addr, "GET", "/health", "").status, 200);
    assert_eq!(send(addr, "GET", "/ready", "").status, 200);
    // Everything else is not open.
    assert_eq!(send(addr, "GET", "/schema", "").status, 401);
}

// ---------------------------------------------------------------------------
// Metrics
// ---------------------------------------------------------------------------

#[test]
fn metrics_render_in_the_prometheus_text_format() {
    let addr = spawn(2, config());
    send(addr, "GET", "/health", "");
    let r = send(addr, "GET", "/metrics", "");

    assert_eq!(r.status, 200);
    assert!(
        r.headers.contains("Content-Type: text/plain; version=0.0.4"),
        "a scraper reads the version to decide how to parse: {}",
        r.headers
    );
    // The pager gauges, which existed all along and could not be read from outside.
    assert!(r.body.contains("# TYPE big_free_pages_reusable gauge"), "{}", r.body);
    assert!(r.body.contains("big_page_count "), "{}", r.body);
    // The request that came before this one was counted.
    assert!(r.body.contains(r#"big_http_responses_total{class="2xx"} 1"#), "{}", r.body);
    // The balancer's numbers are there even on a cluster of one, so a dashboard built against
    // a single node keeps working when the second node arrives. Off, and nothing moving.
    assert!(r.body.contains("big_cluster_balancer_enabled 0"), "{}", r.body);
    assert!(r.body.contains("# TYPE big_cluster_ranges_moving gauge"), "{}", r.body);
    assert!(r.body.contains("big_cluster_peer_load_unanswered_total 0"), "{}", r.body);

    // This server is in memory, and `MemPager` counts no I/O because it does none. Absent
    // rather than zero: a block of zeroes would read as a database nobody is writing to.
    assert!(!r.body.contains("big_storage_"), "io series on a backend with no disk: {}", r.body);
}

/// A file-backed server exports what its backend did to the disk.
///
/// The counterpart to the assertion above, and the reason it is a separate test: the two say
/// that the series appear exactly when there is a backend behind them, which is a different
/// claim from either one alone. This is also the only place the whole chain runs end to end -
/// the pager counting, `Store::metrics` forwarding, and the renderer labelling - over a real
/// socket rather than through a struct literal.
#[test]
#[cfg(unix)]
fn metrics_report_what_the_storage_backend_did() {
    let dir = tempfile::tempdir().unwrap();
    let api = Api::open(dir.path().join("t.big")).unwrap();
    api.create_table("tx").unwrap();
    // A commit, so there is something to have written and flushed.
    api.create_field("tx", "amount", big_db::catalog::FieldKind::Int, 32).unwrap();

    let server = Server::bind_with(api, "127.0.0.1:0", config()).unwrap();
    let addr = server.local_addr().unwrap();
    std::thread::spawn(move || {
        let _ = server.serve_n(1);
    });
    let r = send(addr, "GET", "/metrics", "");

    assert_eq!(r.status, 200);
    assert!(r.body.contains("# TYPE big_storage_writes_total counter"), "{}", r.body);
    // The label is what makes the numbers attributable to an implementation.
    assert!(r.body.contains(r#"big_storage_writes_total{backend="mmap"}"#), "{}", r.body);
    // Creating a table and a field committed, and a commit flushes. Both had to be non-zero
    // for the counters to be wired to anything at all.
    assert!(
        !r.body.contains(r#"big_storage_syncs_total{backend="mmap"} 0"#),
        "the backend reported no flushes after a commit: {}",
        r.body
    );
    // No byte counter for reads on this backend, on purpose: the read that reaches the disk is
    // a page fault this process is never told about.
    assert!(!r.body.contains("big_storage_read_bytes_total"), "{}", r.body);
}

/// **A node started with `--reclaim` gives pages back while it is serving**, which nothing in
/// this tree could do before: `big compact` wants the exclusive lock, so shrinking a database
/// meant stopping the daemon.
///
/// The whole chain, over a real socket: the steward notices the file has enough free space to
/// be worth it, `Api::reclaim` takes the write lock, the pager releases the runs that sit flush
/// against the end of the file, and the counters say what happened.
///
/// **What this deliberately does not claim.** Only the *tail* is released - one live page near
/// the top pins every free page below it - so a database that is emptied and left alone stays
/// large, and the number here is small. Making the general case shrink means relocating the
/// fragments that sit above the free space, which is not built; the rule that makes the tail
/// drain at all is `the_lowest_reusable_page_is_allocated_first` in `big-pager`.
#[test]
#[cfg(unix)]
fn a_node_with_reclaim_on_gives_pages_back_while_serving() {
    let dir = tempfile::tempdir().unwrap();
    let api = Api::open(dir.path().join("t.big")).unwrap();
    api.create_table("tx").unwrap();
    api.create_field("tx", "amount", big_db::catalog::FieldKind::Int, 32).unwrap();

    // One record per shard rather than a pile in one: a bitmap holds a great many records in a
    // handful of pages, and what fills a file is fragments. Enough of them to pass the floor
    // the steward will not act below.
    let records: Vec<u64> = (0..6_000u64).map(|i| i << 20).collect();
    let facts: Vec<_> = records
        .iter()
        .map(|r| big_embed::Fact::Int { field: "amount", record: *r, value: r % 1_000 })
        .collect();
    api.import("tx", &facts).unwrap();
    assert!(api.metrics().page_count > 4_096, "the fixture has to outgrow the reclaim floor");
    api.delete("tx", &records).unwrap();
    // One more commit. A transaction cannot reuse the pages it is itself freeing - they are
    // not past the horizon until it has landed - so the delete's own bookkeeping went to the
    // end of the file. The next write takes the lowest free page instead, which is what moves
    // the live page off the tail and leaves something to give back.
    api.import("tx", &[big_embed::Fact::Int { field: "amount", record: 1, value: 1 }]).unwrap();

    let server =
        Server::bind_with(api, "127.0.0.1:0", ServerConfig { reclaim: true, ..config() }).unwrap();
    let addr = server.local_addr().unwrap();
    let running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let flag = std::sync::Arc::clone(&running);
    let done = std::thread::spawn(move || {
        let _ = server.serve_while(&flag);
    });

    // The steward runs on its own clock, so this is a wait rather than an assertion.
    let started = std::time::Instant::now();
    loop {
        let body = send(addr, "GET", "/metrics", "").body;
        assert!(body.contains("# TYPE big_pages_reclaimed_total counter"), "{body}");
        if !body.contains("big_pages_reclaimed_total 0") {
            break;
        }
        assert!(
            started.elapsed() < std::time::Duration::from_secs(20),
            "nothing was ever given back: {body}"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    running.store(false, std::sync::atomic::Ordering::Relaxed);
    let _ = done.join();
}

/// And off by default, which is the half that keeps every existing deployment as it was.
#[test]
fn a_node_without_reclaim_never_gives_a_page_back() {
    let addr = spawn(1, config());
    let body = send(addr, "GET", "/metrics", "").body;
    assert!(body.contains("big_pages_reclaimed_total 0"), "{body}");
}

// ---------------------------------------------------------------------------
// Credentials
// ---------------------------------------------------------------------------

/// A users file from `name role` shorthand, **hashing each name as its own password**.
///
/// A fixture convention, and a deliberate one: it keeps every call site below reading
/// `Some("ro")` the way it did when a credential was one string, so these tests stay about roles
/// rather than about passwords. The thing they are testing did not change; only the credential
/// carrying it did.
fn users_file(body: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("users");
    let text: String = body
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|line| {
            let mut parts = line.split_whitespace();
            let (name, role) = (parts.next().unwrap(), parts.next().unwrap());
            format!("{name} {role} {}\n", big_http::auth::hash_password(name).unwrap())
        })
        .collect();
    std::fs::write(&path, text).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    (dir, path)
}

#[test]
fn a_missing_token_is_a_401_that_says_how_to_fix_it() {
    let (_dir, path) = users_file("secret admin\n");
    let addr = spawn(1, ServerConfig { auth: Auth::from_file(&path).unwrap(), ..config() });

    let r = send(addr, "GET", "/schema", "");
    assert_eq!(r.status, 401);
    assert!(r.body.contains(r#""code":"unauthenticated""#), "{}", r.body);
    assert!(
        r.headers.contains("WWW-Authenticate: Basic"),
        "every HTTP client library looks for this header: {}",
        r.headers
    );
    assert!(
        r.headers.contains("charset=\"UTF-8\""),
        "RFC 7617, so a client encodes the credential as UTF-8 rather than latin-1: {}",
        r.headers
    );
}

#[test]
fn an_unknown_token_is_not_distinguishable_from_no_token() {
    let (_dir, path) = users_file("secret admin\n");
    let addr = spawn(1, ServerConfig { auth: Auth::from_file(&path).unwrap(), ..config() });
    let r = send_with(addr, "GET", "/schema", "", Some("guess"));
    assert_eq!(r.status, 401);
    assert!(r.body.contains(r#""code":"unauthenticated""#), "{}", r.body);
}

// ---------------------------------------------------------------------------
// Correlation
// ---------------------------------------------------------------------------

#[test]
fn every_response_carries_a_request_id() {
    let addr = spawn(2, config());
    let a = send(addr, "GET", "/health", "");
    let b = send(addr, "GET", "/health", "");
    assert!(a.headers.contains("X-Request-Id: "), "{}", a.headers);
    assert_ne!(
        a.headers, b.headers,
        "two requests must not share an id, or the log cannot be joined to a report"
    );
}

// ---------------------------------------------------------------------------
// Ceilings
// ---------------------------------------------------------------------------

/// A zero budget makes this deterministic: the first checkpoint the scan reaches is already
/// past the deadline. Nothing sleeps, so the test does not depend on how fast the machine is,
/// and the fixture writes real facts so there is a real scan to interrupt.
#[test]
fn a_query_past_its_deadline_is_a_504() {
    let addr = spawn(1, ServerConfig { query_timeout: Some(Duration::ZERO), ..config() });
    let r = send(addr, "POST", "/table/tx/query", "Count(Row(amount > 0))");
    assert_eq!(r.status, 504, "{}", r.body);
    assert!(r.body.contains(r#""code":"query_timeout""#), "{}", r.body);
}

/// The pool is one worker deep with a one-slot queue, so anything past the second concurrent
/// connection has nowhere to go. Every connection here sends nothing at all, which is exactly
/// the slow-loris shape: the ones that get a worker sit in `read` until the read timeout, and
/// the rest are refused straight away by the accepting thread.
#[test]
fn connections_past_the_cap_are_shed_rather_than_queued() {
    let addr = spawn_pooled(ServerConfig {
        workers: 1,
        queue_depth: 1,
        read_timeout: Duration::from_millis(300),
        ..config()
    });

    let mut sockets = Vec::new();
    for _ in 0..8 {
        let s = TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        sockets.push(s);
    }

    let mut shed = 0;
    let mut answered = 0;
    for mut s in sockets {
        let mut raw = String::new();
        let _ = s.read_to_string(&mut raw);
        if raw.contains("server_busy") {
            shed += 1;
        } else if raw.starts_with("HTTP/1.1") {
            answered += 1;
        }
    }

    assert!(shed > 0, "eight connections against a two-deep pool must shed at least one");
    assert!(
        shed + answered == 8,
        "every connection gets an answer of some kind; got {shed} shed and {answered} answered"
    );
}

/// The hole this closes: before there was a read timeout, a connection that sent one byte and
/// then nothing held its thread until the client relented. The assertion is only that the
/// server lets go on its own.
#[test]
fn a_client_that_stops_mid_request_does_not_hold_a_worker() {
    let addr = spawn_pooled(ServerConfig { read_timeout: Duration::from_millis(200), ..config() });

    let mut s = TcpStream::connect(addr).unwrap();
    s.write_all(b"GET /health HTTP/1.1\r\nHost: local").unwrap(); // no blank line, ever
    s.flush().unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();

    let mut raw = String::new();
    let started = std::time::Instant::now();
    let _ = s.read_to_string(&mut raw);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the server waited for a client that was never going to finish"
    );
}

/// The dictionary is the one allocation in this process that grows with cardinality rather
/// than with size, so it is the one an operator has no other way to see coming.
#[test]
fn metrics_report_what_the_row_key_dictionary_costs() {
    let addr = spawn(1, config());
    let r = send(addr, "GET", "/metrics", "");
    assert_eq!(r.status, 200);
    assert!(r.body.contains("big_row_keys "), "{}", r.body);
    assert!(r.body.contains("big_row_key_bytes "), "{}", r.body);
    // Zero rather than absent when there is no ceiling: a series that disappears takes every
    // ratio against it with it.
    assert!(r.body.contains("big_row_key_limit 0"), "{}", r.body);
}

// ---------------------------------------------------------------------------
// Backup
// ---------------------------------------------------------------------------

/// The point of the route: a copy is taken while the server is still answering.
///
/// The assertion that matters is the last one - the daemon serves a request *after* the walk,
/// on the same file, with nothing stopped in between. Before this route existed the only way
/// to reach the copy path was `big backup`, a second process that takes the file's exclusive
/// lock and therefore cannot open a file a daemon is serving at all.
#[test]
fn a_backup_is_taken_while_the_server_keeps_answering() {
    let dir = tempfile::tempdir().unwrap();
    let addr = spawn(
        3,
        ServerConfig { backup_dir: Some(dir.path().to_str().unwrap().to_string()), ..config() },
    );

    let before = send(addr, "POST", "/table/tx/query", "Count(All())");
    assert_eq!(before.status, 200, "{}", before.body);

    let r = send(addr, "POST", "/admin/backup?name=one.big", "");
    assert_eq!(r.status, 200, "{}", r.body);
    assert!(r.body.contains(r#""backup":"one.big""#), "{}", r.body);
    assert!(r.body.contains("\"txn_id\":"), "{}", r.body);

    let written = dir.path().join("one.big");
    assert!(written.exists(), "the backup named a file that was not created");
    assert!(written.metadata().unwrap().len() > 0, "the backup is empty");

    let after = send(addr, "POST", "/table/tx/query", "Count(All())");
    assert_eq!(after.status, 200, "{}", after.body);
    assert_eq!(after.body, before.body, "the copy changed what the source answers");
}

/// A backup is an ordinary database file: restoring it is opening it.
#[test]
fn the_file_a_backup_writes_opens_as_a_database() {
    let dir = tempfile::tempdir().unwrap();
    let addr = spawn(
        1,
        ServerConfig { backup_dir: Some(dir.path().to_str().unwrap().to_string()), ..config() },
    );

    assert_eq!(send(addr, "POST", "/admin/backup?name=two.big", "").status, 200);

    // `spawn` stocked `tx` with thirty-two records, and the copy has to hold every one of them.
    let restored = big_embed::Api::open(dir.path().join("two.big")).unwrap();
    let schema = restored.schema();
    assert_eq!(schema.len(), 1, "the copy lost the schema");
    assert_eq!(schema[0].name, "tx");
}

/// Without a directory the route is not configured, and says which flag is missing.
#[test]
fn a_backup_with_nowhere_to_write_is_refused_by_name() {
    let addr = spawn(1, config());
    let r = send(addr, "POST", "/admin/backup?name=x", "");
    assert_eq!(r.status, 501, "{}", r.body);
    assert!(r.body.contains("backup_not_configured"), "{}", r.body);
    assert!(r.body.contains("--backup-dir"), "{}", r.body);
}

/// The name is a file inside the directory, and cannot be a way out of it.
#[test]
fn a_backup_cannot_name_a_path_outside_its_directory() {
    let dir = tempfile::tempdir().unwrap();
    let addr = spawn(
        5,
        ServerConfig { backup_dir: Some(dir.path().to_str().unwrap().to_string()), ..config() },
    );

    // Raw, not percent-encoded: nothing in this server decodes `%2F`, so an escape attempt
    // arrives exactly as it was typed and has to be refused as it was typed. The `%2F` spelling
    // is in the list too, because a name holding a literal `%` is not one either.
    for name in ["../escaped", "sub/file", "..", ".hidden", "..%2Fescaped"] {
        let r = send(addr, "POST", &format!("/admin/backup?name={name}"), "");
        assert_eq!(r.status, 400, "`{name}` was accepted: {}", r.body);
        assert!(r.body.contains("bad_parameter"), "{}", r.body);
    }
}

/// A backup never overwrites. The second call is a `409`, and the first file is untouched.
#[test]
fn a_backup_refuses_to_overwrite_one_that_is_already_there() {
    let dir = tempfile::tempdir().unwrap();
    let addr = spawn(
        2,
        ServerConfig { backup_dir: Some(dir.path().to_str().unwrap().to_string()), ..config() },
    );

    assert_eq!(send(addr, "POST", "/admin/backup?name=once.big", "").status, 200);
    let again = send(addr, "POST", "/admin/backup?name=once.big", "");
    assert_eq!(again.status, 409, "{}", again.body);
    assert!(again.body.contains("backup_destination_exists"), "{}", again.body);
}

/// The route needs `admin`: it produces a second copy of the whole database, so a credential
/// that can call it is one that can carry the data out.
#[test]
fn a_read_token_cannot_take_a_backup() {
    let dir = tempfile::tempdir().unwrap();
    let (_tokens, path) = users_file("scraper read\n");
    let addr = spawn(
        1,
        ServerConfig {
            auth: Auth::from_file(&path).unwrap(),
            backup_dir: Some(dir.path().to_str().unwrap().to_string()),
            ..config()
        },
    );
    let r = send_with(addr, "POST", "/admin/backup?name=x.big", "", Some("scraper"));
    assert_eq!(r.status, 403, "{}", r.body);
}

/// The group-commit series are always there, so a dashboard built before anybody passed the
/// flag keeps evaluating after somebody does.
///
/// Deliberately **not** a claim that writes were grouped: this server answers requests one at a
/// time, so there is nothing to group and `jobs == commits` is the right answer. What is
/// asserted is that a write went through the coalescer at all, and that it was counted.
#[test]
fn a_node_that_coalesces_writes_counts_what_its_commits_carried() {
    let addr = spawn_coalescing(3, 64);

    let wrote = send(addr, "POST", "/table/tx/import", "amount 1 100\namount 2 200\n");
    assert_eq!(wrote.status, 200, "{}", wrote.body);
    assert_eq!(wrote.body, r#"{"imported":2}"#);

    let r = send(addr, "GET", "/metrics", "");
    assert_eq!(r.status, 200);
    assert!(r.body.contains("# TYPE big_write_commits_total counter"), "{}", r.body);
    assert!(r.body.contains("big_write_commits_total 1"), "{}", r.body);
    assert!(r.body.contains("big_write_commit_jobs_total 1"), "{}", r.body);
    assert!(r.body.contains("big_write_isolations_total 0"), "{}", r.body);
}

/// The off half, and the half that matters more: a node nobody configured must not have gone
/// anywhere near the queue, and must still publish the series as zeroes.
#[test]
fn a_node_that_does_not_coalesce_still_publishes_the_series() {
    let addr = spawn(2, config());

    let wrote = send(addr, "POST", "/table/tx/import", "amount 40 400\n");
    assert_eq!(wrote.status, 200, "{}", wrote.body);

    let r = send(addr, "GET", "/metrics", "");
    assert!(r.body.contains("# TYPE big_write_commits_total counter"), "{}", r.body);
    assert!(
        r.body.contains("big_write_commits_total 0"),
        "a write that never went through the coalescer must not be counted by it: {}",
        r.body
    );
    assert!(r.body.contains("big_write_commit_jobs_total 0"), "{}", r.body);
}

/// A batch the engine refuses is refused the same way, and with the same words, when it went
/// through the coalescer.
#[test]
fn a_refused_batch_reads_the_same_whether_or_not_writes_are_coalesced() {
    let coalescing = spawn_coalescing(1, 64);
    let plain = spawn(1, config());

    let a = send(coalescing, "POST", "/table/tx/import", "nosuchfield 1 100\n");
    let b = send(plain, "POST", "/table/tx/import", "nosuchfield 1 100\n");

    assert_eq!(a.status, b.status, "{} vs {}", a.body, b.body);
    assert_eq!(a.body, b.body);
    assert!(a.body.contains("nosuchfield"), "{}", a.body);
}

/// `?ack=queued` says what it is: a count, and a flag saying it is not durable yet.
#[test]
fn an_early_acknowledgement_says_it_is_not_durable() {
    let addr = spawn_early(3);

    let queued = send(addr, "POST", "/table/tx/import?ack=queued", "amount 1 100\namount 2 200\n");
    assert_eq!(queued.status, 200, "{}", queued.body);
    assert_eq!(queued.body, r#"{"imported":2,"durable":false}"#);

    // The buffer says what is at risk.
    let r = send(addr, "GET", "/metrics", "");
    assert!(r.body.contains("# TYPE big_write_queue_bytes gauge"), "{}", r.body);
    assert!(
        !r.body.contains("big_write_queue_bytes 0\n"),
        "two facts were acknowledged and nothing is held: {}",
        r.body
    );
    assert!(r.body.contains("big_write_acknowledged_lost_total 0"), "{}", r.body);
}

/// The default answer is unchanged, byte for byte. This is the regression that matters most.
#[test]
fn a_write_that_does_not_ask_is_answered_exactly_as_before() {
    let addr = spawn_early(2);

    let durable = send(addr, "POST", "/table/tx/import", "amount 1 100\n");
    assert_eq!(durable.body, r#"{"imported":1}"#, "no new field on a write that did not ask");

    let explicit = send(addr, "POST", "/table/tx/import?ack=commit", "amount 2 200\n");
    assert_eq!(explicit.body, r#"{"imported":1}"#);
}

/// Asking for it where it is not offered is refused by name, never quietly downgraded.
#[test]
fn asking_to_be_answered_early_where_it_is_off_names_the_flag() {
    let addr = spawn_coalescing(1, 64);

    let r = send(addr, "POST", "/table/tx/import?ack=queued", "amount 1 100\n");
    assert_eq!(r.status, 422, "{}", r.body);
    assert!(r.body.contains("ack_not_available"), "{}", r.body);
    assert!(r.body.contains("--write-async"), "and says which flag: {}", r.body);
}

/// A value the parameter does not take is a refusal that lists what it does take.
#[test]
fn an_ack_that_is_not_one_of_the_two_is_refused_with_both() {
    let addr = spawn_early(1);

    let r = send(addr, "POST", "/table/tx/import?ack=whenever", "amount 1 100\n");
    assert_eq!(r.status, 422, "{}", r.body);
    assert!(r.body.contains("commit or queued"), "{}", r.body);
    assert!(r.body.contains("whenever"), "and repeats what it got: {}", r.body);
}

/// **The reason the writer thread exists.** An acknowledged write on a node that then goes
/// quiet still becomes durable, without another request arriving to carry it.
///
/// A wait rather than an assertion, like the reclaim test: the thread runs on `--write-linger`,
/// which is its own clock and not this test's.
#[test]
fn an_early_acknowledgement_is_committed_even_if_nothing_else_arrives() {
    let api = Api::in_memory().unwrap();
    api.create_table("tx").unwrap();
    api.create_field("tx", "amount", big_db::catalog::FieldKind::Int, 32).unwrap();
    api.configure_group_commit(big_embed::GroupConfig {
        enabled: true,
        async_writes: true,
        linger: Duration::from_millis(20),
        ..big_embed::GroupConfig::default()
    });

    let server = Server::bind_with(api, "127.0.0.1:0", config()).unwrap();
    let addr = server.local_addr().unwrap();
    let running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let flag = std::sync::Arc::clone(&running);
    let done = std::thread::spawn(move || {
        let _ = server.serve_while(&flag);
    });

    let queued = send(addr, "POST", "/table/tx/import?ack=queued", "amount 7 700\n");
    assert_eq!(queued.status, 200, "{}", queued.body);

    // Nothing else is sent from here on. The only thing that can commit it is the writer.
    let started = std::time::Instant::now();
    loop {
        let body = send(addr, "GET", "/metrics", "").body;
        if body.contains("big_write_commits_total 1") {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "nothing ever committed the acknowledged write: {body}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    // And it is readable, which is the claim that actually matters.
    let answer = send(addr, "POST", "/sql", "SELECT sum(amount) FROM tx");
    assert!(answer.body.contains("700"), "{}", answer.body);

    let held = send(addr, "GET", "/metrics", "").body;
    assert!(held.contains("big_write_queue_bytes 0"), "and nothing is still at risk: {held}");

    running.store(false, std::sync::atomic::Ordering::Relaxed);
    let _ = done.join();
}

/// A node that offers no early answers starts no writer thread, and nothing about it changes.
#[test]
fn a_node_that_answers_only_durably_publishes_an_empty_queue() {
    let addr = spawn_coalescing(1, 64);
    let r = send(addr, "GET", "/metrics", "");
    assert!(r.body.contains("big_write_queue_bytes 0"), "{}", r.body);
    assert!(r.body.contains("big_write_queue_oldest_seconds 0.000"), "{}", r.body);
    assert!(r.body.contains("big_write_queue_refused_total 0"), "{}", r.body);
}

/// A write says where this node's history got to, and a read can be told to wait for it.
#[test]
fn a_write_says_which_transaction_and_a_read_can_wait_for_it() {
    let addr = spawn(2, config());

    let wrote = send(addr, "POST", "/table/tx/import", "amount 100 1\n");
    assert_eq!(wrote.status, 200, "{}", wrote.body);
    let txn = wrote
        .headers
        .lines()
        .find_map(|l| l.strip_prefix("X-Big-Txn: "))
        .expect("a write says which transaction carried it")
        .trim()
        .to_string();
    // `<node>/<transaction>`. The name is half the value: a bare number could be sent to a node
    // it means nothing on.
    let (node, number) = txn.split_once('/').expect("node/transaction");
    assert_eq!(node, "local", "a server with no cluster file is one node called `local`");
    assert!(number.parse::<u64>().is_ok(), "{txn}");

    // Asking to be caught up to a transaction this node has already passed is free.
    let caught = send(addr, "POST", &format!("/sql?min_txn={txn}"), "SELECT count(*) FROM tx");
    assert_eq!(caught.status, 200, "{}", caught.body);
    assert!(caught.body.contains("33"), "the write is visible: {}", caught.body);
}

/// A transaction id from another node names a history this one does not have. Refused in one
/// round trip rather than waited out and reported as a timeout.
#[test]
fn a_transaction_from_another_node_is_refused_rather_than_waited_for() {
    let addr = spawn(1, config());

    let r = send(addr, "POST", "/sql?min_txn=somewhere-else/9", "SELECT count(*) FROM tx");
    assert_eq!(r.status, 409, "{}", r.body);
    assert!(r.body.contains("wrong_node"), "{}", r.body);
    assert!(r.body.contains("somewhere-else"), "and names both nodes: {}", r.body);
    assert!(r.body.contains("local"), "{}", r.body);
}

/// A transaction this node will never reach times out, with a deadline the caller chose.
#[test]
fn waiting_for_a_transaction_that_never_comes_is_a_timeout_not_a_hang() {
    let addr = spawn(1, config());

    let started = std::time::Instant::now();
    let r = send(addr, "POST", "/sql?min_txn=local/999999&wait=50", "SELECT count(*) FROM tx");
    assert_eq!(r.status, 504, "{}", r.body);
    assert!(r.body.contains("not_caught_up"), "{}", r.body);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "it waited far past the 50ms it was given: {:?}",
        started.elapsed()
    );
}

/// A `min_txn` that is not one is a refusal that says what the shape is.
#[test]
fn a_min_txn_that_is_not_one_says_what_the_shape_is() {
    let addr = spawn(2, config());

    let bare = send(addr, "POST", "/sql?min_txn=12", "SELECT count(*) FROM tx");
    assert_eq!(bare.status, 422, "{}", bare.body);
    assert!(bare.body.contains("X-Big-Txn"), "it names where to get one: {}", bare.body);

    let nonsense = send(addr, "POST", "/sql?min_txn=local/soon", "SELECT count(*) FROM tx");
    assert_eq!(nonsense.status, 422, "{}", nonsense.body);
    assert!(nonsense.body.contains("soon"), "{}", nonsense.body);
}

/// **The claim `GET /watch` has to earn**: subscribe, write from somewhere else, and the new
/// answer arrives without asking for it.
#[test]
fn a_subscriber_is_pushed_the_new_answer_when_it_changes() {
    use std::io::{BufRead, BufReader, Write};

    let api = Api::in_memory().unwrap();
    api.create_table("tx").unwrap();
    api.create_field("tx", "amount", big_db::catalog::FieldKind::Int, 32).unwrap();

    let server = Server::bind_with(
        api,
        "127.0.0.1:0",
        ServerConfig { watch_max: 4, ..config() },
    )
    .unwrap();
    let addr = server.local_addr().unwrap();
    let running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let flag = std::sync::Arc::clone(&running);
    let done = std::thread::spawn(move || {
        let _ = server.serve_while(&flag);
    });

    // Subscribe on a connection of its own and leave it open.
    let mut sub = std::net::TcpStream::connect(addr).unwrap();
    sub.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    write!(
        sub,
        "GET /watch?sql=SELECT%20count(*)%20FROM%20tx&interval=50 HTTP/1.1\r\nHost: x\r\n\r\n"
    )
    .unwrap();
    let mut reader = BufReader::new(sub.try_clone().unwrap());

    // The head says it is a stream rather than a body.
    let mut head = String::new();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        if line == "\r\n" {
            break;
        }
        head.push_str(&line);
    }
    assert!(head.contains("200"), "{head}");
    assert!(head.contains("Transfer-Encoding: chunked"), "{head}");
    assert!(head.contains("text/event-stream"), "{head}");

    // The first answer, pushed without being asked for.
    let first = read_event(&mut reader);
    assert!(first.contains(r#""rows":[[0]]"#), "the table is empty: {first}");

    // Somebody else writes.
    let wrote = send(addr, "POST", "/table/tx/import", "amount 1 100\n");
    assert_eq!(wrote.status, 200, "{}", wrote.body);

    // And the subscriber is told, without having asked again.
    let second = read_event(&mut reader);
    assert!(second.contains(r#""rows":[[1]]"#), "{second}");
    assert!(second.contains("event: answer"), "{second}");
    assert!(second.contains("id: local/"), "the id is where this node's history got to: {second}");

    drop(reader);
    running.store(false, std::sync::atomic::Ordering::Relaxed);
    let _ = done.join();
}

/// Reads one server-sent event out of a chunked stream, skipping the chunk framing.
fn read_event(reader: &mut std::io::BufReader<std::net::TcpStream>) -> String {
    use std::io::BufRead;
    let mut event = String::new();
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).unwrap();
        assert!(n > 0, "the stream ended before an event arrived: {event}");
        let trimmed = line.trim_end();
        // Chunk sizes are bare hex on a line of their own; a blank line ends an event.
        if trimmed.is_empty() {
            if !event.is_empty() {
                return event;
            }
            continue;
        }
        if u64::from_str_radix(trimmed, 16).is_ok() && !trimmed.contains(':') {
            continue;
        }
        event.push_str(&line);
    }
}

/// Off by default, and it says which flag turns it on rather than pretending the route is gone.
#[test]
fn watching_is_refused_when_it_was_never_configured() {
    let addr = spawn(1, config());
    let r = send(addr, "GET", "/watch?sql=SELECT%20count(*)%20FROM%20tx", "");
    assert_eq!(r.status, 503, "{}", r.body);
    assert!(r.body.contains("--watch-max"), "{}", r.body);
}

/// A statement that writes is refused at subscribe, not once per push.
#[test]
fn watching_something_that_is_not_a_select_is_refused_at_subscribe() {
    let addr = spawn_watching(1);
    let r = send(addr, "GET", "/watch?sql=CREATE%20TABLE%20nope", "");
    assert_eq!(r.status, 422, "{}", r.body);
    assert!(r.body.contains("not_a_query"), "{}", r.body);
}
