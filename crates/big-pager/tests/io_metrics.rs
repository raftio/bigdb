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

//! What the backend reports about its own I/O, asserted through `Store` rather than in isolation.
//!
//! The counters themselves are unit-tested in `src/io.rs`. What cannot be tested there is the
//! part that actually breaks: whether the backend *calls* them, on every path and only on the
//! paths that did something. An instrumented method that forgets one call is indistinguishable
//! from a quiet database, which is the failure mode that makes a metric worse than none.
//!
//! Counts and not timings throughout - the same reason `durability.rs` gives. A count is the
//! same number on every machine and so can be asserted; a duration is the disk's answer, not
//! the engine's, and belongs in a benchmark.

#![cfg(unix)]

use big_page::{FragmentKey, LeafBuilder};
use big_pager::*;

fn open(path: &std::path::Path) -> MmapPager {
    MmapPager::open(path, 1 << 20).unwrap()
}

/// One commit that really writes a page, so there is something to count.
fn commit<P: PagerMut>(store: &Store<P>, shard: u64) {
    let mut w = store.begin_write();
    let p = w.alloc().unwrap();
    w.write(p, LeafBuilder::new().finish(p)).unwrap();
    w.set_root(FragmentKey::new(1, 1, 0, shard), p);
    w.commit().unwrap();
}

fn io<P: Pager>(store: &Store<P>) -> IoStats {
    store.metrics().io.expect("the mapped backend keeps its own counts")
}

/// The route an operator actually reads them by: `Store::metrics`, not the pager directly.
///
/// This is the wiring the whole change is for. The backend counting into a struct nobody
/// forwards would be a debugger's convenience, which is exactly what the pager's gauges were
/// before `/metrics` existed.
#[test]
fn the_backend_reports_through_store_metrics() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::init(open(&dir.path().join("t.big"))).unwrap();

    let stats = io(&store);
    assert_eq!(stats.backend, "mmap", "the series would be unattributable without this");
}

/// A commit writes pages and flushes; both land, and the bytes follow the pages.
#[test]
fn a_commit_shows_up_as_writes_and_flushes() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::init(open(&dir.path().join("t.big"))).unwrap();

    let before = io(&store);
    commit(&store, 0);
    let delta = io(&store).since(&before);

    assert!(delta.writes > 0, "a commit wrote nothing");
    assert_eq!(
        delta.write_bytes,
        delta.writes * PAGE_SIZE as u64,
        "copy-on-write never writes part of a page, so these cannot come apart"
    );
    // Both flushes of a commit or neither - the property `Store::flush` exists to hold, seen
    // here from the outside for the first time.
    assert_eq!(delta.syncs, 2, "a full-durability commit issues two flushes");
}

/// Turning durability off is visible in the count, which is the point of exporting it.
///
/// An ingest that turned the knob down and never turned it back is otherwise invisible: the
/// file looks identical, the write rate looks identical, and the only difference is what a
/// power cut would cost. `big_durability` says what was configured; this says what happened.
///
/// **`Barrier` counts two, not one.** It is a weaker *kind* of flush rather than a smaller
/// number of them - `Store::flush` sends it to `sync_data` instead of `sync` - so what this
/// counter separates is full from none, and nothing here distinguishes the two strengths. That
/// is the honest limit of a count: the difference between them is a duration, and it shows up
/// in `sync_nanos` on a real disk and nowhere at all on a machine where the two are the same
/// call.
#[test]
fn the_flush_count_follows_the_durability_level() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::init(open(&dir.path().join("t.big"))).unwrap();

    let mut counts = Vec::new();
    for (i, level) in [Durability::Full, Durability::Barrier, Durability::None].iter().enumerate() {
        store.set_durability(*level).unwrap();
        let before = io(&store);
        commit(&store, i as u64);
        counts.push(io(&store).since(&before).syncs);
    }
    assert_eq!(counts, vec![2, 2, 0], "flushes per commit at full, barrier, none");
}

/// Growth is counted when the file actually grew, and not when it did not.
///
/// The distinction is the whole value of the number: `grow` is called on a commit that needed
/// no new page and returns early, and counting those would turn a metric about file growth into
/// a metric about commits.
#[test]
fn growing_counts_only_when_the_file_really_grew() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.big");
    let pager = open(&path);

    pager.grow(64).unwrap();
    let after_first = pager.io_stats().unwrap().grows;
    assert_eq!(after_first, 1);

    // Already this long. Nothing reaches the disk, so nothing is counted.
    pager.grow(64).unwrap();
    pager.grow(8).unwrap();
    assert_eq!(pager.io_stats().unwrap().grows, after_first, "a no-op grow was counted");

    // The same for the other direction.
    pager.truncate(64).unwrap();
    assert_eq!(pager.io_stats().unwrap().truncates, 0, "a no-op truncate was counted");
    pager.truncate(32).unwrap();
    assert_eq!(pager.io_stats().unwrap().truncates, 1);
}

/// Reads are counted, and a refused read is not.
///
/// Out of bounds never touched the file, so counting it would inflate the read rate with
/// exactly the requests that did no work.
#[test]
fn reads_are_counted_and_a_refused_one_is_not() {
    let dir = tempfile::tempdir().unwrap();
    let pager = open(&dir.path().join("t.big"));
    pager.grow(4).unwrap();

    for pgno in 0..4 {
        pager.read(pgno).unwrap();
    }
    assert_eq!(pager.io_stats().unwrap().reads, 4);

    assert!(pager.read(9).is_err());
    assert_eq!(pager.io_stats().unwrap().reads, 4, "a read that was refused was counted");
}

/// The counters only ever go up, which is what lets a scraper rate them.
///
/// A counter that can fall reads as a restart, and a dashboard that sees one draws a spike of
/// its entire value. Worth a test because `since` subtracts two snapshots taken without a
/// lock, so the possibility is real rather than theoretical.
#[test]
fn nothing_ever_decreases() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::init(open(&dir.path().join("t.big"))).unwrap();

    let mut last = io(&store);
    for shard in 0..8 {
        commit(&store, shard);
        let now = io(&store);
        assert!(now.reads >= last.reads, "reads fell from {} to {}", last.reads, now.reads);
        assert!(now.writes >= last.writes, "writes fell from {} to {}", last.writes, now.writes);
        assert!(now.syncs >= last.syncs, "syncs fell from {} to {}", last.syncs, now.syncs);
        assert!(now.sync_nanos >= last.sync_nanos, "flush time went backwards");
        last = now;
    }
}
