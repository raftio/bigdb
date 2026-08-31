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

use big_fragment::*;
use big_page::Pgno;
use big_pager::{MemPager, Store};
use proptest::prelude::*;
use std::collections::BTreeSet;

fn store() -> Store<MemPager> {
    Store::init(MemPager::new()).unwrap()
}

/// Writes a batch of facts and returns the new fragment root.
fn write(s: &Store<MemPager>, root: Option<Pgno>, bits: &[(RowId, RecordId)]) -> Pgno {
    let mut w = s.begin_write();
    let mut f = FragmentWrite::new(root, 0);
    f.set_bits(&mut w, bits.iter().copied()).unwrap();
    let root = f.root().unwrap();
    w.commit().unwrap();
    root
}

fn reader<'a>(s: &'a Store<MemPager>, root: Pgno) -> FragmentRead<'a, MemPager> {
    FragmentRead::new(s.pager(), root, 0)
}

#[test]
fn a_row_never_straddles_a_container_boundary() {
    // The invariant everything about contiguous row scans rests on.
    for row in [0u64, 1, 7, 1000, 65535] {
        let span = row_ckeys(row);
        for rec in [0u64, 1, 999, SHARD_WIDTH - 1] {
            let ckey = ckey_of(pos_of(row, rec));
            assert!(span.contains(&ckey), "row {row} record {rec} escaped its span");
        }
        assert_eq!(span.end() - span.start() + 1, CONTAINERS_PER_ROW);
    }
}

#[test]
fn set_then_read_back_a_single_fact() {
    let s = store();
    let root = write(&s, None, &[(3, 12345)]);
    let r = reader(&s, root);

    assert!(r.get(3, 12345).unwrap());
    assert!(!r.get(3, 12346).unwrap());
    assert!(!r.get(4, 12345).unwrap());
    assert_eq!(r.row_count(3).unwrap(), 1);
    assert_eq!(r.count().unwrap(), 1);
}

#[test]
fn a_row_spans_every_container_it_needs() {
    let s = store();
    // One record in each of the row's 16 containers.
    let recs: Vec<u64> = (0..CONTAINERS_PER_ROW).map(|i| i * CONTAINER_WIDTH + 5).collect();
    let bits: Vec<(u64, u64)> = recs.iter().map(|r| (2u64, *r)).collect();
    let root = write(&s, None, &bits);

    let set = reader(&s, root).row(2).unwrap();
    assert_eq!(set.len() as u64, CONTAINERS_PER_ROW, "all 16 containers must be present");
    assert_eq!(set.cardinality(), CONTAINERS_PER_ROW);
    assert_eq!(set.records(0).collect::<Vec<_>>(), recs);
}

#[test]
fn rows_are_independent() {
    let s = store();
    let mut root = write(&s, None, &[(0, 1), (0, 2), (0, 3)]);
    root = write(&s, Some(root), &[(1, 2), (1, 4)]);
    let r = reader(&s, root);

    assert_eq!(r.row(0).unwrap().records(0).collect::<Vec<_>>(), vec![1, 2, 3]);
    assert_eq!(r.row(1).unwrap().records(0).collect::<Vec<_>>(), vec![2, 4]);
    assert_eq!(r.rows().unwrap(), vec![0, 1]);
    assert_eq!(r.count().unwrap(), 5);
}

#[test]
fn clearing_removes_only_what_was_named() {
    let s = store();
    let root = write(&s, None, &[(0, 1), (0, 2), (0, 3)]);

    let mut w = s.begin_write();
    let mut f = FragmentWrite::new(Some(root), 0);
    f.clear_bits(&mut w, [(0u64, 2u64)]).unwrap();
    let root = f.root().unwrap();
    w.commit().unwrap();

    assert_eq!(reader(&s, root).row(0).unwrap().records(0).collect::<Vec<_>>(), vec![1, 3]);
}

#[test]
fn clearing_a_whole_container_drops_it() {
    let s = store();
    let root = write(&s, None, &[(0, 1), (0, 2)]);

    let mut w = s.begin_write();
    let mut f = FragmentWrite::new(Some(root), 0);
    f.clear_bits(&mut w, [(0u64, 1u64)]).unwrap();
    let root = f.root().unwrap();
    w.commit().unwrap();

    let r = reader(&s, root);
    assert_eq!(r.count().unwrap(), 1);
    assert!(!r.row(0).unwrap().is_empty());
}

#[test]
fn a_fragment_cleared_to_nothing_has_no_root_at_all() {
    // The tree keeps a root page even when its last container goes, so a fragment emptied by
    // deletes would otherwise hold one live page and one live root record for ever - invisible
    // to every query and never reclaimed. An empty fragment must be indistinguishable from one
    // that was never written.
    let s = store();
    let root = write(&s, None, &[(0, 1), (3, 90_000), (7, 500_000)]);

    let mut w = s.begin_write();
    let mut f = FragmentWrite::new(Some(root), 0);
    f.clear_bits(&mut w, [(0u64, 1u64), (3, 90_000), (7, 500_000)]).unwrap();
    assert_eq!(f.root(), None, "the last container going takes the tree with it");
    w.commit().unwrap();
}

#[test]
fn emptying_a_fragment_returns_its_pages() {
    let s = store();
    // Wide enough to span several leaves and a branch, so this is not one page going back.
    let bits: Vec<(RowId, RecordId)> =
        (0..400u64).flat_map(|row| (0..40u64).map(move |r| (row, r * 3000))).collect();
    let root = write(&s, None, &bits);
    let live_before = s.metrics().page_count - s.metrics().free_pages_reusable;

    let mut w = s.begin_write();
    let mut f = FragmentWrite::new(Some(root), 0);
    f.clear_bits(&mut w, bits.iter().copied()).unwrap();
    assert_eq!(f.root(), None);
    w.commit().unwrap();

    let live_after = s.metrics().page_count - s.metrics().free_pages_reusable;
    assert!(
        live_after < live_before,
        "clearing everything must hand pages back: {live_after} is not below {live_before}"
    );
}

#[test]
fn clearing_bits_that_were_never_set_leaves_no_root_behind() {
    let s = store();
    let mut w = s.begin_write();
    let mut f = FragmentWrite::new(None, 0);
    f.clear_bits(&mut w, [(0u64, 1u64)]).unwrap();
    assert_eq!(f.root(), None, "clearing nothing must not conjure a tree");
    w.commit().unwrap();
}

#[test]
fn write_row_replaces_rather_than_merges() {
    let s = store();
    let root = write(&s, None, &[(5, 1), (5, 2), (5, 3)]);

    let replacement = {
        let mut w = s.begin_write();
        let mut f = FragmentWrite::new(Some(root), 0);
        let mut set = RowSet::new();
        let pos = pos_of(5, 99);
        set.insert(
            slot_of_ckey(ckey_of(pos)),
            big_container::Container::from_values([offset_in_container(pos)]),
        );
        f.write_row(&mut w, 5, &set).unwrap();
        let r = f.root().unwrap();
        w.commit().unwrap();
        r
    };

    let got = reader(&s, replacement).row(5).unwrap();
    assert_eq!(got.records(0).collect::<Vec<_>>(), vec![99], "the old bits must be gone");
}

#[test]
fn rowset_combines_container_by_container() {
    let s = store();
    let root = write(&s, None, &[(0, 1), (0, 2), (0, 70000), (1, 2), (1, 3), (1, 70000)]);
    let r = reader(&s, root);
    let (a, b) = (r.row(0).unwrap(), r.row(1).unwrap());

    assert_eq!(a.and(&b).records(0).collect::<Vec<_>>(), vec![2, 70000]);
    assert_eq!(a.or(&b).records(0).collect::<Vec<_>>(), vec![1, 2, 3, 70000]);
    assert_eq!(a.andnot(&b).records(0).collect::<Vec<_>>(), vec![1]);
    assert_eq!(a.xor(&b).records(0).collect::<Vec<_>>(), vec![1, 3]);
}

#[test]
fn record_ids_survive_the_coordinate_round_trip_across_shards() {
    for shard in [0u64, 1, 381] {
        for local in [0u64, 1, 65535, 65536, SHARD_WIDTH - 1] {
            let rec = shard * SHARD_WIDTH + local;
            assert_eq!(shard_of(rec), shard);
            let pos = pos_of(7, rec);
            assert_eq!(record_of(shard, ckey_of(pos), offset_in_container(pos)), rec);
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(150))]

    #[test]
    fn coordinates_round_trip(row in 0u64..4096, rec in any::<u64>()) {
        let shard = shard_of(rec);
        let pos = pos_of(row, rec);
        let ckey = ckey_of(pos);

        prop_assert!(row_ckeys(row).contains(&ckey));
        prop_assert_eq!(row_of_ckey(ckey), row);
        prop_assert_eq!(record_of(shard, ckey, offset_in_container(pos)), rec);
    }

    /// A fragment must behave exactly like the set of facts written into it.
    #[test]
    fn fragment_tracks_the_facts_written_into_it(
        facts in proptest::collection::btree_set((0u64..40, 0u64..(SHARD_WIDTH - 1)), 0..150)
    ) {
        let s = store();
        let bits: Vec<(u64, u64)> = facts.iter().copied().collect();
        if bits.is_empty() { return Ok(()); }
        let root = write(&s, None, &bits);
        let r = reader(&s, root);

        prop_assert_eq!(r.count().unwrap(), facts.len() as u64);

        let rows: BTreeSet<u64> = facts.iter().map(|(row, _)| *row).collect();
        prop_assert_eq!(r.rows().unwrap(), rows.iter().copied().collect::<Vec<_>>());

        for row in &rows {
            let want: Vec<u64> =
                facts.iter().filter(|(r2, _)| r2 == row).map(|(_, rec)| *rec).collect();
            prop_assert_eq!(r.row(*row).unwrap().records(0).collect::<Vec<_>>(), want.clone());
            prop_assert_eq!(r.row_count(*row).unwrap(), want.len() as u64);
        }
    }

    /// Writing in several batches must end up the same as writing in one.
    #[test]
    fn batching_does_not_change_the_result(
        facts in proptest::collection::btree_set((0u64..20, 0u64..300000), 1..80),
        split in 1usize..40,
    ) {
        let bits: Vec<(u64, u64)> = facts.iter().copied().collect();

        let one = store();
        let root_one = write(&one, None, &bits);

        let many = store();
        let mut root_many = None;
        for chunk in bits.chunks(split.max(1)) {
            root_many = Some(write(&many, root_many, chunk));
        }
        let root_many = root_many.unwrap();

        prop_assert_eq!(
            reader(&one, root_one).count().unwrap(),
            reader(&many, root_many).count().unwrap()
        );
        for (row, rec) in &bits {
            prop_assert!(reader(&many, root_many).get(*row, *rec).unwrap());
        }
    }
}

proptest! {
    /// The batched probe must be indistinguishable from probing each row on its own. It exists
    /// only to save descents, so any difference in answer is a bug and not a trade.
    #[test]
    fn get_many_matches_get_row_by_row(
        bits in proptest::collection::vec((0u64..40, 0u64..200_000), 1..120),
        probe in 0u64..200_000,
    ) {
        let s = store();
        let root = write(&s, None, &bits);
        let r = reader(&s, root);

        // Rows nothing was ever written to are included on purpose: a missing row must come
        // back false rather than being dropped from the result.
        let rows: Vec<RowId> = (0..48).collect();
        let batched = r.get_many(rows.iter().copied(), probe).unwrap();

        prop_assert_eq!(batched.len(), rows.len(), "every row asked for must be answered");
        for row in rows {
            prop_assert_eq!(
                batched.get(&row).copied(),
                Some(r.get(row, probe).unwrap()),
                "row {} disagreed", row
            );
        }
    }
}
