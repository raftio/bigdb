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

//! Absent is not zero, and three separate things depend on the difference.
//!
//! A `HAVING` drops it, an ordering sorts it last, and a cell renders `null`. Each of these
//! would look like a plausible answer if absence collapsed into zero, which is why they are
//! claimed one at a time.

use crate::common::*;
use big_api::Value;
use big_api::{result_set, Absent, Cut, Datum, GroupOrder, Having, Of, OrderBy, Shape, Threshold};

fn shape(having: Option<Having>, order: Option<GroupOrder>) -> Shape {
    Shape::Groups {
        keys: vec![0],
        cells: vec![
            cell("country", Of::Key),
            cell("sum(amount)", Of::Group { plan: 1, absent: Absent::Null }),
        ],
        having,
        order,
        cut: Cut::default(),
    }
}

/// Two groups, and the second plan says nothing about the second of them.
fn values() -> Vec<Value> {
    vec![
        groups(vec![group(1, Some("GB"), Value::Count(1)), group(2, Some("US"), Value::Count(1))]),
        groups(vec![group(1, Some("GB"), Value::Sum(300))]),
    ]
}

#[test]
fn a_group_a_plan_said_nothing_about_renders_null() {
    let set = result_set(&answer(shape(None, None)), &values());

    assert_eq!(
        set.rows,
        vec![
            vec![Datum::Text("GB".to_string()), Datum::Int(300)],
            vec![Datum::Text("US".to_string()), Datum::Null],
        ]
    );
}

#[test]
fn absent_is_zero_when_the_shape_says_so() {
    // Only a `FILTER` puts a shape in this position: the group exists, and the number over the
    // records that survived the filter is genuinely zero rather than unknown.
    let shape = Shape::Groups {
        keys: vec![0],
        cells: vec![cell("n", Of::Group { plan: 1, absent: Absent::Zero })],
        having: None,
        order: None,
        cut: Cut::default(),
    };

    let set = result_set(&answer(shape), &values());

    assert_eq!(set.rows, vec![vec![Datum::Int(300)], vec![Datum::Int(0)]]);
}

#[test]
fn a_having_drops_an_absent_number_rather_than_treating_it_as_zero() {
    let having = Having {
        of: Of::Group { plan: 1, absent: Absent::Null },
        op: ">=",
        value: Threshold::Units(0),
    };

    let set = result_set(&answer(shape(Some(having), None)), &values());

    // `US` has no number at all. `>= 0` would keep a zero, and keeping this one would report a
    // total for a group that produced none.
    assert_eq!(set.rows, vec![vec![Datum::Text("GB".to_string()), Datum::Int(300)]]);
}

#[test]
fn an_ordering_puts_an_absent_number_last_in_both_directions() {
    let by_value = |desc| GroupOrder {
        by: OrderBy::Value { of: Of::Group { plan: 1, absent: Absent::Null } },
        desc,
    };
    let keys = |set: big_api::ResultSet| -> Vec<Datum> {
        set.rows.into_iter().map(|r| r[0].clone()).collect()
    };

    let up = keys(result_set(&answer(shape(None, Some(by_value(false)))), &values()));
    let down = keys(result_set(&answer(shape(None, Some(by_value(true)))), &values()));

    // Absent last both ways. Sorting it first when descending would make "no value" outrank
    // every value there is.
    assert_eq!(up, vec![Datum::Text("GB".to_string()), Datum::Text("US".to_string())]);
    assert_eq!(down, vec![Datum::Text("GB".to_string()), Datum::Text("US".to_string())]);
}

#[test]
fn an_average_over_no_records_is_null_rather_than_a_division_by_zero() {
    let shape = Shape::row(vec![cell("avg(amount)", Of::Ratio { plan: 0, over: 1 })]);

    let set = result_set(&answer(shape), &[Value::Sum(0), Value::Count(0)]);

    assert_eq!(set.rows, vec![vec![Datum::Null]]);
}

#[test]
fn a_group_with_no_interned_name_is_null_and_sorts_after_every_named_one() {
    let shape = Shape::Groups {
        keys: vec![0, 1],
        cells: vec![cell("country", Of::Key)],
        having: None,
        order: None,
        cut: Cut::default(),
    };
    // Two plans, so the union of their groups is put back into key order - which is where the
    // unnamed group has to go last rather than first.
    let values = vec![
        groups(vec![group(9, None, Value::Count(1)), group(1, Some("GB"), Value::Count(1))]),
        groups(vec![group(1, Some("GB"), Value::Count(1))]),
    ];

    let set = result_set(&answer(shape), &values);

    assert_eq!(set.rows, vec![vec![Datum::Text("GB".to_string())], vec![Datum::Null]]);
}

#[test]
fn two_totals_that_differ_by_one_still_order_correctly_past_what_a_float_counts() {
    // The comparison falls back to `f64` for anything fractional, and two integers this large
    // are equal as floats. Comparing them as integers is what keeps the ranking right.
    let big = 1_i128 << 60;
    let shape = Shape::Groups {
        keys: vec![0],
        cells: vec![cell("n", Of::Group { plan: 0, absent: Absent::Null })],
        having: None,
        order: Some(GroupOrder {
            by: OrderBy::Value { of: Of::Group { plan: 0, absent: Absent::Null } },
            desc: true,
        }),
        cut: Cut::default(),
    };
    let values = vec![groups(vec![
        group(1, Some("a"), Value::SignedSum(big)),
        group(2, Some("b"), Value::SignedSum(big + 1)),
    ])];

    let set = result_set(&answer(shape), &values);

    assert_eq!(set.rows, vec![vec![Datum::Int(big + 1)], vec![Datum::Int(big)]]);
}
