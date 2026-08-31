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

//! Bulk loading.
//!
//! Two things to prove, and the second is the only one that justifies the code existing.
//!
//! The first is that it writes the same data the ordinary path would. That is a property test.
//!
//! The second is that it writes *less*. A bulk load that produced identical bytes to a buffered
//! ingest would be a second way to do something already done, and no test about the data it
//! stores could tell the difference - so the byte count is asserted directly, on the workload
//! shape the whole thing is for: ids spread across many shards, arriving in an order that puts a
//! little of every shard into each batch.

use big_db::*;
use big_engine::SHARD_WIDTH;
use big_pager::{CountingPager, MemPager};
use proptest::prelude::*;

fn db() -> Db<CountingPager<MemPager>> {
    let d = Db::open(CountingPager::new(MemPager::new())).unwrap();
    d.create_table("tx").unwrap();
    d.create_field("tx", "amount", FieldKind::Int, 20).unwrap();
    d.create_field("tx", "country", FieldKind::Set, 0).unwrap();
    d.create_field("tx", "active", FieldKind::Bool, 0).unwrap();
    d.create_signed("tx", "delta", 20).unwrap();
    d
}

/// Ids spread across `shards`, the shape that makes every batch touch every fragment.
fn sparse(n: u64, shards: u64) -> Vec<(u64, u64)> {
    (0..n)
        .map(|i| ((i % shards) * SHARD_WIDTH + i / shards, i.wrapping_mul(2_654_435_761) % 100_000))
        .collect()
}

#[test]
fn a_bulk_load_stores_what_it_was_given() {
    let d = db();
    let mut b = d.bulk_load("tx").unwrap();
    b.set_int("amount", 1, 100).unwrap();
    b.set_int("amount", SHARD_WIDTH + 2, 900).unwrap();
    b.set_key("country", 1, "vn").unwrap();
    b.set_key("country", SHARD_WIDTH + 2, "jp").unwrap();
    b.set_bool("active", 1, true).unwrap();
    b.set_signed("delta", 1, -5).unwrap();
    assert_eq!(b.finish().unwrap(), 2);

    let r = d.read();
    assert_eq!(r.get_int("tx", "amount", 1).unwrap(), Some(100));
    assert_eq!(r.get_int("tx", "amount", SHARD_WIDTH + 2).unwrap(), Some(900));
    assert_eq!(r.get_signed("tx", "delta", 1).unwrap(), Some(-5));
    assert_eq!(r.by_key("tx", "country", "vn").unwrap(), [1]);
    assert_eq!(r.by_key("tx", "country", "jp").unwrap(), [SHARD_WIDTH + 2]);
    assert_eq!(r.matching_bool("tx", "active", true).unwrap().records().collect::<Vec<_>>(), [1]);
    assert_eq!(r.count_all("tx").unwrap(), 2);
}

#[test]
fn the_last_write_for_a_record_wins() {
    // The same rule a single transaction has. A load that kept the first value instead would
    // differ from every other write path in the engine.
    let d = db();
    let mut b = d.bulk_load("tx").unwrap();
    b.set_int("amount", 1, 100).unwrap();
    b.set_int("amount", 1, 200).unwrap();
    b.finish().unwrap();
    assert_eq!(d.read().get_int("tx", "amount", 1).unwrap(), Some(200));
}

#[test]
fn loading_nothing_is_not_an_error() {
    let d = db();
    assert_eq!(d.bulk_load("tx").unwrap().finish().unwrap(), 0);
}

#[test]
fn loading_into_a_table_that_already_has_data_is_refused() {
    // Merging would mean reading the fragment back, which is the one thing this exists not to
    // do; overwriting would strand what was there. Neither is worth a silent choice.
    let d = db();
    let mut w = d.write();
    w.set_int("tx", "amount", 1, 5).unwrap();
    w.commit().unwrap();

    let mut b = d.bulk_load("tx").unwrap();
    b.set_int("amount", 2, 6).unwrap();
    assert!(matches!(b.finish(), Err(DbError::BulkLoadNotEmpty { .. })));

    // And nothing was written: a refusal that had already committed half the load would be
    // worse than no refusal at all.
    assert_eq!(d.read().get_int("tx", "amount", 2).unwrap(), None);
}

#[test]
fn a_shard_that_is_untouched_does_not_block_the_load() {
    // The refusal is per fragment, not per table: loading shard 5 into a table whose shard 0
    // has data is still a load into empty fragments.
    let d = db();
    let mut w = d.write();
    w.set_int("tx", "amount", 1, 5).unwrap();
    w.commit().unwrap();

    let mut b = d.bulk_load("tx").unwrap();
    b.set_int("amount", 5 * SHARD_WIDTH, 6).unwrap();
    assert_eq!(b.finish().unwrap(), 1);
    assert_eq!(d.read().get_int("tx", "amount", 5 * SHARD_WIDTH).unwrap(), Some(6));
}

#[test]
fn the_field_kinds_it_cannot_model_are_refused_at_the_call() {
    // Not at `finish`, thousands of facts later. A mutex has to read its shadow and a time
    // quantum writes extra views; both are shapes this path does not model, and getting either
    // subtly wrong would produce a fragment that reads back plausibly.
    let d = db();
    d.create_field("tx", "tier", FieldKind::Mutex, 0).unwrap();
    d.create_time_quantum("tx", "seen", vec![Granularity::Day]).unwrap();

    let mut b = d.bulk_load("tx").unwrap();
    assert!(matches!(b.set_key("tier", 1, "gold"), Err(DbError::WrongFieldKind { .. })));
    assert!(matches!(b.set_key("seen", 1, "x"), Err(DbError::WrongFieldKind { .. })));
    b.finish().unwrap();
}

#[test]
fn a_value_too_wide_is_refused_at_the_call_too() {
    let d = db();
    let mut b = d.bulk_load("tx").unwrap();
    assert!(b.set_int("amount", 1, 1 << 40).is_err());
    assert!(matches!(
        b.set_signed("delta", 1, 1 << 40),
        Err(DbError::SignedValueOutOfRange { .. })
    ));
    b.finish().unwrap();
}

#[test]
fn an_unknown_field_is_refused_at_the_call() {
    let d = db();
    let mut b = d.bulk_load("tx").unwrap();
    assert!(matches!(b.set_int("nope", 1, 1), Err(DbError::UnknownField { .. })));
    b.finish().unwrap();
}

#[test]
fn an_unknown_table_is_refused_before_anything_is_buffered() {
    let d = db();
    assert!(matches!(d.bulk_load("nope"), Err(DbError::UnknownTable(_))));
}

#[test]
fn a_bulk_load_writes_far_fewer_bytes_than_a_buffered_ingest() {
    // The claim the code exists for, on the shape it exists for: 64 shards, and a caller whose
    // batches each contain a little of every shard. The ingest buffer makes the commits fewer;
    // it cannot make them disjoint, so every commit after the first finds each fragment already
    // rooted and rewrites its root-to-leaf path.
    const N: u64 = 40_000;
    const SHARDS: u64 = 64;
    const BUFFER: usize = 2_000;

    let records = sparse(N, SHARDS);

    let a = db();
    let before = a.store().pager().counts().bytes_written();
    let mut ing = a.ingest(BUFFER);
    for (id, v) in &records {
        ing.set_int("tx", "amount", *id, *v).unwrap();
    }
    ing.finish().unwrap();
    let ingested = a.store().pager().counts().bytes_written() - before;

    let b = db();
    let before = b.store().pager().counts().bytes_written();
    let mut bulk = b.bulk_load("tx").unwrap();
    for (id, v) in &records {
        bulk.set_int("amount", *id, *v).unwrap();
    }
    bulk.finish().unwrap();
    let bulked = b.store().pager().counts().bytes_written() - before;

    // Both stored the same thing, which is what makes the comparison mean anything.
    assert_eq!(a.read().count_all("tx").unwrap(), N);
    assert_eq!(b.read().count_all("tx").unwrap(), N);
    for (id, v) in records.iter().take(200) {
        assert_eq!(b.read().get_int("tx", "amount", *id).unwrap(), Some(*v));
    }

    println!(
        "ingest {ingested} bytes, bulk {bulked} bytes, ratio {:.1}x",
        ingested as f64 / bulked as f64
    );
    // Measured at ~19x when this was written. Asserted at 4x, because the point is the order of
    // magnitude and not the digit - a gate on the exact figure belongs in `amplification.rs`,
    // where the workload is fixed on purpose.
    assert!(
        bulked * 4 < ingested,
        "bulk wrote {bulked} bytes against the ingest's {ingested} - the reordering is not paying"
    );
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// Whatever the facts, a bulk load leaves the database in the state an ordinary write
    /// would have left it in.
    #[test]
    fn a_bulk_load_agrees_with_the_ordinary_write_path(
        ints in proptest::collection::btree_map(0u64..3_000_000, 0u64..100_000, 1..40),
        keys in proptest::collection::vec((0u64..3_000_000, "[a-c]{1,3}"), 0..20),
    ) {
        let a = db();
        let mut w = a.write();
        for (r, v) in &ints {
            w.set_int("tx", "amount", *r, *v).unwrap();
        }
        for (r, k) in &keys {
            w.set_key("tx", "country", *r, k).unwrap();
        }
        w.commit().unwrap();

        let b = db();
        let mut bulk = b.bulk_load("tx").unwrap();
        for (r, v) in &ints {
            bulk.set_int("amount", *r, *v).unwrap();
        }
        for (r, k) in &keys {
            bulk.set_key("country", *r, k).unwrap();
        }
        bulk.finish().unwrap();

        prop_assert_eq!(a.read().count_all("tx").unwrap(), b.read().count_all("tx").unwrap());
        for (r, v) in &ints {
            prop_assert_eq!(
                b.read().get_int("tx", "amount", *r).unwrap(),
                Some(*v),
                "record {}", r
            );
        }
        for (r, k) in &keys {
            let want: Vec<RecordId> = a.read().by_key("tx", "country", k).unwrap();
            let got: Vec<RecordId> = b.read().by_key("tx", "country", k).unwrap();
            prop_assert_eq!(got, want, "key {} for record {}", k, r);
        }
    }
}

// ------------------------------------------------------------------------------------------
// What a bulk load does not write
// ------------------------------------------------------------------------------------------

/// **A bulk load writes bitmap fragments and nothing else, and this records what that costs.**
///
/// Nothing in this file wrote a column before, and nothing in it checked for one - which is how a
/// path that silently drops half of what the default engine stores went unnoticed. The three rows
/// below are the whole picture, measured rather than argued:
///
/// | engine | loaded | `count_all` | `column_count` | predicate |
/// |---|---|---|---|---|
/// | `bitmap` | 1000 | 1000 | 0 | correct - it has no columns |
/// | `bitmap+columnar` | 1000 | 1000 | **0** | correct, *from the index* |
/// | `columnar` | 1000 | 1000 | **0** | **0 - wrong** |
///
/// The last row is refused outright now: a load that reports a thousand records and then answers
/// nothing is the worst shape a failure can take. The middle row is **not** refused, because its
/// answers are right and refusing it would break a path that has always been offered - but its
/// segments really are empty, so a size taken from such a file is missing half the engine and a
/// query the planner sends to a scan would answer wrongly.
///
/// If that is fixed - by teaching this path to write columns, or by refusing it too - this test
/// is where the decision gets recorded.
#[test]
fn a_bulk_load_leaves_a_columnar_table_without_its_columns() {
    // The engine that keeps only bitmaps is the one this path was written for.
    let d = Db::in_memory().unwrap();
    d.create_table_with("t", TableEngine::Bitmap).unwrap();
    d.create_field("t", "v", FieldKind::Int, 20).unwrap();
    let mut b = d.bulk_load("t").unwrap();
    for id in 0..1000u64 {
        b.set_int("v", id, id).unwrap();
    }
    assert_eq!(b.finish().unwrap(), 1000);
    assert_eq!(d.read().matching("t", "v", RangeOp::Ge, 500).unwrap().cardinality(), 500);

    // The default engine loads, answers from its index, and holds no columns at all.
    let d = Db::in_memory().unwrap();
    d.create_table_with("t", TableEngine::BitmapColumnar).unwrap();
    d.create_field("t", "v", FieldKind::Int, 20).unwrap();
    let mut b = d.bulk_load("t").unwrap();
    for id in 0..1000u64 {
        b.set_int("v", id, id).unwrap();
    }
    assert_eq!(b.finish().unwrap(), 1000);
    let r = d.read();
    assert_eq!(r.count_all("t").unwrap(), 1000);
    assert_eq!(r.matching("t", "v", RangeOp::Ge, 500).unwrap().cardinality(), 500);
    assert_eq!(
        r.column_count("t", "v").unwrap(),
        0,
        "a bulk load has started writing columns - update this test and the note in `bulk.rs`"
    );

    // And the engine that has only columns is refused, because there it would answer nothing.
    let d = Db::in_memory().unwrap();
    d.create_table_with("t", TableEngine::Columnar).unwrap();
    d.create_field("t", "v", FieldKind::Int, 20).unwrap();
    match d.bulk_load("t") {
        Ok(_) => panic!("a columnar table accepted a load it would answer nothing from"),
        Err(e) => assert_eq!(e.code(), "engine_cannot_answer"),
    };
}
