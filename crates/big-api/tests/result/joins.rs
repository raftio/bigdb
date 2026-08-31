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

//! The arithmetic a join is, which is the reason any of this happens after the merge.
//!
//! For each key both sides hold, the answer is the product of their per-key numbers.
//! Multiplying at each owner and summing would be a plausible number that is wrong, so the
//! multiplication waits until both sides have been merged - and this is where it waits.

use crate::common::*;
use big_api::{result_set, Cut, Datum, Of, Pairing, ResultSet, Shape, Value};

fn shape(per_key: bool, cells: Vec<big_api::Cell>) -> Shape {
    Shape::Join { keys: (0, 1), cells, per_key, having: None, order: None, cut: Cut::default() }
}

/// Two sides sharing `GB` and `US`; `FR` is only on the left and `DE` only on the right.
fn values() -> Vec<Value> {
    vec![
        groups(vec![
            group(1, Some("GB"), Value::Count(2)),
            group(2, Some("US"), Value::Count(3)),
            group(3, Some("FR"), Value::Count(9)),
        ]),
        groups(vec![
            group(7, Some("GB"), Value::Count(5)),
            group(8, Some("US"), Value::Count(1)),
            group(9, Some("DE"), Value::Count(4)),
        ]),
    ]
}

fn rows(set: ResultSet) -> Vec<Vec<Datum>> {
    set.rows
}

#[test]
fn a_key_pairs_to_the_product_of_the_two_sides_per_key_numbers() {
    let cells = vec![
        cell("k", Of::Key),
        cell("count()", Of::Paired { left: 0, right: 1, how: Pairing::Product }),
    ];

    let set = result_set(&answer(shape(true, cells)), &values());

    // 2*5 and 3*1. Nothing here could have been worked out by either owner alone.
    assert_eq!(
        rows(set),
        vec![
            vec![Datum::Text("GB".to_string()), Datum::Int(10)],
            vec![Datum::Text("US".to_string()), Datum::Int(3)],
        ]
    );
}

#[test]
fn a_key_only_one_side_holds_is_no_row_rather_than_a_row_of_nulls() {
    // An inner join is the intersection. `FR` and `DE` pair with nothing.
    let cells = vec![cell("k", Of::Key)];

    let set = result_set(&answer(shape(true, cells)), &values());

    assert_eq!(
        rows(set),
        vec![vec![Datum::Text("GB".to_string())], vec![Datum::Text("US".to_string())]]
    );
}

#[test]
fn the_folded_form_sums_the_products_across_every_shared_key() {
    // `SELECT count(*) FROM a JOIN b ON ...` with no `GROUP BY`: one row, and the number is how
    // many pairs there are altogether.
    let cells = vec![cell("count()", Of::Paired { left: 0, right: 1, how: Pairing::Product })];

    let set = result_set(&answer(shape(false, cells)), &values());

    assert_eq!(rows(set), vec![vec![Datum::Int(13)]]);
}

#[test]
fn counting_the_shared_keys_counts_the_join_rather_than_either_side() {
    let cells = vec![cell("uniq(k)", Of::SharedKeys { left: 0, right: 1 })];

    let set = result_set(&answer(shape(false, cells)), &values());

    assert_eq!(rows(set), vec![vec![Datum::Int(2)]]);
}

#[test]
fn an_extreme_takes_this_sides_number_and_the_other_side_only_decides_membership() {
    // Repeating a value does not make it larger or smaller, so a `max` over a join is this
    // side's max over the keys the other side also holds.
    let cells = vec![
        cell("k", Of::Key),
        cell("max(x)", Of::Paired { left: 0, right: 1, how: Pairing::Greatest }),
    ];

    let set = result_set(&answer(shape(true, cells)), &values());

    assert_eq!(
        rows(set),
        vec![
            vec![Datum::Text("GB".to_string()), Datum::Int(2)],
            vec![Datum::Text("US".to_string()), Datum::Int(3)],
        ]
    );
}

#[test]
fn the_folded_extreme_is_the_extreme_of_the_extremes() {
    let cells = vec![cell("max(x)", Of::Paired { left: 0, right: 1, how: Pairing::Greatest })];

    let set = result_set(&answer(shape(false, cells)), &values());

    assert_eq!(rows(set), vec![vec![Datum::Int(3)]]);
}

#[test]
fn a_group_with_no_interned_name_cannot_be_paired_and_is_left_out() {
    // The join is on the key's string, and a group without one has nothing to match. Fusing
    // every unnamed group into one would invent pairs that do not exist.
    let values = vec![
        groups(vec![group(1, None, Value::Count(2))]),
        groups(vec![group(7, None, Value::Count(5))]),
    ];
    let cells = vec![cell("k", Of::Key)];

    let set = result_set(&answer(shape(true, cells)), &values);

    assert!(rows(set).is_empty());
}
