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

//! Window columns: the arithmetic, over rows built by hand.
//!
//! A plan test can show which fields a window makes the projection read; only this can show what
//! it computes. So these build the `Projected` rows a coordinator holds after the merge and
//! assert the numbers - the ties in particular, which are where the four ranking functions stop
//! agreeing with each other.

use super::common::answer;
use big_embed::{
    result_set, Columns, Datum, Frame, Projected, Projection, Selected, Selection, Shape, Units,
    Value, WinFunc,
};

/// One record's stored values, in the order the plan read its fields.
fn row(values: &[Projection]) -> Projected {
    Projected { record: 0, values: values.to_vec() }
}

fn int(v: i128) -> Projection {
    Projection::Int(v)
}

fn text(s: &str) -> Projection {
    Projection::Text(s.to_string())
}

/// A projection of one read column and one window over it.
fn shape(read: &str, at: usize, name: &str, of: Selection) -> Shape {
    Shape::Table {
        columns: Columns::Named(vec![
            Selected::read(read, Units::PLAIN, None, at),
            Selected { column: name.to_string(), units: Units::PLAIN, apply: None, of },
        ]),
        order: None,
        cut: None,
    }
}

/// A window over field `by`, ascending, with no partition.
fn over(func: WinFunc, arg: Option<usize>, by: usize) -> Selection {
    Selection::Over {
        func,
        arg,
        offset: 1,
        window: Frame { partition: Vec::new(), order: vec![(by, false)] },
    }
}

fn cells(shape: Shape, table: Vec<Projected>) -> Vec<Datum> {
    result_set(&answer(shape), &[Value::Table(table)])
        .rows
        .into_iter()
        .map(|r| r[1].clone())
        .collect()
}

#[test]
fn the_four_rankings_differ_exactly_where_the_ordering_ties() {
    // Values 10, 20, 20, 30 - one tie in the middle, which is the only place `row_number`,
    // `rank` and `dense_rank` can be told apart. A test over distinct values would pass for all
    // three implementations and prove nothing.
    let table = || vec![row(&[int(10)]), row(&[int(20)]), row(&[int(20)]), row(&[int(30)])];
    let of = |f| shape("amount", 0, "n", over(f, None, 0));

    assert_eq!(
        cells(of(WinFunc::RowNumber), table()),
        vec![Datum::Int(1), Datum::Int(2), Datum::Int(3), Datum::Int(4)]
    );
    // Ties share a number and the next one skips: 1, 2, 2, 4.
    assert_eq!(
        cells(of(WinFunc::Rank), table()),
        vec![Datum::Int(1), Datum::Int(2), Datum::Int(2), Datum::Int(4)]
    );
    // Ties share a number and the next one does not: 1, 2, 2, 3.
    assert_eq!(
        cells(of(WinFunc::DenseRank), table()),
        vec![Datum::Int(1), Datum::Int(2), Datum::Int(2), Datum::Int(3)]
    );
    // `(rank - 1) / (rows - 1)`, so the tie carries one number and not two.
    assert_eq!(
        cells(of(WinFunc::PercentRank), table()),
        vec![Datum::Real(0.0), Datum::Real(1.0 / 3.0), Datum::Real(1.0 / 3.0), Datum::Real(1.0)]
    );
    // The share at or *before* this row, so both rows of the tie read 3/4.
    assert_eq!(
        cells(of(WinFunc::CumeDist), table()),
        vec![Datum::Real(0.25), Datum::Real(0.75), Datum::Real(0.75), Datum::Real(1.0)]
    );
}

#[test]
fn a_partition_ranks_its_own_rows_and_nobody_elses() {
    // Two countries interleaved in record order, so a window that forgot to partition would
    // number them 1..4 and this would catch it.
    let table = vec![
        row(&[text("GB"), int(30)]),
        row(&[text("US"), int(10)]),
        row(&[text("GB"), int(10)]),
        row(&[text("US"), int(20)]),
    ];
    let of = Selection::Over {
        func: WinFunc::RowNumber,
        arg: None,
        offset: 1,
        window: Frame { partition: vec![0], order: vec![(1, false)] },
    };
    let set = result_set(&answer(shape("country", 0, "n", of)), &[Value::Table(table)]);

    // Rows keep the order the projection read them in - a window fills cells, it does not sort
    // the answer. So GB's 30 is still first, and carries the 2 its own partition gives it.
    assert_eq!(
        set.rows,
        vec![
            vec![Datum::Text("GB".to_string()), Datum::Int(2)],
            vec![Datum::Text("US".to_string()), Datum::Int(1)],
            vec![Datum::Text("GB".to_string()), Datum::Int(1)],
            vec![Datum::Text("US".to_string()), Datum::Int(2)],
        ]
    );
}

#[test]
fn lag_and_lead_are_absent_at_the_ends_rather_than_wrapping() {
    let table = || vec![row(&[int(10)]), row(&[int(20)]), row(&[int(30)])];

    assert_eq!(
        cells(shape("amount", 0, "lag", over(WinFunc::Lag, Some(0), 0)), table()),
        vec![Datum::Null, Datum::Int(10), Datum::Int(20)]
    );
    assert_eq!(
        cells(shape("amount", 0, "lead", over(WinFunc::Lead, Some(0), 0)), table()),
        vec![Datum::Int(20), Datum::Int(30), Datum::Null]
    );
}

#[test]
fn last_value_is_the_partitions_last_row_and_not_this_one() {
    // **The surprise this surface deliberately avoids.** Under the standard's default frame
    // `last_value` is the *current* row, which is a number nobody wants. With no frame it is
    // what the name says.
    let table = vec![row(&[int(10)]), row(&[int(20)]), row(&[int(30)])];
    let of = |f| shape("amount", 0, "v", over(f, Some(0), 0));

    assert_eq!(
        cells(of(WinFunc::LastValue), table.clone()),
        vec![Datum::Int(30), Datum::Int(30), Datum::Int(30)]
    );
    assert_eq!(
        cells(of(WinFunc::FirstValue), table),
        vec![Datum::Int(10), Datum::Int(10), Datum::Int(10)]
    );
}

#[test]
fn an_aggregate_window_repeats_the_partitions_fold_on_every_row_of_it() {
    let table =
        vec![row(&[text("GB"), int(10)]), row(&[text("GB"), int(30)]), row(&[text("US"), int(5)])];
    let fold = |func| Selection::Over {
        func,
        arg: Some(1),
        offset: 1,
        window: Frame { partition: vec![0], order: Vec::new() },
    };

    assert_eq!(
        cells(shape("country", 0, "sum", fold(WinFunc::Sum)), table.clone()),
        vec![Datum::Int(40), Datum::Int(40), Datum::Int(5)]
    );
    assert_eq!(
        cells(shape("country", 0, "max", fold(WinFunc::Max)), table.clone()),
        vec![Datum::Int(30), Datum::Int(30), Datum::Int(5)]
    );
    assert_eq!(
        cells(shape("country", 0, "avg", fold(WinFunc::Avg)), table),
        vec![Datum::Real(20.0), Datum::Real(20.0), Datum::Real(5.0)]
    );
}

#[test]
fn a_fold_skips_the_records_holding_nothing_and_counts_what_it_folded() {
    // Absence is not zero here, exactly as it is not anywhere else: an average over two values
    // and one absence is over two, and a fold over nothing at all is `null` rather than `0`.
    let table = vec![row(&[int(10)]), row(&[Projection::Absent]), row(&[int(20)])];
    let fold = |func, arg| Selection::Over {
        func,
        arg,
        offset: 1,
        window: Frame { partition: Vec::new(), order: Vec::new() },
    };

    assert_eq!(
        cells(shape("amount", 0, "avg", fold(WinFunc::Avg, Some(0))), table.clone()),
        vec![Datum::Real(15.0); 3]
    );
    // `count(x)` counts the rows holding a value; `count(*)` counts the partition.
    assert_eq!(
        cells(shape("amount", 0, "n", fold(WinFunc::Count, Some(0))), table.clone()),
        vec![Datum::Int(2); 3]
    );
    assert_eq!(
        cells(shape("amount", 0, "n", fold(WinFunc::Count, None)), table),
        vec![Datum::Int(3); 3]
    );

    let empty = vec![row(&[Projection::Absent])];
    assert_eq!(
        cells(shape("amount", 0, "sum", fold(WinFunc::Sum, Some(0))), empty),
        vec![Datum::Null]
    );
}

#[test]
fn a_window_orders_on_the_stored_value_and_buries_absence_last() {
    // The same rule `ORDER BY` over a projection follows, and it has to be the same one: a
    // window that sorted its nulls differently would number rows in an order the answer is not
    // in. Absent last in both directions.
    let table = vec![row(&[int(20)]), row(&[Projection::Absent]), row(&[int(10)])];
    let ranked = |desc| Selection::Over {
        func: WinFunc::RowNumber,
        arg: None,
        offset: 1,
        window: Frame { partition: Vec::new(), order: vec![(0, desc)] },
    };

    // Ascending: 10, 20, then the absent one.
    assert_eq!(
        cells(shape("amount", 0, "n", ranked(false)), table.clone()),
        vec![Datum::Int(2), Datum::Int(3), Datum::Int(1)]
    );
    // Descending: 20, 10, and the absent one is still last.
    assert_eq!(
        cells(shape("amount", 0, "n", ranked(true)), table),
        vec![Datum::Int(1), Datum::Int(3), Datum::Int(2)]
    );
}

#[test]
fn a_window_over_no_rows_and_over_one_row_both_answer() {
    let of = shape("amount", 0, "n", over(WinFunc::RowNumber, None, 0));
    assert!(result_set(&answer(of), &[Value::Table(Vec::new())]).rows.is_empty());

    let of = shape("amount", 0, "pr", over(WinFunc::PercentRank, None, 0));
    // A partition of one has no spread, so `percent_rank` is zero rather than a division by
    // nothing.
    assert_eq!(cells(of, vec![row(&[int(7)])]), vec![Datum::Real(0.0)]);
}

#[test]
fn ntile_gives_the_remainder_to_the_earlier_buckets() {
    // Four rows into three buckets is `1, 1, 2, 3` - the bigger bucket first, which is what
    // every dialect does.
    let of = Selection::Over {
        func: WinFunc::NTile,
        arg: None,
        offset: 3,
        window: Frame { partition: Vec::new(), order: vec![(0, false)] },
    };
    let table = vec![row(&[int(1)]), row(&[int(2)]), row(&[int(3)]), row(&[int(4)])];

    assert_eq!(
        cells(shape("amount", 0, "b", of), table),
        vec![Datum::Int(1), Datum::Int(1), Datum::Int(2), Datum::Int(3)]
    );
}

#[test]
fn a_window_reads_a_field_the_header_does_not_show() {
    // `SELECT country, row_number() OVER (ORDER BY amount)`: the plan reads two fields and the
    // answer has two columns, but they are not the same two. The header shows `country`; the
    // ordering is by `amount`, which the answer never shows.
    let table = vec![row(&[text("GB"), int(30)]), row(&[text("US"), int(10)])];
    let set = result_set(
        &answer(shape("country", 0, "n", over(WinFunc::RowNumber, None, 1))),
        &[Value::Table(table)],
    );

    assert_eq!(set.columns, vec!["country", "n"]);
    assert_eq!(
        set.rows,
        vec![
            vec![Datum::Text("GB".to_string()), Datum::Int(2)],
            vec![Datum::Text("US".to_string()), Datum::Int(1)],
        ]
    );
}

#[test]
fn the_cut_is_applied_after_the_window_has_numbered_every_row() {
    // The whole of what a window costs, asserted: the rows kept are the first two, and their
    // numbers came from a ranking over all four. A `LIMIT` that had ridden in the plan would
    // have left the last two unread and the numbers would say so.
    let mut shape = shape("amount", 0, "n", over(WinFunc::RowNumber, None, 0));
    if let Shape::Table { cut, .. } = &mut shape {
        *cut = Some(2);
    }
    let table = vec![row(&[int(40)]), row(&[int(10)]), row(&[int(30)]), row(&[int(20)])];

    let set = result_set(&answer(shape), &[Value::Table(table)]);

    assert_eq!(set.rows.len(), 2);
    assert_eq!(set.rows[0], vec![Datum::Int(40), Datum::Int(4)]);
    assert_eq!(set.rows[1], vec![Datum::Int(10), Datum::Int(1)]);
}

#[test]
fn a_value_returning_window_carries_the_scale_its_field_keeps() {
    // `lag(price)` is a price, so it renders with the point - the same rule the column it read
    // follows. A window that answered in stored units would be off by a factor of the scale,
    // with both numbers valid and nothing in the answer able to show it.
    let of = Selection::Over {
        func: WinFunc::Lag,
        arg: Some(0),
        offset: 1,
        window: Frame { partition: Vec::new(), order: vec![(0, false)] },
    };
    let shape = Shape::Table {
        columns: Columns::Named(vec![Selected {
            column: "lag".to_string(),
            units: Units::Digits(2),
            apply: None,
            of,
        }]),
        order: None,
        cut: None,
    };
    let table = vec![row(&[int(1250)]), row(&[int(3000)])];

    let set = result_set(&answer(shape), &[Value::Table(table)]);

    assert_eq!(set.rows[0], vec![Datum::Null]);
    assert_eq!(set.rows[1], vec![Datum::Dec { units: 1250, scale: 2 }]);
}

#[test]
fn a_cut_beside_no_window_and_no_order_is_unreachable_but_harmless() {
    // A plain projection still renders through the same arm, unchanged: the columns index into
    // the plan's fields and every one of them is a `Read`.
    let shape = Shape::Table {
        columns: Columns::Named(vec![
            Selected::read("amount", Units::PLAIN, None, 0),
            Selected::read("country", Units::PLAIN, None, 1),
        ]),
        order: None,
        cut: None,
    };
    let table = vec![row(&[int(10), text("GB")])];

    let set = result_set(&answer(shape), &[Value::Table(table)]);

    assert_eq!(set.rows, vec![vec![Datum::Int(10), Datum::Text("GB".to_string())]]);
}
