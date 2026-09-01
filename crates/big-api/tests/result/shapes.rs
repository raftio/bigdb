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

//! Each shape reads the answers it names, and nothing else.

use crate::common::*;
use big_api::{result_set, Absent, Columns, Cut, Datum, Of, Projected, Projection, Shape, Value};

#[test]
fn a_single_row_reads_one_cell_per_plan() {
    let shape = Shape::Row {
        cells: vec![
            cell("count()", Of::Value { plan: 0 }),
            cell("sum(amount)", Of::Value { plan: 1 }),
        ],
    };
    let values = vec![Value::Count(3), Value::Sum(250)];

    let set = result_set(&answer(shape), &values);

    assert_eq!(set.columns, vec!["count()", "sum(amount)"]);
    assert_eq!(set.rows, vec![vec![Datum::Int(3), Datum::Int(250)]]);
}

#[test]
fn an_average_is_a_quotient_and_keeps_its_fractional_part() {
    let shape = Shape::Row { cells: vec![cell("avg(amount)", Of::Ratio { plan: 0, over: 1 })] };
    let values = vec![Value::Sum(250), Value::Count(4)];

    let set = result_set(&answer(shape), &values);

    assert_eq!(set.rows, vec![vec![Datum::Real(62.5)]]);
}

#[test]
fn counting_distinct_counts_the_groups_after_the_merge() {
    // The whole reason this lives here: two nodes each holding the same group must count as
    // one, and they are only one once both answers are in.
    let shape = Shape::Row { cells: vec![cell("uniq(country)", Of::Groups { plan: 0 })] };
    let values = vec![groups(vec![
        group(1, Some("GB"), Value::Count(2)),
        group(2, Some("US"), Value::Count(1)),
    ])];

    let set = result_set(&answer(shape), &values);

    assert_eq!(set.rows, vec![vec![Datum::Int(2)]]);
}

#[test]
fn a_search_answer_is_read_from_after_the_calls() {
    // `calls` is where a probe's index starts, because the two lists are built separately.
    let shape = Shape::Row {
        cells: vec![
            cell("count()", Of::Value { plan: 0 }),
            cell("quantile(amount)", Of::Probe { probe: 0 }),
        ],
    };
    let values = vec![Value::Count(9), Value::Count(41)];

    let set = result_set(&answer_with_probes(shape, 1), &values);

    assert_eq!(set.rows, vec![vec![Datum::Int(9), Datum::Int(41)]]);
}

#[test]
fn top_k_answers_with_the_list_of_keys() {
    let shape = Shape::Row { cells: vec![cell("topK(3)(country)", Of::Keys { plan: 0 })] };
    let values = vec![groups(vec![
        group(1, Some("GB"), Value::Count(9)),
        group(2, Some("US"), Value::Count(4)),
    ])];

    let set = result_set(&answer(shape), &values);

    assert_eq!(set.rows, vec![vec![Datum::Keys(vec!["GB".to_string(), "US".to_string()])]]);
}

#[test]
fn a_group_with_no_interned_name_is_left_out_of_a_list_of_names() {
    // Distinct from the grouped case below, where the same group is a `null` cell: a list of
    // names has nowhere to put a group that has none, and a row does.
    let shape = Shape::Row { cells: vec![cell("topK(3)(country)", Of::Keys { plan: 0 })] };
    let values =
        vec![groups(vec![group(1, Some("GB"), Value::Count(9)), group(2, None, Value::Count(4))])];

    let set = result_set(&answer(shape), &values);

    assert_eq!(set.rows, vec![vec![Datum::Keys(vec!["GB".to_string()])]]);
}

#[test]
fn records_answer_with_one_id_per_row() {
    let shape = Shape::Records { column: "id".to_string(), limit: Some(2) };
    let values = vec![Value::Rows(matching(&[1, 2, 5]))];

    let set = result_set(&answer(shape), &values);

    assert_eq!(set.columns, vec!["id"]);
    assert_eq!(set.rows, vec![vec![Datum::Int(1)], vec![Datum::Int(2)]]);
}

#[test]
fn a_projection_renders_the_values_the_plan_already_cut() {
    let shape = Shape::Table {
        columns: Columns::Named(vec![plain_column("amount"), plain_column("score")]),
    };
    let values = vec![Value::Table(vec![
        Projected { record: 1, values: vec![Projection::Int(100), Projection::Absent] },
        Projected { record: 2, values: vec![Projection::Int(250), Projection::Int(7)] },
    ])];

    let set = result_set(&answer(shape), &values);

    assert_eq!(
        set.rows,
        vec![vec![Datum::Int(100), Datum::Null], vec![Datum::Int(250), Datum::Int(7)],]
    );
}

/// **Where a decimal stops being an integer.** A column of scale two holds 1250 and means
/// 12.50, and this is the last step that knows the difference - everything before it works in
/// the units the field stores, which is what keeps it exact.
#[test]
fn a_decimal_column_is_rendered_with_the_point_its_field_keeps() {
    let shape = Shape::Table {
        columns: Columns::Named(vec![scaled_column("price", 2), plain_column("qty")]),
    };
    let values = vec![Value::Table(vec![
        Projected { record: 1, values: vec![Projection::Int(1250), Projection::Int(3)] },
        // Fewer digits than the scale, which is where a naive placing of the point drops the
        // leading zero and answers `.05`.
        Projected { record: 2, values: vec![Projection::Int(5), Projection::Absent] },
    ])];

    let set = result_set(&answer(shape), &values);

    assert_eq!(
        set.rows,
        vec![
            vec![Datum::Dec { units: 1250, scale: 2 }, Datum::Int(3)],
            vec![Datum::Dec { units: 5, scale: 2 }, Datum::Null],
        ]
    );
    assert_eq!(big_api::fixed(1250, 2), "12.50");
    assert_eq!(big_api::fixed(5, 2), "0.05");
    assert_eq!(big_api::fixed(-5, 2), "-0.05");
    // A scale of zero is the integer itself, which is every field but a decimal.
    assert_eq!(big_api::fixed(1250, 0), "1250");
}

/// A scalar cell out of a decimal field: a `sum`, a `min`, a quantile.
#[test]
fn a_decimal_total_carries_the_scale_its_field_keeps() {
    let scaled = |column: &str, of: Of, scale: u8| big_api::Cell {
        column: column.to_string(),
        of,
        units: big_api::Units::Digits(scale),
    };
    let shape = Shape::Row {
        cells: vec![
            scaled("sum", Of::Value { plan: 0 }, 2),
            cell("count", Of::Value { plan: 1 }),
            // An average is already a quotient and already a float, so it is divided rather
            // than pointed.
            scaled("avg", Of::Ratio { plan: 0, over: 1 }, 2),
        ],
    };
    let values = vec![Value::Sum(1250), Value::Count(2)];

    let set = result_set(&answer(shape), &values);

    assert_eq!(
        set.rows,
        vec![vec![Datum::Dec { units: 1250, scale: 2 }, Datum::Int(2), Datum::Real(6.25)]]
    );
}

#[test]
fn a_union_stacks_its_branches_in_the_order_written() {
    let branch = |plan: usize| Shape::Row { cells: vec![cell("n", Of::Value { plan })] };
    let shape = Shape::Union { branches: vec![branch(0), branch(1)] };
    let values = vec![Value::Count(1), Value::Count(2)];

    let set = result_set(&answer(shape), &values);

    assert_eq!(set.rows, vec![vec![Datum::Int(1)], vec![Datum::Int(2)]]);
}

#[test]
fn a_grouping_joins_its_plans_on_the_row_id() {
    // **Not on the key.** A node that was never told a key renders `null`, and joining on the
    // string would fuse every such group into one.
    let shape = Shape::Groups {
        keys: vec![0],
        cells: vec![
            cell("country", Of::Key),
            cell("count()", Of::Group { plan: 0, absent: Absent::Zero }),
            cell("sum(amount)", Of::Group { plan: 1, absent: Absent::Zero }),
        ],
        having: None,
        order: None,
        cut: Cut::default(),
    };
    let values = vec![
        groups(vec![group(1, Some("GB"), Value::Count(2)), group(2, None, Value::Count(1))]),
        groups(vec![group(1, Some("GB"), Value::Sum(300)), group(2, None, Value::Sum(50))]),
    ];

    let set = result_set(&answer(shape), &values);

    assert_eq!(
        set.rows,
        vec![
            vec![Datum::Text("GB".to_string()), Datum::Int(2), Datum::Int(300)],
            vec![Datum::Null, Datum::Int(1), Datum::Int(50)],
        ]
    );
}

#[test]
fn a_pair_grouping_carries_both_halves_of_its_key() {
    let shape = Shape::Pairs {
        keys: vec![0],
        cells: vec![
            cell("country", Of::Key),
            cell("city", Of::RightKey),
            cell("count()", Of::Group { plan: 0, absent: Absent::Zero }),
        ],
        having: None,
        order: None,
        cut: Cut::default(),
    };
    let values = vec![Value::Pairs(vec![pair(
        group(1, Some("GB"), Value::Count(0)),
        group(7, Some("London"), Value::Count(4)),
    )])];

    let set = result_set(&answer(shape), &values);

    assert_eq!(
        set.rows,
        vec![
            vec![Datum::Text("GB".to_string()), Datum::Text("London".to_string()), Datum::Int(4),]
        ]
    );
}
