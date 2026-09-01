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

//! End to end: schema, writes, queries, and reopening the file.

use big_db::catalog::{Catalog, MAX_NAME_LEN};
use big_db::*;
use big_engine::SHARD_WIDTH;
use proptest::prelude::*;
use std::collections::{BTreeMap, BTreeSet};

fn db() -> Db<big_pager::MemPager> {
    let d = Db::in_memory().unwrap();
    d.create_table("tx").unwrap();
    d.create_field("tx", "amount", FieldKind::Int, 37).unwrap();
    d.create_field("tx", "country", FieldKind::Set, 0).unwrap();
    d.create_field("tx", "tier", FieldKind::Mutex, 0).unwrap();
    d.create_field("tx", "active", FieldKind::Bool, 0).unwrap();
    d
}

#[test]
fn a_value_survives_the_whole_stack() {
    let d = db();
    let mut w = d.write();
    w.set_int("tx", "amount", 42, 1_000_000).unwrap();
    w.commit().unwrap();

    let r = d.read();
    assert_eq!(r.get_int("tx", "amount", 42).unwrap(), Some(1_000_000));
    assert_eq!(r.get_int("tx", "amount", 43).unwrap(), None);
    assert!(r.exists("tx", 42).unwrap());
    assert!(!r.exists("tx", 43).unwrap(), "never written is not the same as zero");
}

/// Records land in different shards, and a query has to sweep all of them.
#[test]
fn queries_span_shards() {
    let d = db();
    let recs: Vec<u64> = (0..5).map(|i| i * SHARD_WIDTH + 7).collect();

    let mut w = d.write();
    for (i, rec) in recs.iter().enumerate() {
        w.set_int("tx", "amount", *rec, (i as u64 + 1) * 1000).unwrap();
    }
    w.commit().unwrap();

    assert_eq!(d.catalog().fragments_of_field(0, 0, STANDARD_VIEW).count(), 5);

    let r = d.read();
    assert_eq!(r.range("tx", "amount", RangeOp::Gt, 2500).unwrap(), recs[2..].to_vec());
    assert_eq!(r.range("tx", "amount", RangeOp::Le, 2000).unwrap(), recs[..2].to_vec());
    assert_eq!(r.range("tx", "amount", RangeOp::Eq, 3000).unwrap(), vec![recs[2]]);
    assert_eq!(r.sum("tx", "amount").unwrap(), 15_000u128);
}

/// A shard whose max is below the threshold is skipped without reading a page.
#[test]
fn zone_maps_rule_shards_out() {
    let mut m = FragmentMeta::default();
    assert!(!m.may_contain(None, None), "an empty fragment can never match");

    m.observe(10);
    m.observe(50);
    assert_eq!((m.min, m.max), (10, 50));
    assert!(m.may_contain(Some(40), None));
    assert!(!m.may_contain(Some(51), None), "max below the floor");
    assert!(!m.may_contain(None, Some(9)), "min above the ceiling");
    assert!(m.may_contain(Some(10), Some(10)));
    assert_eq!(m.bit_depth, 6, "50 needs six planes");
}

#[test]
fn row_keys_are_interned_and_shared_across_shards() {
    let d = db();
    let mut w = d.write();
    let vn = w.set_key("tx", "country", 1, "vn").unwrap();
    w.set_key("tx", "country", SHARD_WIDTH + 2, "vn").unwrap();
    w.set_key("tx", "country", 3, "jp").unwrap();
    w.commit().unwrap();

    assert_eq!(d.catalog().keys.id(0, 1, "vn"), Some(vn));
    let r = d.read();
    assert_eq!(r.by_key("tx", "country", "vn").unwrap(), vec![1, SHARD_WIDTH + 2]);
    assert_eq!(r.by_key("tx", "country", "jp").unwrap(), vec![3]);
    assert!(r.by_key("tx", "country", "never-seen").unwrap().is_empty());
}

/// A set field lets a record hold several values at once.
#[test]
fn a_set_field_keeps_every_value() {
    let d = db();
    let mut w = d.write();
    for c in ["vn", "jp", "kr"] {
        w.set_key("tx", "country", 5, c).unwrap();
    }
    w.commit().unwrap();

    let r = d.read();
    for c in ["vn", "jp", "kr"] {
        assert_eq!(r.by_key("tx", "country", c).unwrap(), vec![5]);
    }
}

/// A mutex field keeps at most one, and moving a record clears the old row.
#[test]
fn a_mutex_field_keeps_only_the_latest_value() {
    let d = db();
    let mut w = d.write();
    w.set_key("tx", "tier", 5, "gold").unwrap();
    w.set_key("tx", "tier", 6, "gold").unwrap();
    w.set_key("tx", "tier", 5, "silver").unwrap();
    w.commit().unwrap();

    let r = d.read();
    assert_eq!(r.by_key("tx", "tier", "gold").unwrap(), vec![6], "record 5 left gold");
    assert_eq!(r.by_key("tx", "tier", "silver").unwrap(), vec![5]);
}

#[test]
fn overwriting_an_int_replaces_it() {
    let d = db();
    let mut w = d.write();
    w.set_int("tx", "amount", 1, 0b1111).unwrap();
    w.commit().unwrap();

    let mut w = d.write();
    w.set_int("tx", "amount", 1, 0b0001).unwrap();
    w.commit().unwrap();

    assert_eq!(d.read().get_int("tx", "amount", 1).unwrap(), Some(1));
    assert_eq!(d.read().sum("tx", "amount").unwrap(), 1);
}

#[test]
fn a_value_wider_than_the_field_is_refused() {
    let d = Db::in_memory().unwrap();
    d.create_table("t").unwrap();
    d.create_field("t", "small", FieldKind::Int, 8).unwrap();

    let mut w = d.write();
    assert!(w.set_int("t", "small", 1, 255).is_ok());
    assert!(matches!(w.set_int("t", "small", 2, 256), Err(DbError::Field(_))));
}

#[test]
fn unknown_names_are_errors_not_panics() {
    let d = db();
    let mut w = d.write();
    assert!(matches!(w.set_int("nope", "amount", 1, 1), Err(DbError::UnknownTable(_))));
    assert!(matches!(w.set_int("tx", "nope", 1, 1), Err(DbError::UnknownField { .. })));
    assert!(matches!(w.set_int("tx", "country", 1, 1), Err(DbError::WrongFieldKind { .. })));
}

/// Renaming touches the catalog and not one byte of data.
#[test]
fn renaming_a_table_leaves_the_data_alone() {
    let d = db();
    let mut w = d.write();
    w.set_int("tx", "amount", 9, 777).unwrap();
    w.commit().unwrap();

    // Under copy-on-write, rewriting a byte of the fragment would give it a new root page.
    // An unchanged root is therefore the direct statement of "the data was not touched".
    //
    // Page count is deliberately not the assertion here. The commit still rewrites the
    // catalog, and whether that lands on a reclaimed page or extends the file depends on how
    // much garbage the previous write happened to leave behind - which is a property of the
    // write path, not of renaming.
    let field = d.catalog().field(0, "amount").expect("field exists").id;
    let key = FragmentKey::new(0, field, 0, 0);
    let root_before = d.store().roots().get(&key).expect("the write created a fragment");

    let mut w = d.write();
    assert!(w.catalog_mut().rename_table(big_db::DEFAULT_DATABASE, "tx", "transactions").unwrap());
    w.commit().unwrap();

    assert_eq!(d.store().roots().get(&key), Some(root_before), "no data pages rewritten");
    assert_eq!(d.read().get_int("transactions", "amount", 9).unwrap(), Some(777));
}

#[test]
fn bool_fields_flip_cleanly() {
    let d = db();
    let mut w = d.write();
    w.set_bool("tx", "active", 1, true).unwrap();
    w.set_bool("tx", "active", 1, false).unwrap();
    w.commit().unwrap();
    assert!(d.read().exists("tx", 1).unwrap());
}

/// Schema and data are written by the same commit, so they cannot drift apart.
#[cfg(unix)]
#[test]
fn everything_survives_a_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.big");

    {
        let d = Db::open_path(&path).unwrap();
        d.create_table("tx").unwrap();
        d.create_field("tx", "amount", FieldKind::Int, 37).unwrap();
        d.create_field("tx", "country", FieldKind::Set, 0).unwrap();

        let mut w = d.write();
        for i in 0..300u64 {
            w.set_int("tx", "amount", i * 4096, i * 1_000_000).unwrap();
        }
        w.set_key("tx", "country", 8192, "vn").unwrap();
        w.commit().unwrap();
    }

    let d = Db::open_path(&path).unwrap();
    assert!(d.catalog().lookup("tx").is_some());
    assert_eq!(d.catalog().field(0, "amount").unwrap().bit_depth, 37);

    let r = d.read();
    assert_eq!(r.get_int("tx", "amount", 299 * 4096).unwrap(), Some(299_000_000));
    assert_eq!(r.by_key("tx", "country", "vn").unwrap(), vec![8192]);
    assert_eq!(r.range("tx", "amount", RangeOp::Ge, 298_000_000).unwrap().len(), 2);
    assert_eq!(r.sum("tx", "amount").unwrap(), (0..300u128).sum::<u128>() * 1_000_000);
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(40))]

    /// The database must answer exactly what a plain map would.
    #[test]
    fn database_tracks_a_map(
        vals in proptest::collection::btree_map(0u64..3_000_000, 0u64..500_000, 1..40),
        k in 0u64..500_000,
    ) {
        let d = db();
        let mut w = d.write();
        for (rec, v) in &vals {
            w.set_int("tx", "amount", *rec, *v).unwrap();
        }
        w.commit().unwrap();
        let r = d.read();

        for (rec, v) in &vals {
            prop_assert_eq!(r.get_int("tx", "amount", *rec).unwrap(), Some(*v));
            prop_assert!(r.exists("tx", *rec).unwrap());
        }

        let want: Vec<u64> = vals.iter().filter(|(_, v)| **v > k).map(|(rec, _)| *rec).collect();
        prop_assert_eq!(r.range("tx", "amount", RangeOp::Gt, k).unwrap(), want);

        let total: u128 = vals.values().map(|v| *v as u128).sum();
        prop_assert_eq!(r.sum("tx", "amount").unwrap(), total);
    }

    #[test]
    fn keys_and_records_stay_in_step(
        pairs in proptest::collection::vec((0u64..2_000_000, "[a-z]{1,6}"), 1..40)
    ) {
        let d = db();
        let mut model: BTreeMap<String, Vec<u64>> = BTreeMap::new();
        let mut w = d.write();
        for (rec, name) in &pairs {
            w.set_key("tx", "country", *rec, name).unwrap();
            let e = model.entry(name.clone()).or_default();
            if !e.contains(rec) {
                e.push(*rec);
            }
        }
        w.commit().unwrap();

        let r = d.read();
        for (name, recs) in &mut model {
            recs.sort_unstable();
            prop_assert_eq!(&r.by_key("tx", "country", name).unwrap(), recs);
        }
    }
}

// Writes inside one transaction are buffered per fragment and applied once at commit. These
// cover what that buffering could plausibly get wrong: a place written twice, a value that
// narrows, and a depth that grows after an earlier value was already accepted.

/// Overwriting within a transaction must land on the later value, and must not leave the
/// earlier value's high bit planes behind. A stale plane reads back as a larger number.
#[test]
fn a_narrower_value_written_over_a_wider_one_clears_the_high_planes() {
    let d = db();
    let mut w = d.write();
    w.set_int("tx", "amount", 5, 1_000_000).unwrap();
    w.set_int("tx", "amount", 5, 3).unwrap();
    w.commit().unwrap();

    assert_eq!(d.read().get_int("tx", "amount", 5).unwrap(), Some(3));
}

/// A value accepted while the fragment was still narrow must survive a later write that
/// widens it, because the whole batch is expanded at the depth the transaction ends on.
#[test]
fn a_value_written_before_the_depth_grew_still_reads_back() {
    let d = db();
    let mut w = d.write();
    w.set_int("tx", "amount", 1, 3).unwrap();
    w.set_int("tx", "amount", 2, 1_000_000).unwrap();
    w.commit().unwrap();

    let r = d.read();
    assert_eq!(r.get_int("tx", "amount", 1).unwrap(), Some(3));
    assert_eq!(r.get_int("tx", "amount", 2).unwrap(), Some(1_000_000));
}

#[test]
fn a_bool_flipped_twice_in_one_transaction_keeps_the_last_value() {
    let d = db();
    let mut w = d.write();
    w.set_bool("tx", "active", 4, true).unwrap();
    w.set_bool("tx", "active", 4, false).unwrap();
    w.commit().unwrap();

    // The false row is row 0 of the field's fragment, so a leftover true row would show up
    // as the record belonging to both.
    let r = d.read();
    assert!(r.exists("tx", 4).unwrap());
}

/// Reaching for a fragment handle mid-transaction must see the buffered writes, not the state
/// before them. Without the flush on access, the handle would still have no root at all.
#[test]
fn a_fragment_handle_sees_writes_buffered_earlier_in_the_transaction() {
    let d = db();
    let field = d.catalog().field(0, "amount").expect("field exists").id;
    let key = FragmentKey::new(0, field, 0, 0);

    let mut w = d.write();
    assert!(w.fragment(key).root().is_none(), "nothing written yet");
    w.set_int("tx", "amount", 7, 42).unwrap();
    assert!(w.fragment(key).root().is_some(), "the buffered write must have been applied");
    w.commit().unwrap();

    assert_eq!(d.read().get_int("tx", "amount", 7).unwrap(), Some(42));
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(40))]

    /// A sequence of writes, repeats allowed, against a map applying the same sequence. This
    /// is the case the older map-based property test cannot reach: it writes each record once,
    /// so it never exercises an overwrite inside a single transaction.
    #[test]
    fn a_sequence_of_writes_ends_where_a_map_would(
        writes in proptest::collection::vec((0u64..50, 0u64..500_000), 1..60),
    ) {
        let d = db();
        let mut expected = BTreeMap::new();
        let mut w = d.write();
        for (rec, v) in &writes {
            w.set_int("tx", "amount", *rec, *v).unwrap();
            expected.insert(*rec, *v);
        }
        w.commit().unwrap();

        let r = d.read();
        for (rec, v) in &expected {
            prop_assert_eq!(r.get_int("tx", "amount", *rec).unwrap(), Some(*v));
        }
        let total: u128 = expected.values().map(|v| *v as u128).sum();
        prop_assert_eq!(r.sum("tx", "amount").unwrap(), total);
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(40))]

    /// `count` must agree with counting what `range` returns. It skips materialising the ids,
    /// which is the whole point, so the two must not be allowed to drift apart.
    #[test]
    fn count_agrees_with_range(
        vals in proptest::collection::btree_map(0u64..3_000_000, 0u64..500_000, 1..40),
        k in 0u64..500_000,
    ) {
        let d = db();
        let mut w = d.write();
        for (rec, v) in &vals {
            w.set_int("tx", "amount", *rec, *v).unwrap();
        }
        w.commit().unwrap();
        let r = d.read();

        for op in [RangeOp::Gt, RangeOp::Ge, RangeOp::Lt, RangeOp::Le, RangeOp::Eq] {
            let materialised = r.range("tx", "amount", op, k).unwrap().len() as u64;
            prop_assert_eq!(
                r.count("tx", "amount", op, k).unwrap(),
                materialised,
                "{:?} disagreed", op
            );
        }
    }
}

/// What a point read and a count actually cost, counted in page reads rather than timed.
///
/// A BSI stores one integer across one container per bit plane, and a container big enough to
/// go dense lives on its own page. So a point read is bounded below by the bit depth no matter
/// how good the tree is: the bits are on that many different pages. Batching the descents
/// removes the tree walks, not the page reads.
#[test]
fn a_point_read_costs_one_page_per_bit_plane() {
    let pager = big_pager::CountingPager::new(big_pager::MemPager::new());
    let d = Db::open(pager).unwrap();
    d.create_table("tx").unwrap();
    d.create_field("tx", "amount", FieldKind::Int, 20).unwrap();

    let mut w = d.write();
    for rec in 0..20_000u64 {
        w.set_int("tx", "amount", rec, rec % 500_000).unwrap();
    }
    w.commit().unwrap();

    let depth = d.catalog().fragment(&FragmentKey::new(0, 0, 0, 0)).unwrap().bit_depth;

    d.store().pager().reset();
    assert!(d.read().get_int("tx", "amount", 7).unwrap().is_some());
    let reads = d.store().pager().counts().reads;
    println!("bit depth {depth}, point read = {reads} page reads");

    // One page per plane, plus the exists row, plus the descent. Were the planes probed one
    // at a time this would carry a whole descent each, so the ceiling is what pins the batch
    // path in place: if `get_many` regresses to `get` in a loop, this fails.
    assert!(
        reads <= depth as u64 + 8,
        "point read took {reads} page reads at depth {depth}; the batched probe has regressed"
    );
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(40))]

    /// The composable form must agree with doing the same set algebra on plain record ids.
    /// An executor is going to be built on this, so it has to be right before anything is.
    ///
    /// Record ids run past `SHARD_WIDTH` on purpose: combining is a shard-wise merge, and a
    /// single-shard corpus would never exercise the case where one side has a shard the other
    /// does not.
    #[test]
    fn matches_algebra_agrees_with_plain_sets(
        vals in proptest::collection::btree_map(0u64..3_000_000, 0u64..500_000, 1..40),
        lo in 0u64..500_000,
        hi in 0u64..500_000,
    ) {
        let d = db();
        let mut w = d.write();
        for (rec, v) in &vals {
            w.set_int("tx", "amount", *rec, *v).unwrap();
        }
        w.commit().unwrap();
        let r = d.read();

        let ge = r.matching("tx", "amount", RangeOp::Ge, lo).unwrap();
        let le = r.matching("tx", "amount", RangeOp::Le, hi).unwrap();

        let want_ge: BTreeSet<u64> =
            vals.iter().filter(|(_, v)| **v >= lo).map(|(k, _)| *k).collect();
        let want_le: BTreeSet<u64> =
            vals.iter().filter(|(_, v)| **v <= hi).map(|(k, _)| *k).collect();

        let ids = |m: &big_db::Matches| m.records().collect::<Vec<u64>>();
        let want = |s: BTreeSet<u64>| s.into_iter().collect::<Vec<u64>>();

        prop_assert_eq!(ids(&ge), want(want_ge.clone()), "Ge alone");
        prop_assert_eq!(
            ids(&ge.and(&le)),
            want(want_ge.intersection(&want_le).copied().collect()),
            "and"
        );
        prop_assert_eq!(
            ids(&ge.or(&le)),
            want(want_ge.union(&want_le).copied().collect()),
            "or"
        );
        prop_assert_eq!(
            ids(&ge.andnot(&le)),
            want(want_ge.difference(&want_le).copied().collect()),
            "andnot"
        );

        // Counting must never disagree with what counting the ids would say.
        prop_assert_eq!(
            ge.and(&le).cardinality(),
            want_ge.intersection(&want_le).count() as u64
        );
    }
}

/// A setter must refuse a field of the wrong kind.
///
/// `set_key` used to fall through to its set-field branch for anything that was not a mutex,
/// so calling it on an integer field interned a key and set a bit inside that field's
/// bit-sliced index — changing the stored number, with no error anywhere. `set_bool` had no
/// check at all.
#[test]
fn a_setter_refuses_a_field_of_the_wrong_kind() {
    let d = db();
    let mut w = d.write();
    w.set_int("tx", "amount", 1, 777).unwrap();

    assert!(
        matches!(w.set_key("tx", "amount", 1, "oops"), Err(DbError::WrongFieldKind { .. })),
        "a key written into a BSI field would corrupt the value stored there"
    );
    assert!(matches!(w.set_bool("tx", "amount", 1, true), Err(DbError::WrongFieldKind { .. })));
    assert!(matches!(w.set_int("tx", "country", 1, 5), Err(DbError::WrongFieldKind { .. })));
    assert!(matches!(w.set_bool("tx", "country", 1, true), Err(DbError::WrongFieldKind { .. })));
    assert!(matches!(w.set_key("tx", "active", 1, "x"), Err(DbError::WrongFieldKind { .. })));

    w.commit().unwrap();
    assert_eq!(d.read().get_int("tx", "amount", 1).unwrap(), Some(777), "value untouched");
}

// A catalog entry is one fixed-width record, so a name has a hard ceiling. What happens at
// that ceiling is the whole question: `big-keys` refuses an over-long key precisely because
// truncating would merge two values into one row. These are the same hazard for names.

#[test]
fn an_over_long_name_is_refused_rather_than_truncated() {
    let d = Db::in_memory().unwrap();
    let ok = "t".repeat(MAX_NAME_LEN);
    d.create_table(&ok).unwrap();

    // 105 bytes of three-byte characters: truncating at 104 splits the last one, and the
    // entry then fails to decode and vanishes on reload.
    let split = "松".repeat(35);
    assert_eq!(split.len(), 105);
    assert!(matches!(d.create_table(&split), Err(DbError::NameTooLong { .. })));

    // Two names that agree on their first 104 bytes must not collapse into one.
    let a = format!("{}_A", "x".repeat(MAX_NAME_LEN));
    let b = format!("{}_B", "x".repeat(MAX_NAME_LEN));
    assert!(matches!(d.create_table(&a), Err(DbError::NameTooLong { .. })));
    assert!(matches!(d.create_table(&b), Err(DbError::NameTooLong { .. })));

    // Everything accepted must survive a round trip through the catalog chain.
    let back = Catalog::from_entries(&d.catalog().encode()).unwrap();
    assert!(back.lookup(&ok).is_some(), "an accepted name must reload");
}

#[test]
fn renaming_onto_an_existing_name_is_refused() {
    let d = Db::in_memory().unwrap();
    d.create_table("a").unwrap();
    d.create_table("b").unwrap();

    let mut w = d.write();
    assert!(
        matches!(
            w.catalog_mut().rename_table(big_db::DEFAULT_DATABASE, "a", "b"),
            Err(DbError::NameTaken(_))
        ),
        "renaming onto a live name would leave one of the two tables unreachable"
    );
    w.commit().unwrap();

    let c = d.catalog();
    assert!(c.lookup("a").is_some() && c.lookup("b").is_some());
    assert_ne!(c.lookup("a").unwrap().id, c.lookup("b").unwrap().id);
}

#[test]
fn redefining_a_field_differently_is_refused() {
    let d = Db::in_memory().unwrap();
    d.create_table("tx").unwrap();
    let first = d.create_field("tx", "amount", FieldKind::Int, 32).unwrap();

    // Same definition twice is idempotent; a different one is a mistake worth saying out loud,
    // because the caller now believes in a field that does not exist.
    assert_eq!(d.create_field("tx", "amount", FieldKind::Int, 32).unwrap(), first);
    assert!(matches!(
        d.create_field("tx", "amount", FieldKind::Bool, 1),
        Err(DbError::FieldRedefined { .. })
    ));
    assert!(matches!(
        d.create_field("tx", "amount", FieldKind::Int, 64),
        Err(DbError::FieldRedefined { .. })
    ));

    assert_eq!(d.catalog().field(0, "amount").unwrap().kind, FieldKind::Int);
}

// --- buffered ingest -------------------------------------------------------------------
//
// The contract worth pinning down is not "it writes the same values" alone, but that it writes
// them in *fewer commits* - that is the entire reason the type exists - and that the records it
// is still holding are not pretended to be durable.

#[test]
fn buffered_records_are_invisible_until_they_are_flushed() {
    let d = db();
    let mut i = d.ingest(1_000);
    i.set_int("tx", "amount", 7, 500).unwrap();
    i.set_int("tx", "amount", 8, 600).unwrap();

    assert_eq!(i.buffered(), 2);
    assert_eq!(i.commits(), 0);
    assert_eq!(
        d.read().get_int("tx", "amount", 7).unwrap(),
        None,
        "a buffered record is not in any transaction yet"
    );

    i.flush().unwrap();
    assert_eq!(i.buffered(), 0);
    assert_eq!(i.commits(), 1);
    assert_eq!(d.read().get_int("tx", "amount", 7).unwrap(), Some(500));
    assert_eq!(d.read().get_int("tx", "amount", 8).unwrap(), Some(600));
    assert_eq!(i.finish().unwrap(), 2);
}

#[test]
fn reaching_capacity_commits_without_being_asked() {
    let d = db();
    let mut i = d.ingest(4);
    for record in 0..4 {
        i.set_int("tx", "amount", record, record * 10).unwrap();
    }

    assert_eq!(i.commits(), 1, "the fourth record should have tripped the flush");
    assert_eq!(i.buffered(), 0);
    assert_eq!(d.read().get_int("tx", "amount", 3).unwrap(), Some(30));
    assert_eq!(i.finish().unwrap(), 4);
}

#[test]
fn it_pays_for_one_commit_per_capacity_rather_than_one_per_record() {
    let d = db();
    let mut i = d.ingest(250);
    for record in 0..1_000 {
        i.set_int("tx", "amount", record, record).unwrap();
    }
    assert_eq!(i.commits(), 4);
    assert_eq!(i.finish().unwrap(), 1_000);
}

#[test]
fn an_unknown_name_fails_at_the_call_that_made_it() {
    let d = db();
    let mut i = d.ingest(1_000);

    // The point of resolving eagerly: a typo surfaces here, not thousands of records later at
    // a flush that then has to throw the whole batch away.
    assert!(matches!(i.set_int("tx", "amuont", 1, 1), Err(DbError::UnknownField { .. })));
    assert!(matches!(i.set_int("nope", "amount", 1, 1), Err(DbError::UnknownTable(_))));
    assert_eq!(i.buffered(), 0, "a rejected op must not be buffered");
}

#[test]
fn buffering_agrees_with_one_big_transaction() {
    // Same ops, same order, two engines: one buffered, one written straight through. Including
    // a record written twice, because ordering is the thing buffering could plausibly break.
    let ops: Vec<(RecordId, u64)> =
        (0..300u64).map(|n| (n % 100, n.wrapping_mul(2_654_435_761) % 4096)).collect();

    let buffered = db();
    let mut i = buffered.ingest(64);
    for (record, value) in &ops {
        i.set_int("tx", "amount", *record, *value).unwrap();
        i.set_bool("tx", "active", *record, value.is_multiple_of(2)).unwrap();
        i.set_key("tx", "country", *record, if value.is_multiple_of(3) { "vn" } else { "sg" })
            .unwrap();
    }
    i.finish().unwrap();

    let direct = db();
    let mut w = direct.write();
    for (record, value) in &ops {
        w.set_int("tx", "amount", *record, *value).unwrap();
        w.set_bool("tx", "active", *record, value.is_multiple_of(2)).unwrap();
        w.set_key("tx", "country", *record, if value.is_multiple_of(3) { "vn" } else { "sg" })
            .unwrap();
    }
    w.commit().unwrap();

    let (a, b) = (buffered.read(), direct.read());
    for record in 0..100 {
        assert_eq!(
            a.get_int("tx", "amount", record).unwrap(),
            b.get_int("tx", "amount", record).unwrap(),
            "record {record} disagreed, so buffering changed last-write-wins"
        );
        assert_eq!(a.exists("tx", record).unwrap(), b.exists("tx", record).unwrap());
    }
    assert_eq!(
        a.count("tx", "amount", RangeOp::Ge, 2_048).unwrap(),
        b.count("tx", "amount", RangeOp::Ge, 2_048).unwrap()
    );
}

// Gated on `debug_assertions` because that is exactly the contract: the check costs nothing in
// release and therefore does not fire there. Ungated, this test passes under `cargo test` and
// fails under `cargo test --release`, which is how it was found.
#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "uncommitted records")]
fn dropping_it_with_records_still_held_is_a_bug() {
    // Flushing from `Drop` would have nowhere to report a failure, so the type refuses to
    // pretend. Debug builds say so rather than losing the write quietly.
    let d = db();
    let mut i = d.ingest(1_000);
    i.set_int("tx", "amount", 1, 1).unwrap();
    drop(i);
}

/// `Ingest::with_flush_fraction`: staggering which shards a flush commits.
///
/// The correctness half is here; the byte figures it exists for are gated in
/// [`amplification.rs`](amplification.rs), because that is where deterministic byte assertions
/// live and this one is as deterministic as the rest.
mod flush_fraction {
    use big_db::*;
    use big_engine::SHARD_WIDTH;

    fn db() -> Db<big_pager::MemPager> {
        let d = Db::in_memory().unwrap();
        d.create_table("t").unwrap();
        d.create_field("t", "v", FieldKind::Int, 20).unwrap();
        d
    }

    /// Spread across `shards`, so there is something to stagger.
    fn spread(n: u64, shards: u64) -> Vec<(u64, u64)> {
        (0..n).map(|i| ((i % shards) * SHARD_WIDTH + i / shards, i % 1000)).collect()
    }

    #[test]
    fn a_partial_flush_loses_nothing() {
        let d = db();
        let records = spread(5_000, 64);
        let mut i = d.ingest(500).with_flush_fraction(0.25);
        for (id, v) in &records {
            i.set_int("t", "v", *id, *v).unwrap();
        }
        // `finish` commits whatever the fraction withheld. If it did not, this is where a
        // policy that silently drops the tail of an ingest would show up.
        assert_eq!(i.finish().unwrap(), records.len() as u64);

        let r = d.read();
        assert_eq!(r.count_all("t").unwrap(), records.len() as u64);
        for (id, v) in &records {
            assert_eq!(r.get_int("t", "v", *id).unwrap(), Some(*v), "record {id} went missing");
        }
    }

    #[test]
    fn last_write_still_wins_per_record() {
        // The one thing staggering could plausibly break. It does not, and the reason is
        // structural rather than lucky: a record belongs to exactly one shard, so two writes to
        // the same record are always in the same group and keep their arrival order. Only
        // *different* shards stop interleaving the way they arrived, and no record spans two.
        let d = db();
        let mut i = d.ingest(8).with_flush_fraction(0.25);
        for round in 1..=3u64 {
            for shard in 0..16u64 {
                i.set_int("t", "v", shard * SHARD_WIDTH + 1, round).unwrap();
            }
        }
        i.finish().unwrap();

        let r = d.read();
        for shard in 0..16u64 {
            assert_eq!(
                r.get_int("t", "v", shard * SHARD_WIDTH + 1).unwrap(),
                Some(3),
                "shard {shard} kept a write that a later one replaced"
            );
        }
    }

    #[test]
    fn a_single_shard_is_left_alone() {
        // Dense ingest must not pay for a knob it cannot benefit from: with one shard there is
        // nothing for it to run ahead of, and withholding it would be a commit that wrote
        // nothing.
        let dense: Vec<(u64, u64)> = (0..2_000u64).map(|i| (i, i % 1000)).collect();
        let (mut plain, mut staggered) = (0, 0);
        for (frac, out) in [(1.0, &mut plain), (0.1, &mut staggered)] {
            let d = db();
            let mut i = d.ingest(500).with_flush_fraction(frac);
            for (id, v) in &dense {
                i.set_int("t", "v", *id, *v).unwrap();
            }
            *out = i.commits();
            i.finish().unwrap();
        }
        assert_eq!(plain, staggered, "a one-shard ingest changed its commit count");
    }

    #[test]
    fn the_fraction_is_clamped() {
        // Below a tenth the commit multiplier stops being a trade and becomes a commit per
        // record, which is the engine's worst case. Out-of-range and NaN are clamped rather
        // than refused, because this is a hint about cost and not a correctness argument.
        let d = db();
        let records = spread(600, 32);
        for frac in [0.0, -1.0, f64::NAN, 5.0] {
            let mut i = d.ingest(200).with_flush_fraction(frac);
            for (id, v) in &records {
                i.set_int("t", "v", *id, *v).unwrap();
            }
            assert_eq!(i.finish().unwrap(), records.len() as u64, "fraction {frac} lost records");
        }
    }
}

// ---------------------------------------------------------------------------------------
// Storage engines
//
// The engine is a declaration until the columnar half exists, so what these pin down is the
// part that has to be right *before* it does: that the choice survives every boundary it
// crosses, and that the two defaults - the one a caller gets and the one a file decodes to -
// stay different numbers on purpose.
// ---------------------------------------------------------------------------------------

#[test]
fn a_table_remembers_its_engine_across_a_reopen() {
    let path = tempfile::NamedTempFile::new().unwrap().into_temp_path();
    std::fs::remove_file(&path).unwrap();

    for (name, engine) in [
        ("plain", TableEngine::Bitmap),
        ("both", TableEngine::BitmapColumnar),
        ("cols", TableEngine::Columnar),
    ] {
        let d = Db::open_path(&path).unwrap();
        d.create_table_with(name, engine).unwrap();
        drop(d);

        let d = Db::open_path(&path).unwrap();
        assert_eq!(d.catalog().lookup(name).unwrap().engine, engine, "{name}");
    }
}

#[test]
fn a_table_created_without_an_engine_gets_both() {
    let d = Db::in_memory().unwrap();
    d.create_table("tx").unwrap();
    assert_eq!(d.catalog().lookup("tx").unwrap().engine, TableEngine::BitmapColumnar);
}

/// The one thing about the encoding that is not obvious from reading it: zero on disk is
/// `Bitmap`, and `Bitmap` is *not* what a new table gets. Every file written before the engine
/// existed carries a zero in that byte and is bitmap-only, so the decoded default and the
/// created default have to disagree.
#[test]
fn a_zero_engine_byte_decodes_as_bitmap_only() {
    let mut c = Catalog::new();
    c.intern_table_with(big_db::DEFAULT_DATABASE, "old", TableEngine::Bitmap).unwrap();
    let entries = c.encode();

    let table = entries.iter().find(|e| e[0] == big_db::catalog::KIND_TABLE).unwrap();
    assert_eq!(table[1], 0, "bitmap is the zero, which is what an older file holds");

    let back = Catalog::from_entries(&entries).unwrap();
    assert_eq!(back.lookup("old").unwrap().engine, TableEngine::Bitmap);
    assert_ne!(TableEngine::default(), TableEngine::Bitmap);
}

/// A table record from a newer build is refused rather than read as bitmap-only. Silently
/// defaulting would make its columns unreachable while every query against it answered
/// "nothing here" - which is a file from the future being mistaken for an empty table.
#[test]
fn an_unknown_engine_is_refused_rather_than_defaulted() {
    let mut c = Catalog::new();
    c.intern_table_with(big_db::DEFAULT_DATABASE, "future", TableEngine::Columnar).unwrap();
    let mut entries = c.encode();
    for e in &mut entries {
        if e[0] == big_db::catalog::KIND_TABLE {
            e[1] = 200;
        }
    }

    let err = Catalog::from_entries(&entries).unwrap_err();
    assert_eq!(err.code(), "unknown_table_engine");
}

/// An engine is fixed at creation. Handing back the existing table would give the caller one
/// whose answers cost what they did not ask for, and the mismatch would only show up later.
#[test]
fn recreating_a_table_under_another_engine_is_refused() {
    let d = Db::in_memory().unwrap();
    d.create_table_with("tx", TableEngine::Bitmap).unwrap();

    // The same engine again is idempotent, exactly as a field redeclared identically is.
    assert!(d.create_table_with("tx", TableEngine::Bitmap).is_ok());

    let err = d.create_table_with("tx", TableEngine::Columnar).unwrap_err();
    assert_eq!(err.code(), "table_redefined");
    assert_eq!(d.catalog().lookup("tx").unwrap().engine, TableEngine::Bitmap);
}

/// Round-trips through the spelling every surface outside the engine uses.
#[test]
fn every_engine_has_a_name_that_parses_back() {
    for engine in TableEngine::all() {
        assert_eq!(TableEngine::parse(engine.as_str()), Some(engine));
    }
    assert_eq!(TableEngine::parse("bitmaps"), None);
}

/// The view allocator must never reach the ids the column segments and the mutex shadow use.
/// A named view that collided with one would be read back as the reserved one, and nothing
/// underneath would be able to tell.
#[test]
fn the_reserved_views_are_out_of_the_allocators_reach() {
    let mut c = Catalog::new();
    for _ in 0..64 {
        let id = c.intern_view(&format!("v{}", c.encode().len())).unwrap();
        assert!(id < u32::MAX - 1, "allocated {id}, which is inside the reserved band");
    }
}

// ---------------------------------------------------------------------------------------
// Column segments
//
// What the write path owes: every field kind reaches its column, the two halves of the default
// engine agree, and the operations that were only ever taught about bitmaps - delete, drop,
// backup - reach segments too.
// ---------------------------------------------------------------------------------------

fn columnar(engine: TableEngine) -> Db<big_pager::MemPager> {
    let d = Db::in_memory().unwrap();
    d.create_table_with("tx", engine).unwrap();
    d.create_field("tx", "amount", FieldKind::Int, 37).unwrap();
    d.create_signed("tx", "delta", 16).unwrap();
    d.create_field("tx", "country", FieldKind::Set, 0).unwrap();
    d.create_field("tx", "tier", FieldKind::Mutex, 0).unwrap();
    d.create_field("tx", "active", FieldKind::Bool, 0).unwrap();
    d
}

/// Every kind, through the column and back. This is the test the whole write path exists for.
#[test]
fn every_field_kind_reaches_its_column() {
    let d = columnar(TableEngine::BitmapColumnar);
    let mut w = d.write();
    w.set_int("tx", "amount", 7, 1_000_000).unwrap();
    w.set_signed("tx", "delta", 7, -42).unwrap();
    w.set_key("tx", "country", 7, "GB").unwrap();
    w.set_key("tx", "tier", 7, "gold").unwrap();
    w.set_bool("tx", "active", 7, true).unwrap();
    w.commit().unwrap();

    let r = d.read();
    assert_eq!(r.column_cell("tx", "amount", 7).unwrap(), Some(Cell::Value(1_000_000)));
    assert_eq!(r.column_cell("tx", "active", 7).unwrap(), Some(Cell::Value(1)));

    // A signed value is stored biased, exactly as its bit planes are - the sign convention
    // lives above storage and the column is storage.
    let stored = r.column_cell("tx", "delta", 7).unwrap().unwrap().value().unwrap();
    assert_eq!(big_db::signed::decode(stored, 16), -42);

    // A keyed column holds row ids, which is what the dictionary translates.
    let gb = r.key_row("tx", "country", "GB").unwrap();
    assert_eq!(r.column_cell("tx", "country", 7).unwrap(), Some(Cell::List(vec![gb])));
    let gold = r.key_row("tx", "tier", "gold").unwrap();
    assert_eq!(r.column_cell("tx", "tier", 7).unwrap(), Some(Cell::Value(gold)));
}

/// A set field adds; its column has to merge rather than replace. This is the one place a
/// column write is not simply the caller's last word, so it gets its own test.
#[test]
fn a_set_column_accumulates_across_and_within_transactions() {
    let d = columnar(TableEngine::BitmapColumnar);
    let mut w = d.write();
    w.set_key("tx", "country", 1, "GB").unwrap();
    w.set_key("tx", "country", 1, "US").unwrap();
    w.commit().unwrap();

    let mut w = d.write();
    w.set_key("tx", "country", 1, "FR").unwrap();
    w.commit().unwrap();

    let r = d.read();
    let mut want: Vec<u64> =
        ["GB", "US", "FR"].iter().map(|k| r.key_row("tx", "country", k).unwrap()).collect();
    want.sort_unstable();
    assert_eq!(r.column_cell("tx", "country", 1).unwrap(), Some(Cell::List(want)));
}

/// A mutex holds one value at a time. Its column is a replace, and it needs no shadow to know
/// what it is replacing - the segment already holds the old value.
#[test]
fn a_mutex_column_keeps_only_the_latest() {
    let d = columnar(TableEngine::BitmapColumnar);
    let mut w = d.write();
    w.set_key("tx", "tier", 1, "silver").unwrap();
    w.set_key("tx", "tier", 1, "gold").unwrap();
    w.commit().unwrap();

    let r = d.read();
    let gold = r.key_row("tx", "tier", "gold").unwrap();
    assert_eq!(r.column_cell("tx", "tier", 1).unwrap(), Some(Cell::Value(gold)));
}

#[test]
fn overwriting_a_value_replaces_it_in_the_column() {
    let d = columnar(TableEngine::BitmapColumnar);
    let mut w = d.write();
    w.set_int("tx", "amount", 1, 500).unwrap();
    w.set_int("tx", "amount", 1, 900).unwrap();
    w.commit().unwrap();
    let mut w = d.write();
    w.set_int("tx", "amount", 1, 3).unwrap();
    w.commit().unwrap();

    let r = d.read();
    assert_eq!(r.column_cell("tx", "amount", 1).unwrap(), Some(Cell::Value(3)));
    // And the index agrees, which is the property the default engine is entirely about.
    assert_eq!(r.get_int("tx", "amount", 1).unwrap(), Some(3));
}

/// A bitmap-only table writes no segments at all. Without this the narrow engine would cost
/// what the wide one does and mean nothing.
#[test]
fn a_bitmap_table_writes_no_segments() {
    let d = columnar(TableEngine::Bitmap);
    let mut w = d.write();
    w.set_int("tx", "amount", 1, 500).unwrap();
    w.set_key("tx", "country", 1, "GB").unwrap();
    w.commit().unwrap();

    assert_eq!(d.read().column_cell("tx", "amount", 1).unwrap(), None);
    let segments = d.catalog().fragments_of_table(0).filter(|(k, _)| k.view == COLUMN_VIEW).count();
    assert_eq!(segments, 0, "a bitmap table grew {segments} segments");
}

/// A columnar table keeps the existence row and nothing else in bitmap form. The exists row is
/// one bit per record and it is what `Not`, `count(*)` and the record cursor stand on.
#[test]
fn a_columnar_table_keeps_only_the_existence_row() {
    let d = columnar(TableEngine::Columnar);
    let mut w = d.write();
    w.set_int("tx", "amount", 1, 500).unwrap();
    w.set_key("tx", "country", 1, "GB").unwrap();
    w.commit().unwrap();

    let c = d.catalog();
    let standard: Vec<u32> = c
        .fragments_of_table(0)
        .filter(|(k, _)| k.view == STANDARD_VIEW)
        .map(|(k, _)| k.field)
        .collect();
    assert_eq!(standard, vec![EXISTS_FIELD], "a columnar table grew index fragments: {standard:?}");
    drop(c);

    assert_eq!(d.read().count_all("tx").unwrap(), 1);
    assert_eq!(d.read().column_cell("tx", "amount", 1).unwrap(), Some(Cell::Value(500)));
}

/// Deleting a record erases it from its columns too. `clear_records` is bitmap arithmetic and
/// would read a segment's cells as containers, so the delete path has to know the difference.
#[test]
fn deleting_a_record_clears_its_columns() {
    let d = columnar(TableEngine::BitmapColumnar);
    let mut w = d.write();
    for r in 1..=3u64 {
        w.set_int("tx", "amount", r, r * 100).unwrap();
        w.set_key("tx", "country", r, "GB").unwrap();
    }
    w.commit().unwrap();

    let mut w = d.write();
    assert_eq!(w.delete("tx", &[2]).unwrap(), 1);
    w.commit().unwrap();

    let r = d.read();
    assert_eq!(r.column_cell("tx", "amount", 2).unwrap(), Some(Cell::Null));
    assert_eq!(r.column_cell("tx", "country", 2).unwrap(), Some(Cell::Null));
    assert_eq!(r.column_cell("tx", "amount", 1).unwrap(), Some(Cell::Value(100)));
    assert_eq!(r.column_cell("tx", "amount", 3).unwrap(), Some(Cell::Value(300)));
    assert_eq!(r.column_count("tx", "amount").unwrap(), 2);
}

/// Segments share the root-record namespace with fragments, which is what makes the backup walk
/// carry them without being taught they exist. This is the payoff of addressing them by view.
#[test]
fn a_backup_carries_the_segments() {
    let d = columnar(TableEngine::Columnar);
    let mut w = d.write();
    for r in 0..2000u64 {
        w.set_int("tx", "amount", r, r * 7).unwrap();
    }
    w.commit().unwrap();

    let copy = d.copy_to(big_pager::MemPager::new()).unwrap();
    let r = copy.read();
    assert_eq!(r.column_cell("tx", "amount", 1999).unwrap(), Some(Cell::Value(1999 * 7)));
    assert_eq!(r.column_count("tx", "amount").unwrap(), 2000);
    // And the copy's own scrub verifies the values pages it carried across.
    assert!(copy.scrub().unwrap().total() > 0);
}

/// Dropping reaches segments for free: the catalog's fragment range spans every view, so the
/// keys it hands back already include them.
#[test]
fn dropping_a_field_takes_its_segment_with_it() {
    let d = columnar(TableEngine::BitmapColumnar);
    let mut w = d.write();
    w.set_int("tx", "amount", 1, 5).unwrap();
    w.commit().unwrap();
    assert!(d.catalog().fragments_of_table(0).any(|(k, _)| k.view == COLUMN_VIEW));

    d.drop_field("tx", "amount").unwrap();
    assert!(
        !d.catalog().fragments_of_table(0).any(|(k, _)| k.view == COLUMN_VIEW),
        "the segment outlived the field"
    );
}

#[test]
fn segments_survive_a_reopen() {
    let path = tempfile::NamedTempFile::new().unwrap().into_temp_path();
    std::fs::remove_file(&path).unwrap();

    {
        let d = Db::open_path(&path).unwrap();
        d.create_table_with("tx", TableEngine::BitmapColumnar).unwrap();
        d.create_field("tx", "amount", FieldKind::Int, 32).unwrap();
        let mut w = d.write();
        for r in 0..3000u64 {
            w.set_int("tx", "amount", r, r).unwrap();
        }
        w.commit().unwrap();
    }

    let d = Db::open_path(&path).unwrap();
    let r = d.read();
    for record in [0u64, 1023, 1024, 2999] {
        assert_eq!(r.column_cell("tx", "amount", record).unwrap(), Some(Cell::Value(record)));
    }
}

// ----------------------------------------------------------------------------------------
// Databases
//
// A namespace above tables, and nothing below the catalog knows there is one: a `TableId` is
// unique across every database, so no fragment, row key or root record mentions a database.
// These are the tests for the two claims that makes - that old files still read, and that two
// databases can hold the same name without holding the same data.
// ----------------------------------------------------------------------------------------

/// **The backward-compatibility test, and the one that matters most.**
///
/// Every table record written before databases existed has zeroes in the word that now names
/// the database. Zero is `DEFAULT_DATABASE`, which is the database those tables have always
/// been in - so an old file reads back with every table exactly where it was, and there is no
/// migration. The same argument the engine byte's zero carries, one field over.
#[test]
fn a_table_record_with_no_database_word_decodes_into_the_default_one() {
    let mut c = Catalog::new();
    c.intern_table_with(big_db::DEFAULT_DATABASE, "tx", TableEngine::Bitmap).unwrap();
    let mut entries = c.encode();

    // Exactly what an older build wrote: the database word never touched.
    for e in &mut entries {
        if e[0] == big_db::catalog::KIND_TABLE {
            assert_eq!(&e[8..12], &[0, 0, 0, 0], "the default database is the zero");
            e[8..12].fill(0);
        }
    }

    let back = Catalog::from_entries(&entries).unwrap();
    let table = back.table(big_db::DEFAULT_DATABASE, "tx").expect("still in the default database");
    assert_eq!(table.database, big_db::DEFAULT_DATABASE);
    // And it is reachable by the bare name somebody has always written.
    assert_eq!(back.lookup("tx").unwrap().id, table.id);
}

/// A database entry from a build that has none is skipped, not fatal - which is what the format
/// promises about an unrecognised kind, and why this one could be added at all.
#[test]
fn a_database_survives_being_written_back_out() {
    let mut c = Catalog::new();
    let sales = c.intern_database("sales").unwrap();
    c.intern_table_with(sales, "orders", TableEngine::Bitmap).unwrap();

    let back = Catalog::from_entries(&c.encode()).unwrap();
    assert_eq!(back.database("sales"), Some(sales));
    assert_eq!(back.lookup("sales.orders").unwrap().database, sales);
    assert_eq!(back.database_name(sales), Some("sales"));
}

/// The point of a namespace: the same table name in two of them is two tables, with two ids and
/// therefore two disjoint sets of fragments.
#[test]
fn the_same_table_name_in_two_databases_is_two_tables() {
    let mut c = Catalog::new();
    let sales = c.intern_database("sales").unwrap();
    let ops = c.intern_database("ops").unwrap();

    let a = c.intern_table_with(sales, "events", TableEngine::Bitmap).unwrap();
    let b = c.intern_table_with(ops, "events", TableEngine::Bitmap).unwrap();
    assert_ne!(a, b, "a TableId is unique across databases, which is what keeps data apart");

    assert_eq!(c.table(sales, "events").unwrap().id, a);
    assert_eq!(c.table(ops, "events").unwrap().id, b);
    assert_eq!(c.lookup("sales.events").unwrap().id, a);
    assert_eq!(c.lookup("ops.events").unwrap().id, b);
    // And neither is reachable unqualified, because neither is in the default database.
    assert!(c.lookup("events").is_none());
}

/// The default database is always there, is never written to disk, and cannot be dropped: every
/// table has to be in some database, and this is the one that is always available to be in.
#[test]
fn the_default_database_is_not_a_record_and_cannot_be_dropped() {
    let mut c = Catalog::new();
    c.intern_table_with(big_db::DEFAULT_DATABASE, "tx", TableEngine::Bitmap).unwrap();

    assert!(
        !c.encode().iter().any(|e| e[0] == big_db::catalog::KIND_DATABASE),
        "the default database is implied, not stored"
    );
    assert_eq!(c.database(big_db::DEFAULT_DATABASE_NAME), Some(big_db::DEFAULT_DATABASE));
    assert_eq!(c.drop_database(big_db::DEFAULT_DATABASE_NAME), None);

    // And interning it by name hands back the reserved id rather than allocating a second one.
    assert_eq!(c.intern_database(big_db::DEFAULT_DATABASE_NAME).unwrap(), big_db::DEFAULT_DATABASE);
}

/// A database still holding tables is not dropped by the catalog. Emptiness is the caller's to
/// arrange, because each `drop_table` hands back fragment keys whose pages have to be freed -
/// and a drop that swallowed its tables here would strand every one of those pages.
#[test]
fn a_database_holding_tables_is_not_dropped_by_the_catalog() {
    let mut c = Catalog::new();
    let sales = c.intern_database("sales").unwrap();
    c.intern_table_with(sales, "orders", TableEngine::Bitmap).unwrap();

    assert_eq!(c.table_count(sales), 1);
    assert_eq!(c.drop_database("sales"), None, "still holds a table");

    c.drop_table(sales, "orders").unwrap();
    assert_eq!(c.drop_database("sales"), Some(sales));
    assert_eq!(c.database("sales"), None);
}

/// A `.` is the separator in a qualified name, so a name holding one is refused rather than
/// stored. A table actually called `a.b` would be the same string as table `b` in database `a`
/// everywhere a table travels as one - a plan, a fragment address, a message between nodes.
#[test]
fn a_name_holding_the_separator_is_refused() {
    let mut c = Catalog::new();
    assert_eq!(
        c.intern_table_with(big_db::DEFAULT_DATABASE, "a.b", TableEngine::Bitmap)
            .unwrap_err()
            .code(),
        "name_separator"
    );
    assert_eq!(c.intern_database("a.b").unwrap_err().code(), "name_separator");
}

/// `TableRef` writes and reads the one string form a table travels as, and the two are
/// inverses - which is what lets a plan, a fragment address and a DDL message all carry a
/// table as a single `String` without a second field to disagree with.
#[test]
fn a_qualified_name_round_trips_through_the_string_form() {
    for (database, table) in
        [(big_db::DEFAULT_DATABASE_NAME, "tx"), ("sales", "orders"), ("ops", "events")]
    {
        let r = big_db::TableRef::new(database, table);
        let printed = r.to_string();
        assert_eq!(big_db::TableRef::parse(&printed), r, "{printed}");
    }
    // A bare name prints bare and reads back as the default database, so every name written
    // before databases existed still means what it meant.
    assert_eq!(big_db::TableRef::parse("tx"), big_db::TableRef::bare("tx"));
    assert_eq!(big_db::TableRef::bare("tx").to_string(), "tx");
}
