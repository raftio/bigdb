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

//! A grouping-sets answer: several groupings, rendered one after the other.
//!
//! What is claimed here is the *rendering*, which is the half a plan test cannot see. A rollup's
//! shape is a `Union` of branches of decreasing arity, and the columns a branch's set does not
//! name are carried as [`Of::Const`] - so these tests are about where the nulls land, and about
//! the three separate readers that have to agree on what a constant is.

use super::common::{answer, cell, group, groups};
use big_embed::{
    result_set, Absent, Cut, Datum, GroupAt, GroupKey, Having, Of, Shape, Threshold, Tuple, Units,
    Value,
};

/// One row of a tuple grouping: the key of each axis, and the number.
fn tuple(keys: &[(u64, &str)], value: Value) -> Tuple {
    Tuple {
        keys: keys
            .iter()
            .map(|(at, key)| GroupKey { at: GroupAt::Row(*at), key: Some((*key).to_string()) })
            .collect(),
        value: Box::new(value),
    }
}

fn tuples_branch(cells: Vec<big_embed::Cell>, axes: u8, plan: usize) -> Shape {
    Shape::Tuples { axes, keys: vec![plan], cells, having: None, order: None, cut: Cut::default() }
}

fn groups_branch(cells: Vec<big_embed::Cell>, plan: usize) -> Shape {
    Shape::Groups { keys: vec![plan], cells, having: None, order: None, cut: Cut::default() }
}

#[test]
fn a_rollup_reads_detail_then_subtotals_then_the_grand_total() {
    // `SELECT country, city, count(*) FROM t GROUP BY country, city WITH ROLLUP`, as the three
    // branches it lowers to. The point of the test is the shape of the *answer*: each row is as
    // wide as the others, and a column its set did not name is `null` rather than missing.
    let shape = Shape::Union {
        branches: vec![
            tuples_branch(
                vec![
                    cell("country", Of::KeyAt { axis: 0 }),
                    cell("city", Of::KeyAt { axis: 1 }),
                    cell("count()", Of::Group { plan: 0, absent: Absent::Zero }),
                ],
                2,
                0,
            ),
            groups_branch(
                vec![
                    cell("country", Of::Key),
                    cell("city", Of::Const { value: None }),
                    cell("count()", Of::Group { plan: 1, absent: Absent::Zero }),
                ],
                1,
            ),
            Shape::row(vec![
                cell("country", Of::Const { value: None }),
                cell("city", Of::Const { value: None }),
                cell("count()", Of::Value { plan: 2 }),
            ]),
        ],
    };
    let values = vec![
        Value::Tuples(vec![
            tuple(&[(1, "GB"), (7, "London")], Value::Count(4)),
            tuple(&[(1, "GB"), (8, "Leeds")], Value::Count(1)),
        ]),
        groups(vec![group(1, Some("GB"), Value::Count(5))]),
        Value::Count(5),
    ];

    let set = result_set(&answer(shape), &values);

    let text = |s: &str| Datum::Text(s.to_string());
    assert_eq!(set.columns, vec!["country", "city", "count()"]);
    assert_eq!(
        set.rows,
        vec![
            // Within a branch the rows are in that shape's own order, which for a tuple
            // grouping is by key *string* per axis - so Leeds precedes London, whatever row
            // either was interned into. Worth pinning: with `ORDER BY` refused over a rollup,
            // this order is the whole of what the client is promised.
            vec![text("GB"), text("Leeds"), Datum::Int(1)],
            vec![text("GB"), text("London"), Datum::Int(4)],
            // The subtotal: every city of GB, folded, with the city column blanked.
            vec![text("GB"), Datum::Null, Datum::Int(5)],
            // The grand total, which names neither column.
            vec![Datum::Null, Datum::Null, Datum::Int(5)],
        ]
    );
}

#[test]
fn a_constant_reads_the_same_through_every_branch_of_one_answer() {
    // **The drift this is here to catch.** `Of` is read by three separate readers - `number`,
    // and the two local ones a tuple grouping and a join go through - and two of those end in a
    // `_ => None` fallback. A constant missing an arm in one of them would render as its number
    // in one branch of a statement and as `null` in another, which is a difference no client
    // could see and no plan test could either.
    //
    // So: the same two constants, in a `Tuples` branch and a `Groups` branch of one answer,
    // asserted to come out identical. That is `grouping(country)` beside a blanked column, which
    // is exactly the pair a rollup renders.
    let cells = || {
        vec![
            cell("blank", Of::Const { value: None }),
            cell("grouping(country)", Of::Const { value: Some(1) }),
            cell("count()", Of::Group { plan: 0, absent: Absent::Zero }),
        ]
    };
    let mut with_key = cells();
    with_key.insert(0, cell("country", Of::Key));
    let mut with_axis = cells();
    with_axis.insert(0, cell("country", Of::KeyAt { axis: 0 }));

    let grouped = result_set(
        &answer(groups_branch(with_key, 0)),
        &[groups(vec![group(1, Some("GB"), Value::Count(5))])],
    );
    let tupled = result_set(
        &answer(tuples_branch(with_axis, 1, 0)),
        &[Value::Tuples(vec![tuple(&[(1, "GB")], Value::Count(5))])],
    );

    assert_eq!(grouped.rows, tupled.rows, "a constant must read the same through both readers");
    assert_eq!(
        grouped.rows,
        vec![vec![Datum::Text("GB".to_string()), Datum::Null, Datum::Int(1), Datum::Int(5)]]
    );
}

#[test]
fn a_having_over_a_rollup_can_empty_the_grand_total_row() {
    // A `Shape::Row`'s `HAVING` decides whether its row exists at all, so a threshold the whole
    // set fails leaves the answer with the branches above it and nothing else. Applied per
    // branch, and after the merge in each - a total under the threshold on one node can be over
    // it once every node has contributed.
    let branches = |threshold: i128| Shape::Union {
        branches: vec![
            groups_branch(
                vec![
                    cell("country", Of::Key),
                    cell("count()", Of::Group { plan: 0, absent: Absent::Zero }),
                ],
                0,
            ),
            Shape::Row {
                cells: vec![
                    cell("country", Of::Const { value: None }),
                    cell("count()", Of::Value { plan: 1 }),
                ],
                having: Some(Having::cmp(
                    Of::Value { plan: 1 },
                    Units::PLAIN,
                    ">",
                    Threshold::Units(threshold),
                )),
            },
        ],
    };
    let values = vec![groups(vec![group(1, Some("GB"), Value::Count(5))]), Value::Count(5)];

    let kept = result_set(&answer(branches(2)), &values);
    let dropped = result_set(&answer(branches(100)), &values);

    assert_eq!(kept.rows.len(), 2, "the grand total is over the threshold and stays");
    assert_eq!(dropped.rows.len(), 1, "the grand total is under it and there is no row at all");
    assert_eq!(dropped.rows[0][0], Datum::Text("GB".to_string()));
}
