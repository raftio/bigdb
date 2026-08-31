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

//! Deleting, and dropping. The half of the engine that lets a mistake be undone.

use big_db::*;
use big_fragment::SHARD_WIDTH;

fn db() -> Db<big_pager::MemPager> {
    let d = Db::in_memory().unwrap();
    d.create_table("tx").unwrap();
    d.create_field("tx", "amount", FieldKind::Int, 37).unwrap();
    d.create_field("tx", "country", FieldKind::Set, 0).unwrap();
    d.create_field("tx", "tier", FieldKind::Mutex, 0).unwrap();
    d.create_field("tx", "active", FieldKind::Bool, 0).unwrap();
    d
}

fn seed(d: &Db<big_pager::MemPager>, records: impl IntoIterator<Item = u64>) {
    let mut w = d.write();
    for r in records {
        w.set_int("tx", "amount", r, r + 100).unwrap();
        w.set_key("tx", "country", r, if r.is_multiple_of(2) { "vn" } else { "jp" }).unwrap();
        w.set_key("tx", "tier", r, "gold").unwrap();
        w.set_bool("tx", "active", r, true).unwrap();
    }
    w.commit().unwrap();
}

// ---------------------------------------------------------------------------
// Deleting a record
// ---------------------------------------------------------------------------

#[test]
fn a_deleted_record_is_gone_from_every_field_at_once() {
    let d = db();
    seed(&d, [1u64, 2, 3]);

    let mut w = d.write();
    assert_eq!(w.delete("tx", &[2]).unwrap(), 1);
    w.commit().unwrap();

    let r = d.read();
    assert!(!r.exists("tx", 2).unwrap());
    assert_eq!(r.get_int("tx", "amount", 2).unwrap(), None);
    assert_eq!(r.by_key("tx", "country", "vn").unwrap(), vec![]);
    assert_eq!(r.matching_key("tx", "tier", "gold").unwrap().records().collect::<Vec<_>>(), [1, 3]);
    assert_eq!(r.matching_bool("tx", "active", true).unwrap().cardinality(), 2);

    // The neighbours are untouched, which is the thing a blunt delete would get wrong.
    assert_eq!(r.get_int("tx", "amount", 1).unwrap(), Some(101));
    assert_eq!(r.get_int("tx", "amount", 3).unwrap(), Some(103));
    assert_eq!(r.all("tx").unwrap().records().collect::<Vec<_>>(), [1, 3]);
}

#[test]
fn deleting_reports_how_many_records_actually_existed() {
    let d = db();
    seed(&d, [1u64, 2]);

    let mut w = d.write();
    // One real, one never written, one duplicate of the real one.
    assert_eq!(w.delete("tx", &[1, 999, 1]).unwrap(), 1);
    w.commit().unwrap();

    assert!(!d.read().exists("tx", 1).unwrap());
    assert!(d.read().exists("tx", 2).unwrap());
}

#[test]
fn deleting_spans_shards() {
    let d = db();
    let records = [1u64, SHARD_WIDTH + 1, SHARD_WIDTH * 3 + 7];
    seed(&d, records);

    let mut w = d.write();
    assert_eq!(w.delete("tx", &records).unwrap(), 3);
    w.commit().unwrap();

    let r = d.read();
    assert_eq!(r.all("tx").unwrap().cardinality(), 0);
    assert_eq!(r.sum("tx", "amount").unwrap(), 0);
}

#[test]
fn a_delete_and_a_rewrite_in_one_transaction_keep_the_rewrite() {
    let d = db();
    seed(&d, [5u64]);

    let mut w = d.write();
    w.delete("tx", &[5]).unwrap();
    w.set_int("tx", "amount", 5, 42).unwrap();
    w.commit().unwrap();

    let r = d.read();
    assert!(r.exists("tx", 5).unwrap());
    assert_eq!(r.get_int("tx", "amount", 5).unwrap(), Some(42));
    // The old value must not survive alongside the new one in some plane.
    assert_eq!(r.sum("tx", "amount").unwrap(), 42);
}

#[test]
fn a_write_then_a_delete_in_one_transaction_leaves_nothing() {
    let d = db();
    let mut w = d.write();
    w.set_int("tx", "amount", 9, 1234).unwrap();
    w.set_key("tx", "country", 9, "vn").unwrap();
    w.delete("tx", &[9]).unwrap();
    w.commit().unwrap();

    let r = d.read();
    assert!(!r.exists("tx", 9).unwrap());
    assert_eq!(r.get_int("tx", "amount", 9).unwrap(), None);
    assert_eq!(r.by_key("tx", "country", "vn").unwrap(), vec![]);
}

#[test]
fn deleting_a_mutex_value_clears_its_shadow_too() {
    // The shadow is what makes a mutex a point lookup. Left behind, it would claim the record
    // still holds a row whose bit is gone - and the next write would try to clear that row.
    let d = db();
    seed(&d, [4u64]);

    let mut w = d.write();
    w.delete("tx", &[4]).unwrap();
    w.commit().unwrap();

    let mut w = d.write();
    w.set_key("tx", "tier", 4, "silver").unwrap();
    w.commit().unwrap();

    let r = d.read();
    assert_eq!(r.matching_key("tx", "tier", "silver").unwrap().cardinality(), 1);
    assert_eq!(r.matching_key("tx", "tier", "gold").unwrap().cardinality(), 0);
}

#[test]
fn deleting_everything_returns_the_pages() {
    let d = db();
    let records: Vec<u64> = (0..4000u64).collect();
    seed(&d, records.iter().copied());
    let live_before = {
        let m = d.store().metrics();
        m.page_count - m.free_pages_reusable
    };

    let mut w = d.write();
    w.delete("tx", &records).unwrap();
    w.commit().unwrap();

    let r = d.read();
    assert_eq!(r.all("tx").unwrap().cardinality(), 0);

    let live_after = {
        let m = d.store().metrics();
        m.page_count - m.free_pages_reusable
    };
    assert!(
        live_after < live_before,
        "an emptied table must hand its pages back: {live_after} is not below {live_before}"
    );
}

#[test]
fn delete_where_undoes_a_selection() {
    // The shape of "undo a wrong import": name the records with a query, then remove them.
    let d = db();
    seed(&d, (0..200u64).collect::<Vec<_>>());

    let doomed = d.read().matching("tx", "amount", RangeOp::Ge, 250).unwrap();
    let expected = doomed.cardinality();
    assert!(expected > 0 && expected < 200, "the fixture must select some but not all");

    let mut w = d.write();
    assert_eq!(w.delete_where("tx", &doomed).unwrap(), expected);
    w.commit().unwrap();

    let r = d.read();
    assert_eq!(r.count("tx", "amount", RangeOp::Ge, 250).unwrap(), 0);
    assert_eq!(r.all("tx").unwrap().cardinality(), 200 - expected);
}

#[test]
fn deleting_from_an_unknown_table_is_an_error_not_a_panic() {
    let d = db();
    let mut w = d.write();
    assert!(matches!(w.delete("nope", &[1]), Err(DbError::UnknownTable(_))));
}

// ---------------------------------------------------------------------------
// Dropping
// ---------------------------------------------------------------------------

#[test]
fn dropping_a_field_removes_it_from_the_schema_and_the_disk() {
    let d = db();
    seed(&d, (0..500u64).collect::<Vec<_>>());
    let live_before = {
        let m = d.store().metrics();
        m.page_count - m.free_pages_reusable
    };

    assert!(d.drop_field("tx", "country").unwrap());

    // Gone from the schema...
    let r = d.read();
    assert!(matches!(r.matching_key("tx", "country", "vn"), Err(DbError::UnknownField { .. })));
    // ...and the other fields are untouched.
    assert_eq!(r.get_int("tx", "amount", 4).unwrap(), Some(104));
    assert_eq!(r.all("tx").unwrap().cardinality(), 500);
    drop(r);

    let live_after = {
        let m = d.store().metrics();
        m.page_count - m.free_pages_reusable
    };
    assert!(
        live_after < live_before,
        "a dropped field must free its trees: {live_after} is not below {live_before}"
    );
}

#[test]
fn dropping_a_table_removes_everything_under_it() {
    let d = db();
    seed(&d, (0..500u64).collect::<Vec<_>>());
    d.create_table("other").unwrap();
    d.create_field("other", "n", FieldKind::Int, 8).unwrap();
    let mut w = d.write();
    w.set_int("other", "n", 1, 7).unwrap();
    w.commit().unwrap();

    assert!(d.drop_table("tx").unwrap());

    let r = d.read();
    assert!(matches!(r.all("tx"), Err(DbError::UnknownTable(_))));
    // The other table is entirely unaffected.
    assert_eq!(r.get_int("other", "n", 1).unwrap(), Some(7));
}

#[test]
fn dropping_something_that_is_not_there_is_false_not_an_error() {
    let d = db();
    assert!(!d.drop_table("nope").unwrap());
    assert!(!d.drop_field("tx", "nope").unwrap());
    assert!(matches!(d.drop_field("nope", "amount"), Err(DbError::UnknownTable(_))));
}

#[test]
fn a_table_id_is_never_handed_out_twice() {
    // The hazard the id counters exist for. Ids used to be `max(existing) + 1`, so dropping
    // the highest table and creating another gave the new one the old id - and with it any
    // fragment or row key that outlived the drop.
    let d = db();
    d.create_table("second").unwrap();
    let dropped = d.catalog().table("second").unwrap().id;

    assert!(d.drop_table("second").unwrap());
    let fresh = d.create_table("third").unwrap();

    assert_ne!(fresh, dropped, "a dropped id must never come back");
}

#[test]
fn a_field_id_is_never_handed_out_twice() {
    let d = db();
    let dropped = {
        let c = d.catalog();
        c.field(c.table("tx").unwrap().id, "active").unwrap().id
    };

    assert!(d.drop_field("tx", "active").unwrap());
    let fresh = d.create_field("tx", "revived", FieldKind::Bool, 0).unwrap();

    assert_ne!(fresh, dropped, "a dropped field id must never come back");
}

#[test]
fn a_recreated_field_starts_empty() {
    // The consequence that matters. If ids were reused, the new field would answer with the
    // old one's bits.
    let d = db();
    seed(&d, (0..50u64).collect::<Vec<_>>());

    assert!(d.drop_field("tx", "country").unwrap());
    d.create_field("tx", "country", FieldKind::Set, 0).unwrap();

    let r = d.read();
    assert_eq!(r.matching_key("tx", "country", "vn").unwrap().cardinality(), 0);
    assert!(r.by_key("tx", "country", "vn").unwrap().is_empty());
}

#[test]
fn a_recreated_table_starts_empty() {
    let d = db();
    seed(&d, (0..50u64).collect::<Vec<_>>());

    assert!(d.drop_table("tx").unwrap());
    d.create_table("tx").unwrap();
    d.create_field("tx", "amount", FieldKind::Int, 37).unwrap();

    let r = d.read();
    assert_eq!(r.all("tx").unwrap().cardinality(), 0);
    assert_eq!(r.get_int("tx", "amount", 4).unwrap(), None);
}

#[test]
fn the_id_counters_survive_a_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("seq.big");

    let dropped = {
        let d = Db::open_path(&path).unwrap();
        d.create_table("a").unwrap();
        let b = d.create_table("b").unwrap();
        d.drop_table("b").unwrap();
        b
    };

    let d = Db::open_path(&path).unwrap();
    let fresh = d.create_table("c").unwrap();
    assert_ne!(fresh, dropped, "the high-water mark has to be on disk, not derived");
}

#[test]
fn a_dropped_table_does_not_come_back_after_a_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("dropped.big");
    {
        let d = Db::open_path(&path).unwrap();
        d.create_table("tx").unwrap();
        d.create_field("tx", "amount", FieldKind::Int, 20).unwrap();
        let mut w = d.write();
        for r in 0..100u64 {
            w.set_int("tx", "amount", r, r).unwrap();
        }
        w.commit().unwrap();
        assert!(d.drop_table("tx").unwrap());
    }

    let d = Db::open_path(&path).unwrap();
    assert!(d.catalog().table("tx").is_none());
    assert!(matches!(d.read().all("tx"), Err(DbError::UnknownTable(_))));
}

#[test]
fn a_backup_taken_after_a_drop_does_not_carry_the_dropped_data() {
    // Drop frees the trees and removes the root records, so the copy walk cannot find them.
    // If a drop ever forgot the root records, this is where it would show up as a backup
    // that is larger than the database it came from.
    let d = db();
    seed(&d, (0..500u64).collect::<Vec<_>>());
    assert!(d.drop_field("tx", "country").unwrap());

    let copy = d.copy_to(big_pager::MemPager::new()).unwrap();
    let r = copy.read();
    assert!(matches!(r.matching_key("tx", "country", "vn"), Err(DbError::UnknownField { .. })));
    assert_eq!(r.all("tx").unwrap().cardinality(), 500);
}

// ---------------------------------------------------------------------------
// Query memory ceilings
// ---------------------------------------------------------------------------

#[test]
fn a_query_over_its_memory_ceiling_is_refused_not_attempted() {
    let d = db();
    // Spread across shards so the fan-out holds several row sets at once, which is the peak
    // the byte ceiling exists to catch.
    seed(&d, (0..2000u64).map(|r| r * 701));

    let r = d.read().with_limits(QueryLimits { max_bytes: 64, max_records: usize::MAX });
    let err = r.all("tx").unwrap_err();
    assert!(
        matches!(err, DbError::QueryTooLarge { unit: "bytes", .. }),
        "expected a byte-ceiling refusal, got {err:?}"
    );
}

#[test]
fn materialising_too_many_records_is_refused_before_anything_is_allocated() {
    let d = db();
    seed(&d, (0..500u64).collect::<Vec<_>>());

    let r = d.read().with_limits(QueryLimits { max_bytes: usize::MAX, max_records: 10 });
    let err = r.range("tx", "amount", RangeOp::Ge, 0).unwrap_err();
    match err {
        DbError::QueryTooLarge { limit, needed, unit } => {
            assert_eq!((limit, unit), (10, "records"));
            assert_eq!(needed, 500, "the exact size is known before it is built");
        }
        other => panic!("expected a record-ceiling refusal, got {other:?}"),
    }

    // Counting the same thing is still fine: it never names a record.
    assert_eq!(r.count("tx", "amount", RangeOp::Ge, 0).unwrap(), 500);
}

#[test]
fn the_default_ceilings_do_not_get_in_the_way() {
    let d = db();
    seed(&d, (0..5000u64).collect::<Vec<_>>());
    let r = d.read();
    assert_eq!(r.all("tx").unwrap().cardinality(), 5000);
    assert_eq!(r.range("tx", "amount", RangeOp::Ge, 0).unwrap().len(), 5000);
    assert_eq!(r.by_key("tx", "country", "vn").unwrap().len(), 2500);
}

#[test]
fn the_ceiling_is_per_read_transaction_not_per_call() {
    // A budget that reset every call would not bound anything: the peak is several row sets
    // alive at once, which is exactly what a sequence of calls inside one query produces.
    let d = db();
    seed(&d, (0..2000u64).collect::<Vec<_>>());

    // Measured rather than guessed. Contiguous records compress to a single run of a few
    // bytes, so any hard-coded figure here would be a statement about the container encoding
    // rather than about the budget.
    let probe = d.read();
    probe.all("tx").unwrap();
    let one_scan = probe.spent_bytes();
    assert!(one_scan > 0, "a scan has to cost something to be worth capping");

    let r = d.read().with_limits(QueryLimits { max_bytes: one_scan * 3, max_records: usize::MAX });
    for i in 0..3 {
        assert!(r.all("tx").is_ok(), "scan {i} should still be within budget");
    }
    assert!(
        matches!(r.all("tx"), Err(DbError::QueryTooLarge { unit: "bytes", .. })),
        "the fourth scan must exhaust a budget sized for three"
    );

    // A fresh transaction starts with a fresh budget.
    assert!(d.read().all("tx").is_ok());
}
