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

//! Iteration: listing a table, and paging any result.
//!
//! Two claims to keep honest, and they fail differently.
//!
//! The first is correctness: walking a table page by page must produce every record exactly
//! once, in order, whatever the page size and however the ids are spread. That one is a
//! property test.
//!
//! The second is that it is a *cursor* and not a filter. A cursor resumes; a filter walks
//! everything it has already returned in order to skip it, which makes a full scan cost the
//! square of the table. Nothing about the answers distinguishes the two, so it is asserted in
//! pages read rather than in ids returned.

use big_db::*;
use big_fragment::SHARD_WIDTH;
use proptest::prelude::*;

fn db() -> Db<big_pager::MemPager> {
    let d = Db::in_memory().unwrap();
    d.create_table("tx").unwrap();
    d.create_field("tx", "amount", FieldKind::Int, 37).unwrap();
    d
}

fn seed(d: &Db<big_pager::MemPager>, records: impl IntoIterator<Item = u64>) {
    let mut w = d.write();
    for r in records {
        w.set_int("tx", "amount", r, r).unwrap();
    }
    w.commit().unwrap();
}

/// The whole table, walked `page` records at a time, exactly as a client would.
fn walk(d: &Db<big_pager::MemPager>, page: usize) -> Vec<RecordId> {
    let r = d.read();
    let mut out: Vec<RecordId> = Vec::new();
    let mut from = 0u64;
    loop {
        let batch = r.scan_records("tx", from, page).unwrap();
        let Some(&last) = batch.last() else { break };
        out.extend(&batch);
        // Saturating, because `u64::MAX` is a legal record id and its successor is not. A wrap
        // here would restart the scan at zero and never terminate.
        from = last.saturating_add(1);
        if last == u64::MAX {
            break;
        }
    }
    out
}

#[test]
fn an_empty_table_yields_nothing() {
    assert_eq!(walk(&db(), 10), Vec::<RecordId>::new());
}

#[test]
fn every_record_comes_back_in_order() {
    let d = db();
    seed(&d, [5u64, 1, 900, 3]);
    assert_eq!(walk(&d, 100), [1, 3, 5, 900]);
}

#[test]
fn the_walk_spans_shards() {
    let d = db();
    let ids = [0u64, 7, SHARD_WIDTH, SHARD_WIDTH + 3, 5 * SHARD_WIDTH, 5 * SHARD_WIDTH + 1];
    seed(&d, ids);
    assert_eq!(walk(&d, 2), ids);
}

#[test]
fn a_page_smaller_than_a_shard_still_finishes_that_shard() {
    // The case a per-shard loop gets wrong: a page that fills before the shard is exhausted has
    // to resume inside the same shard, not skip to the next one.
    let d = db();
    seed(&d, 0..50u64);
    assert_eq!(walk(&d, 7), (0..50).collect::<Vec<_>>());
}

#[test]
fn a_cursor_past_the_end_yields_nothing() {
    let d = db();
    seed(&d, [1u64, 2, 3]);
    assert_eq!(d.read().scan_records("tx", 4, 10).unwrap(), Vec::<RecordId>::new());
}

#[test]
fn the_cursor_is_a_floor_not_a_skip_count() {
    // Resuming from an id that was never written lands on the next one that was, rather than
    // being an offset into the result.
    let d = db();
    seed(&d, [10u64, 20, 30]);
    assert_eq!(d.read().scan_records("tx", 11, 10).unwrap(), [20, 30]);
    assert_eq!(d.read().scan_records("tx", 20, 10).unwrap(), [20, 30]);
}

#[test]
fn a_limit_of_zero_returns_nothing_rather_than_everything() {
    let d = db();
    seed(&d, [1u64, 2, 3]);
    assert_eq!(d.read().scan_records("tx", 0, 0).unwrap(), Vec::<RecordId>::new());
}

#[test]
fn deleted_records_are_not_listed() {
    let d = db();
    seed(&d, [1u64, 2, 3, SHARD_WIDTH]);
    let mut w = d.write();
    w.delete("tx", &[2, SHARD_WIDTH]).unwrap();
    w.commit().unwrap();
    assert_eq!(walk(&d, 2), [1, 3]);
}

#[test]
fn an_unknown_table_is_an_error_not_an_empty_page() {
    let d = db();
    assert!(matches!(d.read().scan_records("nope", 0, 10), Err(DbError::UnknownTable(_))));
}

#[test]
fn the_last_representable_record_can_be_reached() {
    // `u64::MAX` is a legal id. A cursor that adds one to it and wraps would restart the scan
    // at zero, so the walk above saturates - and this is the record that proves it has to.
    let d = db();
    // Written through `mark_exists` rather than `seed`: the id is the point, and the field is
    // 37 bits wide, so storing the id as its own value would fail for a reason unrelated to
    // this test.
    let mut w = d.write();
    w.mark_exists("tx", u64::MAX).unwrap();
    w.commit().unwrap();
    assert_eq!(walk(&d, 10), [u64::MAX]);
}

#[test]
fn paging_a_query_result_agrees_with_taking_it_whole() {
    // `Matches::records_from` and `Matches::records` are two ways of reading one set, and a
    // client that pages must not see a different answer from one that does not.
    let d = db();
    seed(&d, (0..200u64).map(|i| i * 3));

    let r = d.read();
    let m = r.matching("tx", "amount", RangeOp::Ge, 100).unwrap();
    let whole: Vec<RecordId> = m.records().collect();

    let mut paged: Vec<RecordId> = Vec::new();
    let mut from = 0u64;
    loop {
        let batch: Vec<RecordId> = m.records_from(from).take(11).collect();
        let Some(&last) = batch.last() else { break };
        paged.extend(&batch);
        from = last + 1;
    }
    assert_eq!(paged, whole);
}

#[test]
fn paging_costs_the_result_not_the_result_times_the_pages() {
    // The claim that separates a cursor from a filter, and the only one the answers cannot
    // show. A scan in pages of 100 reads a bounded multiple of what one unpaged scan reads; a
    // filter would re-walk everything it had already returned and grow with the square of the
    // table.
    use big_pager::CountingPager;

    let d = Db::open(CountingPager::new(big_pager::MemPager::new())).unwrap();
    d.create_table("tx").unwrap();
    d.create_field("tx", "amount", FieldKind::Int, 37).unwrap();
    let mut w = d.write();
    // Sixteen shards, so the walk crosses fragments rather than staying inside one page.
    for i in 0..8_000u64 {
        w.set_int("tx", "amount", (i % 16) * SHARD_WIDTH + i / 16, i).unwrap();
    }
    w.commit().unwrap();

    let before = d.store().pager().counts().reads;
    assert_eq!(d.read().scan_records("tx", 0, usize::MAX).unwrap().len(), 8_000);
    let one_shot = d.store().pager().counts().reads - before;

    let before = d.store().pager().counts().reads;
    let mut n = 0;
    let mut from = 0u64;
    loop {
        let batch = d.read().scan_records("tx", from, 100).unwrap();
        let Some(&last) = batch.last() else { break };
        n += batch.len();
        from = last + 1;
    }
    let paged = d.store().pager().counts().reads - before;

    assert_eq!(n, 8_000);
    // 80 pages over 16 shards: each page re-reads the shard it lands in, so a constant factor
    // of a few is expected and a factor of eighty is the filter this is not.
    assert!(
        paged < one_shot * 12,
        "80 pages read {paged} pages against {one_shot} for one pass - the cursor is re-walking"
    );
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(40))]

    /// However the ids are spread and whatever the page size, walking the table page by page
    /// returns exactly the records it holds, once each, ascending.
    #[test]
    fn a_paged_walk_is_the_whole_table_exactly_once(
        ids in proptest::collection::btree_set(0u64..3_000_000, 1..80),
        page in 1usize..17,
    ) {
        let d = db();
        seed(&d, ids.iter().copied());
        let walked = walk(&d, page);
        prop_assert_eq!(walked, ids.into_iter().collect::<Vec<_>>());
    }
}
