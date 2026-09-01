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

//! **What the write path is allowed to skip.**
//!
//! Two things a commit used to do per fact it now does once: mark a record as existing, and
//! observe a value into the zone map. Both are safe only because of a property of the batch
//! rather than of the fact - the existence bit is the same for every field of one record, and
//! `observe` is a minimum and a maximum, which fold. A property of the batch is exactly the kind
//! of assumption that holds until an ordering nobody pictured turns up, so the orderings are
//! written down here.

use big_db::*;

fn stocked() -> Db<big_pager::MemPager> {
    let d = Db::in_memory().unwrap();
    d.create_table("t").unwrap();
    d.create_field("t", "amount", FieldKind::Int, 32).unwrap();
    d.create_field("t", "country", FieldKind::Set, 0).unwrap();
    d.create_field("t", "active", FieldKind::Bool, 0).unwrap();
    d
}

/// The memo remembers one record, so facts that arrive record by record hit it and facts that
/// interleave miss it. Missing has to cost nothing but the saving.
#[test]
fn every_record_exists_when_facts_arrive_interleaved() {
    let d = stocked();
    let ids: Vec<u64> = (0..64).map(|i| i * 3).collect();

    let mut w = d.write();
    // Column-major: every record's amount, then every record's country, then every flag. No two
    // consecutive facts name the same record, so the memo answers "different" every time.
    for id in &ids {
        w.set_int("t", "amount", *id, id % 500).unwrap();
    }
    for id in &ids {
        w.set_key("t", "country", *id, "GB").unwrap();
    }
    for id in &ids {
        w.set_bool("t", "active", *id, id % 2 == 0).unwrap();
    }
    w.commit().unwrap();

    let r = d.read();
    assert_eq!(r.count_all("t").unwrap(), ids.len() as u64);
    for id in &ids {
        assert!(r.exists("t", *id).unwrap(), "record {id} lost its existence bit");
    }
}

/// One record id in two tables is two existence bits. A memo keyed on the record alone would
/// write the first and skip the second.
#[test]
fn the_same_record_id_exists_in_two_tables() {
    let d = stocked();
    d.create_table("u").unwrap();
    d.create_field("u", "amount", FieldKind::Int, 32).unwrap();

    let mut w = d.write();
    w.set_int("t", "amount", 7, 1).unwrap();
    w.set_int("u", "amount", 7, 2).unwrap();
    w.commit().unwrap();

    let r = d.read();
    assert!(r.exists("t", 7).unwrap(), "the first table lost it");
    assert!(r.exists("u", 7).unwrap(), "the second table lost it");
}

/// `delete` flushes the whole buffer before it looks for anything, so a record written before
/// one and again after it spans a flush - which is where a memo that outlived its buffer would
/// skip a bit that is no longer buffered.
#[test]
fn a_record_written_either_side_of_a_flush_still_exists() {
    let d = stocked();
    let mut w = d.write();
    w.set_int("t", "amount", 11, 5).unwrap();
    // Deletes nothing, and flushes everything on the way to finding that out.
    w.delete("t", &[999_999]).unwrap();
    w.set_key("t", "country", 11, "GB").unwrap();
    w.commit().unwrap();

    assert!(d.read().exists("t", 11).unwrap());
    assert_eq!(d.read().count_all("t").unwrap(), 1);
}

/// **The zone map is folded now and applied at the flush, so the depth a batch ends on has to
/// be the depth every record in it was expanded at.**
///
/// Both records are written in one transaction and the second is what widens the field from one
/// plane to twenty. Applied in the wrong order, the first record is expanded at yesterday's
/// depth and reads back as something else.
#[test]
fn a_value_written_before_the_depth_grew_reads_back_whole() {
    let d = stocked();
    let mut w = d.write();
    w.set_int("t", "amount", 1, 1).unwrap();
    w.set_int("t", "amount", 2, 1_000_000).unwrap();
    w.set_int("t", "amount", 3, 999).unwrap();
    w.commit().unwrap();

    let r = d.read();
    assert_eq!(r.get_int("t", "amount", 1).unwrap(), Some(1));
    assert_eq!(r.get_int("t", "amount", 2).unwrap(), Some(1_000_000));
    assert_eq!(r.get_int("t", "amount", 3).unwrap(), Some(999));
}

/// The same, across a shard boundary: each shard keeps its own zone map, and folding is per
/// fragment rather than per field.
#[test]
fn each_shard_keeps_its_own_zone_map() {
    let d = stocked();
    let far = big_engine::SHARD_WIDTH + 4;
    let mut w = d.write();
    w.set_int("t", "amount", 4, 7).unwrap();
    w.set_int("t", "amount", far, 900_000).unwrap();
    w.commit().unwrap();

    let r = d.read();
    assert_eq!(r.get_int("t", "amount", 4).unwrap(), Some(7));
    assert_eq!(r.get_int("t", "amount", far).unwrap(), Some(900_000));
    assert_eq!(r.count("t", "amount", RangeOp::Gt, 100_000).unwrap(), 1);
    assert_eq!(r.count("t", "amount", RangeOp::Lt, 100_000).unwrap(), 1);
}

/// **The clear half of a bit-sliced write, which is the half a load never exercises.**
///
/// The second transaction finds the record already there, so its zero planes become clears
/// rather than no-ops. A grouping that lost them would leave the old value's high bits standing
/// and the record would read back larger than it is.
#[test]
fn overwriting_a_wide_value_with_a_narrow_one_leaves_nothing_behind() {
    let d = stocked();
    let ids: Vec<u64> = (0..200).map(|i| i * 5).collect();

    let mut w = d.write();
    for id in &ids {
        w.set_int("t", "amount", *id, 1_048_575).unwrap();
    }
    w.commit().unwrap();

    let mut w = d.write();
    for id in &ids {
        w.set_int("t", "amount", *id, 3).unwrap();
    }
    w.commit().unwrap();

    let r = d.read();
    for id in &ids {
        assert_eq!(r.get_int("t", "amount", *id).unwrap(), Some(3), "record {id} kept a stale bit");
    }
    assert_eq!(r.count("t", "amount", RangeOp::Eq, 3).unwrap(), ids.len() as u64);
    assert_eq!(r.count("t", "amount", RangeOp::Gt, 3).unwrap(), 0);
}

/// A batch big enough to cross the fan-out threshold, so the parallel grouping is what answers.
/// Values are spread over every plane and records over several containers.
#[test]
fn a_batch_large_enough_to_be_grouped_in_parallel_reads_back() {
    let d = stocked();
    let n = 120_000u64;
    let mut w = d.write();
    for i in 0..n {
        w.set_int("t", "amount", i, (i * 2_654_435_761) % 1_048_576).unwrap();
    }
    w.commit().unwrap();

    let r = d.read();
    assert_eq!(r.count_all("t").unwrap(), n);
    for i in [0u64, 1, 65_535, 65_536, 70_000, n - 1] {
        let want = (i * 2_654_435_761) % 1_048_576;
        assert_eq!(r.get_int("t", "amount", i).unwrap(), Some(want), "record {i}");
    }
    let brute = (0..n).filter(|i| (i * 2_654_435_761) % 1_048_576 > 500_000).count() as u64;
    assert_eq!(r.count("t", "amount", RangeOp::Gt, 500_000).unwrap(), brute);
}
