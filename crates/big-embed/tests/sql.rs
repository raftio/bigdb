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

//! The SQL surface against a real database, answering next to the query language it translates
//! into.
//!
//! Every test here asks the same question twice - once in SQL, once in PQL - and compares the
//! answers rather than checking the SQL one against a number written by hand. A number can be
//! wrong in both places; a disagreement between the two surfaces cannot be anything but a bug
//! in the translation.

use big_db::catalog::FieldKind;
use big_embed::*;

fn stocked() -> Api<big_pager::MemPager> {
    let api = Api::in_memory().unwrap();
    api.create_table("tx").unwrap();
    api.create_field("tx", "amount", FieldKind::Int, 32).unwrap();
    api.create_field("tx", "country", FieldKind::Set, 0).unwrap();
    api.create_field("tx", "active", FieldKind::Bool, 0).unwrap();
    let rows: [(u64, u64, &str, bool); 5] = [
        (1, 100, "GB", true),
        (2, 900, "US", false),
        (3, 500, "GB", true),
        (4, 700, "FR", true),
        (5, 200, "US", true),
    ];
    for (id, amount, country, active) in rows {
        api.import(
            "tx",
            &[
                Fact::Int { field: "amount", record: id, value: amount },
                Fact::Key { field: "country", record: id, value: country },
                Fact::Bool { field: "active", record: id, value: active },
            ],
        )
        .unwrap();
    }
    api
}

/// Asks both surfaces and insists they agree, returning the answer for a caller that wants to
/// check the number as well.
///
/// One question per statement here, which is what makes the comparison meaningful: the PQL side
/// has no way to ask two at once, so a statement that made several plans would be compared
/// against the first of them and the test would say less than it looks like it says.
fn both(api: &Api<big_pager::MemPager>, sql: &str, pql: &str) -> Value {
    let (from_sql, _) = api.sql(sql, &QueryOptions::default()).unwrap();
    let [from_sql] = from_sql.as_slice() else {
        panic!("`{sql}` made {} plans, and this comparison takes one", from_sql.len())
    };
    let from_pql = api.query("tx", pql).unwrap();
    assert_eq!(format!("{from_sql:?}"), format!("{from_pql:?}"), "\n  sql: {sql}\n  pql: {pql}\n");
    from_sql.clone()
}

/// The one answer a single-plan statement produced.
fn one(api: &Api<big_pager::MemPager>, sql: &str) -> (Value, Shape) {
    let (values, answer) = api.sql(sql, &QueryOptions::default()).unwrap();
    let [value] = values.as_slice() else {
        panic!("`{sql}` made {} plans, and this helper takes one", values.len())
    };
    (value.clone(), answer.shape)
}

#[test]
fn the_two_surfaces_answer_the_same_questions_identically() {
    let api = stocked();
    assert_eq!(
        both(&api, "SELECT count(*) FROM tx WHERE amount >= 500", "Count(Row(amount >= 500))")
            .as_count(),
        Some(3)
    );
    assert_eq!(
        both(
            &api,
            "SELECT count(*) FROM tx WHERE country = 'GB' AND active = true",
            "Count(Intersect(Row(country=\"GB\"), Row(active=true)))"
        )
        .as_count(),
        Some(2)
    );
    assert_eq!(
        both(&api, "SELECT sum(amount) FROM tx", "Sum(All(), field=amount)").as_sum(),
        Some(2400)
    );
    both(
        &api,
        "SELECT country, count(*) FROM tx GROUP BY country",
        "GroupBy(All(), field=country)",
    );
    both(
        &api,
        "SELECT country, count(*) AS n FROM tx GROUP BY country ORDER BY n DESC LIMIT 2",
        "TopN(All(), field=country, n=2)",
    );
    both(
        &api,
        "SELECT country, sum(amount) FROM tx GROUP BY country",
        "GroupBy(All(), field=country, aggregate=Sum(field=amount))",
    );
}

/// The shape is the part the plan does not carry, so it is the part worth asserting on its own.
#[test]
fn the_shape_says_how_the_answer_becomes_columns() {
    let api = stocked();
    let (value, shape) = one(&api, "SELECT count(DISTINCT country) FROM tx");
    // The plan is a `Distinct`; the counting is the shape's job, after any merge.
    assert_eq!(shape, Shape::row(vec![Cell::plain("count".to_string(), Of::Groups { plan: 0 })]));
    assert_eq!(value.as_groups().unwrap().len(), 3);

    let (_, shape) = one(&api, "SELECT country, count(*) FROM tx GROUP BY country");
    assert_eq!(shape.columns(), vec!["country", "count"]);

    // `SELECT *` is every column the table declares, in declaration order, and the header is
    // filled in against the schema - the statement was written before anything knew the table.
    let (value, shape) = one(&api, "SELECT * FROM tx WHERE active = true");
    assert_eq!(shape.columns(), vec!["amount", "country", "active"]);
    assert_eq!(value.as_table().unwrap().len(), 4);
}

/// A select list of several aggregates is several plans, and the shape says which cell reads
/// which.
///
/// The claim being tested is that nothing below `Api::sql` learns they were written together:
/// each plan is the plan that aggregate would have got on its own, which is what keeps the
/// merge - and every peer - unchanged.
#[test]
fn several_aggregates_are_several_plans_over_the_same_records() {
    let api = stocked();
    let (plans, _, answer) = api
        .plan_sql("SELECT count(*), sum(amount), max(amount) FROM tx WHERE active = true")
        .unwrap();
    assert_eq!(plans.len(), 3);
    assert_eq!(
        answer.shape,
        Shape::row(vec![
            Cell::plain("count".to_string(), Of::Value { plan: 0 }),
            // Resolved, because a shape that reached here has met the schema: `amount`
            // keeps no digits after the point, so its cells are plain.
            Cell::plain("sum", Of::Value { plan: 1 }),
            Cell::plain("max", Of::Value { plan: 2 }),
        ])
    );
    // Each plan is what the aggregate alone would have produced.
    for (i, alone) in [
        "SELECT count(*) FROM tx WHERE active = true",
        "SELECT sum(amount) FROM tx WHERE active = true",
        "SELECT max(amount) FROM tx WHERE active = true",
    ]
    .iter()
    .enumerate()
    {
        let (one, _, _) = api.plan_sql(alone).unwrap();
        assert_eq!(plans[i], one[0], "plan {i} differs from `{alone}`");
    }

    let (values, _) = api
        .sql(
            "SELECT count(*), sum(amount), max(amount) FROM tx WHERE active = true",
            &QueryOptions::default(),
        )
        .unwrap();
    // Records 1, 3, 4 and 5 are active: 100 + 500 + 700 + 200.
    assert_eq!(values[0].as_count(), Some(4));
    assert_eq!(values[1].as_sum(), Some(1500));
    assert_eq!(values[2].as_extreme(), Some(Some(700)));
}

/// The same question asked twice in one statement is asked once.
///
/// Not an optimisation so much as the obvious reading: `count(*)` and the count `avg` divides by
/// are the same number, and a statement that fanned out twice for it would be paying for a
/// second copy of an answer it already had.
#[test]
fn a_repeated_question_is_one_plan() {
    let api = stocked();
    let (plans, _, answer) = api.plan_sql("SELECT count(*), avg(amount) FROM tx").unwrap();
    assert_eq!(plans.len(), 2, "a count and a sum, with the count shared");
    assert_eq!(
        answer.shape,
        Shape::row(vec![
            Cell::plain("count".to_string(), Of::Value { plan: 0 }),
            Cell::plain("avg", Of::Ratio { plan: 1, over: 0 }),
        ])
    );
}

/// A select list longer than the fan-out allows is refused by name, before anything runs.
///
/// The cap is on *distinct* calls, because that is what the statement costs: identical ones
/// deduplicate into a single fan-out, so a select list that repeats itself is one question
/// however long it is written.
#[test]
fn a_statement_may_not_ask_for_more_plans_than_the_cap() {
    let api = Api::in_memory().unwrap();
    api.create_table("wide").unwrap();
    for i in 0..=big_sql::MAX_CALLS {
        api.create_field("wide", &format!("f{i}"), FieldKind::Int, 16).unwrap();
    }

    let list = |n: usize| (0..n).map(|i| format!("sum(f{i})")).collect::<Vec<_>>().join(", ");
    let (plans, _, _) =
        api.plan_sql(&format!("SELECT {} FROM wide", list(big_sql::MAX_CALLS))).unwrap();
    assert_eq!(plans.len(), big_sql::MAX_CALLS);

    let e =
        api.plan_sql(&format!("SELECT {} FROM wide", list(big_sql::MAX_CALLS + 1))).unwrap_err();
    assert_eq!(e.code(), "sql_too_many_aggregates");

    // Seventeen copies of one question is still one question.
    let repeated = (0..big_sql::MAX_CALLS + 1)
        .map(|i| format!("sum(f0) AS s{i}"))
        .collect::<Vec<_>>()
        .join(", ");
    let (plans, _, _) = api.plan_sql(&format!("SELECT {repeated} FROM wide")).unwrap();
    assert_eq!(plans.len(), 1);
}

/// A refusal is not a parse failure and not a storage failure, and the code says which.
#[test]
fn refusals_and_schema_errors_carry_their_codes_through_the_facade() {
    let api = stocked();
    let code = |sql: &str| {
        api.sql(sql, &QueryOptions::default()).err().map(|e| e.code()).unwrap_or("accepted")
    };
    // A join on a keyed column is answered; a cross join has no key to pair records on.
    assert_eq!(code("SELECT count(*) FROM tx, other"), "sql_no_joins");
    // A projection with no limit is a full scan rather than a refusal.
    assert_eq!(code("SELECT amount FROM tx"), "accepted");
    // A write reaching `Api::sql` is refused because this is the un-clustered door: a write
    // goes to the shard owners, and an id it does not name is allocated by the schema leader.
    // `Cluster::sql` is the one that runs it - see `Api::plan_sql`.
    assert_eq!(code("INSERT INTO tx (amount) VALUES (1)"), "sql_read_only");
    assert_eq!(code("INSERT INTO tx (_record_id, amount) VALUES (1, 2)"), "sql_read_only");
    // A column list is still required, and that is the parser's refusal rather than this door's.
    assert_eq!(code("INSERT INTO tx VALUES (1)"), "sql_insert_shape");
    // The same codes the query language gives, because they are the same mistakes.
    assert_eq!(code("SELECT count(*) FROM nope"), "unknown_table");
    assert_eq!(code("SELECT count(*) FROM tx WHERE nope = 1"), "unknown_field");
    assert_eq!(code("SELECT sum(country) FROM tx"), "operator_not_allowed");
    // **`EXPLAIN` gets its own refusal, and not the write one above.** This door resolves a
    // statement to plans; an explanation has none, because the whole of what it asks for is
    // that nothing runs. Telling the caller this surface is read-only would be telling them
    // something untrue about a statement that reads - `Cluster::sql` answers it, as rows.
    //
    // `big-sql/tests/gates.rs` excuses `sql_explain_rows` from its corpus on the strength of
    // this assertion, which is why it is here rather than left to the reader of the match.
    assert_eq!(code("EXPLAIN SELECT count(*) FROM tx"), "sql_explain_rows");
    assert_eq!(code("EXPLAIN CREATE TABLE never (a UINT(32))"), "sql_explain_rows");
    // ...and having been refused, it is still not a statement that ran.
    assert_eq!(code("SELECT count(*) FROM never"), "unknown_table");
}

/// Planning is pure, so a statement that will not resolve is refused without a read
/// transaction ever being opened - which is what lets a coordinator refuse before the network.
#[test]
fn a_statement_can_be_planned_without_being_run() {
    let api = stocked();
    let (plans, _, answer) = api.plan_sql("SELECT count(*) FROM tx WHERE amount > 100").unwrap();
    assert_eq!(plans[0].table(), "tx");
    assert_eq!(
        answer.shape,
        Shape::row(vec![Cell::plain("count".to_string(), Of::Value { plan: 0 })])
    );
    assert!(api.plan_sql("SELECT count(*) FROM nope").is_err());
}

/// A timeout bounds the statement, not each plan in it.
///
/// The number that matters is not how long this takes but what it is measured against: a
/// sixteen-plan statement handed the full budget sixteen times would hold a worker and a socket
/// for sixteen times the configured timeout, and no client could tell that from a slow query.
#[test]
fn the_wall_clock_budget_is_spent_once_across_every_plan_of_a_statement() {
    let api = stocked();
    let opts = QueryOptions {
        database: None,
        limits: None,
        // Already spent by the time the first plan runs, so the first one refuses rather than
        // the eighth. A zero budget is the storage layer's own deadline, not a second error
        // meaning the same thing.
        timeout: Some(std::time::Duration::ZERO),
        cancel: None,
        shards: None,
    };
    let e = api
        .sql("SELECT count(*), sum(amount), min(amount), max(amount) FROM tx", &opts)
        .unwrap_err();
    assert_eq!(e.code(), "query_timeout", "{e}");

    // And a budget that has not been spent lets every plan through.
    let opts = QueryOptions {
        database: None,
        limits: None,
        timeout: Some(std::time::Duration::from_secs(30)),
        cancel: None,
        shards: None,
    };
    let (values, _) =
        api.sql("SELECT count(*), sum(amount), min(amount), max(amount) FROM tx", &opts).unwrap();
    assert_eq!(values.len(), 4);
}

// ------------------------------------------------------------------------------------------
// Databases
//
// A namespace above tables. What these are about is the two things that makes true end to end:
// the same table name in two databases is two tables, and every statement written before
// databases existed still means what it meant.
// ------------------------------------------------------------------------------------------

/// `big-sql` and `big-db` each name the default database, in crates that cannot see each other -
/// `big-sql` links no storage. This is the assertion that keeps the two spellings one name.
#[test]
fn the_two_crates_agree_on_what_the_default_database_is_called() {
    assert_eq!(big_sql::DEFAULT_DATABASE, big_db::DEFAULT_DATABASE_NAME);
}

/// The whole feature, end to end: create a database, put a table in it, ask about it by its
/// qualified name, and drop it.
#[test]
fn a_table_in_a_database_is_reached_by_its_qualified_name() {
    let api = Api::in_memory().unwrap();
    api.create_database("sales").unwrap();
    api.create_table("sales.orders").unwrap();
    api.create_field("sales.orders", "amount", FieldKind::Int, 32).unwrap();
    api.import("sales.orders", &[Fact::Int { field: "amount", record: 1, value: 10 }]).unwrap();

    let opts = QueryOptions::default();
    let (values, _) = api.sql("SELECT count(*) FROM sales.orders", &opts).unwrap();
    assert_eq!(values[0].as_count(), Some(1));

    // The same statement without the qualifier is about a different table, which is not there.
    assert_eq!(api.sql("SELECT count(*) FROM orders", &opts).unwrap_err().code(), "unknown_table");

    // Unless the request says which database it is against, which is what `?database=` does.
    let scoped = QueryOptions { database: Some("sales".to_string()), ..QueryOptions::default() };
    let (values, _) = api.sql("SELECT count(*) FROM orders", &scoped).unwrap();
    assert_eq!(values[0].as_count(), Some(1));
}

/// Two databases holding the same table name hold two different tables, with disjoint data.
/// This is the property the whole namespace exists for, so it is asserted on the counts rather
/// than on the ids.
#[test]
fn the_same_name_in_two_databases_holds_different_data() {
    let api = Api::in_memory().unwrap();
    for (database, records) in [("sales", 1..=3u64), ("ops", 1..=7u64)] {
        api.create_database(database).unwrap();
        let table = format!("{database}.events");
        api.create_table(&table).unwrap();
        api.create_field(&table, "amount", FieldKind::Int, 32).unwrap();
        let facts: Vec<_> =
            records.map(|r| Fact::Int { field: "amount", record: r, value: r * 10 }).collect();
        api.import(&table, &facts).unwrap();
    }

    let opts = QueryOptions::default();
    let count = |sql: &str| api.sql(sql, &opts).unwrap().0[0].as_count().unwrap();
    assert_eq!(count("SELECT count(*) FROM sales.events"), 3);
    assert_eq!(count("SELECT count(*) FROM ops.events"), 7);
}

/// A statement that names no database is about the default one, which is where every table
/// written before databases existed lives. The point of this test is that the common case did
/// not move.
#[test]
fn an_unqualified_name_still_means_the_default_database() {
    let api = stocked();
    let opts = QueryOptions::default();

    let bare = api.sql("SELECT count(*) FROM tx", &opts).unwrap().0[0].as_count();
    let qualified = api.sql("SELECT count(*) FROM default.tx", &opts).unwrap().0[0].as_count();
    assert_eq!(bare, qualified, "`tx` and `default.tx` are one table");

    // And `SHOW TABLES` finds it under the bare name it was created with.
    let set = api.show_tables();
    assert!(set.rows.iter().any(|r| r[0] == Datum::Text("tx".to_string())), "{set:?}");
}

/// A database that holds nothing is still in `SHOW DATABASES`. It was derived from the tables
/// once, which made a freshly created database invisible until something was put in it - the
/// one moment an operator is asking the question in order to confirm the `CREATE` landed.
#[test]
fn an_empty_database_is_still_listed() {
    let api = Api::in_memory().unwrap();
    api.create_database("sales").unwrap();

    let set = api.databases();
    let named = |n: &str| set.rows.iter().find(|r| r[0] == Datum::Text(n.to_string()));
    assert_eq!(named("sales").map(|r| &r[1]), Some(&Datum::Int(0)), "{set:?}");
    assert_eq!(named("default").map(|r| &r[1]), Some(&Datum::Int(0)), "{set:?}");

    // And the count is the tables it holds, once it holds one.
    api.create_table("sales.orders").unwrap();
    let set = api.databases();
    let sales = set.rows.iter().find(|r| r[0] == Datum::Text("sales".to_string()));
    assert_eq!(sales.map(|r| &r[1]), Some(&Datum::Int(1)), "{set:?}");
}

/// `DROP DATABASE` refuses while the database still holds tables, and `CASCADE` is the word
/// that takes them with it. The default is `RESTRICT` because this is one word away from being
/// the most expensive statement on this surface.
#[test]
fn dropping_a_database_that_holds_tables_needs_cascade() {
    let api = Api::in_memory().unwrap();
    api.create_database("sales").unwrap();
    api.create_table("sales.orders").unwrap();

    let e = api.drop_database("sales", false).unwrap_err();
    assert_eq!(e.code(), "database_not_empty", "{e}");
    assert!(e.to_string().contains("CASCADE"), "the refusal says what to write: {e}");

    assert!(api.drop_database("sales", true).unwrap());
    assert!(!api.databases_named("sales"));
    // And the table went with it, rather than being left unreachable.
    //
    // `unknown_table`, not `unknown_database`: the planner resolves a name through
    // `Catalog::lookup`, which answers `None` for an absent database and an absent table alike
    // - deliberately, because the planner has no concept of a database and giving it one would
    // put a namespace in `big-plan` to keep in step with this one. The two *are* told apart
    // where a caller can act on the difference: `Catalog::table_ref` and every DDL path.
    let opts = QueryOptions::default();
    assert_eq!(
        api.sql("SELECT count(*) FROM sales.orders", &opts).unwrap_err().code(),
        "unknown_table"
    );
}

/// The default database cannot be dropped: every table is in some database, and this is the one
/// that is always there to be in.
#[test]
fn the_default_database_cannot_be_dropped() {
    let api = Api::in_memory().unwrap();
    let e = api.drop_database(big_db::DEFAULT_DATABASE_NAME, true).unwrap_err();
    assert_eq!(e.code(), "drop_default_database", "{e}");
}

/// `SHOW CREATE TABLE` has to answer with a statement that recreates the table *where it is*.
/// An unqualified one would recreate it in whichever database the next request was against.
#[test]
fn show_create_answers_with_the_qualified_name() {
    let api = Api::in_memory().unwrap();
    api.create_database("sales").unwrap();
    api.create_table("sales.orders").unwrap();
    api.create_field("sales.orders", "amount", FieldKind::Int, 32).unwrap();

    let set = api.show_create("sales.orders").unwrap();
    let Datum::Text(statement) = &set.rows[0][0] else { panic!("a statement is text") };
    assert!(statement.starts_with("CREATE TABLE sales.orders"), "{statement}");

    // And it reads back through the parser as the same table.
    let big_sql::Sql::Ddl(big_sql::Ddl::CreateTable { database, table, .. }) =
        big_sql::translate(statement).unwrap()
    else {
        panic!("a CREATE TABLE is a schema change")
    };
    assert_eq!((database.as_deref(), table.as_str()), (Some("sales"), "orders"));
}

/// A cross-database join costs what a same-database join costs, because a join here pairs
/// records through the *string* a keyed column was interned from - and a string is the same
/// string whichever namespace the table holding it is in.
#[test]
fn a_join_reaches_across_databases() {
    let api = Api::in_memory().unwrap();
    for database in ["sales", "ops"] {
        api.create_database(database).unwrap();
        let table = format!("{database}.events");
        api.create_table(&table).unwrap();
        api.create_field(&table, "category", FieldKind::Set, 0).unwrap();
        api.import(&table, &[Fact::Key { field: "category", record: 1, value: "books" }]).unwrap();
    }

    let opts = QueryOptions::default();
    let (values, _) = api
        .sql(
            "SELECT count(*) FROM sales.events a JOIN ops.events b ON a.category = b.category",
            &opts,
        )
        .unwrap();
    // One record on each side sharing one key, so the join is one pair.
    assert_eq!(values.len(), 2, "one grouped count per side");
}

/// A `HAVING` with no `GROUP BY` decides whether the one row exists, against a real database.
///
/// Written against the rendered rows rather than the plan's answer, because the plan is not
/// where the decision happens: the count is the same number either way, and what the `HAVING`
/// changes is whether it is handed back. A test on the value would pass with the clause
/// ignored entirely.
///
/// The threshold is checked at the coordinator for a reason a single node cannot show: a total
/// under it on one node can be over it once the others have contributed. `big-cluster`'s own
/// tests hold that half; this one holds that the clause is applied at all, and in which
/// direction.
#[test]
fn an_ungrouped_having_keeps_the_row_or_empties_the_answer() {
    let api = stocked();
    let rows = |sql: &str| {
        let (values, answer) = api.sql(sql, &QueryOptions::default()).unwrap();
        result_set(&answer, &values).rows
    };

    // Five records, so the same statement either answers with the count or with nothing.
    assert_eq!(rows("SELECT count(*) FROM tx"), vec![vec![Datum::Int(5)]]);
    assert_eq!(rows("SELECT count(*) FROM tx HAVING count(*) > 3"), vec![vec![Datum::Int(5)]]);
    assert!(rows("SELECT count(*) FROM tx HAVING count(*) > 5").is_empty());
    // The boundary, in both directions - an off-by-one here is the whole of what could be wrong.
    assert_eq!(rows("SELECT count(*) FROM tx HAVING count(*) >= 5"), vec![vec![Datum::Int(5)]]);
    assert!(rows("SELECT count(*) FROM tx HAVING count(*) < 5").is_empty());

    // The `WHERE` runs first and the `HAVING` sees what it left: two records are `GB`.
    let gb = "SELECT count(*) FROM tx WHERE country = 'GB'";
    assert_eq!(rows(&format!("{gb} HAVING count(*) = 2")), vec![vec![Datum::Int(2)]]);
    assert!(rows(&format!("{gb} HAVING count(*) = 5")).is_empty());

    // An aggregate that reads a field, so the threshold goes through the same unit conversion a
    // `WHERE` comparison does rather than being compared as written.
    let total = "SELECT sum(amount) FROM tx";
    assert_eq!(rows(&format!("{total} HAVING sum(amount) > 2000")), vec![vec![Datum::Int(2400)]]);
    assert!(rows(&format!("{total} HAVING sum(amount) > 2400")).is_empty());
}

/// `now()`, whose value cannot live in a golden file.
///
/// The corpus can pin what `toDate` and `date_trunc` answer because their answers are functions
/// of the rows. This one is a function of the clock, so what is asserted here is the part that
/// is a property rather than a value: it is a moment, it is roughly the present, and every use
/// of it in one statement is the *same* moment - which is the claim the whole design rests on,
/// and the one a second clock read per call site would quietly break.
#[test]
fn now_is_one_instant_for_the_whole_statement() {
    let api = Api::in_memory().unwrap();
    api.create_table("e").unwrap();
    api.create_field("e", "ts", FieldKind::DateTime, 64).unwrap();
    api.import("e", &[Fact::Signed { field: "ts", record: 1, value: 1_700_000_000 }]).unwrap();

    let rows = |sql: &str| {
        let (values, answer) = api.sql(sql, &QueryOptions::default()).unwrap();
        result_set(&answer, &values).rows
    };

    // A moment, rendered as one, and not the bare count of seconds it is stored as.
    let got = rows("SELECT now(), now() FROM e");
    let [row] = &got[..] else { panic!("expected one row") };
    let [Datum::Timestamp(a), Datum::Timestamp(b)] = row[..] else {
        panic!("expected two timestamps in one row, got {row:?}")
    };
    assert_eq!(a, b, "two `now()`s in one statement are two reads of one clock");

    // Roughly the present: after this feature was written and not absurdly far ahead. A wide
    // window on purpose - this is checking that a clock was read at all, not what it said.
    assert!((1_760_000_000..4_000_000_000).contains(&a), "{a} is not a plausible now");

    // The same instant reaches a `WHERE`, which is the other half of the claim: the value there
    // comes from the parser rather than from a second read at the coordinator.
    let before = rows("SELECT count(*) FROM e WHERE ts < now()");
    let [row] = &before[..] else { panic!("expected one row") };
    let [Datum::Int(n)] = row[..] else { panic!("expected one count, got {row:?}") };
    assert_eq!(n, 1, "a record written in 2023 is before now");
    let after = rows("SELECT count(*) FROM e WHERE ts > now()");
    let [row] = &after[..] else { panic!("expected one row") };
    let [Datum::Int(n)] = row[..] else { panic!("expected one count, got {row:?}") };
    assert_eq!(n, 0);
}

/// **A statement may lower what the operator configured and never raise it.**
///
/// The one property that makes clamping silently defensible instead of a hole: an operator sets
/// a ceiling so that no client can hold a worker for longer than that, and a `SETTINGS` clause
/// that could raise it would make every such ceiling advisory. Asserted over both directions of
/// each key, because "takes the minimum" is only half a claim if nothing checks the half where
/// the client asked for less.
#[test]
fn a_settings_clause_only_ever_narrows_what_the_operator_allowed() {
    use std::time::Duration;

    let configured = QueryOptions {
        timeout: Some(Duration::from_secs(10)),
        limits: Some(big_db::QueryLimits { max_bytes: 1_000, max_records: 100 }),
        ..QueryOptions::default()
    };

    // Asking for more of each: every one is clamped back to the operator's number.
    let more = configured.narrowed_by(&big_sql::Settings {
        max_execution_time: Some(3_600),
        max_memory_usage: Some(1_000_000),
        max_result_rows: Some(1_000_000),
        // Not narrowed by `QueryOptions` at all: it bounds a write, and `QueryOptions` carries
        // nothing about writes. Set here so the struct stays exhaustive and a key added later
        // has to be thought about rather than defaulted past.
        max_delete_records: None,
    });
    assert_eq!(more.timeout, Some(Duration::from_secs(10)));
    assert_eq!(more.limits.unwrap().max_bytes, 1_000);
    assert_eq!(more.limits.unwrap().max_records, 100);

    // Asking for less: the statement's own number wins, which is the point of the clause.
    let less = configured.narrowed_by(&big_sql::Settings {
        max_execution_time: Some(2),
        max_memory_usage: Some(500),
        max_result_rows: Some(5),
        max_delete_records: None,
    });
    assert_eq!(less.timeout, Some(Duration::from_secs(2)));
    assert_eq!(less.limits.unwrap().max_bytes, 500);
    assert_eq!(less.limits.unwrap().max_records, 5);

    // A key the statement did not write is left exactly as configured, rather than reset to a
    // default by the act of writing a different key.
    let one = configured.narrowed_by(&big_sql::Settings {
        max_result_rows: Some(5),
        ..big_sql::Settings::default()
    });
    assert_eq!(one.timeout, Some(Duration::from_secs(10)), "a deadline nobody named survived");
    assert_eq!(one.limits.unwrap().max_bytes, 1_000, "a ceiling nobody named survived");

    // With nothing configured, the storage layer's defaults are still the ceiling: being the
    // first to name a number must not be a way to raise one.
    let unconfigured = QueryOptions::default();
    let asked = unconfigured.narrowed_by(&big_sql::Settings {
        max_memory_usage: Some(u64::MAX),
        max_result_rows: Some(u64::MAX),
        ..big_sql::Settings::default()
    });
    let default = big_db::QueryLimits::default();
    assert_eq!(asked.limits.unwrap().max_bytes, default.max_bytes);
    assert_eq!(asked.limits.unwrap().max_records, default.max_records);
    // ...but a deadline genuinely is unbounded until somebody names one, so this is the one
    // field a statement can introduce rather than only tighten.
    assert_eq!(
        unconfigured
            .narrowed_by(&big_sql::Settings {
                max_execution_time: Some(30),
                ..big_sql::Settings::default()
            })
            .timeout,
        Some(Duration::from_secs(30))
    );
}
