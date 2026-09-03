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

//! Joins: two ordinary groupings and a shape, and the arithmetic that pairs them.

use super::common::*;

/// A join lowers to one ordinary grouping per table, and the pairing is a shape.
///
/// **Nothing below the planner learns that a join happened.** Each call is a single-table
/// grouping that the existing fan-out and merge already answer; what makes it a join is
/// arithmetic over the two per-key answers, done at the coordinator. That is the whole reason a
/// join could be added without a `Plan` variant or a merge arm.
#[test]
fn a_join_is_two_groupings_and_a_shape() {
    let s = translate("SELECT count(*) FROM t JOIN u ON t.category = u.category").unwrap();
    assert_eq!(s.calls.len(), 2);
    assert_eq!(s.tables(), vec!["t", "u"]);
    // Each side's call is exactly the grouping it would have got written alone.
    let sql = "SELECT count(*) FROM t JOIN u ON t.category = u.category";
    assert_eq!(resolved(sql, 0), plan_pql("Distinct(All(), field=category)"));
    assert_eq!(resolved(sql, 1), plan_pql_of("u", "Distinct(All(), field=category)"));
    assert_eq!(
        s.answer.shape,
        Shape::Join {
            axes: 1,
            sides: vec![
                JoinSide { keyed: Keying::By { plan: 0, axis: 0 }, required: true },
                JoinSide { keyed: Keying::By { plan: 1, axis: 0 }, required: true },
            ],
            cells: vec![Cell::plain(
                "count".to_string(),
                Of::Paired { plan: 0, side: 0, how: Pairing::Product }
            )],
            per_key: false,
            having: None,
            order: None,
            cut: Cut::default(),
        }
    );
}

/// Each table's `WHERE` narrows its own side, and only its own side.
#[test]
fn a_where_over_a_join_is_split_by_the_table_each_term_names() {
    let both = "SELECT count(*) FROM t AS a JOIN u AS b ON a.category = b.category \
                WHERE a.amount >= 500 AND b.active = true";
    assert_eq!(resolved(both, 0), plan_pql("Distinct(Row(amount >= 500), field=category)"));
    assert_eq!(resolved(both, 1), plan_pql_of("u", "Distinct(Row(active=true), field=category)"));

    // A term naming one table only narrows that one; the other keeps every record.
    let one = "SELECT count(*) FROM t a JOIN u b ON a.category = b.category WHERE a.amount > 5";
    assert_eq!(resolved(one, 0), plan_pql("Distinct(Row(amount > 5), field=category)"));
    assert_eq!(resolved(one, 1), plan_pql_of("u", "Distinct(All(), field=category)"));
}

/// An aggregate over a join names the side it measures, and is paired against the other side's
/// count.
///
/// The pairing rule is the arithmetic: a total is scaled by how many times each record repeats
/// in the product, and an extreme is not - repeating a value does not make it larger.
#[test]
fn an_aggregate_over_a_join_is_paired_against_the_other_sides_count() {
    let s = translate(
        "SELECT sum(a.amount), min(a.amount), max(b.price) \
         FROM t a JOIN u b ON a.category = b.category",
    )
    .unwrap();
    // Two counts, then one grouped aggregate per aggregate in the select list.
    assert_eq!(s.calls.len(), 5);
    assert_eq!(s.tables(), vec!["t", "u"]);

    let Shape::Join { cells, .. } = &s.answer.shape else { panic!("expected a join") };
    // `sum` on the left is scaled by the right's count; `min` on the left only asks whether the
    // right holds the key; `max` on the right is scaled by the left's count - which is the
    // extreme's rule the other way round.
    assert_eq!(cells[0].of, Of::Paired { plan: 2, side: 0, how: Pairing::Product });
    assert_eq!(cells[1].of, Of::Paired { plan: 3, side: 0, how: Pairing::Least });
    assert_eq!(cells[2].of, Of::Paired { plan: 4, side: 1, how: Pairing::Greatest });
}

/// `GROUP BY` over a join is the join key, which is the only column both sides agree about.
#[test]
fn a_join_groups_by_its_key_and_nothing_else() {
    let s = translate(
        "SELECT a.category, count(*) FROM t a JOIN u b ON a.category = b.category \
         GROUP BY a.category ORDER BY count(*) DESC LIMIT 5",
    )
    .unwrap();
    let Shape::Join { cells, per_key, order, cut, .. } = &s.answer.shape else {
        panic!("expected a join")
    };
    assert!(per_key);
    assert_eq!(cells[0].of, Of::Key);
    // The ranking is a sort of the merged answer: `TopN` ranks one table's groups, and what is
    // being ranked here is a product of two tables'.
    assert_eq!(
        *order,
        Some(GroupOrder {
            by: OrderBy::Value { of: Of::Paired { plan: 0, side: 0, how: Pairing::Product } },
            desc: true,
        })
    );
    assert_eq!(cut.limit, Some(5));

    // Grouping by anything but the join key would be a grouping over a pair of columns nothing
    // stored.
    assert_eq!(
        code(
            "SELECT a.country, count(*) FROM t a JOIN u b ON a.category = b.category \
              GROUP BY a.country"
        ),
        "sql_unsupported"
    );
}

/// The sides of a join, as the shape says whether each one has to match.
fn required_of(sql: &str) -> Vec<bool> {
    let s = translate(sql).unwrap();
    let Shape::Join { sides, .. } = &s.answer.shape else { panic!("expected a join") };
    sides.iter().map(|s| s.required).collect()
}

/// **An outer join is one flag per side, and the plans below it do not change at all.**
///
/// Each side is still the ordinary grouping it would have been written alone; what `LEFT` says
/// is that a key the right side is missing is still a row, which is a fact about the key space
/// and is settled after the merge. That is why this costs a word in the shape rather than a
/// second lowering.
#[test]
fn an_outer_join_changes_which_sides_must_match_and_nothing_below_it() {
    let inner = "SELECT count(*) FROM t JOIN u ON t.category = u.category";
    let left = "SELECT count(*) FROM t LEFT JOIN u ON t.category = u.category";

    assert_eq!(required_of(inner), vec![true, true]);
    assert_eq!(required_of(left), vec![true, false]);
    // The same two calls, spelled the same way, for both.
    assert_eq!(resolved(left, 0), resolved(inner, 0));
    assert_eq!(resolved(left, 1), resolved(inner, 1));
}

#[test]
fn right_clears_every_side_already_in_scope_and_full_clears_them_all() {
    assert_eq!(
        required_of("SELECT count(*) FROM t RIGHT JOIN u ON t.category = u.category"),
        vec![false, true]
    );
    assert_eq!(
        required_of("SELECT count(*) FROM t FULL OUTER JOIN u ON t.category = u.category"),
        vec![false, false]
    );
    // A star of three, joined in one at a time: `RIGHT` is about every table written before it,
    // which is what "all the rows of the right-hand side" means once the rows are keys.
    assert_eq!(
        required_of(
            "SELECT count(*) FROM t JOIN u ON t.category = u.category \
             RIGHT JOIN v ON t.category = v.category"
        ),
        vec![false, false, true]
    );
    // A `LEFT` only ever excuses the table it brings in, so an inner join after one still has
    // to match.
    assert_eq!(
        required_of(
            "SELECT count(*) FROM t LEFT JOIN u ON t.category = u.category \
             JOIN v ON t.category = v.category"
        ),
        vec![true, false, true]
    );
}

/// **A `WHERE` on an optional side makes it required, which is SQL's own rule.**
///
/// The rows an outer join adds are null on that side, so a predicate about one of its columns
/// is false there and drops them - the difference between putting a condition in the `WHERE`
/// and putting it in the `ON`. The one thing that would escape this is `IS NULL`, and there are
/// no nulls here to write it with.
#[test]
fn a_where_on_an_optional_side_makes_it_required_again() {
    assert_eq!(
        required_of(
            "SELECT count(*) FROM t a LEFT JOIN u b ON a.category = b.category \
             WHERE b.amount > 5"
        ),
        vec![true, true]
    );
    // A predicate on the required side leaves the optional one optional: it narrows which of
    // the left's records are there, not whether an unmatched key is a row.
    assert_eq!(
        required_of(
            "SELECT count(*) FROM t a LEFT JOIN u b ON a.category = b.category \
             WHERE a.amount > 5"
        ),
        vec![true, false]
    );
}
