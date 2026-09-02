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

//! The producer against a real server, over a real socket.
//!
//! Nothing is mocked, for the reason `big-bin`'s tests are not: what this crate risks getting
//! wrong is whether the statements it writes are the statements one server actually commits,
//! and a fixture would keep agreeing long after the two had parted company.
//!
//! `Server::serve_n(n)` takes **`n` connections**, not `n` requests - `listener.incoming()
//! .take(n)` - and `handle` then answers requests on each until the client is done with it.
//! That is what makes `stocked(1)` a proof of keep-alive rather than a coincidence: a producer
//! that opened a socket per flush would be waiting on the second one for ever.

use big_embed::{Api, FieldKind, MemPager};
use big_http::{Server, ServerConfig};
use big_message::{Config, Error, Producer, Value};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// A table with one of everything this crate can write into.
fn database() -> Api<MemPager> {
    let api = Api::in_memory().unwrap();
    api.create_table("tx").unwrap();
    api.create_field("tx", "amount", FieldKind::Int, 32).unwrap();
    api.create_field("tx", "country", FieldKind::Set, 0).unwrap();
    api
}

/// A server that will take exactly `connections` and then stop.
fn stocked(connections: usize) -> SocketAddr {
    let server = Server::bind_with(database(), "127.0.0.1:0", ServerConfig::default()).unwrap();
    let addr = server.local_addr().unwrap();
    std::thread::spawn(move || {
        let _ = server.serve_n(connections);
    });
    addr
}

/// Short waits, so a test that has gone wrong fails rather than hangs.
fn quick() -> Config {
    Config { io_timeout: Duration::from_secs(3), retries: 1, ..Config::default() }
}

fn producer(addr: SocketAddr, config: Config) -> Producer {
    Producer::open(&addr.to_string(), "tx", &["amount", "country"], None, config)
        .expect("the table and the columns are writable")
}

/// One request, on its own connection, the way an onlooker would ask.
fn ask(addr: SocketAddr, method: &str, target: &str, body: &str) -> String {
    let mut stream = TcpStream::connect(addr).expect("the server is listening");
    let request = format!(
        "{method} {target} HTTP/1.1\r\nHost: {addr}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).unwrap();
    let mut answer = String::new();
    stream.read_to_string(&mut answer).unwrap();
    answer.split_once("\r\n\r\n").expect("headers then a body").1.to_string()
}

fn sql(addr: SocketAddr, statement: &str) -> String {
    ask(addr, "POST", "/sql", statement)
}

/// The record ids a table holds, which is the one way to see what was allocated.
///
/// A route rather than a `SELECT`, because `_record_id` is a column of an answer rather than
/// something a bare select list may name on its own.
fn records(addr: SocketAddr) -> String {
    ask(addr, "GET", "/table/tx/records", "")
}

#[test]
fn a_producer_writes_the_rows_it_is_given() {
    let addr = stocked(8);
    let mut p = producer(addr, quick());

    p.send(&[Value::Int(100), Value::Text("GB")]).unwrap();
    p.send(&[Value::Int(900), Value::Text("US")]).unwrap();
    let flushed = p.close().unwrap();

    assert_eq!(flushed.inserted, 2);
    assert_eq!(sql(addr, "SELECT count(*) FROM tx"), r#"{"columns":["count"],"rows":[[2]]}"#);
    assert_eq!(
        sql(addr, "SELECT count(*) FROM tx WHERE country = 'GB'"),
        r#"{"columns":["count"],"rows":[[1]]}"#
    );
}

/// The property the whole crate is built around, checked against a real table.
#[test]
fn the_records_written_are_never_named_by_the_client() {
    let addr = stocked(8);
    let mut p = producer(addr, quick());
    p.send(&[Value::Int(1), Value::Text("GB")]).unwrap();
    p.send(&[Value::Int(2), Value::Text("US")]).unwrap();
    p.close().unwrap();

    // Ids the client never chose, allocated from zero because the table was empty. If a record
    // id had leaked into the statement these would be whatever the client picked instead.
    assert_eq!(records(addr), r#"{"records":[0,1],"next":null}"#);
}

#[test]
fn a_value_that_looks_like_sql_is_stored_as_the_text_it_is() {
    let addr = stocked(8);
    let mut p = producer(addr, quick());

    let hostile = "x'); DROP TABLE tx; --";
    p.send(&[Value::Int(1), Value::Text(hostile)]).unwrap();
    p.send(&[Value::Int(2), Value::Text("O'Brien")]).unwrap();
    p.close().unwrap();

    // The table is still there, and the text came back as itself rather than as a statement.
    assert_eq!(sql(addr, "SELECT count(*) FROM tx"), r#"{"columns":["count"],"rows":[[2]]}"#);
    assert_eq!(
        sql(addr, "SELECT count(*) FROM tx WHERE country = 'x''); DROP TABLE tx; --'"),
        r#"{"columns":["count"],"rows":[[1]]}"#
    );
    assert_eq!(
        sql(addr, "SELECT count(*) FROM tx WHERE country = 'O''Brien'"),
        r#"{"columns":["count"],"rows":[[1]]}"#
    );
}

/// Keep-alive, proved rather than asserted: one connection, and five flushes down it.
#[test]
fn many_batches_go_down_one_connection() {
    // One for the producer, one for the query afterwards. A producer that opened a socket per
    // flush would find nobody accepting its second.
    let addr = stocked(2);
    let mut p = producer(addr, quick());

    for i in 0..5u64 {
        p.send(&[Value::Int(i), Value::Text("GB")]).unwrap();
        let flushed = p.flush().unwrap();
        assert_eq!(flushed.inserted, 1, "flush {i} did not land");
    }
    p.close().unwrap();

    assert_eq!(sql(addr, "SELECT count(*) FROM tx"), r#"{"columns":["count"],"rows":[[5]]}"#);
}

/// The cost of letting the server allocate, pinned in a test rather than left to be discovered.
#[test]
fn two_flushes_of_the_same_messages_create_two_sets_of_records() {
    let addr = stocked(8);
    let mut p = producer(addr, quick());

    for _ in 0..2 {
        p.send(&[Value::Int(100), Value::Text("GB")]).unwrap();
        p.flush().unwrap();
    }
    p.close().unwrap();

    // Identical messages, and *two* records - because the ids were allocated, not addressed.
    // The import route would have written one. This is the whole trade, and it is not a bug.
    assert_eq!(sql(addr, "SELECT count(*) FROM tx"), r#"{"columns":["count"],"rows":[[2]]}"#);
    assert_eq!(records(addr), r#"{"records":[0,1],"next":null}"#);
}

#[test]
fn a_refusal_is_reported_with_the_servers_own_words_and_never_retried() {
    let addr = stocked(8);
    let mut p = Producer::open(&addr.to_string(), "tx", &["nope"], None, quick()).unwrap();

    p.send(&[Value::Int(1)]).unwrap();
    let err = p.flush().unwrap_err();

    match &err {
        Error::Refused { status, code, message } => {
            assert_eq!(*status, 422, "{err}");
            assert_eq!(code, "unknown_field");
            assert!(
                message.contains("nope"),
                "the server's own sentence names the field: {message}"
            );
        }
        other => panic!("expected a refusal, got {other}"),
    }

    // And it is remembered: a refusal about a fixed column list refuses the next batch too.
    let again = p.send(&[Value::Int(2)]).unwrap_err();
    assert_eq!(again, err, "a stopped producer hands back the reason it stopped");
}

#[test]
fn a_producer_will_not_open_on_a_column_that_names_the_record() {
    let addr = stocked(1);
    for columns in [&["_record_id", "amount"][..], &["_RECORD_ID"][..]] {
        let opened = Producer::open(&addr.to_string(), "tx", columns, None, quick());
        assert!(opened.is_err(), "{columns:?} names the record id and must not open");
    }
}

#[test]
fn a_message_too_large_for_any_batch_names_which_message() {
    let addr = stocked(1);
    // A ceiling small enough that one ordinary row is already past it.
    let config = Config { max_bytes: 128, ..quick() };
    let mut p = producer(addr, config);

    p.send(&[Value::Int(1), Value::Text("GB")]).unwrap();
    let huge = "x".repeat(256);
    match p.send(&[Value::Int(2), Value::Text(&huge)]).unwrap_err() {
        Error::MessageTooLarge { at, len, cap } => {
            assert_eq!(at, 1, "the second message is the one that could not be sent");
            assert!(len > cap, "{len} is not past {cap}");
        }
        other => panic!("expected MessageTooLarge, got {other}"),
    }
}

/// A server that takes the whole request and then dies without answering.
///
/// This is the shape of every failure that must **not** be retried: the statement is on the
/// wire in full, so it may have been committed, and sending it again would allocate a second
/// set of records for the same messages.
#[test]
fn a_request_that_failed_after_it_was_written_is_reported_as_unknown_and_not_resent() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(AtomicUsize::new(0));

    let counted = Arc::clone(&seen);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            counted.fetch_add(1, Ordering::SeqCst);
            // Read something, so the client's write has certainly completed, and then drop the
            // connection without a word.
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf);
        }
    });

    let mut p = producer(addr, quick());
    p.send(&[Value::Int(1), Value::Text("GB")]).unwrap();

    match p.flush().unwrap_err() {
        Error::Unknown(_) => {}
        other => panic!("expected Unknown, got {other}"),
    }

    // The one assertion this test exists for. `retries` is 1, so a rule that retried this would
    // have opened a second connection.
    assert_eq!(seen.load(Ordering::SeqCst), 1, "the request must not be sent again");
}

/// A request that provably never arrived *is* retried, which is the other half of the rule.
#[test]
fn a_request_that_never_reached_a_server_is_retried_and_then_reported() {
    // Bound and dropped: the port is one nothing is listening on, so every connect refuses.
    let addr = {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap()
    };

    let mut p = producer(addr, quick());
    p.send(&[Value::Int(1), Value::Text("GB")]).unwrap();

    match p.flush().unwrap_err() {
        Error::Connect(_) => {}
        other => panic!("expected Connect, got {other}"),
    }
}

/// A closing producer is asked what it did, and the answer is all of it.
///
/// The byte ceiling is set small enough that the twenty-five rows go in many statements.
/// Reporting the last one would answer a number that looks like an answer and is wrong by every
/// batch before it.
#[test]
fn closing_answers_every_flush_rather_than_the_last() {
    let addr = stocked(4);
    let mut p = producer(addr, Config { max_bytes: 64, ..quick() });

    for i in 0..25u64 {
        p.send(&[Value::Int(i), Value::Text("GB")]).unwrap();
    }
    assert_eq!(p.close().unwrap().inserted, 25);

    assert_eq!(sql(addr, "SELECT count(*) FROM tx"), r#"{"columns":["count"],"rows":[[25]]}"#);
}

#[test]
fn nothing_pending_is_not_a_request() {
    let addr = stocked(1);
    let mut p = producer(addr, quick());
    // No connection is opened, so this works against a server that has not been asked for one.
    assert_eq!(p.flush().unwrap().inserted, 0);
    assert_eq!(p.pending(), 0);
    assert!(!p.due());
}
