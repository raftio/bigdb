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

//! Dropping the day views a time quantum field has outgrown.
//!
//! A time quantum field writes a copy of every fact into the view for its day, and until now
//! nothing ever removed one: a table ingesting for two years held two years of day views
//! whether or not anybody would ever ask about the first one. These tests are about what goes
//! and, more importantly, about what stays - the standard view is not a day view, so the
//! records themselves are still there and every question that carries no time still sees them.

use big_db::*;
use big_pager::MemPager;

const DAY: i64 = 86_400;

/// Ten consecutive days, one record each, all carrying the same key.
fn seeded() -> (Db<MemPager>, Vec<i64>) {
    let d = Db::in_memory().unwrap();
    d.create_table("v").unwrap();
    d.create_time_quantum("v", "visit", Vec::new()).unwrap();

    // A fixed instant rather than "now", so the day names are the same on every run and in
    // every time zone. 2026-01-01T00:00:00Z.
    let epoch = 1_767_225_600i64;
    let days: Vec<i64> = (0..10).map(|i| epoch + i * DAY).collect();

    let mut w = d.write();
    for (i, at) in days.iter().enumerate() {
        w.set_time("v", "visit", i as u64 + 1, "home", *at).unwrap();
    }
    w.commit().unwrap();
    (d, days)
}

fn in_window(d: &Db<MemPager>, from: i64, to: i64) -> u64 {
    d.read().matching_key_between("v", "visit", "home", Some(from), Some(to)).unwrap().cardinality()
}

#[test]
fn every_day_is_answerable_before_anything_is_dropped() {
    let (d, days) = seeded();
    assert_eq!(in_window(&d, days[0], days[9]), 10);
}

/// The cutoff day itself is kept. "Keep from here on" that quietly took one more day would be
/// off by one in the direction nobody checks.
#[test]
fn dropping_before_a_day_keeps_that_day() {
    let (d, days) = seeded();

    let dropped = d.drop_days_before("v", "visit", days[4]).unwrap();
    assert!(dropped > 0, "four days of fragments should have gone");

    assert_eq!(in_window(&d, days[4], days[9]), 6, "the days that were kept must all answer");
    assert_eq!(in_window(&d, days[0], days[3]), 0, "the days that were dropped must answer none");
    // And the boundary itself, asked on its own.
    assert_eq!(in_window(&d, days[4], days[4]), 1);
}

/// The honest shape of the operation: it drops the index by day, not the records. A query with
/// no time still sees every one of them, which is what makes this retention on an index rather
/// than a delete wearing a different name.
#[test]
fn the_records_themselves_are_still_there() {
    let (d, days) = seeded();
    let before = d.read().matching_key("v", "visit", "home").unwrap().cardinality();
    assert_eq!(before, 10);

    d.drop_days_before("v", "visit", days[5]).unwrap();

    let after = d.read().matching_key("v", "visit", "home").unwrap().cardinality();
    assert_eq!(after, 10, "dropping day views must not remove a record from the standard view");
}

#[test]
fn dropping_twice_is_a_no_op_the_second_time() {
    let (d, days) = seeded();
    let first = d.drop_days_before("v", "visit", days[3]).unwrap();
    assert!(first > 0);
    assert_eq!(d.drop_days_before("v", "visit", days[3]).unwrap(), 0);
}

/// Against a field that has no day views, "nothing was dropped" and "this field never had days"
/// would be the same answer, and they call for opposite actions. So it is refused.
#[test]
fn a_field_that_is_not_a_time_quantum_is_refused_rather_than_answering_zero() {
    let d = Db::in_memory().unwrap();
    d.create_table("v").unwrap();
    d.create_field("v", "country", FieldKind::Set, 0).unwrap();

    let e = d.drop_days_before("v", "country", 1_767_225_600).unwrap_err();
    assert!(matches!(e, DbError::WrongFieldKind { .. }), "{e}");
}

#[test]
fn an_unknown_table_or_field_is_named() {
    let (d, days) = seeded();
    assert!(matches!(
        d.drop_days_before("nope", "visit", days[0]).unwrap_err(),
        DbError::UnknownTable(_)
    ));
    assert!(matches!(
        d.drop_days_before("v", "nope", days[0]).unwrap_err(),
        DbError::UnknownField { .. }
    ));
}

/// The pages behind the dropped views go back to the freelist rather than being orphaned.
#[test]
fn the_pages_behind_a_dropped_day_are_freed() {
    let (d, days) = seeded();
    let before = d.store().metrics().fragments;
    d.drop_days_before("v", "visit", days[9]).unwrap();
    let after = d.store().metrics().fragments;
    assert!(after < before, "the catalog still holds {after} fragments, down from {before}");
    // And the whole thing still scrubs, which is the check that nothing was left pointing at a
    // page that has been handed back.
    d.scrub().expect("the database has to stay consistent after a retention drop");
}
