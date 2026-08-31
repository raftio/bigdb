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

//! Signed integers, end to end.
//!
//! The whole design rests on one property, tested in `signed.rs` itself: the bias is monotonic,
//! so every comparison below it works unchanged. What is tested here is that nothing above it
//! forgot - a range query that compares stored values, a zone map built from stored values, a
//! sum that has to take the bias back out once per record rather than once per sum.
//!
//! The failure mode this file exists for is not "signed values do not work". It is that they
//! work for positives and quietly stop at zero, which every ordinary test would pass.

use big_db::*;
use big_fragment::SHARD_WIDTH;
use proptest::prelude::*;

const DEPTH: u32 = 20;

fn db() -> Db<big_pager::MemPager> {
    let d = Db::in_memory().unwrap();
    d.create_table("acct").unwrap();
    d.create_signed("acct", "balance", DEPTH).unwrap();
    d.create_field("acct", "count", FieldKind::Int, DEPTH).unwrap();
    d
}

fn seed(d: &Db<big_pager::MemPager>, vals: impl IntoIterator<Item = (u64, i64)>) {
    let mut w = d.write();
    for (r, v) in vals {
        w.set_signed("acct", "balance", r, v).unwrap();
    }
    w.commit().unwrap();
}

fn matching(d: &Db<big_pager::MemPager>, op: RangeOp, k: i64) -> Vec<RecordId> {
    d.read().matching_signed("acct", "balance", op, k).unwrap().records().collect()
}

#[test]
fn a_negative_value_round_trips() {
    let d = db();
    seed(&d, [(1u64, -5i64), (2, 0), (3, 7)]);
    let r = d.read();
    assert_eq!(r.get_signed("acct", "balance", 1).unwrap(), Some(-5));
    assert_eq!(r.get_signed("acct", "balance", 2).unwrap(), Some(0));
    assert_eq!(r.get_signed("acct", "balance", 3).unwrap(), Some(7));
    assert_eq!(r.get_signed("acct", "balance", 4).unwrap(), None);
}

#[test]
fn the_extremes_of_the_declared_range_round_trip() {
    let d = db();
    let (lo, hi) = (big_db::signed::min_value(DEPTH), big_db::signed::max_value(DEPTH));
    seed(&d, [(1u64, lo), (2, hi)]);
    let r = d.read();
    assert_eq!(r.get_signed("acct", "balance", 1).unwrap(), Some(lo));
    assert_eq!(r.get_signed("acct", "balance", 2).unwrap(), Some(hi));
}

#[test]
fn a_value_outside_the_range_is_refused_not_wrapped() {
    let d = db();
    let mut w = d.write();
    let too_big = big_db::signed::max_value(DEPTH) + 1;
    assert!(matches!(
        w.set_signed("acct", "balance", 1, too_big),
        Err(DbError::SignedValueOutOfRange { .. })
    ));
}

#[test]
fn comparisons_order_negatives_below_positives() {
    // The claim the whole encoding exists for. Under two's complement `-1 > 1`, and every one
    // of these would come back with the negatives on the wrong side.
    let d = db();
    seed(&d, [(1u64, -100i64), (2, -1), (3, 0), (4, 1), (5, 100)]);

    assert_eq!(matching(&d, RangeOp::Gt, 0), [4, 5]);
    assert_eq!(matching(&d, RangeOp::Ge, 0), [3, 4, 5]);
    assert_eq!(matching(&d, RangeOp::Lt, 0), [1, 2]);
    assert_eq!(matching(&d, RangeOp::Le, 0), [1, 2, 3]);
    assert_eq!(matching(&d, RangeOp::Gt, -1), [3, 4, 5]);
    assert_eq!(matching(&d, RangeOp::Lt, -1), [1]);
    assert_eq!(matching(&d, RangeOp::Eq, -1), [2]);
    assert_eq!(matching(&d, RangeOp::Ne, -1), [1, 3, 4, 5]);
}

#[test]
fn comparisons_work_across_shards() {
    // Zone maps are built from stored values, and a fragment is skipped when its window rules
    // the bound out. A bias applied per fragment rather than per field would make those windows
    // incomparable and skip the wrong shards.
    let d = db();
    seed(&d, [(1u64, -50i64), (SHARD_WIDTH, 50), (2 * SHARD_WIDTH, -5)]);
    assert_eq!(matching(&d, RangeOp::Lt, 0), [1, 2 * SHARD_WIDTH]);
    assert_eq!(matching(&d, RangeOp::Gt, 0), [SHARD_WIDTH]);
}

#[test]
fn a_bound_past_the_end_of_the_range_is_answered_not_refused() {
    // `> 10_000_000` on a field that stops well below it is a legitimate question, and the
    // answer does not depend on the schema. Clamping alone would get it wrong: `> max` and
    // `>= max` clamp to the same stored bound, and only one of them matches the largest record.
    let d = db();
    seed(&d, [(1u64, -5i64), (2, 5)]);
    let huge = big_db::signed::max_value(DEPTH) + 1_000;
    let tiny = big_db::signed::min_value(DEPTH) - 1_000;

    assert_eq!(matching(&d, RangeOp::Gt, huge), Vec::<RecordId>::new());
    assert_eq!(matching(&d, RangeOp::Ge, huge), Vec::<RecordId>::new());
    assert_eq!(matching(&d, RangeOp::Lt, huge), [1, 2]);
    assert_eq!(matching(&d, RangeOp::Le, huge), [1, 2]);
    assert_eq!(matching(&d, RangeOp::Gt, tiny), [1, 2]);
    assert_eq!(matching(&d, RangeOp::Lt, tiny), Vec::<RecordId>::new());
    assert_eq!(matching(&d, RangeOp::Eq, huge), Vec::<RecordId>::new());
}

#[test]
fn a_bound_past_the_range_matches_records_not_the_table() {
    // "Everything" means every record that holds a value, not every record in the table. A
    // record with no balance is not less than a large number - it has no balance.
    let d = db();
    seed(&d, [(1u64, 5i64)]);
    let mut w = d.write();
    w.set_int("acct", "count", 2, 9).unwrap();
    w.commit().unwrap();

    assert_eq!(d.read().count_all("acct").unwrap(), 2);
    assert_eq!(matching(&d, RangeOp::Lt, big_db::signed::max_value(DEPTH) + 1_000), [1]);
}

#[test]
fn a_sum_over_negatives_is_negative() {
    // The bias is per record, so it comes back out `count` times rather than once. Getting that
    // wrong is invisible on a single record and wrong by a multiple of 2^19 on anything else.
    let d = db();
    seed(&d, [(1u64, -10i64), (2, -20), (3, 5)]);
    let r = d.read();
    let all = r.all("acct").unwrap();
    assert_eq!(r.sum_signed_where("acct", "balance", &all).unwrap(), -25);
    assert_eq!(r.min_signed_where("acct", "balance", &all).unwrap(), Some(-20));
    assert_eq!(r.max_signed_where("acct", "balance", &all).unwrap(), Some(5));
}

#[test]
fn a_sum_ignores_records_that_hold_no_value() {
    // The subtlety in `sum_signed_where`: the bias must be removed once per record that has a
    // value, not once per record in the filter. A table with nulls would otherwise come out
    // wrong by 2^19 per null.
    let d = db();
    seed(&d, [(1u64, -10i64), (2, 4)]);
    let mut w = d.write();
    w.set_int("acct", "count", 3, 1).unwrap();
    w.commit().unwrap();

    let r = d.read();
    let all = r.all("acct").unwrap();
    assert_eq!(all.cardinality(), 3, "the filter must include the record with no balance");
    assert_eq!(r.sum_signed_where("acct", "balance", &all).unwrap(), -6);
}

#[test]
fn a_sum_over_nothing_is_zero_not_a_bias() {
    let d = db();
    seed(&d, [(1u64, 3i64)]);
    let r = d.read();
    let none = Matches::new();
    assert_eq!(r.sum_signed_where("acct", "balance", &none).unwrap(), 0);
    assert_eq!(r.min_signed_where("acct", "balance", &none).unwrap(), None);
}

#[test]
fn the_two_integer_kinds_refuse_each_others_setters() {
    // `is_bsi` covers both, so without an explicit refusal `set_int` on a signed field would
    // store an unbiased number that reads back as something else entirely.
    let d = db();
    let mut w = d.write();
    assert!(matches!(w.set_int("acct", "balance", 1, 5), Err(DbError::WrongFieldKind { .. })));
    assert!(matches!(w.set_signed("acct", "count", 1, 5), Err(DbError::WrongFieldKind { .. })));
}

#[test]
fn a_field_kind_from_the_future_is_refused_rather_than_skipped() {
    // The prerequisite for adding a kind at all, and a bug that predates signed integers.
    // `FieldKind::from_u8` returning `None` used to `continue`: the field vanished from the
    // catalog, its data stayed on disk unreachable, and every query naming it answered "unknown
    // field" as though the schema had never had it. A file from a newer build was
    // indistinguishable from one missing a field, and the two call for opposite actions.
    let d = db();
    let mut entries = d.catalog().encode();
    let field = entries
        .iter_mut()
        .find(|e| e[0] == big_db::catalog::KIND_FIELD)
        .expect("the fixture has fields");
    field[1] = 200;

    let err = Catalog::from_entries(&entries).expect_err("an unknown kind must not be silent");
    assert!(matches!(err, DbError::UnknownFieldKind { kind: 200, .. }), "{err:?}");
    assert!(err.to_string().contains("newer big"), "{err}");
}

#[test]
fn signed_values_survive_a_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("signed.big");
    {
        let d = Db::open_path(&path).unwrap();
        d.create_table("acct").unwrap();
        d.create_signed("acct", "balance", DEPTH).unwrap();
        let mut w = d.write();
        w.set_signed("acct", "balance", 1, -42).unwrap();
        w.commit().unwrap();
    }
    let d = Db::open_path(&path).unwrap();
    assert_eq!(d.read().get_signed("acct", "balance", 1).unwrap(), Some(-42));
}

#[test]
fn deleting_a_record_removes_its_signed_value() {
    let d = db();
    seed(&d, [(1u64, -7i64), (2, 7)]);
    let mut w = d.write();
    w.delete("acct", &[1]).unwrap();
    w.commit().unwrap();

    let r = d.read();
    assert_eq!(r.get_signed("acct", "balance", 1).unwrap(), None);
    assert_eq!(r.sum_signed_where("acct", "balance", &r.all("acct").unwrap()).unwrap(), 7);
}

#[test]
fn a_query_can_name_a_negative_bound() {
    let d = db();
    seed(&d, [(1u64, -100i64), (2, -1), (3, 0), (4, 100)]);
    let r = d.read();

    let ids = |text: &str| -> Vec<RecordId> {
        match big_exec::query(&r, "acct", text).unwrap() {
            big_exec::Value::Rows(m) => m.records().collect(),
            other => panic!("expected records, got {other:?}"),
        }
    };
    assert_eq!(ids("Row(balance < 0)"), [1, 2]);
    assert_eq!(ids("Row(balance >= -1)"), [2, 3, 4]);
    assert_eq!(ids("Row(balance = -100)"), [1]);

    // The aggregate comes back signed because the field is, not because the call is.
    assert!(matches!(
        big_exec::query(&r, "acct", "Sum(All(), field=\"balance\")").unwrap(),
        big_exec::Value::SignedSum(-1)
    ));
    assert!(matches!(
        big_exec::query(&r, "acct", "Min(All(), field=\"balance\")").unwrap(),
        big_exec::Value::SignedExtreme(Some(-100))
    ));
    assert!(matches!(
        big_exec::query(&r, "acct", "Max(All(), field=\"balance\")").unwrap(),
        big_exec::Value::SignedExtreme(Some(100))
    ));
}

#[test]
fn a_negative_bound_on_an_unsigned_field_is_refused() {
    // `-1` against a `u64` field is a mistake worth naming rather than a comparison that
    // matches nothing. Keeping the signed literal a distinct variant is what makes this
    // reachable at all.
    let d = db();
    seed(&d, [(1u64, 1i64)]);
    let r = d.read();
    assert!(big_exec::query(&r, "acct", "Row(count < -1)").is_err());
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(40))]

    /// Whatever the values, a range query agrees with filtering them by hand.
    #[test]
    fn range_queries_agree_with_arithmetic(
        vals in proptest::collection::btree_map(0u64..2_000_000, -400_000i64..400_000, 1..40),
        k in -400_000i64..400_000,
    ) {
        let d = db();
        seed(&d, vals.iter().map(|(r, v)| (*r, *v)));
        let r = d.read();
        for op in [RangeOp::Gt, RangeOp::Ge, RangeOp::Lt, RangeOp::Le, RangeOp::Eq, RangeOp::Ne] {
            let got: Vec<RecordId> =
                r.matching_signed("acct", "balance", op, k).unwrap().records().collect();
            let want: Vec<RecordId> = vals
                .iter()
                .filter(|(_, v)| match op {
                    RangeOp::Gt => **v > k,
                    RangeOp::Ge => **v >= k,
                    RangeOp::Lt => **v < k,
                    RangeOp::Le => **v <= k,
                    RangeOp::Eq => **v == k,
                    RangeOp::Ne => **v != k,
                })
                .map(|(r, _)| *r)
                .collect();
            prop_assert_eq!(got, want, "op {:?} against {}", op, k);
        }
    }

    /// And a sum agrees with adding them up.
    #[test]
    fn sums_agree_with_arithmetic(
        vals in proptest::collection::btree_map(0u64..2_000_000, -400_000i64..400_000, 1..40),
    ) {
        let d = db();
        seed(&d, vals.iter().map(|(r, v)| (*r, *v)));
        let r = d.read();
        let all = r.all("acct").unwrap();
        let want: i128 = vals.values().map(|v| *v as i128).sum();
        prop_assert_eq!(r.sum_signed_where("acct", "balance", &all).unwrap(), want);
        prop_assert_eq!(
            r.min_signed_where("acct", "balance", &all).unwrap(),
            vals.values().copied().min()
        );
        prop_assert_eq!(
            r.max_signed_where("acct", "balance", &all).unwrap(),
            vals.values().copied().max()
        );
    }
}
