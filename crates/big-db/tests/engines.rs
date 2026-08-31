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

//! **Three engines, one answer.**
//!
//! There are now two ways to answer almost every question this engine takes: read an index, or
//! scan a segment. A database with two paths to one number is a database that is quietly wrong
//! half the time, and no amount of care inside either path prevents that - only asking both and
//! comparing does.
//!
//! So this file is the gate on the whole columnar effort. It builds the *same* data under each
//! engine, asks every verb, and requires the answers to be identical. A verb that one engine
//! cannot answer is named here explicitly rather than skipped, because "this engine refuses it"
//! and "nobody tested it" look the same in a passing suite.

use big_db::catalog::TableEngine;
use big_db::*;
use big_engine::SHARD_WIDTH;
use std::collections::BTreeMap;

/// Every engine the build has, read from `big_engine::ENGINES` rather than listed here.
///
/// The point of the registry is that this file does not get to fall behind it: an engine added
/// to `big-engine` and forgotten here would be an engine nothing in this suite ever created,
/// and "it works" and "nobody tested it" look the same in a passing suite.
fn engines() -> Vec<TableEngine> {
    TableEngine::all().collect()
}

/// The same table, the same facts, under one engine.
///
/// Records are spread over three shards on purpose: every verb here merges per-shard answers,
/// and a single-shard fixture would never exercise the merge that a scan and an index each do
/// their own way.
fn stocked(engine: TableEngine) -> Db<big_pager::MemPager> {
    let d = Db::in_memory().unwrap();
    d.create_table_with("tx", engine).unwrap();
    d.create_field("tx", "amount", FieldKind::Int, 32).unwrap();
    d.create_signed("tx", "delta", 16).unwrap();
    d.create_field("tx", "country", FieldKind::Set, 0).unwrap();
    d.create_field("tx", "tier", FieldKind::Mutex, 0).unwrap();
    d.create_field("tx", "active", FieldKind::Bool, 0).unwrap();

    let countries = ["GB", "US", "FR"];
    let tiers = ["gold", "silver"];
    let mut w = d.write();
    for i in 0..300u64 {
        // Three shards, and ids that are not contiguous inside one - which is what a real
        // global id space looks like and what makes the block boundaries fall unevenly.
        let record = (i % 3) * SHARD_WIDTH + (i / 3) * 7;
        w.set_int("tx", "amount", record, (i * 37) % 5000).unwrap();
        if i % 4 != 0 {
            w.set_signed("tx", "delta", record, (i as i64 % 61) - 30).unwrap();
        }
        w.set_key("tx", "country", record, countries[(i % 3) as usize]).unwrap();
        if i % 5 == 0 {
            // A second country, so the set column really is a set on some records.
            w.set_key("tx", "country", record, countries[((i + 1) % 3) as usize]).unwrap();
        }
        w.set_key("tx", "tier", record, tiers[(i % 2) as usize]).unwrap();
        w.set_bool("tx", "active", record, i % 3 == 0).unwrap();
    }
    w.commit().unwrap();
    d
}

/// Runs `ask` under all three engines and requires one answer.
fn agree<T: PartialEq + std::fmt::Debug>(
    what: &str,
    ask: impl Fn(&DbRead<'_, big_pager::MemPager>) -> T,
) -> T {
    let mut answers = Vec::new();
    for engine in engines() {
        let d = stocked(engine);
        answers.push((engine, ask(&d.read())));
    }
    for pair in answers.windows(2) {
        assert_eq!(pair[0].1, pair[1].1, "{what}: {:?} and {:?} disagree", pair[0].0, pair[1].0);
    }
    answers.pop().expect("three engines").1
}

#[test]
fn a_range_predicate_agrees() {
    for (op, k) in [
        (RangeOp::Gt, 2500),
        (RangeOp::Ge, 2500),
        (RangeOp::Lt, 100),
        (RangeOp::Le, 0),
        (RangeOp::Eq, 37),
        (RangeOp::Ne, 37),
    ] {
        let got = agree(&format!("matching {op:?} {k}"), |r| {
            let m = r.matching("tx", "amount", op, k).unwrap();
            (m.cardinality(), m.records().collect::<Vec<_>>())
        });
        assert!(got.0 > 0, "{op:?} {k} matched nothing; the fixture proves nothing");
    }
}

#[test]
fn a_signed_predicate_agrees() {
    for (op, k) in [(RangeOp::Gt, 0i64), (RangeOp::Lt, 0), (RangeOp::Ge, -30), (RangeOp::Eq, -5)] {
        agree(&format!("matching_signed {op:?} {k}"), |r| {
            r.matching_signed("tx", "delta", op, k).unwrap().records().collect::<Vec<_>>()
        });
    }
}

#[test]
fn a_key_lookup_agrees() {
    for key in ["GB", "US", "FR", "nowhere"] {
        agree(&format!("matching_key {key}"), |r| {
            r.matching_key("tx", "country", key).unwrap().records().collect::<Vec<_>>()
        });
    }
    for key in ["gold", "silver"] {
        agree(&format!("mutex {key}"), |r| {
            r.matching_key("tx", "tier", key).unwrap().records().collect::<Vec<_>>()
        });
    }
}

#[test]
fn a_boolean_lookup_agrees() {
    for value in [true, false] {
        agree(&format!("matching_bool {value}"), |r| {
            r.matching_bool("tx", "active", value).unwrap().records().collect::<Vec<_>>()
        });
    }
}

#[test]
fn the_universe_and_its_size_agree() {
    agree("all", |r| r.all("tx").unwrap().records().collect::<Vec<_>>());
    agree("count_all", |r| r.count_all("tx").unwrap());
    agree("scan_records", |r| r.scan_records("tx", 0, 50).unwrap());
    agree("exists", |r| (r.exists("tx", 0).unwrap(), r.exists("tx", 999_999).unwrap()));
}

#[test]
fn aggregates_agree() {
    agree("sum", |r| r.sum("tx", "amount").unwrap());
    agree("sum_where", |r| {
        let hits = r.matching_key("tx", "country", "GB").unwrap();
        r.sum_where("tx", "amount", &hits).unwrap()
    });
    agree("min/max", |r| {
        let hits = r.matching_key("tx", "country", "US").unwrap();
        (r.min_where("tx", "amount", &hits).unwrap(), r.max_where("tx", "amount", &hits).unwrap())
    });
    agree("count_values", |r| {
        let all = r.all("tx").unwrap();
        (
            r.count_values("tx", "amount", &all).unwrap(),
            // `delta` is absent on a quarter of the records, which is the case that separates
            // "records in the filter" from "records holding a value".
            r.count_values("tx", "delta", &all).unwrap(),
        )
    });
}

/// The signed aggregates are where the two paths could most easily diverge: a sum is corrected
/// by a bias per record, so it depends on a *count* being right as well as a total.
#[test]
fn signed_aggregates_agree() {
    agree("sum_signed_where", |r| {
        let all = r.all("tx").unwrap();
        r.sum_signed_where("tx", "delta", &all).unwrap()
    });
    agree("min/max signed", |r| {
        let all = r.all("tx").unwrap();
        (
            r.min_signed_where("tx", "delta", &all).unwrap(),
            r.max_signed_where("tx", "delta", &all).unwrap(),
        )
    });
}

#[test]
fn groupings_agree() {
    // By name rather than by row id: interning order is a node's own numbering, and a scan and
    // an index have no reason to intern in the same order.
    agree("group_counts", |r| {
        let all = r.all("tx").unwrap();
        r.group_counts("tx", "country", &all)
            .unwrap()
            .into_iter()
            .map(|(row, n)| (r.row_key("tx", "country", row).unwrap().to_string(), n))
            .collect::<BTreeMap<_, _>>()
    });

    agree("group_matches", |r| {
        let hits = r.matching("tx", "amount", RangeOp::Gt, 2000).unwrap();
        r.group_matches("tx", "tier", &hits)
            .unwrap()
            .into_iter()
            .map(|(row, m)| {
                (r.row_key("tx", "tier", row).unwrap().to_string(), m.records().collect::<Vec<_>>())
            })
            .collect::<BTreeMap<_, _>>()
    });
}

/// A grouping over a set field must count a record once per value it holds, not once. This is
/// the case where a hash aggregate over a scan and a per-row intersection over an index are
/// most obviously different pieces of code doing one job.
#[test]
fn a_grouping_over_a_multi_valued_column_agrees() {
    let totals = agree("group_counts over a set", |r| {
        let all = r.all("tx").unwrap();
        r.group_counts("tx", "country", &all)
            .unwrap()
            .into_iter()
            .map(|(row, n)| (r.row_key("tx", "country", row).unwrap().to_string(), n))
            .collect::<BTreeMap<_, _>>()
    });
    let counted: u64 = totals.values().sum();
    assert!(counted > 300, "records holding two countries were only counted once: {totals:?}");
}

/// Predicates compose across engines because both produce an ordinary `Matches`. Without this
/// the scan path would be a second answer rather than the same answer by another route.
#[test]
fn predicates_compose_the_same_way() {
    agree("intersect", |r| {
        let a = r.matching("tx", "amount", RangeOp::Gt, 1000).unwrap();
        let b = r.matching_key("tx", "country", "GB").unwrap();
        a.and(&b).records().collect::<Vec<_>>()
    });
    agree("union", |r| {
        let a = r.matching_key("tx", "country", "US").unwrap();
        let b = r.matching_key("tx", "country", "FR").unwrap();
        a.or(&b).records().collect::<Vec<_>>()
    });
    agree("not", |r| {
        let all = r.all("tx").unwrap();
        let gb = r.matching_key("tx", "country", "GB").unwrap();
        all.andnot(&gb).records().collect::<Vec<_>>()
    });
}

/// Deleting has to reach both halves, or the two engines would start disagreeing after the
/// first delete rather than after the first write.
#[test]
fn the_answers_still_agree_after_a_delete() {
    agree("after delete", |_| ());
    let mut answers = Vec::new();
    for engine in engines() {
        let d = stocked(engine);
        let mut w = d.write();
        let doomed: Vec<RecordId> =
            d.read().matching_key("tx", "country", "GB").unwrap().records().take(20).collect();
        w.delete("tx", &doomed).unwrap();
        w.commit().unwrap();

        let r = d.read();
        answers.push((
            engine,
            (
                r.count_all("tx").unwrap(),
                r.matching_key("tx", "country", "GB").unwrap().cardinality(),
                r.sum("tx", "amount").unwrap(),
            ),
        ));
    }
    for pair in answers.windows(2) {
        assert_eq!(pair[0].1, pair[1].1, "{:?} and {:?} disagree", pair[0].0, pair[1].0);
    }
}

// ------------------------------------------------------------------------------------------
// What a scan cannot do, named rather than skipped
// ------------------------------------------------------------------------------------------

/// A time window reads the per-day views a time quantum field writes, and those are an index
/// construct. Refused rather than answered empty, because "no records in that window" and "this
/// table cannot see time at all" call for opposite actions.
#[test]
fn a_time_window_is_refused_on_a_table_with_no_index() {
    for (engine, works) in [(TableEngine::BitmapColumnar, true), (TableEngine::Columnar, false)] {
        let d = Db::in_memory().unwrap();
        d.create_table_with("tx", engine).unwrap();
        d.create_time_quantum("tx", "visit", vec![Granularity::Day]).unwrap();

        let mut w = d.write();
        w.set_time("tx", "visit", 1, "GB", 1_700_000_000).unwrap();
        w.commit().unwrap();

        let got = d.read().matching_key_between("tx", "visit", "GB", None, None);
        assert_eq!(got.is_ok(), works, "{engine:?}");
        if !works {
            assert_eq!(got.unwrap_err().code(), "engine_cannot_answer");
        }
    }
}

/// A plain key lookup on a time quantum field still works under every engine: it asks which
/// records hold the key, which is a question a segment answers as well as an index does. Only
/// the *window* needs the views.
#[test]
fn a_time_quantum_key_still_answers_without_its_views() {
    agree("time quantum key", |_| ());
    let mut answers = Vec::new();
    for engine in engines() {
        let d = Db::in_memory().unwrap();
        d.create_table_with("tx", engine).unwrap();
        d.create_time_quantum("tx", "visit", vec![Granularity::Day]).unwrap();
        let mut w = d.write();
        for r in 0..10u64 {
            w.set_time("tx", "visit", r, if r % 2 == 0 { "GB" } else { "US" }, 1_700_000_000)
                .unwrap();
        }
        w.commit().unwrap();
        answers.push((
            engine,
            d.read().matching_key("tx", "visit", "GB").unwrap().records().collect::<Vec<_>>(),
        ));
    }
    for pair in answers.windows(2) {
        assert_eq!(pair[0].1, pair[1].1, "{:?} and {:?} disagree", pair[0].0, pair[1].0);
    }
}

// ------------------------------------------------------------------------------------------
// Which path a table takes
// ------------------------------------------------------------------------------------------

/// The routing rule, pinned: if there is an index, a predicate uses it; otherwise it scans.
///
/// Asserted through what each engine *reads* rather than through an internal flag, because the
/// flag is not the promise - the promise is that a `bitmap+columnar` table takes the same path
/// as a `bitmap` one. A table that quietly started scanning would still answer correctly and
/// would still pass every other test in this file, which is exactly why this one exists.
///
/// **What it deliberately does not assert is that the index is cheaper.** It is not, here: a
/// bit-sliced index costs one read per bit plane whatever the predicate selects, while a narrow
/// column packs small enough to sit inside its leaf cells and costs almost nothing. The numbers
/// below are recorded rather than ranked, because ranking them would be asserting a cost model
/// this engine does not have - see `DbRead::scans` for why it does not have one yet.
#[test]
fn a_table_with_an_index_answers_predicates_from_it() {
    use big_pager::{CountingPager, MemPager};

    let mut page_reads = Vec::new();
    for engine in engines() {
        let d = Db::open(CountingPager::new(MemPager::new())).unwrap();
        d.create_table_with("tx", engine).unwrap();
        d.create_field("tx", "amount", FieldKind::Int, 32).unwrap();
        let mut w = d.write();
        for r in 0..5000u64 {
            w.set_int("tx", "amount", r, r).unwrap();
        }
        w.commit().unwrap();

        let before = d.store().pager().counts().reads;
        // Highly selective: an index answers it by intersecting bit planes and never reads a
        // value, where a scan has to decode every block in range.
        let hits = d.read().matching("tx", "amount", RangeOp::Eq, 4999).unwrap();
        assert_eq!(hits.cardinality(), 1);
        page_reads.push((engine, d.store().pager().counts().reads - before));
    }

    let by = |e: TableEngine| page_reads.iter().find(|(x, _)| *x == e).unwrap().1;
    assert_eq!(
        by(TableEngine::Bitmap),
        by(TableEngine::BitmapColumnar),
        "a table with both halves read a different number of pages than the index-only one, \
         which means the predicate stopped using the index"
    );
    // Recorded, not ranked. A `Eq` on a 13-bit column over 5000 records: the index reads a
    // page per bit plane, the segment reads the handful of leaves its blocks are inlined in.
    // If either of these moves a long way, the routing rule is worth revisiting - which is the
    // whole reason the figures are written down.
    assert_eq!(
        (by(TableEngine::Bitmap), by(TableEngine::Columnar)),
        (16, 1),
        "\npage reads for one selective predicate: index {}, scan {}.\n\
         Recorded rather than ranked - see this test's comment. Update the line if this is a \
         deliberate change.",
        by(TableEngine::Bitmap),
        by(TableEngine::Columnar)
    );
}

/// The memory ceiling has to guard the scan while it runs, not report on it afterwards.
///
/// A grouping over a high-cardinality column is the one read that can outgrow its own input, and
/// the index path charges each group as it builds it. A scan that charged only after finishing
/// would hold the whole answer before noticing - which is the opposite of what a ceiling is for.
#[test]
fn a_scan_is_refused_while_it_runs_rather_than_after() {
    let d = Db::in_memory().unwrap();
    d.create_table_with("tx", TableEngine::Columnar).unwrap();
    d.create_field("tx", "k", FieldKind::Set, 0).unwrap();

    let mut w = d.write();
    for r in 0..20_000u64 {
        w.set_key("tx", "k", r, &format!("k{r}")).unwrap();
    }
    w.commit().unwrap();

    let read = d.read().with_limits(QueryLimits { max_bytes: 4096, max_records: 1 << 24 });
    let all = read.all("tx").unwrap();
    let err = read.group_matches("tx", "k", &all).unwrap_err();
    assert_eq!(err.code(), "query_too_large");
    // What it spent must be near the ceiling rather than near the whole answer: twenty thousand
    // groups held before the refusal would be far past this.
    assert!(read.spent_bytes() < 4096 * 8, "spent {} bytes before refusing", read.spent_bytes());
}
