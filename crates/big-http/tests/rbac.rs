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

//! **Privileges over a real socket**, which is the only place the two halves meet.
//!
//! `big-rbac` proves the decision, `big-embed` proves the administration, and the corpus proves
//! what a statement demands. None of them can prove the thing this file is about: that a request
//! arriving with a password ends up judged by the grants that password's role holds - through the
//! users file, the argon2 verification, the route's own guard, and the statement's demands, in
//! that order.
//!
//! Two surfaces reach the same resolver and neither can be dropped: `POST /sql`, where the
//! statement says what it needs, and the REST routes, which never reach `Cluster::run` at all and
//! would be an open door if only the SQL path were guarded.

mod common;
use common::{send_as, spawn_with_auth};

/// Hashes are slow on purpose, so every test here shares one and the fixture spells it out.
const PW: &str = "s3cret";

fn users() -> String {
    let hash = big_http::auth::hash_password(PW).unwrap();
    // `nobody` holds a role the catalog was never told about, which is the state an operator is
    // in between editing the users file and running `CREATE ROLE`.
    [
        format!("root superuser {hash}"),
        format!("reader analyst {hash}"),
        format!("writer loader {hash}"),
        format!("nobody not-made-yet {hash}"),
    ]
    .join("\n")
}

/// `sales` holds `orders`; `ops` holds `ledger`. Two databases, so a grant that leaked across
/// them would be visible rather than merely possible.
fn stocked(api: &big_embed::Api<big_pager::MemPager>) {
    api.create_database("sales").unwrap();
    api.create_database("ops").unwrap();
    api.create_table("sales.orders").unwrap();
    api.create_table("ops.ledger").unwrap();
    api.create_field("sales.orders", "amount", big_embed::FieldKind::Int, 32).unwrap();

    api.create_role("analyst").unwrap();
    api.set_grant(
        "analyst",
        Some("sales"),
        None,
        big_rbac::Privileges::from_iter([big_rbac::Privilege::Select]),
    )
    .unwrap();

    api.create_role("loader").unwrap();
    api.set_grant(
        "loader",
        Some("sales"),
        Some("orders"),
        big_rbac::Privileges::from_iter([big_rbac::Privilege::Select, big_rbac::Privilege::Insert]),
    )
    .unwrap();
}

fn sql(addr: std::net::SocketAddr, user: &str, statement: &str) -> (u16, String) {
    send_as(addr, user, PW, "POST", "/sql", statement)
}

#[test]
fn a_grant_reaches_the_statement_it_is_about() {
    let addr = spawn_with_auth(2, &users(), stocked);
    let (status, body) = sql(addr, "reader", "SELECT count(*) FROM sales.orders");
    assert_eq!(status, 200, "{body}");

    // The same statement against the database nobody granted them.
    let (status, body) = sql(addr, "reader", "SELECT count(*) FROM ops.ledger");
    assert_eq!(status, 403, "{body}");
    assert!(body.contains("analyst"), "the refusal names the role: {body}");
}

/// A privilege is per verb, not per level: reading is not most of the way to writing.
#[test]
fn a_reader_cannot_write_and_a_writer_cannot_change_the_schema() {
    let addr = spawn_with_auth(3, &users(), stocked);
    let (status, body) =
        sql(addr, "reader", "INSERT INTO sales.orders (_record_id, amount) VALUES (1, 5)");
    assert_eq!(status, 403, "{body}");

    let (status, body) =
        sql(addr, "writer", "INSERT INTO sales.orders (_record_id, amount) VALUES (1, 5)");
    assert_eq!(status, 200, "{body}");

    // `loader` holds SELECT and INSERT on the one table and nothing else.
    let (status, body) = sql(addr, "writer", "DROP TABLE sales.orders");
    assert_eq!(status, 403, "{body}");
}

/// A table-level grant does not spread to its neighbours, and a database-level one does not
/// spread to another database.
#[test]
fn a_grant_does_not_reach_past_the_object_it_names() {
    let addr = spawn_with_auth(2, &users(), |api| {
        stocked(api);
        api.create_table("sales.refunds").unwrap();
    });
    // `loader` was granted `sales.orders`, not `sales.*`.
    let (status, body) = sql(addr, "writer", "SELECT count(*) FROM sales.refunds");
    assert_eq!(status, 403, "{body}");
    // `analyst` was granted `sales.*`, so a table added later is included.
    let (status, body) = sql(addr, "reader", "SELECT count(*) FROM sales.refunds");
    assert_eq!(status, 200, "{body}");
}

/// The bootstrap. It holds everything without a single grant behind it, which is what a database
/// whose catalog has no roles yet is recovered through.
#[test]
fn the_reserved_role_holds_everything_without_being_granted_anything() {
    let addr = spawn_with_auth(3, &users(), stocked);
    for statement in [
        "SELECT count(*) FROM ops.ledger",
        "CREATE ROLE auditor",
        "GRANT SELECT ON ops.* TO auditor",
    ] {
        let (status, body) = sql(addr, "root", statement);
        assert_eq!(status, 200, "`{statement}`: {body}");
    }
}

/// **A users file naming a role the catalog does not have is not an error - it is no
/// privileges.** This is the state an upgrade leaves every existing deployment in, and the
/// server has to keep answering so that `superuser` can create the roles.
#[test]
fn a_role_the_catalog_never_heard_of_holds_nothing() {
    let addr = spawn_with_auth(2, &users(), stocked);
    // Authentication succeeds - the password is right.
    let (status, body) = sql(addr, "nobody", "SELECT count(*) FROM sales.orders");
    assert_eq!(status, 403, "not a 401: the credential is fine, the privileges are absent");
    assert!(body.contains("not-made-yet"), "{body}");

    // And a listing, which demands nothing, still answers.
    let (status, body) = sql(addr, "nobody", "SHOW DATABASES");
    assert_eq!(status, 200, "{body}");
}

/// **The REST routes never reach `Cluster::run`**, so a guard that only covered SQL would leave
/// them open. This is the test that would catch that.
#[test]
fn the_rest_routes_are_guarded_by_the_same_grants() {
    let addr = spawn_with_auth(4, &users(), stocked);
    // `analyst` may read `sales`...
    let (status, body) = send_as(addr, "reader", PW, "GET", "/table/sales.orders/records", "");
    assert_eq!(status, 200, "{body}");
    // ...and may not read `ops`.
    let (status, body) = send_as(addr, "reader", PW, "GET", "/table/ops.ledger/records", "");
    assert_eq!(status, 403, "{body}");
    // ...and may not create a table in it, which `POST /table/{t}` is.
    let (status, body) = send_as(addr, "reader", PW, "POST", "/table/sales.invoices", "");
    assert_eq!(status, 403, "{body}");
    // The reserved role can.
    let (status, body) = send_as(addr, "root", PW, "POST", "/table/sales.invoices", "");
    assert_eq!(status, 200, "{body}");
}

/// A bare table name in a path means the request's database, exactly as it does in a statement.
#[test]
fn a_rest_route_resolves_a_bare_name_against_the_requested_database() {
    let addr = spawn_with_auth(2, &users(), stocked);
    // The claim is about the *guard*, so what matters is refused or not - whether the handler
    // then finds the table is a different question with its own test.
    let (status, body) =
        send_as(addr, "reader", PW, "GET", "/table/orders/records?database=sales", "");
    assert_ne!(status, 403, "granted on `sales.*`, so the guard lets it through: {body}");
    let (status, body) =
        send_as(addr, "reader", PW, "GET", "/table/ledger/records?database=ops", "");
    assert_eq!(status, 403, "{body}");
}

/// Operating the server is one privilege, held on the server or nowhere - not something a role
/// that happened to be able to read tables also gets.
#[test]
fn reading_tables_does_not_make_somebody_an_operator() {
    let addr = spawn_with_auth(2, &users(), stocked);
    let (status, body) = send_as(addr, "reader", PW, "GET", "/metrics", "");
    assert_eq!(status, 403, "{body}");
    let (status, body) = send_as(addr, "root", PW, "GET", "/metrics", "");
    assert_eq!(status, 200, "{body}");
}

/// **`OPERATE` is grantable in SQL even though no statement demands it.** Denying it never
/// produces a query-side refusal - only a `403` on the routes it guards - and that asymmetry is
/// not a reason to keep it out of `GRANT`. This is the test that would catch a regression back
/// to refusing it at the parser.
#[test]
fn granting_operate_lets_a_reader_reach_the_operator_routes() {
    let addr = spawn_with_auth(4, &users(), stocked);
    let (status, body) = send_as(addr, "reader", PW, "GET", "/metrics", "");
    assert_eq!(status, 403, "{body}");

    let (status, body) = sql(addr, "root", "GRANT OPERATE ON *.* TO analyst");
    assert_eq!(status, 200, "{body}");

    let (status, body) = send_as(addr, "reader", PW, "GET", "/metrics", "");
    assert_eq!(status, 200, "no reload, no restart: {body}");
    let (status, body) = send_as(addr, "reader", PW, "GET", "/verify", "");
    assert_eq!(status, 200, "{body}");
}

/// Administering roles is `ROLES` on the server, which is not something a database-wide grant
/// includes - or the fence would not hold for one hop.
#[test]
fn granting_needs_the_privilege_to_grant() {
    let addr = spawn_with_auth(2, &users(), stocked);
    let (status, body) = sql(addr, "reader", "GRANT SELECT ON sales.* TO loader");
    assert_eq!(status, 403, "{body}");
    let (status, body) = sql(addr, "reader", "CREATE ROLE sneaky");
    assert_eq!(status, 403, "{body}");
}

/// No credential at all is a `401`, and it carries the header a client needs to fix it. A `403`
/// says the opposite thing - stop, this will never work - so the two must not be confused.
#[test]
fn no_credential_is_a_401_and_a_wrong_privilege_is_a_403() {
    let addr = spawn_with_auth(2, &users(), stocked);
    let (status, body) = common::send(addr, "POST", "/sql", "SELECT count(*) FROM sales.orders");
    assert_eq!(status, 401, "{body}");

    let (status, body) = sql(addr, "reader", "SELECT count(*) FROM ops.ledger");
    assert_eq!(status, 403, "{body}");
}

/// **A `403` shadows the `404` a missing table would have given**, and that is correct: the
/// privilege is checked before anything is planned, so answering `404` would make this surface a
/// way to ask which tables exist for somebody with no privilege to know.
#[test]
fn a_typo_in_a_database_nobody_may_read_is_a_403_not_a_404() {
    let addr = spawn_with_auth(2, &users(), stocked);
    let (status, body) = sql(addr, "reader", "SELECT count(*) FROM ops.no_such_table");
    assert_eq!(status, 403, "{body}");
    // The same typo where they *may* read is the ordinary missing-table answer.
    let (status, body) = sql(addr, "reader", "SELECT count(*) FROM sales.no_such_table");
    assert_eq!(status, 404, "{body}");
}

/// `EXPLAIN` demands what the statement demands, so it cannot be used to describe a statement
/// the caller may not make.
#[test]
fn explain_is_refused_wherever_the_statement_would_be() {
    let addr = spawn_with_auth(2, &users(), stocked);
    let (status, body) = sql(addr, "reader", "EXPLAIN SELECT count(*) FROM ops.ledger");
    assert_eq!(status, 403, "{body}");
    let (status, body) = sql(addr, "reader", "EXPLAIN SELECT count(*) FROM sales.orders");
    assert_eq!(status, 200, "{body}");
}

/// A grant is live: it applies to the next statement, with nothing reloaded and no restart. That
/// is the half of the model that is *not* true of the users file, and it is the reason grants are
/// in the catalog rather than beside the passwords.
#[test]
fn a_grant_applies_to_the_very_next_statement() {
    let addr = spawn_with_auth(3, &users(), stocked);
    let (status, _) = sql(addr, "reader", "SELECT count(*) FROM ops.ledger");
    assert_eq!(status, 403);

    let (status, body) = sql(addr, "root", "GRANT SELECT ON ops.* TO analyst");
    assert_eq!(status, 200, "{body}");

    let (status, body) = sql(addr, "reader", "SELECT count(*) FROM ops.ledger");
    assert_eq!(status, 200, "no reload, no restart: {body}");
}
