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

//! What a statement lowers to: the calls, and the shape that reads their answers.

use super::common::*;
use big_sql::{Columns, Selected, Units};

/// The six questions the analytical benchmark asks, in both languages.
///
/// These are the ones that matter most, because they are the ones the report puts in a table:
/// if SQL and PQL plan them differently then the two columns are not measuring one engine.
#[test]
fn the_six_benchmark_questions_plan_identically() {
    same("SELECT count(*) FROM t WHERE amount >= 786432", "Count(Row(amount >= 786432))");
    same(
        "SELECT count(*) FROM t WHERE country = 'c07' AND active = true AND amount >= 786432",
        "Count(Intersect(Row(country=\"c07\"), Row(active=true), Row(amount >= 786432)))",
    );
    same(
        "SELECT sum(amount) FROM t WHERE amount >= 786432",
        "Sum(Row(amount >= 786432), field=amount)",
    );
    same("SELECT category, count(*) FROM t GROUP BY category", "GroupBy(All(), field=category)");
    same(
        "SELECT category, count(*) AS n FROM t GROUP BY category ORDER BY n DESC LIMIT 8",
        "TopN(All(), field=category, n=8)",
    );
    same(
        "SELECT count(DISTINCT category) FROM t WHERE amount >= 786432",
        "Distinct(Row(amount >= 786432), field=category)",
    );
}

#[test]
fn the_aggregates_and_their_shapes() {
    // A sum reads a field, so its cell remembers which one until a schema says what that
    // field keeps - see `Units`. `resolve` turns it into digits; translation never sees one.
    assert_eq!(
        translate("SELECT sum(amount) FROM t").unwrap().answer.shape,
        Shape::Row { cells: vec![measured("sum", Of::Value { plan: 0 }, "t", "amount")] }
    );
    assert_eq!(
        translate("SELECT min(amount) AS lowest FROM t").unwrap().answer.shape,
        Shape::Row { cells: vec![measured("lowest", Of::Value { plan: 0 }, "t", "amount")] }
    );
    same("SELECT max(amount) FROM t", "Max(All(), field=amount)");
    assert_eq!(
        translate("SELECT count(DISTINCT category) FROM t").unwrap().answer.shape,
        Shape::Row { cells: vec![Cell::plain("count".to_string(), Of::Groups { plan: 0 })] }
    );
    // `SELECT *` is unexpanded here: translation sees no schema, and only a schema knows what
    // the star came to. The limit rides along for the fall back to a record listing, which is
    // what a table with nothing readable resolves to.
    assert_eq!(
        translate("SELECT * FROM t WHERE active = true LIMIT 10").unwrap().answer.shape,
        Shape::Table { columns: Columns::All { table: "t".to_string(), limit: Some(10) } }
    );
    assert_eq!(
        translate("SELECT category, count(*) FROM t GROUP BY category").unwrap().answer.shape,
        Shape::Groups {
            keys: vec![0],
            cells: vec![
                Cell::plain("category".to_string(), Of::Key),
                Cell::plain("count".to_string(), Of::Group { plan: 0, absent: Absent::Zero }),
            ],
            having: None,
            order: None,
            cut: Cut::default(),
        }
    );
    // Column order follows the select list, because that is the order the caller asked for.
    assert_eq!(
        translate("SELECT count(*) AS n, category FROM t GROUP BY category").unwrap().answer.shape,
        Shape::Groups {
            keys: vec![0],
            cells: vec![
                Cell::plain("n".to_string(), Of::Group { plan: 0, absent: Absent::Zero }),
                Cell::plain("category".to_string(), Of::Key),
            ],
            having: None,
            order: None,
            cut: Cut::default(),
        }
    );
    // A ranking is cut by the plan, so the shape does not cut it again.
    assert_eq!(
        translate(
            "SELECT category, count(*) FROM t GROUP BY category ORDER BY count(*) DESC LIMIT 3"
        )
        .unwrap()
        .answer
        .shape,
        Shape::Groups {
            keys: vec![0],
            cells: vec![
                Cell::plain("category".to_string(), Of::Key),
                Cell::plain("count".to_string(), Of::Group { plan: 0, absent: Absent::Zero }),
            ],
            having: None,
            order: None,
            cut: Cut::default(),
        }
    );
}

/// A projection lowers to the one call that reads values back, carrying its own cut.
///
/// The cut is in the *plan* and not in the shape, which is the whole decision: a projection
/// costs a point read per record per column, so a limit applied to the answer would be a limit
/// applied after paying for it.
#[test]
fn a_projection_carries_its_cut_in_the_plan() {
    same("SELECT amount FROM t LIMIT 100", "Project(All(), field=amount, n=100)");
    same(
        "SELECT amount, price FROM t WHERE country = 'GB' LIMIT 10",
        "Project(Row(country=\"GB\"), field=amount, field=price, n=10)",
    );
    // Column order and names follow the select list, aliases included - and each column
    // remembers the field it reads, so a decimal is scaled back on the way out. Unresolved
    // here, because translation sees no schema: `Shape::resolve` turns these into digits.
    assert_eq!(
        translate("SELECT amount AS a, price FROM t LIMIT 5").unwrap().answer.shape,
        Shape::Table {
            columns: Columns::Named(vec![
                Selected {
                    column: "a".to_string(),
                    units: Units::Written { table: "t".to_string(), field: "amount".to_string() },
                },
                Selected {
                    column: "price".to_string(),
                    units: Units::Written { table: "t".to_string(), field: "price".to_string() },
                },
            ]),
        }
    );
    // The two spellings of the cut must not both apply: the shape has no limit of its own.
    let Shape::Table { .. } = translate("SELECT amount FROM t LIMIT 5").unwrap().answer.shape
    else {
        panic!("expected a table")
    };
}

#[test]
fn the_lexer_reads_what_sql_writes() {
    // `''` is one quote, and case does not matter to a keyword.
    same("select COUNT(*) from t where country = 'it''s'", "Count(Row(country=\"it's\"))");
    // A comment to the end of the line.
    same("SELECT count(*) FROM t -- why\n WHERE amount > 5", "Count(Row(amount > 5))");
    // A quoted identifier is never a keyword.
    same("SELECT count(*) FROM \"t\" WHERE \"amount\" > 5", "Count(Row(amount > 5))");
}

#[test]
fn conditions_lower_to_the_set_operations_that_answer_them() {
    same(
        "SELECT count(*) FROM t WHERE amount > 5 OR amount < 2",
        "Count(Union(Row(amount > 5), Row(amount < 2)))",
    );
    same(
        "SELECT count(*) FROM t WHERE country IN ('GB', 'FR')",
        "Count(Union(Row(country=\"GB\"), Row(country=\"FR\")))",
    );
    // One value is the comparison it already was.
    same("SELECT count(*) FROM t WHERE country IN ('GB')", "Count(Row(country=\"GB\"))");
    same(
        "SELECT count(*) FROM t WHERE amount BETWEEN 5 AND 9",
        "Count(Intersect(Row(amount >= 5), Row(amount <= 9)))",
    );
    same("SELECT count(*) FROM t WHERE NOT active = true", "Count(Not(Row(active=true)))");
    same("SELECT count(*) FROM t", "Count(All())");
    // `<>` is `!=`.
    same("SELECT count(*) FROM t WHERE amount <> 5", "Count(Row(amount != 5))");
}

/// `a AND NOT b` is a difference, and the rewrite is worth stating: `Not` has to build the
/// table's exists row to complement against and `Difference` does not.
#[test]
fn a_negated_term_inside_a_conjunction_becomes_a_difference() {
    same(
        "SELECT count(*) FROM t WHERE amount > 5 AND NOT country = 'GB'",
        "Count(Difference(Row(amount > 5), Row(country=\"GB\")))",
    );
    same(
        "SELECT count(*) FROM t WHERE amount > 5 AND active = true AND NOT country = 'GB'",
        "Count(Difference(Intersect(Row(amount > 5), Row(active=true)), Row(country=\"GB\")))",
    );
    // Nothing but negations: the complement of what they select together.
    same(
        "SELECT count(*) FROM t WHERE NOT country = 'GB' AND NOT country = 'FR'",
        "Count(Not(Union(Row(country=\"GB\"), Row(country=\"FR\"))))",
    );
    // `NOT IN` is the same shape written another way.
    same(
        "SELECT count(*) FROM t WHERE amount > 5 AND country NOT IN ('GB', 'FR')",
        "Count(Difference(Row(amount > 5), Union(Row(country=\"GB\"), Row(country=\"FR\"))))",
    );
}

/// Nesting is flattened because the executor short-circuits a flat intersection that has
/// already emptied, and cannot see through a tree of two-term ones.
#[test]
fn associative_terms_flatten_into_one_variadic_call() {
    same(
        "SELECT count(*) FROM t WHERE (amount > 5 AND active = true) AND country = 'GB'",
        "Count(Intersect(Row(amount > 5), Row(active=true), Row(country=\"GB\")))",
    );
}

#[test]
fn a_decimal_is_resolved_against_the_field_scale_by_the_planner() {
    // `price > 12.50` on a field with two decimal places means `> 1250`, and the rule lives in
    // the planner where PQL already keeps it.
    same("SELECT count(*) FROM t WHERE price > 12.50", "Count(Row(price > 12.50))");
    let too_precise = translate("SELECT count(*) FROM t WHERE price > 12.505").unwrap();
    assert!(matches!(
        big_plan::plan("t", &too_precise.calls[0].call, &Stub),
        Err(big_plan::PlanError::TooPrecise { .. })
    ));
    same("SELECT count(*) FROM t WHERE balance > -5", "Count(Row(balance > -5))");
}
