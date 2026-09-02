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

//! The loader against a real server, over a real socket.
//!
//! Nothing is mocked, for the same reason `big-cli`'s tests are not: the risk this crate carries
//! is a *sequence* of requests agreeing with what one server actually commits, and a fixture
//! would keep agreeing after the two had parted company. Every assertion below went through
//! `Server::bind`, a loopback port, and `big_bin::client::run` - the same function `main` calls.
//!
//! Ids are written four digits wide so that every line is exactly sixteen bytes. That is not
//! tidiness: it makes a checkpoint's offset a number this file can state rather than compute,
//! and an offset nobody can predict is an offset no test can check.

use big_api::Api;
use big_bin::client::http::Client;
use big_bin::exit;
use big_db::catalog::FieldKind;
use big_http::{Server, ServerConfig};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::sync::Arc;

/// Sixteen bytes, always.
const LINE: usize = 16;

fn facts(ids: std::ops::RangeInclusive<u32>) -> String {
    ids.map(|id| format!("country {id:04} GB\n")).collect()
}

fn ids(ids: std::ops::RangeInclusive<u32>) -> String {
    ids.map(|id| format!("{id}\n")).collect()
}

/// What a run produced: the code a shell would see, and the two streams a user would.
struct Run {
    code: i32,
    out: String,
    err: String,
}

fn run(addr: SocketAddr, args: &[&str]) -> Run {
    run_with(addr, args, "")
}

/// Drives the loader exactly as `main` does. `tty` is false so that progress writes plain
/// lines a test can read rather than a carriage return it cannot.
fn run_with(addr: SocketAddr, args: &[&str], stdin: &str) -> Run {
    let mut input = std::io::Cursor::new(stdin.as_bytes().to_vec());
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let owned: Vec<String> = ["--addr".to_string(), addr.to_string()]
        .into_iter()
        .chain(args.iter().map(|a| (*a).to_string()))
        .collect();
    let code = {
        let mut io = big_bin::Io {
            input: &mut input,
            out: &mut out,
            err: &mut err,
            out_tty: false,
            err_tty: false,
        };
        // No environment: every test says what it means on the command line.
        big_bin::client::run(&owned, &mut io, &|_| None)
    };
    Run {
        code,
        out: String::from_utf8(out).expect("stdout is UTF-8"),
        err: String::from_utf8(err).expect("stderr is UTF-8"),
    }
}

fn database() -> Api<big_pager::MemPager> {
    let api = Api::in_memory().unwrap();
    api.create_table("tx").unwrap();
    api.create_field("tx", "country", FieldKind::Set, 0).unwrap();
    api
}

/// A server that will answer exactly `requests` and then stop. The thread is left running, as
/// in `big-cli`'s tests: a listener with nobody accepting is what an interrupted load meets.
fn stocked(requests: usize) -> SocketAddr {
    let server = Server::bind_with(database(), "127.0.0.1:0", ServerConfig::default()).unwrap();
    let addr = server.local_addr().unwrap();
    std::thread::spawn(move || {
        let _ = server.serve_n(requests);
    });
    addr
}

/// One value out of a `{"key":n}` answer, read with the client's own reader.
fn number(body: &str, key: &str) -> u64 {
    big_bin::client::json::parse(body)
        .unwrap_or_else(|e| panic!("not JSON: {body:?} ({e})"))
        .get(key)
        .and_then(big_bin::client::json::Value::cell)
        .unwrap_or_else(|| panic!("no `{key}` in {body:?}"))
        .parse()
        .unwrap()
}

/// How many records hold `country = GB`, asked the way a user would.
fn counted(addr: SocketAddr) -> u64 {
    let client = Client { addr: addr.to_string(), token: None, timeout: None };
    let response = client.send("POST", "/table/tx/query", "Count(Row(country=\"GB\"))").unwrap();
    assert!(response.ok(), "the query was refused: {}", response.body);
    number(&response.body, "count")
}

fn write(dir: &Path, name: &str, text: &str) -> String {
    let path = dir.join(name);
    std::fs::write(&path, text).unwrap();
    path.to_str().unwrap().to_string()
}

// ------------------------------------------------------------------------------------------
// Why this binary exists
// ------------------------------------------------------------------------------------------

/// The gap, demonstrated rather than asserted from a constant.
///
/// A body past `MAX_BODY` is refused on its `Content-Length`, before a byte of it is read - so
/// this sends the header and no body, which is the cheap way to reach the same refusal. This is
/// the whole reason a loader exists: one request cannot carry a file.
#[test]
fn one_request_cannot_carry_more_than_the_ceiling() {
    let addr = stocked(1);
    let mut socket = TcpStream::connect(addr).unwrap();
    let over = (big_http::MAX_BODY + 1).to_string();
    socket
        .write_all(
            format!(
                "POST /table/tx/import HTTP/1.1\r\nHost: t\r\nContent-Length: {over}\r\n\
                 Connection: close\r\n\r\n"
            )
            .as_bytes(),
        )
        .unwrap();
    let mut answer = String::new();
    socket.read_to_string(&mut answer).unwrap();
    assert!(answer.starts_with("HTTP/1.1 413"), "{answer}");
    assert!(answer.contains("request_too_large"), "{answer}");
}

/// Hand-maintained against `big_http::MAX_BODY`, not derived from it.
///
/// Deriving would make the default correct by construction and prove nothing. Written out, it
/// breaks if either number moves - which is the direction that actually goes wrong, because a
/// default at or above the ceiling turns every chunk into a `413`.
#[test]
fn the_default_chunk_leaves_room_under_the_ceiling() {
    // A `const` block rather than a runtime assertion: this compares two constants, so the
    // build is a better place to find out than the test run is.
    const {
        assert!(
            big_bin::ingest::args::DEFAULT_CHUNK_BYTES < big_http::MAX_BODY,
            "a default chunk must fit under the server's body ceiling, or every chunk is a 413"
        );
    };
}

// ------------------------------------------------------------------------------------------
// The loop
// ------------------------------------------------------------------------------------------

#[test]
fn a_file_that_needs_many_requests_lands_whole() {
    // Ten chunks of ten lines, and one query to check them.
    let addr = stocked(11);
    let dir = tempfile::tempdir().unwrap();
    let path = write(dir.path(), "facts", &facts(1..=100));

    let r = run(addr, &["import", "tx", &path, "--chunk-lines", "10"]);
    assert_eq!(r.code, exit::OK, "{}", r.err);
    // stdout carries what the server said, and only that: one column, one row, tsv because
    // these streams are not terminals. A script counting facts reads this and nothing else.
    assert_eq!(r.out, "imported\n100\n", "{}", r.out);
    // What the loop did is on stderr, where it cannot get into a pipe.
    assert!(r.err.contains("chunks 10"), "{}", r.err);
    assert!(r.err.contains(&format!("bytes {}", 100 * LINE)), "{}", r.err);
    assert_eq!(counted(addr), 100);
}

/// The property every other one rests on: a chunk sent twice is a chunk sent once.
#[test]
fn running_the_same_load_twice_changes_nothing() {
    let addr = stocked(21);
    let dir = tempfile::tempdir().unwrap();
    let path = write(dir.path(), "facts", &facts(1..=100));

    for _ in 0..2 {
        let r = run(addr, &["import", "tx", &path, "--chunk-lines", "10"]);
        assert_eq!(r.code, exit::OK, "{}", r.err);
    }
    assert_eq!(counted(addr), 100, "a second run added records that were already there");
}

#[test]
fn delete_takes_the_same_loop() {
    // 10 imports, 5 deletes, 1 query.
    let addr = stocked(16);
    let dir = tempfile::tempdir().unwrap();
    let facts_path = write(dir.path(), "facts", &facts(1..=100));
    let ids_path = write(dir.path(), "ids", &ids(1..=50));

    assert_eq!(run(addr, &["import", "tx", &facts_path, "--chunk-lines", "10"]).code, exit::OK);
    let r = run(addr, &["delete", "tx", &ids_path, "--chunk-lines", "10"]);
    assert_eq!(r.code, exit::OK, "{}", r.err);
    assert_eq!(r.out, "deleted\n50\n", "{}", r.out);
    assert_eq!(counted(addr), 50);
}

#[test]
fn a_pipe_is_a_load_like_any_other() {
    let addr = stocked(11);
    let r = run_with(addr, &["import", "tx", "-", "--chunk-lines", "10"], &facts(1..=100));
    assert_eq!(r.code, exit::OK, "{}", r.err);
    assert_eq!(counted(addr), 100);
}

/// Nothing is sent, so there is nothing to send it to.
#[test]
fn a_dry_run_reaches_no_server() {
    let dir = tempfile::tempdir().unwrap();
    let path = write(dir.path(), "facts", &facts(1..=100));
    // Port 1 has nothing on it. A run that reached the network would fail here.
    let addr: SocketAddr = "127.0.0.1:1".parse().unwrap();
    let r = run(addr, &["import", "tx", &path, "--chunk-lines", "10", "--dry-run"]);
    assert_eq!(r.code, exit::OK, "{}", r.err);
    assert!(r.err.contains("chunks 10"), "{}", r.err);
    assert_eq!(r.out, "would_send\n0\n", "{}", r.out);
}

// ------------------------------------------------------------------------------------------
// When it goes wrong
// ------------------------------------------------------------------------------------------

/// A refusal stops the load, names the byte, and is never retried.
#[test]
fn a_line_the_server_refuses_stops_the_load_where_it_is() {
    // Three chunks land; the fourth carries the bad line and is refused.
    let addr = stocked(4);
    let dir = tempfile::tempdir().unwrap();
    let text = format!("{}{}{}", facts(1..=30), "country 0031\n", facts(32..=100));
    let path = write(dir.path(), "facts", &text);
    let check = dir.path().join("load.ck");

    let r = run(
        addr,
        &["import", "tx", &path, "--chunk-lines", "10", "--resume", check.to_str().unwrap()],
    );
    assert_eq!(r.code, exit::REFUSED, "{}", r.err);
    // The server's own code and the server's own sentence, not a rewording.
    assert!(r.err.contains("malformed_line"), "{}", r.err);
    assert!(r.err.contains(&format!("stopped at byte {}", 30 * LINE)), "{}", r.err);

    // The checkpoint holds what landed, and only what landed.
    let found =
        big_bin::ingest::Checkpoint::read(&check).unwrap().expect("a checkpoint was written");
    assert_eq!(found.offset, (30 * LINE) as u64);
    assert_eq!(found.wrote, 30);
}

/// A line longer than a whole request cannot be split into two the server would read as one.
#[test]
fn a_line_past_the_chunk_ceiling_is_a_usage_error() {
    let addr = stocked(0);
    let dir = tempfile::tempdir().unwrap();
    let path = write(dir.path(), "facts", &format!("country 1 {}\n", "G".repeat(200)));
    let r = run(addr, &["import", "tx", &path, "--chunk-bytes", "64"]);
    assert_eq!(r.code, exit::USAGE, "{}", r.err);
    assert!(r.err.contains("--chunk-bytes"), "{}", r.err);
}

#[test]
fn a_checkpoint_from_another_load_is_refused_rather_than_applied() {
    let addr = stocked(0);
    let dir = tempfile::tempdir().unwrap();
    let path = write(dir.path(), "facts", &facts(1..=100));
    let check = dir.path().join("load.ck");
    big_bin::ingest::Checkpoint {
        target: "/table/other/import".to_string(),
        input: path.clone(),
        size: (100 * LINE) as u64,
        offset: 320,
        lines: 20,
        wrote: 20,
    }
    .write(&check)
    .unwrap();

    let r = run(addr, &["import", "tx", &path, "--resume", check.to_str().unwrap()]);
    assert_eq!(r.code, exit::USAGE, "{}", r.err);
    assert!(r.err.contains("/table/other/import"), "{}", r.err);
}

// ------------------------------------------------------------------------------------------
// Resume
// ------------------------------------------------------------------------------------------

/// The load this crate is for: it is interrupted, and the second run does not start over.
///
/// The server answers three chunks and stops accepting. The fourth request is written into a
/// connection nobody is reading, so the client's own deadline ends it - which is exactly the
/// case Decision 4 covers: `big serve` may or may not have committed that chunk, and re-sending it
/// is correct either way.
///
/// **`--in-flight 1`, stated rather than inherited.** Every count below - three answered, an
/// offset of exactly thirty lines, nine requests left, seventy lines re-read - is arithmetic
/// that only holds while one request is outstanding at a time. Under a wider window the client
/// needs a request the server was never told to answer, and the test hangs instead of failing;
/// that is what happened when `DEFAULT_IN_FLIGHT` moved to two. The windowed case is
/// `a_windowed_load_resumes_without_losing_a_record`, which deliberately predicts nothing.
#[test]
fn an_interrupted_load_resumes_where_it_stopped() {
    let server =
        Arc::new(Server::bind_with(database(), "127.0.0.1:0", ServerConfig::default()).unwrap());
    let addr = server.local_addr().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = write(dir.path(), "facts", &facts(1..=100));
    let check = dir.path().join("load.ck");
    let args = [
        "import",
        "tx",
        path.as_str(),
        "--chunk-lines",
        "10",
        "--in-flight",
        "1",
        "--resume",
        check.to_str().unwrap(),
        "--retries",
        "0",
        "--timeout",
        "1",
    ];

    let first = Arc::clone(&server);
    let serving = std::thread::spawn(move || {
        let _ = first.serve_n(3);
    });
    let r = run(addr, &args);
    assert_eq!(r.code, exit::UNREACHABLE, "{}\n{}", r.out, r.err);
    serving.join().unwrap();

    // Thirty lines acknowledged, and the checkpoint says so.
    let found =
        big_bin::ingest::Checkpoint::read(&check).unwrap().expect("a checkpoint was written");
    assert_eq!(found.offset, (30 * LINE) as u64);

    // Seven chunks are left, plus the one the interrupted run left in the backlog, plus a query.
    let second = Arc::clone(&server);
    let serving = std::thread::spawn(move || {
        let _ = second.serve_n(9);
    });
    let r = run(addr, &args);
    assert_eq!(r.code, exit::OK, "{}", r.err);
    assert!(r.err.contains("resuming"), "{}", r.err);
    // Seventy lines this time, not a hundred: the first thirty were not read again.
    assert!(r.err.contains(&format!("bytes {}", 70 * LINE)), "{}", r.err);
    // And the total is about the load rather than about the leg.
    assert_eq!(r.out, "imported\n100\n", "{}", r.out);

    assert_eq!(counted(addr), 100);
    serving.join().unwrap();

    // A finished load leaves no checkpoint: one that stayed would make the same command run
    // again do nothing, which looks exactly like a load that worked.
    assert!(big_bin::ingest::Checkpoint::read(&check).unwrap().is_none());
}

/// A window in flight loses nothing when the load is interrupted and resumed.
///
/// **The test the window exists to earn.** With `--in-flight` above one the acknowledgements
/// stop being the only thing outstanding: a killed run has chunks the server may or may not
/// have written, and the checkpoint may only advance across a *contiguous* run of successes. A
/// checkpoint that jumped to the furthest acknowledgement instead would skip whatever gap sat
/// behind it, and the load would come back short with nothing anywhere saying so.
///
/// So this asserts the only thing that settles it: after the interruption and the resume, every
/// record is there.
#[test]
fn a_windowed_load_resumes_without_losing_a_record() {
    let server =
        Arc::new(Server::bind_with(database(), "127.0.0.1:0", ServerConfig::default()).unwrap());
    let addr = server.local_addr().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = write(dir.path(), "facts", &facts(1..=100));
    let check = dir.path().join("windowed.ck");
    let args = [
        "import",
        "tx",
        path.as_str(),
        "--chunk-lines",
        "10",
        "--in-flight",
        "3",
        "--resume",
        check.to_str().unwrap(),
        "--retries",
        "0",
        "--timeout",
        "1",
    ];

    // Cut short mid-window: three answered, and whatever else the window had sent is left
    // unanswered until the client's own timeout gives up on it.
    let first = Arc::clone(&server);
    std::thread::spawn(move || {
        let _ = first.serve_n(3);
    });
    let r = run(addr, &args);
    assert_eq!(r.code, exit::UNREACHABLE, "{}\n{}", r.out, r.err);

    // The checkpoint is a contiguous prefix, so it can only be a whole number of chunks and
    // never past what was acknowledged.
    let found =
        big_bin::ingest::Checkpoint::read(&check).unwrap().expect("a checkpoint was written");
    assert!(found.offset <= (30 * LINE) as u64, "checkpoint ran ahead: {}", found.offset);
    assert_eq!(found.offset as usize % LINE, 0, "checkpoint fell inside a line");

    // Generous, and not joined: what the window left in the backlog is not a number this test
    // should have to predict, and predicting it wrong would hang rather than fail.
    let second = Arc::clone(&server);
    std::thread::spawn(move || {
        let _ = second.serve_n(64);
    });
    let r = run(addr, &args);
    assert_eq!(r.code, exit::OK, "{}\n{}", r.out, r.err);

    // The whole point: nothing was skipped over.
    assert_eq!(counted(addr), 100, "a record went missing across the resume");
}
