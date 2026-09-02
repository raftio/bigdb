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

//! The sink against a real bigdb and a scripted Redis.
//!
//! bigdb is real, bound in-process, because it is this repository's and a fixture would drift
//! from it. Redis is not: it is another project's server, so what is honest here is a peer that
//! speaks the protocol this crate implements and answers from a script. That makes the tests
//! deterministic - a pending list in a known state, a block that expires exactly when it should
//! - which a real Redis would not.
//!
//! What the script also gives is the thing worth asserting: **every command the sink sent, in
//! order.** The ordering rule this crate is built on - acknowledge after the write, never before
//! - is only checkable by looking at what was sent and what was not.

use std::io::{BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use big_embed::{Api, FieldKind, MemPager};
use big_http::{Server, ServerConfig};
use big_message::Config as ProducerConfig;
use big_message_redis::{resp, Config, Mapping, Sink};

// ---- bigdb ------------------------------------------------------------------------------

fn database() -> Api<MemPager> {
    let api = Api::in_memory().unwrap();
    api.create_table("tx").unwrap();
    api.create_field("tx", "amount", FieldKind::Int, 32).unwrap();
    api.create_field("tx", "country", FieldKind::Set, 0).unwrap();
    api.create_field("tx", "msg_id", FieldKind::Set, 0).unwrap();
    api
}

fn bigdb(connections: usize) -> SocketAddr {
    let server = Server::bind_with(database(), "127.0.0.1:0", ServerConfig::default()).unwrap();
    let addr = server.local_addr().unwrap();
    std::thread::spawn(move || {
        let _ = server.serve_n(connections);
    });
    addr
}

fn sql(addr: SocketAddr, statement: &str) -> String {
    let mut stream = TcpStream::connect(addr).unwrap();
    let request = format!(
        "POST /sql HTTP/1.1\r\nHost: {addr}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{statement}",
        statement.len()
    );
    stream.write_all(request.as_bytes()).unwrap();
    let mut answer = String::new();
    std::io::Read::read_to_string(&mut stream, &mut answer).unwrap();
    answer.split_once("\r\n\r\n").unwrap().1.to_string()
}

// ---- the scripted Redis -----------------------------------------------------------------

/// A peer that answers each command from a script and writes down what it was asked.
struct Fake {
    addr: SocketAddr,
    /// Every command received, as `["XACK", "events", ...]`.
    seen: Arc<Mutex<Vec<Vec<String>>>>,
}

/// What to answer, keyed by the command's first word. `XREADGROUP` answers are taken in order:
/// the first is the pending read the sink does on start-up, the rest are the loop's.
fn fake(reads: Vec<String>) -> Fake {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&seen);

    std::thread::spawn(move || {
        let Ok((socket, _)) = listener.accept() else { return };
        let mut reader = BufReader::new(socket.try_clone().unwrap());
        let mut out = socket;
        let mut reads = reads.into_iter();
        loop {
            let Ok(value) = resp::read_value(&mut reader) else { return };
            let Ok(args) = value.array() else { return };
            let words: Vec<String> =
                args.iter().filter_map(|a| a.text().ok()).map(str::to_string).collect();
            let Some(head) = words.first().map(|w| w.to_ascii_uppercase()) else { return };
            recorded.lock().unwrap().push(words.clone());

            let reply = match head.as_str() {
                "XGROUP" => "+OK\r\n".to_string(),
                // Every read after the script runs out is an expired block, which is what a
                // quiet stream answers and what `--once` stops on.
                "XREADGROUP" => reads.next().unwrap_or_else(|| "*-1\r\n".to_string()),
                "XACK" => format!(":{}\r\n", words.len().saturating_sub(3)),
                "XAUTOCLAIM" => "*2\r\n$3\r\n0-0\r\n*0\r\n".to_string(),
                _ => "+OK\r\n".to_string(),
            };
            if out.write_all(reply.as_bytes()).is_err() {
                return;
            }
        }
    });

    Fake { addr, seen }
}

impl Fake {
    fn commands(&self) -> Vec<Vec<String>> {
        self.seen.lock().unwrap().clone()
    }

    fn acked(&self) -> Vec<String> {
        self.commands()
            .into_iter()
            .filter(|c| c[0].eq_ignore_ascii_case("XACK"))
            .flat_map(|c| c.into_iter().skip(3))
            .collect()
    }
}

/// `[[stream, [[id, [field, value, ...]]]]]` as the bytes Redis would send.
fn entries(stream: &str, entries: &[(&str, &[(&str, &str)])]) -> String {
    let mut out = format!("*1\r\n*2\r\n${}\r\n{stream}\r\n*{}\r\n", stream.len(), entries.len());
    for (id, fields) in entries {
        out.push_str(&format!("*2\r\n${}\r\n{id}\r\n*{}\r\n", id.len(), fields.len() * 2));
        for (name, value) in *fields {
            out.push_str(&format!("${}\r\n{name}\r\n${}\r\n{value}\r\n", name.len(), value.len()));
        }
    }
    out
}

fn config(redis: SocketAddr, addr: SocketAddr, map: &[&str]) -> Config {
    Config {
        redis: redis.to_string(),
        password: None,
        stream: "events".to_string(),
        group: "g1".to_string(),
        consumer: "c1".to_string(),
        addr: addr.to_string(),
        token: None,
        table: "tx".to_string(),
        map: map.iter().map(|m| Mapping::parse(m).unwrap()).collect(),
        dedup_field: None,
        batch: 100,
        block: Duration::from_millis(10),
        claim_after: None,
        skip_incomplete: false,
    }
}

fn quick() -> ProducerConfig {
    ProducerConfig { io_timeout: Duration::from_secs(3), retries: 1, ..ProducerConfig::default() }
}

// ---- the tests --------------------------------------------------------------------------

#[test]
fn entries_become_rows_and_are_acknowledged() {
    let db = bigdb(8);
    let redis = fake(vec![
        "*-1\r\n".to_string(), // the start-up pending read: nothing left behind
        entries(
            "events",
            &[
                ("1700-0", &[("amount", "100"), ("country", "GB")]),
                ("1700-1", &[("amount", "900"), ("country", "US")]),
            ],
        ),
    ]);

    let mut sink =
        Sink::open(config(redis.addr, db, &["amount:int=amount", "country=country"]), quick())
            .unwrap();
    sink.recover().unwrap();
    let report = sink.run(true).unwrap();

    assert_eq!(report.written, 2);
    assert_eq!(report.acknowledged, 2);
    assert_eq!(sql(db, "SELECT count(*) FROM tx"), r#"{"columns":["count"],"rows":[[2]]}"#);
    assert_eq!(
        sql(db, "SELECT count(*) FROM tx WHERE country = 'GB'"),
        r#"{"columns":["count"],"rows":[[1]]}"#
    );
    assert_eq!(redis.acked(), vec!["1700-0".to_string(), "1700-1".to_string()]);
}

/// The ordering rule, checked from the side that can see it.
#[test]
fn a_write_the_server_refuses_acknowledges_nothing() {
    let db = bigdb(8);
    let redis =
        fake(vec!["*-1\r\n".to_string(), entries("events", &[("1700-0", &[("amount", "100")])])]);

    // A column the table does not have, so the flush is refused.
    let mut sink = Sink::open(config(redis.addr, db, &["amount:int=nope"]), quick()).unwrap();
    sink.recover().unwrap();
    let stopped = sink.run(true).unwrap_err();

    assert!(format!("{stopped}").contains("unknown_field"), "{stopped}");
    assert!(
        redis.acked().is_empty(),
        "an entry whose row was refused must stay pending: {:?}",
        redis.commands()
    );
    assert_eq!(sql(db, "SELECT count(*) FROM tx"), r#"{"columns":["count"],"rows":[[0]]}"#);
}

#[test]
fn the_dedup_column_carries_the_entrys_own_stream_id() {
    let db = bigdb(8);
    let redis =
        fake(vec!["*-1\r\n".to_string(), entries("events", &[("1700-7", &[("amount", "5")])])]);

    let mut config = config(redis.addr, db, &["amount:int=amount"]);
    config.dedup_field = Some("msg_id".to_string());
    let mut sink = Sink::open(config, quick()).unwrap();
    sink.recover().unwrap();
    sink.run(true).unwrap();

    // Written as an ordinary key, which is what makes a restart able to ask about it.
    assert_eq!(
        sql(db, "SELECT count(*) FROM tx WHERE msg_id = '1700-7'"),
        r#"{"columns":["count"],"rows":[[1]]}"#
    );
}

/// A restart finds its pending entries already written, and acknowledges them instead of
/// writing them twice.
#[test]
fn a_pending_entry_already_in_the_table_is_acknowledged_rather_than_rewritten() {
    let db = bigdb(16);
    // The row is already there, exactly as an interrupted run would have left it.
    sql(db, "INSERT INTO tx (amount, msg_id) VALUES (5,'1700-7')");

    let redis = fake(vec![entries("events", &[("1700-7", &[("amount", "5")])])]);
    let mut config = config(redis.addr, db, &["amount:int=amount"]);
    config.dedup_field = Some("msg_id".to_string());
    let mut sink = Sink::open(config, quick()).unwrap();
    sink.recover().unwrap();

    let report = sink.report();
    assert_eq!(report.deduplicated, 1);
    assert_eq!(report.written, 0, "the row was there; writing it again is the bug");
    assert_eq!(redis.acked(), vec!["1700-7".to_string()]);
    assert_eq!(sql(db, "SELECT count(*) FROM tx"), r#"{"columns":["count"],"rows":[[1]]}"#);
}

/// And without the dedup column there is nothing to ask, so the same restart writes it twice.
#[test]
fn without_a_dedup_column_a_pending_entry_is_written_again() {
    let db = bigdb(16);
    sql(db, "INSERT INTO tx (amount) VALUES (5)");

    let redis = fake(vec![entries("events", &[("1700-7", &[("amount", "5")])])]);
    let mut sink = Sink::open(config(redis.addr, db, &["amount:int=amount"]), quick()).unwrap();
    sink.recover().unwrap();

    // Two records for one message. This is the at-least-once bargain, pinned rather than
    // discovered: the server allocates the ids, so a repeat is a new record.
    assert_eq!(sql(db, "SELECT count(*) FROM tx"), r#"{"columns":["count"],"rows":[[2]]}"#);
    assert_eq!(redis.acked(), vec!["1700-7".to_string()]);
}

#[test]
fn an_entry_the_mapping_does_not_fit_stops_the_sink() {
    let db = bigdb(8);
    let redis =
        fake(vec!["*-1\r\n".to_string(), entries("events", &[("1700-0", &[("country", "GB")])])]);

    let mut sink = Sink::open(config(redis.addr, db, &["amount:int=amount"]), quick()).unwrap();
    sink.recover().unwrap();
    let stopped = sink.run(true).unwrap_err();

    let said = format!("{stopped}");
    assert!(said.contains("1700-0") && said.contains("amount"), "{said}");
    assert!(redis.acked().is_empty(), "nothing was written, so nothing is acknowledged");
}

#[test]
fn an_entry_the_mapping_does_not_fit_is_acknowledged_when_skipping_is_asked_for() {
    let db = bigdb(8);
    let redis = fake(vec![
        "*-1\r\n".to_string(),
        entries("events", &[("1700-0", &[("country", "GB")]), ("1700-1", &[("amount", "9")])]),
    ]);

    let mut config = config(redis.addr, db, &["amount:int=amount"]);
    config.skip_incomplete = true;
    let mut sink = Sink::open(config, quick()).unwrap();
    sink.recover().unwrap();
    let report = sink.run(true).unwrap();

    assert_eq!(report.skipped, 1);
    assert_eq!(report.written, 1);
    // Both acknowledged: an entry left pending would be delivered again for ever, and it will
    // be as wrong the next time.
    assert_eq!(redis.acked(), vec!["1700-0".to_string(), "1700-1".to_string()]);
    assert_eq!(sql(db, "SELECT count(*) FROM tx"), r#"{"columns":["count"],"rows":[[1]]}"#);
}

#[test]
fn a_sink_with_no_mapping_will_not_open() {
    let db = bigdb(1);
    let redis = fake(Vec::new());
    let mut config = config(redis.addr, db, &["amount:int=amount"]);
    config.map.clear();
    assert!(Sink::open(config, quick()).is_err());
}
