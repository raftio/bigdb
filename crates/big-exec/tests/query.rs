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

//! Query text in, answer out, checked against doing the same thing with plain Rust.

use big_db::{catalog::FieldKind, catalog::TableEngine, Db};
use big_exec::{query, Projection, Value};
use big_pager::MemPager;
use proptest::prelude::*;
use std::collections::{BTreeMap, BTreeSet};

/// Records span more than one shard on purpose: every combining step is a shard-wise merge,
/// and a single-shard corpus never exercises a shard present on one side only.
const SHARD: u64 = 1 << 20;

struct Fixture {
    db: Db<MemPager>,
    amount: BTreeMap<u64, u64>,
    country: BTreeMap<u64, String>,
    active: BTreeMap<u64, bool>,
}

fn fixture(rows: &[(u64, u64, &str, bool)]) -> Fixture {
    let db = Db::in_memory().unwrap();
    db.create_table("tx").unwrap();
    db.create_field("tx", "amount", FieldKind::Int, 32).unwrap();
    db.create_field("tx", "country", FieldKind::Set, 0).unwrap();
    db.create_field("tx", "active", FieldKind::Bool, 0).unwrap();

    let mut f =
        Fixture { db, amount: BTreeMap::new(), country: BTreeMap::new(), active: BTreeMap::new() };
    let mut w = f.db.write();
    for (rec, amount, country, active) in rows {
        w.set_int("tx", "amount", *rec, *amount).unwrap();
        w.set_key("tx", "country", *rec, country).unwrap();
        w.set_bool("tx", "active", *rec, *active).unwrap();
        f.amount.insert(*rec, *amount);
        f.country.insert(*rec, country.to_string());
        f.active.insert(*rec, *active);
    }
    w.commit().unwrap();
    f
}

impl Fixture {
    fn ids(&self, text: &str) -> Vec<u64> {
        let r = self.db.read();
        match query(&r, "tx", text).unwrap() {
            Value::Rows(m) => m.records().collect(),
            other => panic!("expected rows from `{text}`, got {other:?}"),
        }
    }

    fn count(&self, text: &str) -> u64 {
        let r = self.db.read();
        query(&r, "tx", text).unwrap().as_count().expect("expected a count")
    }

    fn sum(&self, text: &str) -> u128 {
        let r = self.db.read();
        query(&r, "tx", text).unwrap().as_sum().expect("expected a sum")
    }
}

fn sample() -> Fixture {
    fixture(&[
        (1, 100, "GB", true),
        (2, 250, "US", false),
        (3, 500, "GB", true),
        (SHARD + 4, 750, "US", true),
        (SHARD + 5, 900, "GB", false),
        (2 * SHARD + 6, 50, "VN", true),
    ])
}

#[test]
fn a_comparison_selects_records() {
    assert_eq!(sample().ids("Row(amount > 400)"), vec![3, SHARD + 4, SHARD + 5]);
}

#[test]
fn a_key_selects_across_shards() {
    assert_eq!(sample().ids(r#"Row(country="GB")"#), vec![1, 3, SHARD + 5]);
}

#[test]
fn a_boolean_selects_by_row() {
    assert_eq!(sample().ids("Row(active=true)"), vec![1, 3, SHARD + 4, 2 * SHARD + 6]);
}

#[test]
fn intersect_union_and_difference_compose() {
    let f = sample();
    assert_eq!(f.ids(r#"Intersect(Row(country="GB"), Row(amount > 200))"#), vec![3, SHARD + 5]);
    assert_eq!(
        f.ids(r#"Union(Row(country="VN"), Row(amount > 800))"#),
        vec![SHARD + 5, 2 * SHARD + 6]
    );
    assert_eq!(f.ids(r#"Difference(Row(country="GB"), Row(active=true))"#), vec![SHARD + 5]);
}

/// `Not` is only meaningful against the set of records that exist, which is what makes the
/// `_exists` field earn its cost. A record never written must not appear.
#[test]
fn not_complements_against_what_exists() {
    let f = sample();
    assert_eq!(f.ids(r#"Not(Row(country="GB"))"#), vec![2, SHARD + 4, 2 * SHARD + 6]);
    assert_eq!(f.ids("All()"), vec![1, 2, 3, SHARD + 4, SHARD + 5, 2 * SHARD + 6]);
    // Nothing outside the six written records leaks in.
    assert_eq!(f.ids("Not(All())"), Vec::<u64>::new());
}

#[test]
fn count_and_sum_answer_over_a_selection() {
    let f = sample();
    assert_eq!(f.count(r#"Count(Row(country="GB"))"#), 3);
    assert_eq!(f.sum(r#"Sum(Row(country="GB"), field="amount")"#), 100 + 500 + 900);
    assert_eq!(f.sum(r#"Sum(All(), field="amount")"#), 100 + 250 + 500 + 750 + 900 + 50);
}

#[test]
fn an_unknown_key_is_an_empty_answer_not_an_error() {
    assert_eq!(sample().ids(r#"Row(country="ZZ")"#), Vec::<u64>::new());
}

#[test]
fn a_bad_query_is_refused_rather_than_answered_wrongly() {
    let f = sample();
    let r = f.db.read();
    for text in ["Row(nope > 1)", r#"Row(amount = "GB")"#, "Bogus(All())"] {
        assert!(query(&r, "tx", text).is_err(), "`{text}` should not have planned");
    }
    assert!(query(&r, "no_such_table", "All()").is_err());
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(24))]

    /// The executor must agree with doing the same set algebra on plain collections. This is
    /// the whole contract: everything above can be built on `Matches` only if combining it
    /// means what it looks like it means.
    #[test]
    fn queries_agree_with_plain_collections(
        rows in proptest::collection::btree_map(
            0u64..(3 * SHARD),
            (0u64..1000, prop::sample::select(vec!["GB", "US", "VN"]), any::<bool>()),
            1..25,
        ),
        k in 0u64..1000,
    ) {
        let input: Vec<(u64, u64, &str, bool)> =
            rows.iter().map(|(r, (a, c, b))| (*r, *a, *c, *b)).collect();
        let f = fixture(&input);

        let gt: BTreeSet<u64> = rows.iter().filter(|(_, (a, _, _))| *a > k).map(|(r, _)| *r).collect();
        let gb: BTreeSet<u64> = rows.iter().filter(|(_, (_, c, _))| *c == "GB").map(|(r, _)| *r).collect();
        let all: BTreeSet<u64> = rows.keys().copied().collect();
        let sorted = |s: BTreeSet<u64>| s.into_iter().collect::<Vec<u64>>();

        prop_assert_eq!(f.ids(&format!("Row(amount > {k})")), sorted(gt.clone()));
        prop_assert_eq!(f.ids(r#"Row(country="GB")"#), sorted(gb.clone()));
        prop_assert_eq!(
            f.ids(&format!(r#"Intersect(Row(amount > {k}), Row(country="GB"))"#)),
            sorted(gt.intersection(&gb).copied().collect())
        );
        prop_assert_eq!(
            f.ids(&format!(r#"Union(Row(amount > {k}), Row(country="GB"))"#)),
            sorted(gt.union(&gb).copied().collect())
        );
        prop_assert_eq!(
            f.ids(&format!("Not(Row(amount > {k}))")),
            sorted(all.difference(&gt).copied().collect())
        );
        prop_assert_eq!(f.count(&format!("Count(Row(amount > {k}))")), gt.len() as u64);

        let want: u128 = rows.iter().filter(|(r, _)| gb.contains(r))
            .map(|(_, (a, _, _))| *a as u128).sum();
        prop_assert_eq!(f.sum(r#"Sum(Row(country="GB"), field="amount")"#), want);
    }
}

impl Fixture {
    fn extreme(&self, text: &str) -> Option<u64> {
        let r = self.db.read();
        query(&r, "tx", text).unwrap().as_extreme().expect("expected a min or max")
    }

    /// Groups as `(key, count)`, which is what a caller actually wants back.
    fn groups(&self, text: &str) -> Vec<(String, u64)> {
        let r = self.db.read();
        let v = query(&r, "tx", text).unwrap();
        v.as_groups()
            .expect("expected groups")
            .iter()
            .map(|g| {
                let n = match g.value.as_ref() {
                    Value::Count(n) => *n,
                    Value::Sum(n) => *n as u64,
                    Value::Extreme(v) => v.unwrap_or(0),
                    other => panic!("unexpected group value {other:?}"),
                };
                (g.key.clone().unwrap_or_default(), n)
            })
            .collect()
    }
}

#[test]
fn min_and_max_answer_over_a_selection() {
    let f = sample();
    assert_eq!(f.extreme(r#"Min(Row(country="GB"), field="amount")"#), Some(100));
    assert_eq!(f.extreme(r#"Max(Row(country="GB"), field="amount")"#), Some(900));
    assert_eq!(f.extreme(r#"Min(All(), field="amount")"#), Some(50));
    // Nothing matched is absent, not zero. A zero would be a real value someone had stored.
    assert_eq!(f.extreme(r#"Min(Row(country="ZZ"), field="amount")"#), None);
}

#[test]
fn distinct_lists_the_keys_present_in_a_selection() {
    let f = sample();
    assert_eq!(
        f.groups(r#"Distinct(All(), field="country")"#),
        vec![("GB".into(), 3u64), ("US".into(), 2), ("VN".into(), 1)]
    );
    assert_eq!(
        f.groups(r#"Distinct(Row(amount > 400), field="country")"#),
        vec![("GB".into(), 2u64), ("US".into(), 1)]
    );
}

/// The case that makes a shard-local top-n wrong: a key that leads no single shard but wins
/// once the shards are added up.
#[test]
fn topn_sums_every_shard_before_ranking() {
    const S: u64 = 1 << 20;
    let f = fixture(&[
        // "spread" leads nowhere but totals three.
        (1, 1, "spread", true),
        (S + 1, 1, "spread", true),
        (2 * S + 1, 1, "spread", true),
        // "local" leads shard 0 with two, and has nothing anywhere else.
        (2, 1, "local", true),
        (3, 1, "local", true),
    ]);
    assert_eq!(
        f.groups(r#"TopN(All(), field="country", n=1)"#),
        vec![("spread".into(), 3u64)],
        "ranking each shard first would have picked `local`"
    );
    assert_eq!(
        f.groups(r#"TopN(All(), field="country", n=5)"#),
        vec![("spread".into(), 3u64), ("local".into(), 2)]
    );
}

#[test]
fn group_by_runs_an_aggregate_per_group() {
    let f = sample();
    assert_eq!(
        f.groups(r#"GroupBy(All(), field="country", aggregate=Sum(field="amount"))"#),
        vec![("GB".into(), 1500u64), ("US".into(), 1000), ("VN".into(), 50)]
    );
    assert_eq!(
        f.groups(r#"GroupBy(All(), field="country", aggregate=Max(field="amount"))"#),
        vec![("GB".into(), 900u64), ("US".into(), 750), ("VN".into(), 50)]
    );
    // Without an aggregate a group carries its count.
    assert_eq!(
        f.groups(r#"GroupBy(Row(active=true), field="country")"#),
        vec![("GB".into(), 2u64), ("US".into(), 1), ("VN".into(), 1)]
    );
}

#[test]
fn grouping_a_field_that_has_no_rows_is_refused() {
    let f = sample();
    let r = f.db.read();
    for text in [
        r#"Distinct(All(), field="amount")"#,
        r#"TopN(All(), field="amount", n=3)"#,
        r#"GroupBy(All(), field="amount")"#,
    ] {
        assert!(query(&r, "tx", text).is_err(), "`{text}` groups an integer field");
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(24))]

    /// Grouping and the extremes, against the same thing done with plain collections.
    #[test]
    fn grouping_agrees_with_plain_collections(
        rows in proptest::collection::btree_map(
            0u64..(3 * SHARD),
            (0u64..1000, prop::sample::select(vec!["GB", "US", "VN"]), any::<bool>()),
            1..25,
        ),
    ) {
        let input: Vec<(u64, u64, &str, bool)> =
            rows.iter().map(|(r, (a, c, b))| (*r, *a, *c, *b)).collect();
        let f = fixture(&input);

        let mut by_country: BTreeMap<&str, Vec<u64>> = BTreeMap::new();
        for (a, c, _) in rows.values() {
            by_country.entry(c).or_default().push(*a);
        }

        let want_counts: Vec<(String, u64)> =
            by_country.iter().map(|(c, v)| (c.to_string(), v.len() as u64)).collect();
        prop_assert_eq!(f.groups(r#"Distinct(All(), field="country")"#), want_counts.clone());

        let want_sums: Vec<(String, u64)> =
            by_country.iter().map(|(c, v)| (c.to_string(), v.iter().sum())).collect();
        prop_assert_eq!(
            f.groups(r#"GroupBy(All(), field="country", aggregate=Sum(field="amount"))"#),
            want_sums
        );

        // TopN must be the same multiset, merely ordered by count.
        let mut ranked = want_counts.clone();
        ranked.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        prop_assert_eq!(f.groups(r#"TopN(All(), field="country", n=99)"#), ranked);

        let all: Vec<u64> = rows.values().map(|(a, _, _)| *a).collect();
        prop_assert_eq!(f.extreme(r#"Min(All(), field="amount")"#), all.iter().copied().min());
        prop_assert_eq!(f.extreme(r#"Max(All(), field="amount")"#), all.iter().copied().max());
    }
}

/// A time quantum field writes the same fact into one view per granularity, so a range query
/// reads only the days it asks about. Before this, the views were written and never readable.
#[test]
fn a_time_window_reads_only_the_days_it_asks_for() {
    const DAY: i64 = 86_400;
    let db = Db::in_memory().unwrap();
    db.create_table("tx").unwrap();
    db.create_time_quantum("tx", "visit", vec![]).unwrap();

    // Three visits to one page, on three consecutive days.
    let mut w = db.write();
    for (rec, day) in [(1u64, 0i64), (2, 1), (3, 2)] {
        w.set_time("tx", "visit", rec, "home", day * DAY + 3600).unwrap();
    }
    w.set_time("tx", "visit", 4, "about", 3600).unwrap();
    w.commit().unwrap();

    let r = db.read();
    let ids = |text: &str| match query(&r, "tx", text).unwrap() {
        Value::Rows(m) => m.records().collect::<Vec<u64>>(),
        other => panic!("expected rows, got {other:?}"),
    };

    // Without a window the standard view answers, exactly as an ordinary key would.
    assert_eq!(ids(r#"Row(visit="home")"#), vec![1, 2, 3]);

    assert_eq!(ids(&format!(r#"Row(visit="home", from=0, to={DAY})"#)), vec![1, 2]);
    assert_eq!(ids(&format!(r#"Row(visit="home", from={}, to={})"#, DAY, 2 * DAY)), vec![2, 3]);
    assert_eq!(
        ids(&format!(r#"Row(visit="home", from={}, to={})"#, 5 * DAY, 6 * DAY)),
        Vec::<u64>::new()
    );

    // An open end means the whole of time on that side.
    assert_eq!(ids(&format!(r#"Row(visit="home", from={DAY})"#)), vec![2, 3]);
    assert_eq!(ids(r#"Row(visit="about", from=0, to=0)"#), vec![4]);
}

/// A decimal comparison has to be rewritten into the units actually stored, end to end.
#[test]
fn a_decimal_comparison_means_what_it_says() {
    let db = Db::in_memory().unwrap();
    db.create_table("tx").unwrap();
    db.create_decimal("tx", "price", 32, 2).unwrap();

    let mut w = db.write();
    for (rec, cents) in [(1u64, 199u64), (2, 500), (3, 1250)] {
        w.set_int("tx", "price", rec, cents).unwrap();
    }
    w.commit().unwrap();

    let r = db.read();
    let ids = |text: &str| match query(&r, "tx", text).unwrap() {
        Value::Rows(m) => m.records().collect::<Vec<u64>>(),
        other => panic!("expected rows, got {other:?}"),
    };

    // `> 5` on a two-place field is `> 500`, not `> 5`. Treating it as 5 would match all three.
    assert_eq!(ids("Row(price > 5)"), vec![3]);
    assert_eq!(ids("Row(price > 1.99)"), vec![2, 3]);
    assert_eq!(ids("Row(price >= 1.99)"), vec![1, 2, 3]);
    assert_eq!(ids("Row(price == 12.50)"), vec![3]);

    assert!(query(&r, "tx", "Row(price > 1.999)").is_err(), "more precision than stored");
}

// ------------------------------------------------------------------------------------------
// Projecting out of column segments
// ------------------------------------------------------------------------------------------

/// A keyed column can be projected exactly where its values are stored, and nowhere else.
///
/// This is the one thing the engine choice changes about *what can be asked*, rather than about
/// what it costs: a bitmap records which records hold a key and never which key a record holds,
/// so on a bitmap-only table there is no read to allow.
#[test]
fn a_keyed_column_projects_only_where_the_values_are_stored() {
    for (engine, allowed) in [
        (TableEngine::Bitmap, false),
        (TableEngine::BitmapColumnar, true),
        (TableEngine::Columnar, true),
    ] {
        let d = Db::in_memory().unwrap();
        d.create_table_with("tx", engine).unwrap();
        d.create_field("tx", "amount", FieldKind::Int, 32).unwrap();
        d.create_field("tx", "country", FieldKind::Set, 0).unwrap();

        let mut w = d.write();
        w.set_int("tx", "amount", 1, 100).unwrap();
        w.set_key("tx", "country", 1, "GB").unwrap();
        w.commit().unwrap();

        let read = d.read();
        let got = big_exec::query(&read, "tx", r#"Project(All(), field="country", n=10)"#);
        assert_eq!(
            got.is_ok(),
            allowed,
            "{engine:?} projecting a keyed column: {:?}",
            got.err().map(|e| e.to_string())
        );

        if allowed {
            let rows = got.unwrap();
            let table = rows.as_table().unwrap();
            assert_eq!(table.len(), 1);
            assert_eq!(table[0].values, vec![Projection::Text("GB".to_string())]);
        }
    }
}

/// A record holding several keys comes back as several, and one holding a single key comes back
/// as that key rather than a list of one - a client that asked for `country` wants `"GB"`.
#[test]
fn a_set_column_projects_every_value_it_holds() {
    let d = Db::in_memory().unwrap();
    d.create_table_with("tx", TableEngine::Columnar).unwrap();
    d.create_field("tx", "country", FieldKind::Set, 0).unwrap();

    let mut w = d.write();
    w.set_key("tx", "country", 1, "GB").unwrap();
    w.set_key("tx", "country", 2, "GB").unwrap();
    w.set_key("tx", "country", 2, "US").unwrap();
    w.commit().unwrap();

    let read = d.read();
    let value = big_exec::query(&read, "tx", r#"Project(All(), field="country", n=10)"#).unwrap();
    let table = value.as_table().unwrap();
    assert_eq!(table[0].values, vec![Projection::Text("GB".to_string())]);
    assert_eq!(table[1].values, vec![Projection::Texts(vec!["GB".to_string(), "US".to_string()])]);
}

/// The two engines that store values must agree with the one that does not, wherever both can
/// answer. A projection read out of a segment and one rebuilt from bit planes are two paths to
/// one number, and two paths to one number is exactly where a database goes quietly wrong.
#[test]
fn a_projection_reads_the_same_whichever_engine_stored_it() {
    let mut answers = Vec::new();
    for engine in TableEngine::all() {
        let d = Db::in_memory().unwrap();
        d.create_table_with("tx", engine).unwrap();
        d.create_field("tx", "amount", FieldKind::Int, 32).unwrap();
        d.create_signed("tx", "delta", 16).unwrap();

        let mut w = d.write();
        for r in 0..50u64 {
            w.set_int("tx", "amount", r, r * 37).unwrap();
            if r % 3 == 0 {
                w.set_signed("tx", "delta", r, -(r as i64)).unwrap();
            }
        }
        w.commit().unwrap();

        let read = d.read();
        let value =
            big_exec::query(&read, "tx", r#"Project(All(), field="amount", field="delta", n=100)"#)
                .unwrap();
        answers.push(value.as_table().unwrap().to_vec());
    }

    assert_eq!(answers[0], answers[1], "bitmap and bitmap+columnar disagree");
    assert_eq!(answers[1], answers[2], "bitmap+columnar and columnar disagree");
    // And the answer is actually right, not merely consistent.
    assert_eq!(answers[0][3].values, vec![Projection::Int(111), Projection::Int(-3)]);
    assert_eq!(answers[0][4].values, vec![Projection::Int(148), Projection::Absent]);
}

// ------------------------------------------------------------------------------------------
// Grouping by a calendar bucket
// ------------------------------------------------------------------------------------------

/// A table of dates, shard-spread, for the bucket walk to cut up.
fn days(rows: &[(u64, &str)]) -> Db<MemPager> {
    let db = Db::in_memory().unwrap();
    db.create_table("tx").unwrap();
    db.create_field("tx", "d", FieldKind::Date, 32).unwrap();
    db.create_field("tx", "amount", FieldKind::Int, 32).unwrap();
    let mut w = db.write();
    for (rec, written) in rows {
        w.set_signed("tx", "d", *rec, big_civil::parse_date(written).unwrap()).unwrap();
        w.set_int("tx", "amount", *rec, 10).unwrap();
    }
    w.commit().unwrap();
    db
}

/// Each bucket as `(written start, count)`, which is what the grouping means in plain terms.
fn buckets(db: &Db<MemPager>, text: &str) -> Vec<(String, u64)> {
    let r = db.read();
    match query(&r, "tx", text).unwrap() {
        Value::Groups(gs) => gs
            .iter()
            .map(|g| match g.at {
                big_exec::GroupAt::Bucket { start, .. } => (
                    big_civil::format_date(start),
                    g.value.as_count().or_else(|| g.value.as_sum().map(|s| s as u64)).unwrap(),
                ),
                other => panic!("expected a bucket, got {other:?}"),
            })
            .collect(),
        other => panic!("expected groups, got {other:?}"),
    }
}

/// **The boundaries are the whole test.** Every date here is chosen to sit against an edge: the
/// last day of a month, the first day of the next, a leap day, and a new year. A walk that
/// closed its ranges at the wrong end would move one of them into the neighbouring bucket, and
/// the counts are what says it did not.
#[test]
fn a_month_grouping_cuts_the_calendar_where_the_months_end() {
    let db = days(&[
        (1, "2024-01-01"),
        (2, "2024-01-31"),
        (3, "2024-02-01"),
        (4, "2024-02-29"),
        (SHARD + 5, "2024-03-01"),
        (SHARD + 6, "2024-12-31"),
        (2 * SHARD + 7, "2025-01-01"),
    ]);
    assert_eq!(
        buckets(&db, "GroupByBucket(All(), field=d, unit=\"month\", n=1000)"),
        vec![
            ("2024-01-01".to_string(), 2),
            ("2024-02-01".to_string(), 2),
            ("2024-03-01".to_string(), 1),
            ("2024-12-01".to_string(), 1),
            ("2025-01-01".to_string(), 1),
        ]
    );
}

/// **A month nothing happened in is not a group.** The calendar between March and December has
/// eight of them and none appears above, which is what keeps a sparse column costing its
/// contents rather than its span - and is what `GROUP BY` means: the values that are there.
#[test]
fn an_empty_bucket_is_not_a_group() {
    let db = days(&[(1, "2024-01-15"), (2, "2024-06-15")]);
    let got = buckets(&db, "GroupByBucket(All(), field=d, unit=\"month\", n=1000)");
    assert_eq!(got, vec![("2024-01-01".to_string(), 1), ("2024-06-01".to_string(), 1)]);
}

/// A year is the same walk with a coarser step, and the counts must be the sums of the months'.
#[test]
fn a_coarser_boundary_is_the_finer_one_s_buckets_added_up() {
    let db = days(&[
        (1, "2024-01-01"),
        (2, "2024-05-05"),
        (3, "2024-12-31"),
        (SHARD + 4, "2025-07-07"),
        (2 * SHARD + 5, "2023-02-02"),
    ]);
    let years = buckets(&db, "GroupByBucket(All(), field=d, unit=\"year\", n=1000)");
    assert_eq!(
        years,
        vec![
            ("2023-01-01".to_string(), 1),
            ("2024-01-01".to_string(), 3),
            ("2025-01-01".to_string(), 1),
        ]
    );
    let months = buckets(&db, "GroupByBucket(All(), field=d, unit=\"month\", n=1000)");
    assert_eq!(months.iter().map(|(_, n)| n).sum::<u64>(), years.iter().map(|(_, n)| n).sum());
}

/// The filter narrows what is bucketed, and a bucket left with nothing drops out entirely.
#[test]
fn the_filter_decides_which_records_are_bucketed_at_all() {
    let db = days(&[(1, "2024-01-10"), (2, "2024-01-20"), (3, "2024-02-10")]);
    assert_eq!(
        buckets(&db, "GroupByBucket(Row(d < \"2024-01-15\"), field=d, unit=\"month\", n=1000)"),
        vec![("2024-01-01".to_string(), 1)]
    );
}

/// A record holding no date is in no bucket, so these counts sum to the number of records that
/// hold a value rather than to `count(*)`. The likeliest way to get a bucket grouping wrong.
#[test]
fn a_record_with_no_value_is_in_no_bucket() {
    let db = days(&[(1, "2024-01-10"), (2, "2024-01-20")]);
    // A third record exists in the table, with an amount and no date at all.
    let mut w = db.write();
    w.set_int("tx", "amount", 3, 10).unwrap();
    w.commit().unwrap();

    let total: u64 = buckets(&db, "GroupByBucket(All(), field=d, unit=\"month\", n=1000)")
        .iter()
        .map(|(_, n)| n)
        .sum();
    assert_eq!(total, 2, "the record with no date must be in no bucket");
    let r = db.read();
    assert_eq!(query(&r, "tx", "Count(All())").unwrap().as_count().unwrap(), 3);
}

/// The bound is on the buckets the values span, not on the groups that come back - so a wide
/// span of mostly-empty buckets is refused rather than walked.
#[test]
fn more_buckets_than_the_plan_allows_is_refused_rather_than_cut() {
    let db = days(&[(1, "2024-01-01"), (2, "2026-01-01")]);
    let r = db.read();
    assert!(query(&r, "tx", "GroupByBucket(All(), field=d, unit=\"month\", n=1000)").is_ok());
    let err = query(&r, "tx", "GroupByBucket(All(), field=d, unit=\"month\", n=3)").unwrap_err();
    assert!(format!("{err}").contains("coarser"), "{err}");
}
