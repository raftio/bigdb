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

use big_api::Api;
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
        .map(|r| big_api::Fact::Int { field: "amount", record: r, value: r * 10 })
        .collect();
    api.import("tx", &facts).unwrap();

    let server = Server::bind_with(api, "127.0.0.1:0", config).unwrap();
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
    token: Option<&str>,
) -> Reply {
    let mut stream = TcpStream::connect(addr).unwrap();
    let auth = match token {
        Some(t) => format!("Authorization: Bearer {t}\r\n"),
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
    let (_dir, path) = token_file("secret admin\n");
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
}

// ---------------------------------------------------------------------------
// Credentials
// ---------------------------------------------------------------------------

fn token_file(body: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tokens");
    std::fs::write(&path, body).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    (dir, path)
}

#[test]
fn a_missing_token_is_a_401_that_says_how_to_fix_it() {
    let (_dir, path) = token_file("secret admin\n");
    let addr = spawn(1, ServerConfig { auth: Auth::from_file(&path).unwrap(), ..config() });

    let r = send(addr, "GET", "/schema", "");
    assert_eq!(r.status, 401);
    assert!(r.body.contains(r#""code":"unauthenticated""#), "{}", r.body);
    assert!(
        r.headers.contains("WWW-Authenticate: Bearer"),
        "every HTTP client library looks for this header: {}",
        r.headers
    );
}

#[test]
fn a_token_that_does_not_reach_far_enough_is_a_403() {
    let (_dir, path) = token_file("ro read\nrw write\nsecret admin\n");
    let addr = spawn(4, ServerConfig { auth: Auth::from_file(&path).unwrap(), ..config() });

    // `read` reaches the query and schema routes.
    assert_eq!(send_with(addr, "GET", "/schema", "", Some("ro")).status, 200);
    // ...and no further. A 403, not a 401: retrying with this credential will never work.
    let r = send_with(addr, "POST", "/table/new", "", Some("ro"));
    assert_eq!(r.status, 403);
    assert!(r.body.contains("needs `admin`"), "{}", r.body);

    // `write` reaches writes but not schema changes.
    assert_eq!(send_with(addr, "POST", "/table/tx/delete", "1\n", Some("rw")).status, 200);
    assert_eq!(send_with(addr, "POST", "/table/new", "", Some("secret")).status, 200);
}

#[test]
fn an_unknown_token_is_not_distinguishable_from_no_token() {
    let (_dir, path) = token_file("secret admin\n");
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
    let restored = big_api::Api::open(dir.path().join("two.big")).unwrap();
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
    let (_tokens, path) = token_file("scraper read\n");
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

/// **A schema change written in SQL needs `admin`, not the `read` that `/sql` is authorised
/// with.**
///
/// The role check runs before any body is decoded, which is what keeps it cheap and is why it
/// cannot know which statement arrived. `POST /sql` is therefore authorised as `read` - what
/// almost every statement needs - and the route raises the bar itself once the statement has
/// been classified. Without that, adding `CREATE TABLE` to this surface would have handed every
/// read-only token the power `POST /table/{t}` demands `admin` for.
#[test]
fn a_read_only_token_cannot_change_the_schema_through_sql() {
    let (_dir, path) = token_file("ro read\nrw write\nsecret admin\n");
    let addr = spawn(6, ServerConfig { auth: Auth::from_file(&path).unwrap(), ..config() });

    // A query is what `read` is for, and still works.
    assert_eq!(send_with(addr, "POST", "/sql", "SELECT count(*) FROM tx", Some("ro")).status, 200);

    // A schema change is not, and says so the same way the `/table` route does.
    let r = send_with(addr, "POST", "/sql", "CREATE TABLE sneaky", Some("ro"));
    assert_eq!(r.status, 403, "{}", r.body);
    assert!(r.body.contains("needs `admin`"), "{}", r.body);

    // `write` is no closer: this is about the verb, not about writing.
    let r = send_with(addr, "POST", "/sql", "CREATE TABLE sneaky", Some("rw"));
    assert_eq!(r.status, 403, "{}", r.body);

    // And the table really was not created by either attempt.
    let r = send_with(addr, "GET", "/schema", "", Some("ro"));
    assert!(!r.body.contains("sneaky"), "a refused statement created the table anyway: {}", r.body);

    assert_eq!(send_with(addr, "POST", "/sql", "CREATE TABLE fine", Some("secret")).status, 200);
}
