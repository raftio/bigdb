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

//! The clauses a shape carries: `ORDER BY`, `HAVING`, `LIMIT`, `OFFSET`, `DISTINCT`.

use super::common::*;

/// The `ORDER BY category` DuckDB is given for the same question is a no-op here, because
/// groups already come back ordered by key. Accepting it and doing nothing is the honest
/// answer; refusing it would refuse a statement whose result is already correct.
#[test]
fn ascending_by_the_grouped_column_is_the_order_groups_already_have() {
    same(
        "SELECT category, count(*) FROM t GROUP BY category ORDER BY category",
        "GroupBy(All(), field=category)",
    );
    same(
        "SELECT category, count(*) FROM t GROUP BY category ORDER BY category ASC",
        "GroupBy(All(), field=category)",
    );
    // And the shape is told to do nothing, rather than told to sort into the order the answer
    // already arrives in.
    assert_eq!(
        order_of("SELECT category, count(*) FROM t GROUP BY category ORDER BY category"),
        None
    );
}

/// The orderings the plan cannot carry, which the coordinator performs on the merged answer.
///
/// Each one asserts two things that have to agree: the plan is the ordinary grouped plan, with
/// no ranking folded into it, and the shape carries the sort. A shape that asked for a sort the
/// plan had already done would reshuffle a correct answer; a plan that ranked under a shape
/// that also sorted would pay for the ranking twice.
#[test]
fn an_ordering_the_plan_cannot_carry_is_done_after_the_merge() {
    // Descending by key: the same plan, sorted the other way round afterwards.
    same(
        "SELECT category, count(*) FROM t GROUP BY category ORDER BY category DESC",
        "GroupBy(All(), field=category)",
    );
    assert_eq!(
        order_of("SELECT category, count(*) FROM t GROUP BY category ORDER BY category DESC"),
        Some(GroupOrder { by: OrderBy::Key, desc: true })
    );

    // Ascending by count. `TopN` ranks downwards and only downwards, so this is a sort.
    same(
        "SELECT category, count(*) FROM t GROUP BY category ORDER BY count(*) ASC",
        "GroupBy(All(), field=category)",
    );
    assert_eq!(
        order_of("SELECT category, count(*) FROM t GROUP BY category ORDER BY count(*) ASC"),
        Some(GroupOrder {
            by: OrderBy::Value { of: Of::Group { plan: 0, absent: Absent::Zero } },
            desc: false
        })
    );

    // The query this phase exists for: the biggest totals, which is not the biggest counts.
    same(
        "SELECT category, sum(amount) FROM t GROUP BY category ORDER BY sum(amount) DESC",
        "GroupBy(All(), field=category, aggregate=Sum(field=amount))",
    );
    assert_eq!(
        order_of("SELECT category, sum(amount) FROM t GROUP BY category ORDER BY sum(amount) DESC"),
        Some(GroupOrder {
            by: OrderBy::Value { of: Of::Group { plan: 0, absent: Absent::Zero } },
            desc: true
        })
    );

    // An alias names either half, exactly as it does in the select list.
    assert_eq!(
        order_of(
            "SELECT category AS c, sum(amount) AS total FROM t GROUP BY category \
             ORDER BY total DESC"
        ),
        Some(GroupOrder {
            by: OrderBy::Value { of: Of::Group { plan: 0, absent: Absent::Zero } },
            desc: true
        })
    );
}

/// `ORDER BY count(*) DESC` stays a `TopN`, because that is the one ordering the plan can do
/// in the same pass it groups in.
///
/// The assertion that earns its place is the second: the shape must *not* also sort, or the
/// coordinator would redo work the plan has already done, on every query that ranks.
#[test]
fn the_ranking_stays_in_the_plan() {
    same(
        "SELECT category, count(*) FROM t GROUP BY category ORDER BY count(*) DESC LIMIT 3",
        "TopN(All(), field=category, n=3)",
    );
    assert_eq!(
        order_of("SELECT category, count(*) FROM t GROUP BY category ORDER BY count(*) DESC"),
        None
    );
}

/// An `OFFSET` over groups pages a list this surface has already materialised.
#[test]
fn an_offset_pages_the_groups_after_the_merge() {
    // The plan is asked for `offset + limit` groups, because the window wanted begins further
    // down the ranking than the limit alone describes. Asking for `n=10` and then skipping 20
    // would answer with nothing at all.
    same(
        "SELECT category, count(*) FROM t GROUP BY category ORDER BY count(*) DESC \
         LIMIT 10 OFFSET 20",
        "TopN(All(), field=category, n=30)",
    );
    let Shape::Groups { cut, .. } = translate(
        "SELECT category, count(*) FROM t GROUP BY category ORDER BY count(*) DESC \
         LIMIT 10 OFFSET 20",
    )
    .unwrap()
    .answer
    .shape
    else {
        panic!("expected a grouped shape")
    };
    // The shape takes the window out of the ranking the plan cut to, so it keeps both numbers.
    assert_eq!((cut.offset, cut.limit), (Some(20), Some(10)));

    // Without a ranking there is nothing for the plan to cut, and both numbers wait in the
    // shape over the whole grouping.
    same(
        "SELECT category FROM t GROUP BY category LIMIT 5 OFFSET 5",
        "Distinct(All(), field=category)",
    );
}

/// A `HAVING` on the aggregate the select list asked for, which is the only one the answer
/// holds a number for.
#[test]
fn having_reads_the_aggregate_the_answer_carries() {
    // The plan is unchanged: the filtering is a view of the answer, not a different question.
    same(
        "SELECT category, sum(amount) FROM t GROUP BY category HAVING sum(amount) >= 500",
        "GroupBy(All(), field=category, aggregate=Sum(field=amount))",
    );
    // And the threshold is still written, because only a schema knows what units it is in.
    let Shape::Groups { having: Some(h), .. } = translate(
        "SELECT category, sum(amount) FROM t GROUP BY category HAVING sum(amount) >= 500",
    )
    .unwrap()
    .answer
    .shape
    else {
        panic!("expected a grouped shape with a HAVING")
    };
    assert_eq!(h.op, ">=");
    assert_eq!(
        h.value,
        Threshold::Written {
            table: "t".to_string(),
            field: "amount".to_string(),
            value: Literal::Int(500),
        }
    );

    // A `HAVING` naming any other number is refused: the answer holds one per group.
    assert_eq!(
        code("SELECT category, sum(amount) FROM t GROUP BY category HAVING count(*) > 5"),
        "sql_unsupported"
    );
    assert_eq!(
        code("SELECT category, sum(amount) FROM t GROUP BY category HAVING max(amount) > 5"),
        "sql_unsupported"
    );
    assert_eq!(
        code("SELECT category, sum(amount) FROM t GROUP BY category HAVING sum(price) > 5"),
        "sql_unsupported"
    );
}

/// `SELECT DISTINCT c` is `SELECT c ... GROUP BY c`, which is what standard SQL says it is.
#[test]
fn select_distinct_is_the_grouping_it_is_defined_as() {
    same("SELECT DISTINCT category FROM t", "Distinct(All(), field=category)");
    // The two spellings must produce the same plan, not merely similar ones.
    assert_eq!(
        plan_sql("SELECT DISTINCT category FROM t"),
        plan_sql("SELECT category FROM t GROUP BY category")
    );
    assert_eq!(
        translate("SELECT DISTINCT category FROM t").unwrap().answer.shape,
        Shape::Groups {
            keys: vec![0],
            cells: vec![Cell { column: "category".to_string(), of: Of::Key }],
            having: None,
            order: None,
            cut: Cut::default(),
        }
    );
    // A `WHERE` narrows it exactly as it narrows the `GROUP BY` spelling.
    same(
        "SELECT DISTINCT category FROM t WHERE amount >= 500",
        "Distinct(Row(amount >= 500), field=category)",
    );
}

/// `HAVING` filters groups after the merge, so it lives in the shape and not in the plan.
#[test]
fn having_filters_groups_without_changing_the_plan() {
    // The plan is what it would have been without the `HAVING`: the filtering is a view of the
    // answer, not a different question to ask the index.
    same(
        "SELECT category, count(*) FROM t GROUP BY category HAVING count(*) > 5",
        "GroupBy(All(), field=category)",
    );
    // And with only the key asked for, the plan is the `Distinct` that lists them - the counts
    // it produces anyway are what the predicate reads.
    same(
        "SELECT category FROM t GROUP BY category HAVING count(*) > 5",
        "Distinct(All(), field=category)",
    );
    assert_eq!(
        translate("SELECT category, count(*) FROM t GROUP BY category HAVING count(*) >= 10")
            .unwrap()
            .answer
            .shape,
        Shape::Groups {
            keys: vec![0],
            cells: vec![
                Cell { column: "category".to_string(), of: Of::Key },
                Cell {
                    column: "count".to_string(),
                    of: Of::Group { plan: 0, absent: Absent::Zero }
                },
            ],
            having: Some(Having {
                of: Of::Group { plan: 0, absent: Absent::Zero },
                op: ">=",
                value: Threshold::Units(10),
            }),
            order: None,
            cut: Cut::default(),
        }
    );
}

/// A ranking under a `HAVING` cannot let the plan carry the cut.
///
/// `TopN(n=3)` picks three groups before anything is filtered, so a predicate applied afterwards
/// would return fewer than three - and SQL says `HAVING` runs first. The limit therefore waits
/// in the shape, and the plan is asked for the whole ranking.
#[test]
fn a_limit_under_having_waits_until_after_the_filter() {
    same(
        "SELECT category, count(*) FROM t GROUP BY category HAVING count(*) > 5 \
         ORDER BY count(*) DESC LIMIT 3",
        "TopN(All(), field=category)",
    );
    assert_eq!(
        translate(
            "SELECT category, count(*) FROM t GROUP BY category HAVING count(*) > 5 \
             ORDER BY count(*) DESC LIMIT 3"
        )
        .unwrap()
        .answer
        .shape,
        Shape::Groups {
            keys: vec![0],
            cells: vec![
                Cell { column: "category".to_string(), of: Of::Key },
                Cell {
                    column: "count".to_string(),
                    of: Of::Group { plan: 0, absent: Absent::Zero }
                },
            ],
            having: Some(Having {
                of: Of::Group { plan: 0, absent: Absent::Zero },
                op: ">",
                value: Threshold::Units(5),
            }),
            order: None,
            cut: Cut { limit: Some(3), ..Cut::default() },
        }
    );
    // Without a `HAVING` the cut goes back into the plan, where it was.
    same(
        "SELECT category, count(*) FROM t GROUP BY category ORDER BY count(*) DESC LIMIT 3",
        "TopN(All(), field=category, n=3)",
    );
}

/// Every comparison a `HAVING` accepts keeps the right groups.
#[test]
fn having_keeps_what_the_comparison_says() {
    let h = |op: &'static str, value: i128| Having {
        of: Of::Group { plan: 0, absent: Absent::Zero },
        op,
        value: Threshold::Units(value),
    };
    let n = |v: i128| Some(v);
    assert!(h(">", 5).keeps(n(6)) && !h(">", 5).keeps(n(5)));
    assert!(h(">=", 5).keeps(n(5)) && !h(">=", 5).keeps(n(4)));
    assert!(h("<", 5).keeps(n(4)) && !h("<", 5).keeps(n(5)));
    assert!(h("<=", 5).keeps(n(5)) && !h("<=", 5).keeps(n(6)));
    assert!(h("=", 5).keeps(n(5)) && !h("=", 5).keeps(n(4)));
    assert!(h("!=", 5).keeps(n(4)) && !h("!=", 5).keeps(n(5)));

    // A group with no number at all - a `min` over records that hold no value - fails every
    // comparison rather than being read as zero. `!= 5` is the one that would otherwise let it
    // through, which is why it is asserted next to the others rather than trusted.
    for op in [">", ">=", "<", "<=", "=", "!="] {
        assert!(!h(op, 5).keeps(None), "`{op}` let a group with no value through");
    }

    // A threshold that never met a schema drops every group. Visibly wrong beats a `HAVING`
    // that looks like it simply matched a lot.
    let unresolved = Having {
        of: Of::Group { plan: 0, absent: Absent::Zero },
        op: ">",
        value: Threshold::Written {
            table: "t".to_string(),
            field: "amount".to_string(),
            value: Literal::Int(5),
        },
    };
    assert!(!unresolved.keeps(n(1_000_000)));
}

#[test]
fn grouping_with_an_aggregate_of_another_column() {
    same(
        "SELECT category, sum(amount) FROM t GROUP BY category",
        "GroupBy(All(), field=category, aggregate=Sum(field=amount))",
    );
    // Only the keys were asked for, which is what `Distinct` lists.
    same("SELECT category FROM t GROUP BY category", "Distinct(All(), field=category)");
    assert_eq!(
        translate("SELECT category FROM t GROUP BY category").unwrap().answer.shape,
        Shape::Groups {
            keys: vec![0],
            cells: vec![Cell { column: "category".to_string(), of: Of::Key }],
            having: None,
            order: None,
            cut: Cut::default(),
        }
    );
}

/// A key and its bounds written against the same time quantum column are one window.
///
/// **A time quantum field carries a key and a time**, so `visit = 'home' AND visit >= <t>` is
/// one question about one column rather than two - and `Row(visit="home", from=…)` answers it
/// from that field's day views instead of everything it ever recorded.
#[test]
fn a_key_and_its_bounds_on_a_time_column_fuse_into_a_window() {
    same(
        "SELECT count(*) FROM t WHERE visit = 'home' AND visit BETWEEN 100 AND 200",
        "Count(Row(visit=\"home\", from=100, to=200))",
    );
    // One bound alone, either end.
    same(
        "SELECT count(*) FROM t WHERE visit = 'home' AND visit >= 100",
        "Count(Row(visit=\"home\", from=100))",
    );
    same(
        "SELECT count(*) FROM t WHERE visit = 'home' AND visit <= 200",
        "Count(Row(visit=\"home\", to=200))",
    );
    // Written the other way round, and beside a condition on another column that is not part of
    // the window.
    same(
        "SELECT count(*) FROM t WHERE visit >= 100 AND visit = 'home' AND amount > 5",
        "Count(Intersect(Row(visit=\"home\", from=100), Row(amount > 5)))",
    );
    // The key alone is still the plain key predicate it was.
    same("SELECT count(*) FROM t WHERE visit = 'home'", "Count(Row(visit=\"home\"))");
}

/// A window over a column with no views by time is refused, where it used to answer with
/// nothing at all.
///
/// **This is the bug the field classes were split to make visible.** `Rows::KeyBetween` reads
/// the day views a time quantum field writes; against a plain set field there are none, so the
/// read returned an *empty set* - indistinguishable from a window that genuinely matched
/// nothing. The planner could not refuse it because the planner could not see the difference,
/// and PQL can be written straight into the same hole.
#[test]
fn a_window_over_a_column_with_no_time_views_is_refused() {
    // `country` is an ordinary keyed column.
    let e = translate("SELECT count(*) FROM t WHERE country = 'GB' AND country BETWEEN 1 AND 2")
        .unwrap();
    let ask = &e.calls[0];
    let err = big_plan::plan(&ask.table, &ask.call, &Stub).unwrap_err();
    assert_eq!(err.code(), "operator_not_allowed");
    assert!(err.to_string().contains("a time window"), "{err}");

    // And the same hole in the query language itself, which is where it was reachable before.
    let call = big_plan::parse("Count(Row(country=\"GB\", from=1, to=2))").unwrap();
    let err = big_plan::plan("t", &call, &Stub).unwrap_err();
    assert_eq!(err.code(), "operator_not_allowed");
}
