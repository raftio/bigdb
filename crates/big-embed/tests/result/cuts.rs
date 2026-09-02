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

//! `HAVING`, then `ORDER BY`, then `OFFSET`, then `LIMIT`, then `WITH TIES`.
//!
//! The order SQL specifies and the only one that is right here: a limit applied before the
//! predicate would count rows the predicate is about to drop, and an offset applied before the
//! sort would skip into a list nobody asked for.

use crate::common::*;
use big_embed::{
    result_set, Absent, Cut, Datum, GroupOrder, Having, Of, OrderBy, ResultSet, Shape, Threshold,
    Units, Value,
};

fn of() -> Of {
    Of::Group { plan: 0, absent: Absent::Null }
}

fn shape(having: Option<Having>, order: Option<GroupOrder>, cut: Cut) -> Shape {
    Shape::Groups {
        keys: vec![0],
        cells: vec![cell("k", Of::Key), cell("n", of())],
        having,
        order,
        cut,
    }
}

/// Five groups whose counts are 5, 4, 3, 3, 1 - two of them tied.
fn values() -> Vec<Value> {
    vec![groups(vec![
        group(1, Some("a"), Value::Count(5)),
        group(2, Some("b"), Value::Count(4)),
        group(3, Some("c"), Value::Count(3)),
        group(4, Some("d"), Value::Count(3)),
        group(5, Some("e"), Value::Count(1)),
    ])]
}

fn keys(set: ResultSet) -> Vec<String> {
    set.rows
        .into_iter()
        .map(|r| match &r[0] {
            Datum::Text(s) => s.clone(),
            other => panic!("expected a key, got {other:?}"),
        })
        .collect()
}

fn descending() -> GroupOrder {
    GroupOrder { by: OrderBy::Value { of: of() }, desc: true }
}

#[test]
fn a_limit_takes_the_first_rows_of_the_ordering() {
    let cut = Cut { offset: None, limit: Some(2), ties: false };

    let set = result_set(&answer(shape(None, Some(descending()), cut)), &values());

    assert_eq!(keys(set), vec!["a", "b"]);
}

#[test]
fn an_offset_is_applied_before_the_limit_and_after_the_ordering() {
    let cut = Cut { offset: Some(1), limit: Some(2), ties: false };

    let set = result_set(&answer(shape(None, Some(descending()), cut)), &values());

    assert_eq!(keys(set), vec!["b", "c"]);
}

#[test]
fn with_ties_keeps_every_row_the_ordering_cannot_tell_from_the_last_one() {
    // `c` and `d` both count 3. A limit of three would cut between them, which is an answer
    // that depends on which node replied first.
    let cut = Cut { offset: None, limit: Some(3), ties: true };

    let set = result_set(&answer(shape(None, Some(descending()), cut)), &values());

    assert_eq!(keys(set), vec!["a", "b", "c", "d"]);
}

#[test]
fn limit_zero_with_ties_keeps_nothing() {
    // There is no last row for anything to tie with, so the ties rule has nothing to extend.
    let cut = Cut { offset: None, limit: Some(0), ties: true };

    let set = result_set(&answer(shape(None, Some(descending()), cut)), &values());

    assert!(keys(set).is_empty());
}

#[test]
fn a_having_is_applied_before_the_limit_counts_anything() {
    // Three groups survive `> 2`. A limit of two applied first would have counted `e` towards
    // the two and answered with one row.
    let having = Having::cmp(of(), Units::PLAIN, ">", Threshold::Units(2));
    let cut = Cut { offset: Some(2), limit: Some(2), ties: false };

    let set = result_set(&answer(shape(Some(having), Some(descending()), cut)), &values());

    assert_eq!(keys(set), vec!["c", "d"]);
}

#[test]
fn ordering_by_key_breaks_nothing_and_ties_on_the_key_itself() {
    let order = GroupOrder { by: OrderBy::Key, desc: true };
    let cut = Cut { offset: None, limit: Some(2), ties: true };

    let set = result_set(&answer(shape(None, Some(order), cut)), &values());

    assert_eq!(keys(set), vec!["e", "d"]);
}

#[test]
fn an_offset_past_the_end_is_no_rows_rather_than_the_last_one() {
    let cut = Cut { offset: Some(99), limit: Some(2), ties: false };

    let set = result_set(&answer(shape(None, Some(descending()), cut)), &values());

    assert!(keys(set).is_empty());
}
