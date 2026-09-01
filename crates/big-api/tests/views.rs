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

//! Substituting a stored `SELECT` for the name it is kept under.
//!
//! This is where a view is *read*, so it is where the claims about one live. `big-sql` holds no
//! catalog and can only pin the two statements and the body rule (`tests/testdata/view.test`);
//! everything below needs a stored statement to expand, which is what this file has.
//!
//! The expansion is asserted against the **query language the statement lowers to**, not against
//! a rewritten parse tree. That is the thing that has to be right: a view is only correct if
//! `FROM v` asks the engine exactly what writing the substitution by hand would have asked.

use big_api::{Api, Sql};
use big_db::FieldKind;
use big_pager::MemPager;

/// `tx` with four fields, a view over it, and a view over that.
///
/// `secret` exists to be the column no view exposes - a view is only a view if something is on
/// the other side of it.
fn fixture() -> Api<MemPager> {
    let api = Api::in_memory().unwrap();
    api.create_table("tx").unwrap();
    api.create_field("tx", "amount", FieldKind::Int, 32).unwrap();
    api.create_field("tx", "country", FieldKind::Set, 32).unwrap();
    api.create_field("tx", "secret", FieldKind::Set, 32).unwrap();
    api.create_view("big", "SELECT amount, country AS cc FROM tx WHERE amount >= 500", false)
        .unwrap();
    api
}

/// The query language one statement becomes, as one string per call.
///
/// Generic over the pager so the reopen case below, which needs a file, asserts the same way
/// every other case does.
fn calls<P: big_pager::PagerMut + Sync>(api: &Api<P>, sql: &str) -> String {
    match api.translate(sql).unwrap() {
        Sql::Query(s) => s
            .calls
            .iter()
            .map(|a| format!("{}: {:?}", a.table, a.call))
            .collect::<Vec<_>>()
            .join("|"),
        other => panic!("`{sql}` is not a query: {other:?}"),
    }
}

fn code<P: big_pager::PagerMut + Sync>(api: &Api<P>, sql: &str) -> String {
    match api.translate(sql) {
        Err(e) => e.code().to_string(),
        Ok(_) => panic!("`{sql}` was accepted"),
    }
}

/// **The whole of what a view is**: the base table underneath, and the body's `WHERE` applied
/// whether or not the reader wrote one.
#[test]
fn a_view_reads_its_base_table_through_its_own_filter() {
    let api = fixture();
    // No `WHERE` of its own, so the view's is the whole condition - and the table is `tx`.
    let bare = calls(&api, "SELECT count(*) FROM big");
    assert!(bare.starts_with("tx: "), "{bare}");
    assert!(bare.contains(r#"field: "amount", op: ">=", value: Int(500)"#), "{bare}");

    // With one, the two are intersected. Writing the substitution out by hand has to give the
    // same calls, which is the claim a view rests on.
    let read = calls(&api, "SELECT count(*) FROM big WHERE cc = 'GB'");
    let hand = calls(&api, "SELECT count(*) FROM tx WHERE amount >= 500 AND country = 'GB'");
    assert_eq!(read, hand);
}

/// A view renames, and the rename resolves wherever the reader wrote it.
#[test]
fn a_column_the_view_renamed_resolves_under_the_name_it_exposes() {
    let api = fixture();
    // In a `WHERE`, in the select list, in a `GROUP BY` - each has to be remapped, because a
    // name the view does not expose has to be refused wherever it was written.
    for sql in [
        "SELECT count(*) FROM big WHERE cc = 'GB'",
        "SELECT cc, count(*) FROM big GROUP BY cc",
        "SELECT count(DISTINCT cc) FROM big",
    ] {
        let out = calls(&api, sql);
        assert!(out.contains("country"), "`{sql}` did not reach `country`: {out}");
        assert!(!out.contains("\"cc\""), "`{sql}` left the exposed name in: {out}");
    }
    // The underlying name is *not* a second spelling: a view that renamed `country` to `cc`
    // exposes `cc`, and `country` is now one of the columns it hides.
    assert_eq!(code(&api, "SELECT count(*) FROM big WHERE country = 'GB'"), "sql_view_column");
}

/// **The feature, stated as a test.** A view that leaked the columns it does not name would not
/// be a view.
#[test]
fn a_column_the_view_does_not_expose_is_refused() {
    let api = fixture();
    for sql in [
        "SELECT secret FROM big",
        "SELECT count(*) FROM big WHERE secret = 'x'",
        "SELECT count(DISTINCT secret) FROM big",
        "SELECT cc FROM big GROUP BY cc ORDER BY secret",
        "SELECT count(*) FROM big b WHERE b.secret = 'x'",
    ] {
        assert_eq!(code(&api, sql), "sql_view_column", "{sql}");
    }
    // And the table underneath is untouched by any of it: what a view hides, it hides from
    // statements that go through it, not from the table.
    assert!(calls(&api, "SELECT count(*) FROM tx WHERE secret = 'x'").contains("secret"));
}

/// The view's name goes on as an alias, so a qualifier the reader wrote still resolves.
///
/// That is what makes the substitution invisible: nothing rewrites `b.cc` into anything, because
/// `b` is still the label of the source it names.
#[test]
fn a_qualifier_still_resolves_after_the_table_underneath_replaces_the_view() {
    let api = fixture();
    let aliased = calls(&api, "SELECT count(*) FROM big b WHERE b.cc = 'GB'");
    let plain = calls(&api, "SELECT count(*) FROM big WHERE cc = 'GB'");
    assert_eq!(aliased, plain);
    // The view's own name works as the qualifier too, when no alias replaced it.
    assert_eq!(calls(&api, "SELECT count(*) FROM big WHERE big.cc = 'GB'"), plain);
}

/// A view over a view is the same substitution twice, and both filters survive it.
#[test]
fn a_view_over_a_view_carries_both_filters_down_to_the_table() {
    let api = fixture();
    api.create_view("big_gb", "SELECT amount FROM big WHERE cc = 'GB'", false).unwrap();
    let read = calls(&api, "SELECT count(*) FROM big_gb");
    let hand = calls(&api, "SELECT count(*) FROM tx WHERE amount >= 500 AND country = 'GB'");
    assert_eq!(read, hand);
    // And the inner view's projection still narrows: `big_gb` exposes `amount` and nothing else,
    // so the column `big` exposed is now hidden one level further out.
    assert_eq!(code(&api, "SELECT count(*) FROM big_gb WHERE cc = 'GB'"), "sql_view_column");
}

/// Views nest to a bound, and past it the expansion is refused rather than run.
///
/// A cycle cannot be built - `CREATE VIEW` refuses a body naming something that is not there
/// yet - so this bound is not what stops one. What it bounds is the statement a `FROM` becomes:
/// each level is another filter ANDed on and another round of renaming, and a chain deep enough
/// to matter is a schema nobody arrived at on purpose. It also bounds a file some other tool
/// wrote, where the "cannot be built" argument does not hold.
#[test]
fn views_nested_past_the_bound_are_refused_rather_than_expanded() {
    let api = fixture();
    // A chain: each view reads the one before it, exposing the one column that survives.
    api.create_view("v0", "SELECT amount FROM big", false).unwrap();
    for i in 1..big_sql::MAX_VIEW_DEPTH + 2 {
        api.create_view(&format!("v{i}"), &format!("SELECT amount FROM v{}", i - 1), false)
            .unwrap();
    }
    // Well inside the bound, so it still expands all the way down to the table.
    assert!(calls(&api, "SELECT count(*) FROM v3").starts_with("tx: "));
    // Past it, and the refusal names the depth rather than failing somewhere further in.
    let deep = format!("SELECT count(*) FROM v{}", big_sql::MAX_VIEW_DEPTH + 1);
    assert_eq!(code(&api, &deep), "sql_view_depth");
}

/// **The silent one.** A view's body resolves in the view's own database, not the reader's.
///
/// A view written by somebody standing in `sales` says what it says; where a reader happens to
/// stand later cannot change which table it names. Getting this wrong answers from the wrong
/// table without erroring, which is why it is a test rather than a comment.
#[test]
fn a_views_body_resolves_in_the_views_own_database() {
    let api = Api::in_memory().unwrap();
    api.create_database("sales").unwrap();
    api.create_database("ops").unwrap();
    // The same table name in both, so a body resolved in the wrong one still finds *a* table -
    // which is exactly how this bug would hide.
    for db in ["sales", "ops"] {
        api.create_table(&format!("{db}.orders")).unwrap();
        api.create_field(&format!("{db}.orders"), "amount", FieldKind::Int, 32).unwrap();
    }
    // Created in `sales`, over a bare `orders` - which means `sales.orders`.
    api.create_view("sales.big", "SELECT amount FROM orders WHERE amount >= 500", false).unwrap();

    // Read from a request against `ops`. The body still means `sales.orders`.
    let Sql::Query(s) = api.translate_in("SELECT count(*) FROM sales.big", "ops").unwrap() else {
        panic!("a SELECT is a query")
    };
    assert_eq!(s.calls[0].table, "sales.orders");
}

/// `CREATE VIEW` refuses a body naming nothing, which is also what makes a cycle impossible.
#[test]
fn a_view_over_a_table_that_is_not_there_is_refused_at_creation() {
    let api = fixture();
    let e = api.create_view("v", "SELECT a FROM nope", false).unwrap_err();
    assert_eq!(e.code(), "unknown_table");
    // Nothing was stored, so the name is still free.
    assert!(api.views().iter().all(|v| v.name != "v"));
    // A view *over a view* is fine, because that one is already there.
    api.create_view("v", "SELECT amount FROM big", false).unwrap();
}

/// A view and a table share one namespace, because a `FROM` has to resolve to one of them.
#[test]
fn a_view_and_a_table_cannot_share_a_name() {
    let api = fixture();
    assert_eq!(
        api.create_view("tx", "SELECT amount FROM tx", false).unwrap_err().code(),
        "view_name_taken"
    );
    assert_eq!(api.create_table("big").unwrap_err().code(), "view_name_taken");
}

/// Redefining says so. An identical definition is idempotent; a different one needs the word.
#[test]
fn a_second_definition_is_refused_unless_it_says_or_replace() {
    let api = fixture();
    let body = "SELECT amount, country AS cc FROM tx WHERE amount >= 500";
    // The same statement again changes nothing and is not an error - the rule an identical
    // `CREATE TABLE` follows.
    assert!(!api.create_view("big", body, false).unwrap());

    let wider = "SELECT amount, country AS cc FROM tx WHERE amount >= 100";
    assert_eq!(api.create_view("big", wider, false).unwrap_err().code(), "view_redefined");
    api.create_view("big", wider, true).unwrap();
    // And the new statement is what reads now.
    assert!(calls(&api, "SELECT count(*) FROM big").contains("Int(100)"));
}

/// A view is forgotten by name, and forgetting one frees no pages - it owns none.
#[test]
fn dropping_a_view_leaves_the_table_underneath_exactly_as_it_was() {
    let api = fixture();
    let before = calls(&api, "SELECT count(*) FROM tx WHERE amount >= 500");
    assert!(api.drop_view("big").unwrap());
    assert!(!api.drop_view("big").unwrap(), "a second drop finds nothing");
    assert_eq!(calls(&api, "SELECT count(*) FROM tx WHERE amount >= 500"), before);
    // The name now expands to nothing and is left as written, so it reaches the planner as an
    // ordinary table name - and the planner is what says there is no such table. **Not a view
    // refusal**: translating still holds no opinion about which tables exist, which is what
    // keeps "this is not a statement" and "there is no such table" two different answers.
    assert_eq!(api.plan_sql("SELECT count(*) FROM big").unwrap_err().code(), "unknown_table");
}

/// A view survives a reopen, which is the whole point of it being in the catalog.
///
/// The body here is deliberately longer than the 104 bytes one catalog record holds, and carries
/// a multi-byte character, so what this really asserts is that the chunking round-trips: the
/// bytes are joined before they are validated, not one record at a time.
#[test]
fn a_view_longer_than_one_catalog_record_survives_a_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    // Long enough to span records, and the `é` is placed so that some chunk boundary falls
    // inside it for at least one of the lengths this produces.
    let body = format!(
        "SELECT amount, country AS a_rather_long_exposed_column_name_{} FROM tx WHERE country = 'écureuil'",
        "x".repeat(60)
    );
    assert!(body.len() > 104, "the body has to span records for this to test anything");
    {
        let api = Api::open(&path).unwrap();
        api.create_table("tx").unwrap();
        api.create_field("tx", "amount", FieldKind::Int, 32).unwrap();
        api.create_field("tx", "country", FieldKind::Set, 32).unwrap();
        api.create_view("long", &body, false).unwrap();
    }
    let api = Api::open(&path).unwrap();
    let views = api.views();
    assert_eq!(views.len(), 1);
    assert_eq!(views[0].text, body, "the statement came back changed");
    // And it still reads, which is the thing the text existing is for.
    assert!(calls(&api, "SELECT count(*) FROM long").contains("écureuil"));
}

/// A `DESCRIBE` of a view lists the columns it exposes, under the names it exposes them by.
#[test]
fn describing_a_view_lists_what_it_exposes_and_not_what_it_hides() {
    let api = fixture();
    let set = api.describe("big").unwrap();
    let names: Vec<String> = set
        .rows
        .iter()
        .map(|r| match &r[0] {
            big_api::Datum::Text(t) => t.clone(),
            other => panic!("a name is text: {other:?}"),
        })
        .collect();
    assert_eq!(names, ["amount", "cc"]);
    // The kind is the underlying column's, because that is what the bits are.
    assert_eq!(set.rows[1][1], big_api::Datum::Text("set".to_string()));
}

/// `SHOW TABLES` lists both, under a `type` column; `SHOW CREATE VIEW` answers with the stored
/// statement, and it round-trips through this crate's own parser.
#[test]
fn a_view_is_in_the_listings_and_writes_itself_back_out() {
    let api = fixture();
    let set = api.show_tables();
    assert_eq!(set.columns, ["name", "type", "engine", "fields"]);
    let row = set.rows.iter().find(|r| r[0] == big_api::Datum::Text("big".to_string())).unwrap();
    assert_eq!(row[1], big_api::Datum::Text("VIEW".to_string()));
    // A view stores nothing, so it has no engine to report rather than a made-up one.
    assert_eq!(row[2], big_api::Datum::Null);

    let set = api.show_create("big").unwrap();
    let big_api::Datum::Text(statement) = &set.rows[0][0] else { panic!("a statement is text") };
    // The gate `SHOW CREATE TABLE` has, applied here: what comes out has to read back as the
    // same view. Otherwise a schema somebody dumped and replayed is a different schema.
    let big_sql::Sql::Ddl(big_sql::Ddl::CreateView { name, body, .. }) =
        big_sql::translate(statement).unwrap()
    else {
        panic!("`{statement}` did not read back as a CREATE VIEW")
    };
    assert_eq!(name, "big");
    assert_eq!(body, api.views()[0].text);
}
