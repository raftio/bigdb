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

//! The server driven over a real socket.
//!
//! Nothing is mocked: a listener is bound on a loopback port, requests go out over TCP and the
//! bytes coming back are parsed as text. A hand-written HTTP reader is exactly the kind of
//! thing that passes a unit test and fails on a real client.

mod common;
use common::{send, spawn};

use std::io::{Read, Write};
use std::net::TcpStream;

#[test]
fn a_database_can_be_built_and_queried_over_http() {
    let addr = spawn(7);

    assert_eq!(send(addr, "POST", "/table/tx", "").0, 200);
    assert_eq!(send(addr, "POST", "/table/tx/field/amount?kind=int&bit_depth=32", "").0, 200);
    assert_eq!(send(addr, "POST", "/table/tx/field/country?kind=set", "").0, 200);

    let (status, body) = send(
        addr,
        "POST",
        "/table/tx/import",
        "amount 1 100\ncountry 1 GB\namount 2 900\ncountry 2 US\n",
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, r#"{"imported":4}"#);

    let (_, body) = send(addr, "POST", "/table/tx/query", r#"Count(Row(country="GB"))"#);
    assert_eq!(body, r#"{"count":1}"#);

    let (_, body) = send(addr, "POST", "/table/tx/query", r#"Sum(All(), field="amount")"#);
    assert_eq!(body, r#"{"sum":1000}"#);

    let (_, body) = send(addr, "GET", "/schema", "");
    assert!(body.contains(r#""name":"tx""#), "{body}");
    assert!(body.contains(r#""name":"amount""#), "{body}");
}

/// Listing and paging.
///
/// The response envelope changed shape for every query that returns records - `next` is now
/// always present - so the first assertion here is about the shape, not the paging. A client
/// that never asks for a page must still get every record it used to.
#[test]
fn records_can_be_listed_and_paged() {
    let addr = spawn(12);

    assert_eq!(send(addr, "POST", "/table/tx", "").0, 200);
    assert_eq!(send(addr, "POST", "/table/tx/field/amount?kind=int&bit_depth=32", "").0, 200);
    let facts: String = (1..=5).map(|i| format!("amount {i} {i}\n")).collect();
    assert_eq!(send(addr, "POST", "/table/tx/import", &facts).0, 200);

    // No paging asked for: everything, and a null cursor.
    let (status, body) = send(addr, "GET", "/table/tx/records", "");
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, r#"{"records":[1,2,3,4,5],"next":null}"#);

    // A full page reports a cursor even when it is the last one. The listing asked for two ids
    // and got two; whether a third exists costs another read, and the client finds out from the
    // empty page rather than from a shard read on every page of the scan.
    let (_, body) = send(addr, "GET", "/table/tx/records?limit=2", "");
    assert_eq!(body, r#"{"records":[1,2],"next":2}"#);

    let (_, body) = send(addr, "GET", "/table/tx/records?limit=2&after=2", "");
    assert_eq!(body, r#"{"records":[3,4],"next":4}"#);

    let (_, body) = send(addr, "GET", "/table/tx/records?limit=2&after=4", "");
    assert_eq!(body, r#"{"records":[5],"next":null}"#);

    // A query that returns records pages too, and knows when it has run out - it holds the
    // whole result, so it can see one id past the page.
    let (_, body) = send(addr, "POST", "/table/tx/query?limit=2", "All()");
    assert_eq!(body, r#"{"records":[1,2],"next":2}"#);

    let (_, body) = send(addr, "POST", "/table/tx/query?after=2", "All()");
    assert_eq!(body, r#"{"records":[3,4,5],"next":null}"#);

    // Paging a query that does not return records is refused rather than ignored: a client
    // sending `limit` believes it is paging, and answering with an unpaged count would answer a
    // question it did not ask.
    let (status, body) = send(addr, "POST", "/table/tx/query?limit=2", "Count(All())");
    assert_eq!(status, 422, "{body}");
    assert!(body.contains("not_pageable"), "{body}");

    let (status, body) = send(addr, "GET", "/table/tx/records?limit=nope", "");
    assert_eq!(status, 422, "{body}");
    assert!(body.contains("bad_parameter"), "{body}");

    let (status, _) = send(addr, "GET", "/table/nope/records", "");
    assert_eq!(status, 404);
}

/// Every class of client mistake gets the status that describes it, rather than one `400` for
/// all of them. The distinctions here are the point: a proxy's retry policy and a client's
/// error handling both read the status and nothing else.
#[test]
fn client_errors_get_the_status_that_describes_them() {
    let addr = spawn(6);
    send(addr, "POST", "/table/tx", "");

    // Unparseable text is the only thing that is really "malformed".
    let (status, body) = send(addr, "POST", "/table/tx/query", "Row(((");
    assert_eq!(status, 400, "{body}");
    assert!(body.contains(r#""code":"parse_error""#), "{body}");

    // A query that parses but names a field that is not there is well formed and wrong: 422.
    // Not 404 - the URI it was sent to exists, and saying otherwise would send a client
    // looking for a missing endpoint.
    let (status, body) = send(addr, "POST", "/table/tx/query", "Row(nope > 1)");
    assert_eq!(status, 422, "{body}");
    assert!(body.contains(r#""code":"unknown_field""#), "{body}");

    // A table is what the URI names, so a missing one really is a missing resource.
    let (status, body) = send(addr, "POST", "/table/ghost/query", "Count(All())");
    assert_eq!(status, 404, "{body}");
    assert!(body.contains(r#""code":"unknown_table""#), "{body}");

    // An unknown route is a 404, not a 400: the request was fine, there is nothing there.
    assert_eq!(send(addr, "GET", "/nope", "").0, 404);

    // A malformed field declaration is refused before it can create anything.
    let (status, body) = send(addr, "POST", "/table/tx/field/x?kind=nonsense", "");
    assert_eq!(status, 400, "{body}");
    assert!(body.contains(r#""code":"bad_parameter""#), "{body}");
}

/// A row key can hold a quote or a newline, and the response has to stay valid JSON.
#[test]
fn user_data_is_escaped_on_the_way_out() {
    let addr = spawn(4);
    send(addr, "POST", "/table/tx", "");
    send(addr, "POST", "/table/tx/field/label?kind=set", "");
    send(addr, "POST", "/table/tx/import", "label 1 he said \"hi\"\n");

    let (status, body) = send(addr, "POST", "/table/tx/query", r#"Distinct(All(), field="label")"#);
    assert_eq!(status, 200, "{body}");
    assert!(body.contains(r#""he said \"hi\"""#), "quote must be escaped: {body}");
}

/// The body limit is checked against the declared length before anything is allocated, so a
/// client cannot ask the process to reserve a gigabyte by claiming it will send one.
#[test]
fn an_oversized_body_is_refused_without_being_read() {
    let addr = spawn(1);
    let mut stream = TcpStream::connect(addr).unwrap();
    stream
        .write_all(b"POST /table/tx/query HTTP/1.1\r\nHost: x\r\nContent-Length: 999999999\r\n\r\n")
        .unwrap();
    stream.flush().unwrap();

    let mut raw = String::new();
    stream.read_to_string(&mut raw).unwrap();
    assert!(raw.starts_with("HTTP/1.1 413 "), "{raw}");
}

#[test]
fn records_can_be_deleted_over_http() {
    let addr = spawn(6);
    send(addr, "POST", "/table/tx", "");
    send(addr, "POST", "/table/tx/field/amount?kind=int&bit_depth=20", "");
    send(addr, "POST", "/table/tx/import", "amount 1 10\namount 2 20\namount 3 30\n");

    let (status, body) = send(addr, "POST", "/table/tx/delete", "2\n3\n");
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, "{\"deleted\":2}");

    let (_, body) = send(addr, "POST", "/table/tx/query", "Count(All())");
    assert!(body.contains('1'), "one record should be left, got {body}");
}

#[test]
fn deleting_an_unwritten_record_is_not_an_error() {
    let addr = spawn(3);
    send(addr, "POST", "/table/tx", "");
    send(addr, "POST", "/table/tx/field/amount?kind=int&bit_depth=20", "");

    // Retrying a delete has to be safe, so "already gone" is a success with a count of zero.
    let (status, body) = send(addr, "POST", "/table/tx/delete", "42\n");
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, "{\"deleted\":0}");
}

#[test]
fn a_malformed_delete_line_is_refused_before_anything_is_removed() {
    let addr = spawn(5);
    send(addr, "POST", "/table/tx", "");
    send(addr, "POST", "/table/tx/field/amount?kind=int&bit_depth=20", "");
    send(addr, "POST", "/table/tx/import", "amount 1 10\n");

    let (status, body) = send(addr, "POST", "/table/tx/delete", "1\nnot-a-number\n");
    assert_eq!(status, 400, "{body}");

    let (_, body) = send(addr, "POST", "/table/tx/query", "Count(All())");
    assert!(body.contains('1'), "the valid line must not have been applied, got {body}");
}

#[test]
fn a_field_can_be_dropped_over_http() {
    let addr = spawn(6);
    send(addr, "POST", "/table/tx", "");
    send(addr, "POST", "/table/tx/field/amount?kind=int&bit_depth=20", "");
    send(addr, "POST", "/table/tx/field/note?kind=set", "");

    let (status, body) = send(addr, "DELETE", "/table/tx/field/note", "");
    assert_eq!(status, 200, "{body}");

    let (_, schema) = send(addr, "GET", "/schema", "");
    assert!(schema.contains("amount"), "{schema}");
    assert!(!schema.contains("note"), "the dropped field must be gone from the schema: {schema}");
}

#[test]
fn a_table_can_be_dropped_over_http() {
    let addr = spawn(4);
    send(addr, "POST", "/table/tx", "");
    send(addr, "POST", "/table/tx/field/amount?kind=int&bit_depth=20", "");

    let (status, _) = send(addr, "DELETE", "/table/tx", "");
    assert_eq!(status, 200);

    let (_, schema) = send(addr, "GET", "/schema", "");
    assert!(!schema.contains("tx"), "{schema}");
}

#[test]
fn dropping_what_is_not_there_is_a_404() {
    let addr = spawn(3);
    send(addr, "POST", "/table/tx", "");

    assert_eq!(send(addr, "DELETE", "/table/nope", "").0, 404);
    assert_eq!(send(addr, "DELETE", "/table/tx/field/nope", "").0, 404);
}

/// A connection carries more than one request when the client asks for it, and the answers
/// come back in order.
///
/// This is what makes the fan-out between nodes affordable: without it a coordinator pays a
/// handshake per peer per request.
#[test]
fn a_connection_can_carry_more_than_one_request() {
    let addr = spawn(1);
    let mut stream = TcpStream::connect(addr).unwrap();

    for _ in 0..3 {
        let request = "GET /health HTTP/1.1\r\nHost: localhost\r\n\
                       Connection: keep-alive\r\nContent-Length: 0\r\n\r\n";
        stream.write_all(request.as_bytes()).unwrap();
        stream.flush().unwrap();

        let (status, body, connection) = read_one(&mut stream);
        assert_eq!(status, 200, "{body}");
        assert_eq!(body, r#"{"status":"ok"}"#);
        assert_eq!(connection.as_deref(), Some("keep-alive"), "the server closed early");
    }
}

/// **Keep-alive is opt-in, which is not what HTTP/1.1 says.** A persistent connection holds a
/// worker from a fixed pool, so a client that says nothing gets what it has always got.
#[test]
fn a_client_that_does_not_ask_gets_one_request_per_connection() {
    let addr = spawn(1);
    let mut stream = TcpStream::connect(addr).unwrap();
    let request = "GET /health HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\r\n";
    stream.write_all(request.as_bytes()).unwrap();
    stream.flush().unwrap();

    let (status, _, connection) = read_one(&mut stream);
    assert_eq!(status, 200);
    assert_eq!(connection.as_deref(), Some("close"));

    // And the close is real: nothing more comes off this socket.
    let mut rest = Vec::new();
    stream.read_to_end(&mut rest).unwrap();
    assert!(rest.is_empty(), "{rest:?}");
}

/// One response off a connection that may carry another, so the body is read by its declared
/// length rather than by waiting for the socket to close.
fn read_one(stream: &mut TcpStream) -> (u16, String, Option<String>) {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        assert_eq!(stream.read(&mut byte).unwrap(), 1, "the connection ended mid-header");
        head.push(byte[0]);
    }
    let head = String::from_utf8(head).unwrap();
    let mut lines = head.lines();
    let status: u16 = lines.next().unwrap().split(' ').nth(1).unwrap().parse().unwrap();

    let header = |name: &str| {
        head.lines()
            .filter_map(|l| l.split_once(':'))
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.trim().to_string())
    };
    let length: usize = header("content-length").unwrap().parse().unwrap();
    let mut body = vec![0u8; length];
    stream.read_exact(&mut body).unwrap();
    (status, String::from_utf8(body).unwrap(), header("connection"))
}

/// A decimal reports its scale, because a decimal without one is an integer wearing a different
/// name: `price > 5` means `> 500` on a field with two of them, and a client reading the schema
/// had no way to know that.
#[test]
fn the_schema_says_what_a_number_means() {
    let addr = spawn(4);
    assert_eq!(send(addr, "POST", "/table/tx", "").0, 200);
    assert_eq!(
        send(addr, "POST", "/table/tx/field/price?kind=decimal&bit_depth=32&scale=2", "").0,
        200
    );
    assert_eq!(send(addr, "POST", "/table/tx/field/visit?kind=timequantum", "").0, 200);

    let (_, body) = send(addr, "GET", "/schema", "");
    assert!(body.contains(r#""name":"price","kind":"decimal","bit_depth":32,"scale":2"#), "{body}");
    // A field with neither carries neither: two fields that mean nothing would be two fields a
    // reader has to know which kinds to ignore them for.
    assert!(
        !body.contains(r#""name":"visit","kind":"timequantum","bit_depth":32,"scale""#),
        "{body}"
    );
}

/// The engine a table was created under is reported, because a client cannot derive it from
/// anything else it can see - and it is what decides whether a query it is about to write will
/// be answered by an index or by a scan.
#[test]
fn the_schema_says_which_engine_a_table_uses() {
    let addr = spawn(5);

    // No parameter means the default, which is both halves rather than the narrow one.
    assert_eq!(send(addr, "POST", "/table/plain", "").0, 200);
    assert_eq!(send(addr, "POST", "/table/cols?engine=columnar", "").0, 200);
    assert_eq!(send(addr, "POST", "/table/idx?engine=bitmap", "").0, 200);

    let (_, body) = send(addr, "GET", "/schema", "");
    assert!(body.contains(r#""name":"plain","engine":"bitmap+columnar""#), "{body}");
    assert!(body.contains(r#""name":"cols","engine":"columnar""#), "{body}");
    assert!(body.contains(r#""name":"idx","engine":"bitmap""#), "{body}");
}

/// An engine name nobody has is refused at the parameter rather than quietly becoming the
/// default. A caller who misspelled `columnar` wanted columns, and a table that silently came
/// back with an index instead is a cost surprise found much later.
#[test]
fn an_unknown_engine_is_refused_rather_than_defaulted() {
    let addr = spawn(1);
    let (status, body) = send(addr, "POST", "/table/tx?engine=colunmar", "");
    assert_eq!(status, 400, "{body}");
    assert!(body.contains("bad_parameter"), "{body}");
}

/// A table outside the default database, reached the two ways a client can spell it.
///
/// `/import` used to be reachable by neither. It looks its table up in the schema snapshot to
/// learn how each field reads a value, and that lookup compared the whole path segment against
/// `TableInfo::name` - which is the bare name - so `sales.orders` matched nothing. `?database=`
/// fared no better: it reached the RBAC guard and was then dropped, so the write resolved
/// `orders` in the default database and found nothing there either.
///
/// The other three table routes took the qualified path already, because the read and write
/// paths parse one (`TableRef::parse`); only the parameter was ignored. Both spellings now mean
/// the same thing on all four, which is what the route table has always said `?database=` does.
#[test]
fn a_table_in_another_database_is_reachable_by_path_and_by_parameter() {
    // Nine, counted: `spawn` serves exactly this many and then stops, so a test that sends one
    // too many fails on the request rather than by hanging.
    let addr = spawn(9);

    assert_eq!(send(addr, "POST", "/database/sales", "").0, 200);
    assert_eq!(send(addr, "POST", "/table/sales.orders", "").0, 200);
    assert_eq!(send(addr, "POST", "/table/sales.orders/field/amount?kind=int", "").0, 200);

    // Qualified in the path.
    let (status, body) = send(addr, "POST", "/table/sales.orders/import", "amount 1 100\n");
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, r#"{"imported":1}"#);

    // The same table, named by parameter instead.
    let (status, body) =
        send(addr, "POST", "/table/orders/import?database=sales", "amount 2 900\n");
    assert_eq!(status, 200, "{body}");

    // Both writes landed in the one table, so it holds two records either way it is asked.
    let (_, body) = send(addr, "POST", "/table/sales.orders/query", "Count(All())");
    assert_eq!(body, r#"{"count":2}"#);
    let (_, body) = send(addr, "POST", "/table/orders/query?database=sales", "Count(All())");
    assert_eq!(body, r#"{"count":2}"#);

    let (_, body) = send(addr, "GET", "/table/orders/records?database=sales", "");
    assert_eq!(body, r#"{"records":[1,2],"next":null}"#);

    let (status, body) = send(addr, "POST", "/table/orders/delete?database=sales", "1\n");
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, r#"{"deleted":1}"#);
}

/// A bare name still means the default database, even when another database has that table.
///
/// The half of the change that could go wrong quietly: folding `?database=` into the name must
/// not make an unqualified request start resolving somewhere else.
#[test]
fn an_unqualified_name_still_means_the_default_database() {
    let addr = spawn(10);

    assert_eq!(send(addr, "POST", "/database/sales", "").0, 200);
    for table in ["orders", "sales.orders"] {
        assert_eq!(send(addr, "POST", &format!("/table/{table}"), "").0, 200);
        assert_eq!(send(addr, "POST", &format!("/table/{table}/field/amount?kind=int"), "").0, 200);
    }

    // One record into each, by the two spellings.
    assert_eq!(send(addr, "POST", "/table/orders/import", "amount 1 1\n").0, 200);
    assert_eq!(send(addr, "POST", "/table/sales.orders/import", "amount 1 1\namount 2 2\n").0, 200);

    // They are two tables, and neither write reached the other.
    let (_, body) = send(addr, "POST", "/table/orders/query", "Count(All())");
    assert_eq!(body, r#"{"count":1}"#, "the default database's table");
    let (_, body) = send(addr, "POST", "/table/sales.orders/query", "Count(All())");
    assert_eq!(body, r#"{"count":2}"#, "the one in `sales`");

    // And a path that qualifies wins over a parameter that disagrees, being the more specific
    // of the two rather than an error nobody could act on.
    let (_, body) =
        send(addr, "POST", "/table/sales.orders/query?database=nosuchdb", "Count(All())");
    assert_eq!(body, r#"{"count":2}"#);
}
