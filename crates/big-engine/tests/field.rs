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

use big_engine::bitmap::field::*;
use big_engine::bitmap::{FragmentRead, FragmentWrite, RecordId};
use big_page::Pgno;
use big_pager::{MemPager, Store};
use proptest::prelude::*;
use std::collections::BTreeMap;

const DEPTH: u32 = 37;

fn store() -> Store<MemPager> {
    Store::init(MemPager::new()).unwrap()
}

fn reader(s: &Store<MemPager>, root: Pgno) -> FragmentRead<'_, MemPager> {
    FragmentRead::new(s.pager(), root, 0)
}

/// Writes a batch of BSI values in one transaction.
fn put_values(
    s: &Store<MemPager>,
    root: Option<Pgno>,
    bsi: &Bsi,
    vals: &[(RecordId, u64)],
) -> Pgno {
    let mut w = s.begin_write();
    let mut f = FragmentWrite::new(root, 0);
    for (rec, v) in vals {
        bsi.set(&mut w, &mut f, *rec, *v).unwrap();
    }
    let root = f.root().unwrap();
    w.commit().unwrap();
    root
}

#[test]
fn a_value_round_trips_and_null_is_distinguishable_from_zero() {
    let s = store();
    let bsi = Bsi::new(DEPTH);
    let root = put_values(&s, None, &bsi, &[(1, 0), (2, 12_345_678_901), (3, 1)]);
    let r = reader(&s, root);

    assert_eq!(bsi.get(&r, 1).unwrap(), Some(0), "zero is a value");
    assert_eq!(bsi.get(&r, 2).unwrap(), Some(12_345_678_901));
    assert_eq!(bsi.get(&r, 3).unwrap(), Some(1));
    assert_eq!(bsi.get(&r, 4).unwrap(), None, "never written is NULL, not zero");
    assert_eq!(bsi.count(&r, None).unwrap(), 3);
}

#[test]
fn overwriting_replaces_rather_than_ors_the_planes() {
    let s = store();
    let bsi = Bsi::new(DEPTH);
    let mut root = put_values(&s, None, &bsi, &[(1, 0b1111)]);
    root = put_values(&s, Some(root), &bsi, &[(1, 0b0001)]);
    assert_eq!(bsi.get(&reader(&s, root), 1).unwrap(), Some(0b0001));
}

#[test]
fn a_value_wider_than_the_depth_is_refused_not_truncated() {
    let s = store();
    let bsi = Bsi::new(8);
    let mut w = s.begin_write();
    let mut f = FragmentWrite::new(None, 0);
    assert!(matches!(
        bsi.set(&mut w, &mut f, 1, 256),
        Err(FieldError::ValueTooWide { value: 256, bit_depth: 8 })
    ));
}

#[test]
fn range_queries_walk_the_planes_from_the_top() {
    let s = store();
    let bsi = Bsi::new(DEPTH);
    let vals: Vec<(u64, u64)> = (0..100u64).map(|i| (i, i * 7)).collect();
    let root = put_values(&s, None, &bsi, &vals);
    let r = reader(&s, root);

    let recs = |op, k| {
        let mut v: Vec<u64> = bsi.range(&r, op, k).unwrap().records(0).collect();
        v.sort_unstable();
        v
    };
    let want = |f: &dyn Fn(u64) -> bool| {
        vals.iter().filter(|(_, v)| f(*v)).map(|(rec, _)| *rec).collect::<Vec<_>>()
    };

    assert_eq!(recs(RangeOp::Gt, 350), want(&|v| v > 350));
    assert_eq!(recs(RangeOp::Ge, 350), want(&|v| v >= 350));
    assert_eq!(recs(RangeOp::Lt, 350), want(&|v| v < 350));
    assert_eq!(recs(RangeOp::Le, 350), want(&|v| v <= 350));
    assert_eq!(recs(RangeOp::Eq, 350), want(&|v| v == 350));
    assert_eq!(recs(RangeOp::Ne, 350), want(&|v| v != 350));
}

#[test]
fn sum_min_max_use_planes_not_records() {
    let s = store();
    let bsi = Bsi::new(DEPTH);
    let vals: Vec<(u64, u64)> = vec![(1, 5), (2, 100), (3, 70_000_000_000), (4, 0)];
    let root = put_values(&s, None, &bsi, &vals);
    let r = reader(&s, root);

    assert_eq!(bsi.sum(&r, None).unwrap(), 70_000_000_105u128);
    assert_eq!(bsi.max(&r, None).unwrap(), Some(70_000_000_000));
    assert_eq!(bsi.min(&r, None).unwrap(), Some(0));

    let big = bsi.range(&r, RangeOp::Gt, 50).unwrap();
    assert_eq!(bsi.sum(&r, Some(&big)).unwrap(), 70_000_000_100u128);
    assert_eq!(bsi.count(&r, Some(&big)).unwrap(), 2);
    assert_eq!(bsi.min(&r, Some(&big)).unwrap(), Some(100));
}

#[test]
fn clearing_a_record_makes_it_null_again() {
    let s = store();
    let bsi = Bsi::new(DEPTH);
    let root = put_values(&s, None, &bsi, &[(1, 42), (2, 43)]);

    let mut w = s.begin_write();
    let mut f = FragmentWrite::new(Some(root), 0);
    bsi.clear(&mut w, &mut f, 1).unwrap();
    let root = f.root().unwrap();
    w.commit().unwrap();

    let r = reader(&s, root);
    assert_eq!(bsi.get(&r, 1).unwrap(), None);
    assert_eq!(bsi.get(&r, 2).unwrap(), Some(43));
    assert_eq!(bsi.count(&r, None).unwrap(), 1);
}

#[test]
fn bool_is_a_mutex_with_two_rows() {
    let s = store();
    let mut w = s.begin_write();
    let mut f = FragmentWrite::new(None, 0);
    BoolField::set(&mut w, &mut f, 1, true).unwrap();
    BoolField::set(&mut w, &mut f, 2, false).unwrap();
    BoolField::set(&mut w, &mut f, 1, false).unwrap();
    let root = f.root().unwrap();
    w.commit().unwrap();

    let r = reader(&s, root);
    assert_eq!(BoolField::get(&r, 1).unwrap(), Some(false), "the old row must be cleared");
    assert_eq!(BoolField::get(&r, 2).unwrap(), Some(false));
    assert_eq!(BoolField::get(&r, 3).unwrap(), None);
}

#[test]
fn set_field_lets_one_record_sit_in_many_rows() {
    let s = store();
    let mut w = s.begin_write();
    let mut f = FragmentWrite::new(None, 0);
    for row in [1u64, 5, 9] {
        SetField::set(&mut w, &mut f, row, 42).unwrap();
    }
    SetField::set(&mut w, &mut f, 5, 43).unwrap();
    let root = f.root().unwrap();
    w.commit().unwrap();

    let r = reader(&s, root);
    assert_eq!(SetField::rows_of(&r, 42).unwrap(), vec![1, 5, 9]);
    assert_eq!(SetField::count(&r, 5).unwrap(), 2);
}

/// The shadow BSI is what makes clearing the old bit cheap, so it must stay in step.
#[test]
fn mutex_moves_a_record_and_the_shadow_agrees() {
    let s = store();
    let m = MutexField::new(20);

    // Two fragments of the same shard, moved in one transaction. That is the whole point of
    // the handle not owning a borrow of the transaction.
    let mut w = s.begin_write();
    let mut vals = FragmentWrite::new(None, 0);
    let mut shadow = FragmentWrite::new(None, 0);

    m.put(&mut w, &mut vals, &mut shadow, 7, 3).unwrap();
    m.put(&mut w, &mut vals, &mut shadow, 8, 3).unwrap();
    m.put(&mut w, &mut vals, &mut shadow, 7, 9).unwrap();

    let (vroot, sroot) = (vals.root().unwrap(), shadow.root().unwrap());
    w.commit().unwrap();

    let vr = reader(&s, vroot);
    let sr = reader(&s, sroot);
    assert_eq!(m.get(&sr, 7).unwrap(), Some(9), "the shadow tracks the move");
    assert_eq!(m.get(&sr, 8).unwrap(), Some(3));
    m.verify(&vr, &sr, 7).unwrap();
    m.verify(&vr, &sr, 8).unwrap();
    assert_eq!(m.row(&vr, 3).unwrap().records(0).collect::<Vec<_>>(), vec![8], "row 3 lost 7");
    assert_eq!(m.row(&vr, 9).unwrap().records(0).collect::<Vec<_>>(), vec![7]);
}

#[test]
fn time_quantum_decomposes_into_view_names() {
    // 2026-08-27T05:00:00Z
    let ts = 1_787_806_800i64;
    let t = decompose(ts);
    assert_eq!((t.year, t.month, t.day), (2026, 8, 27));

    assert_eq!(views(ts, DEFAULT_GRANULARITY), vec!["20260827"]);
    assert_eq!(
        views(ts, &[Granularity::Year, Granularity::Month, Granularity::Day, Granularity::Hour]),
        vec!["2026", "202608", "20260827", "2026082705"]
    );
}

/// Finding a range of days by comparing view names only works if those names sort the way the
/// days do. Nothing enforces that but the zero padding, so it is asserted rather than assumed.
#[test]
fn day_view_names_sort_chronologically() {
    let from = 1_787_806_800i64;
    let names: Vec<String> = (0..=5).map(|d| day_view(from + d * 86_400)).collect();

    assert_eq!(names.first().unwrap(), "20260827");
    assert_eq!(names.last().unwrap(), "20260901", "must roll over the month");
    assert!(names.windows(2).all(|w| w[0] < w[1]), "later days must sort later: {names:?}");
    assert!(names.iter().all(|n| n.len() == DAY_VIEW_LEN));

    // The coarser views are shorter, so they sort before any day inside them and never land in
    // the middle of a day range.
    let coarse = views(from, &[Granularity::Year, Granularity::Month]);
    assert!(coarse.iter().all(|c| c.as_str() < names[0].as_str()));
}

#[test]
fn epoch_and_pre_epoch_dates_decompose_correctly() {
    assert_eq!(decompose(0), DateTime { year: 1970, month: 1, day: 1, hour: 0 });
    assert_eq!(decompose(-1), DateTime { year: 1969, month: 12, day: 31, hour: 23 });
    assert_eq!(decompose(951_782_400), DateTime { year: 2000, month: 2, day: 29, hour: 0 });
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(80))]

    /// Range queries must agree with filtering the values by hand.
    #[test]
    fn bsi_matches_a_plain_map(
        vals in proptest::collection::btree_map(0u64..2000, 0u64..1_000_000, 1..60),
        k in 0u64..1_000_000,
    ) {
        let s = store();
        let bsi = Bsi::new(20);
        let entries: Vec<(u64, u64)> = vals.iter().map(|(a, b)| (*a, *b)).collect();
        let root = put_values(&s, None, &bsi, &entries);
        let r = reader(&s, root);

        for (rec, v) in &vals {
            prop_assert_eq!(bsi.get(&r, *rec).unwrap(), Some(*v));
        }

        let check = |op: RangeOp, f: &dyn Fn(u64) -> bool| -> Result<()> {
            let mut got: Vec<u64> = bsi.range(&r, op, k).unwrap().records(0).collect();
            got.sort_unstable();
            let want: Vec<u64> =
                vals.iter().filter(|(_, v)| f(**v)).map(|(rec, _)| *rec).collect();
            assert_eq!(got, want, "{op:?} {k}");
            Ok(())
        };
        check(RangeOp::Gt, &|v| v > k).unwrap();
        check(RangeOp::Ge, &|v| v >= k).unwrap();
        check(RangeOp::Lt, &|v| v < k).unwrap();
        check(RangeOp::Le, &|v| v <= k).unwrap();
        check(RangeOp::Eq, &|v| v == k).unwrap();

        let total: u128 = vals.values().map(|v| *v as u128).sum();
        prop_assert_eq!(bsi.sum(&r, None).unwrap(), total);
        prop_assert_eq!(bsi.max(&r, None).unwrap(), vals.values().copied().max());
        prop_assert_eq!(bsi.min(&r, None).unwrap(), vals.values().copied().min());
    }

    /// Values written in any order must read back the same, since planes are set and cleared
    /// rather than merged.
    #[test]
    fn last_write_wins_per_record(
        writes in proptest::collection::vec((0u64..50, 0u64..100_000), 1..80)
    ) {
        let s = store();
        let bsi = Bsi::new(20);
        let mut model: BTreeMap<u64, u64> = BTreeMap::new();
        let mut root = None;
        for w in writes.chunks(7) {
            root = Some(put_values(&s, root, &bsi, w));
            for (rec, v) in w {
                model.insert(*rec, *v);
            }
        }
        let r = reader(&s, root.unwrap());
        for (rec, v) in &model {
            prop_assert_eq!(bsi.get(&r, *rec).unwrap(), Some(*v));
        }
    }
}
