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

//! `count_all`, and the one property that makes it worth having: it must agree with the
//! expensive way of asking the same question, under every history that can produce a table.
//!
//! The risk this file exists to cover is not arithmetic. It is that the cached cardinality a
//! leaf cell carries drifts from the container it points at - a container emptied by a delete,
//! a container promoted to its own page, a container rewritten by a copy. Nothing else in the
//! tree reads that number without also reading the payload, so nothing else would notice.

use big_db::*;
use big_engine::SHARD_WIDTH;
use proptest::prelude::*;

fn db() -> Db<big_pager::MemPager> {
    let d = Db::in_memory().unwrap();
    d.create_table("tx").unwrap();
    d.create_field("tx", "amount", FieldKind::Int, 37).unwrap();
    d.create_field("tx", "country", FieldKind::Set, 0).unwrap();
    d
}

fn seed(d: &Db<big_pager::MemPager>, records: impl IntoIterator<Item = u64>) {
    let mut w = d.write();
    for r in records {
        w.set_int("tx", "amount", r, r + 100).unwrap();
        w.set_key("tx", "country", r, if r.is_multiple_of(2) { "vn" } else { "jp" }).unwrap();
    }
    w.commit().unwrap();
}

/// The cheap count and the expensive count, side by side. Every test here asserts both, because
/// a `count_all` that is merely self-consistent would pass a test that only asked it twice.
fn both(d: &Db<big_pager::MemPager>) -> (u64, u64) {
    let r = d.read();
    (r.count_all("tx").unwrap(), r.all("tx").unwrap().cardinality())
}

#[test]
fn an_empty_table_counts_zero() {
    let d = db();
    assert_eq!(both(&d), (0, 0));
}

#[test]
fn a_table_with_no_fragments_at_all_counts_zero() {
    // Distinct from the case above only in that nothing has ever been written, so the field
    // has no fragment to scan rather than an empty one. `per_fragment` skips a missing root;
    // this is the test that says so.
    let d = Db::in_memory().unwrap();
    d.create_table("tx").unwrap();
    assert_eq!(d.read().count_all("tx").unwrap(), 0);
}

#[test]
fn counting_spans_shards() {
    let d = db();
    seed(&d, [0u64, 1, SHARD_WIDTH, SHARD_WIDTH + 5, 9 * SHARD_WIDTH]);
    assert_eq!(both(&d), (5, 5));
}

#[test]
fn writing_the_same_record_twice_counts_it_once() {
    let d = db();
    seed(&d, [7u64]);
    seed(&d, [7u64]);
    assert_eq!(both(&d), (1, 1));
}

#[test]
fn a_delete_is_reflected_in_the_count() {
    let d = db();
    seed(&d, [1u64, 2, 3, SHARD_WIDTH + 1]);

    let mut w = d.write();
    w.delete("tx", &[2, SHARD_WIDTH + 1]).unwrap();
    w.commit().unwrap();

    assert_eq!(both(&d), (2, 2));
}

#[test]
fn emptying_a_shard_entirely_leaves_no_phantom_count() {
    // The interesting half of a delete: the last record of a shard goes, the fragment frees its
    // tree and drops its root record. A count that read a stale root would still find the old
    // cardinality sitting on a page nothing points at any more.
    let d = db();
    seed(&d, [1u64, SHARD_WIDTH]);

    let mut w = d.write();
    w.delete("tx", &[SHARD_WIDTH]).unwrap();
    w.commit().unwrap();

    assert_eq!(both(&d), (1, 1));
}

#[test]
fn dropping_the_table_leaves_nothing_to_count() {
    let d = db();
    seed(&d, [1u64, 2, 3]);
    d.drop_table("tx").unwrap();
    assert!(matches!(d.read().count_all("tx"), Err(DbError::UnknownTable(_))));
}

#[test]
fn an_unknown_table_is_an_error_not_a_zero() {
    // Zero is a real answer. A table that does not exist has no answer at all, and collapsing
    // the two would make a typo in a table name read as an empty table.
    let d = db();
    assert!(matches!(d.read().count_all("nope"), Err(DbError::UnknownTable(_))));
}

#[test]
fn dropping_a_field_does_not_change_how_many_records_exist() {
    // Records are marked in the exists field, which is not any declared field. Dropping
    // `amount` removes values, never records.
    let d = db();
    seed(&d, [1u64, 2, 3]);
    d.drop_field("tx", "amount").unwrap();
    assert_eq!(both(&d), (3, 3));
}

#[test]
fn a_dense_shard_counts_correctly() {
    // Past the promotion threshold a container moves to its own page and the leaf cell keeps
    // only a pointer, a cardinality and a checksum. That cardinality is derived from the page
    // by `push_dense`, so this is the case where the cheap count and the payload could most
    // easily disagree.
    let d = db();
    seed(&d, 0..20_000u64);
    assert_eq!(both(&d), (20_000, 20_000));
}

#[test]
fn the_count_survives_a_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("count.big");
    {
        let d = Db::open_path(&path).unwrap();
        d.create_table("tx").unwrap();
        d.create_field("tx", "amount", FieldKind::Int, 37).unwrap();
        let mut w = d.write();
        for r in [1u64, 2, SHARD_WIDTH] {
            w.set_int("tx", "amount", r, r).unwrap();
        }
        w.commit().unwrap();
    }
    let d = Db::open_path(&path).unwrap();
    let r = d.read();
    assert_eq!(r.count_all("tx").unwrap(), 3);
    assert_eq!(r.all("tx").unwrap().cardinality(), 3);
}

#[test]
fn count_all_reads_fewer_pages_than_materialising() {
    // The whole point of the operation. Without this the cheap path is only cheap by
    // assertion, and a later change that quietly routed it through `all()` would pass every
    // other test in this file.
    //
    // The ids are deliberately scattered rather than consecutive. A contiguous run of records
    // compresses to a single interval and stays inline in the leaf cell, so both paths read the
    // same one page and the test would prove nothing. Alternating ids cannot be run-encoded and
    // cannot fit an array cell, so each container is promoted onto a page of its own - which is
    // exactly the page `count_all` must not read.
    use big_pager::CountingPager;

    let d = Db::open(CountingPager::new(big_pager::MemPager::new())).unwrap();
    d.create_table("tx").unwrap();
    d.create_field("tx", "amount", FieldKind::Int, 37).unwrap();
    let mut w = d.write();
    for r in 0..20_000u64 {
        w.set_int("tx", "amount", r * 2, r).unwrap();
    }
    w.commit().unwrap();

    let before = d.store().pager().counts().reads;
    assert_eq!(d.read().count_all("tx").unwrap(), 20_000);
    let cheap = d.store().pager().counts().reads - before;

    let before = d.store().pager().counts().reads;
    assert_eq!(d.read().all("tx").unwrap().cardinality(), 20_000);
    let expensive = d.store().pager().counts().reads - before;

    assert!(cheap < expensive, "count_all read {cheap} pages, all() read {expensive}");
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(40))]

    /// The property the operation stands on: however the table got into this state, the number
    /// off the leaf cells is the number off the containers.
    #[test]
    fn the_cheap_count_always_agrees_with_the_expensive_one(
        writes in proptest::collection::vec(0u64..3_000_000, 1..60),
        deletes in proptest::collection::vec(0u64..3_000_000, 0..30),
    ) {
        let d = db();
        seed(&d, writes.iter().copied());

        let mut w = d.write();
        w.delete("tx", &deletes).unwrap();
        w.commit().unwrap();

        let (cheap, expensive) = both(&d);
        prop_assert_eq!(cheap, expensive);
    }

    /// The same property for grouping, which counts intersections instead of containers.
    ///
    /// `group_counts` walks the fragment once and counts each row's overlap with the filter
    /// without building it. The expensive way - materialise the row, intersect, ask the length
    /// - is what it replaced, and the two can only disagree by dropping or double-counting a
    /// record, which a grouped answer would report as a plausible number. Under a history of
    /// writes and deletes, because that is what moves containers between representations and
    /// the counting routine is chosen by both operands' representation.
    #[test]
    fn a_counted_grouping_always_agrees_with_a_built_one(
        writes in proptest::collection::vec(0u64..3_000_000, 1..60),
        deletes in proptest::collection::vec(0u64..3_000_000, 0..30),
        floor in 0u64..3_000_100,
    ) {
        let d = db();
        seed(&d, writes.iter().copied());

        let mut w = d.write();
        w.delete("tx", &deletes).unwrap();
        w.commit().unwrap();

        let r = d.read();
        // Two filters: everything, and a predicate that leaves a sparser bitmap behind.
        for filter in [
            r.all("tx").unwrap(),
            r.matching("tx", "amount", RangeOp::Ge, floor).unwrap(),
        ] {
            let counted = r.group_counts("tx", "country", &filter).unwrap();

            // The expensive way, spelled out here rather than kept in the engine: for each row
            // of the field, build the intersection and measure it.
            let mut built: Vec<(u64, u64)> = Vec::new();
            for (field, _, row) in r.row_keys("tx").unwrap() {
                if field != "country" {
                    continue;
                }
                let n = r.matching_row("tx", "country", row).unwrap().and(&filter).cardinality();
                if n > 0 {
                    built.push((row, n));
                }
            }
            built.sort_unstable();

            prop_assert_eq!(counted, built);
        }
    }
}
