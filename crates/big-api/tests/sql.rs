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

use big_api::*;
use big_db::catalog::FieldKind;

/// The record's own column name, which `SELECT *` answers under.
fn big_sql_record_column() -> String {
    big_api::RECORD_COLUMN.to_string()
}

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
    assert_eq!(
        shape,
        Shape::Row { cells: vec![Cell::plain("count".to_string(), Of::Groups { plan: 0 })] }
    );
    assert_eq!(value.as_groups().unwrap().len(), 3);

    let (_, shape) = one(&api, "SELECT country, count(*) FROM tx GROUP BY country");
    assert_eq!(shape.columns(), vec!["country", "count"]);

    let (value, shape) = one(&api, "SELECT * FROM tx WHERE active = true");
    assert_eq!(shape, Shape::Records { column: big_sql_record_column(), limit: None });
    assert_eq!(value.as_rows().unwrap().cardinality(), 4);
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
        Shape::Row {
            cells: vec![
                Cell::plain("count".to_string(), Of::Value { plan: 0 }),
                // Resolved, because a shape that reached here has met the schema: `amount`
                // keeps no digits after the point, so its cells are plain.
                Cell::plain("sum", Of::Value { plan: 1 }),
                Cell::plain("max", Of::Value { plan: 2 }),
            ]
        }
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
        Shape::Row {
            cells: vec![
                Cell::plain("count".to_string(), Of::Value { plan: 0 }),
                Cell::plain("avg", Of::Ratio { plan: 1, over: 0 }),
            ]
        }
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
        Shape::Row { cells: vec![Cell::plain("count".to_string(), Of::Value { plan: 0 })] }
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
