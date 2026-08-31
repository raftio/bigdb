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

//! The durability knob, counted rather than trusted.
//!
//! Timing cannot test this: a relaxed commit is faster on a real disk and identical on a
//! `MemPager`, so a benchmark would prove nothing about what was actually promised. What can be
//! asserted is the number of flushes a commit issues, which is the thing the levels differ in.
//!
//! The property this file exists to protect is the one in `Store::flush`: a commit issues both
//! of its flushes or neither. Skipping only the first would let the meta page reach the disk
//! ahead of the pages it names, and a crash there is not lost data but a file that does not
//! open. Every test that counts flushes therefore checks the count is even.

use big_page::LeafBuilder;
use big_pager::*;

fn store() -> Store<CountingPager<MemPager>> {
    Store::open_or_init(CountingPager::new(MemPager::new())).unwrap()
}

/// One commit that actually writes a page, so there is something to flush.
fn commit(store: &Store<CountingPager<MemPager>>) {
    let mut w = store.begin_write();
    let p = w.alloc().unwrap();
    w.write(p, LeafBuilder::new().finish(p)).unwrap();
    w.set_root(FragmentKey::new(1, 1, 0, 0), p);
    w.commit().unwrap();
}

fn syncs_for(level: Durability) -> u64 {
    let s = store();
    s.set_durability(level).unwrap();
    s.pager().reset();
    commit(&s);
    s.pager().counts().syncs
}

#[test]
fn the_default_is_the_strongest_level() {
    // Not merely "some level". A file opened by a caller who never heard of this knob must be
    // written exactly as it was before the knob existed.
    assert_eq!(store().durability(), Durability::Full);
}

#[test]
fn full_flushes_twice_per_commit() {
    assert_eq!(syncs_for(Durability::Full), 2);
}

#[test]
fn barrier_still_flushes_twice_per_commit() {
    // Barrier is a weaker *kind* of flush, not a smaller number of them. A backend that cannot
    // tell the two apart - `MemPager` here - performs the strong one, which is why this counts
    // two rather than zero.
    assert_eq!(syncs_for(Durability::Barrier), 2);
}

#[test]
fn none_flushes_not_at_all() {
    assert_eq!(syncs_for(Durability::None), 0);
}

#[test]
fn no_level_flushes_an_odd_number_of_times() {
    // The invariant behind all three, stated on its own so that a future level cannot be added
    // without meeting it. An odd count means one of the two flushes was dropped, and the only
    // one it could be is the first - which is the one that must never go.
    for level in [Durability::Full, Durability::Barrier, Durability::None] {
        let n = syncs_for(level);
        assert_eq!(n % 2, 0, "{} issued {n} flushes", level.label());
    }
}

#[test]
fn a_relaxed_commit_is_still_readable_and_still_atomic() {
    // Relaxing changes when bytes reach the platter, never what the file says. Everything a
    // committed transaction promised about visibility holds unchanged.
    let s = store();
    s.set_durability(Durability::None).unwrap();
    commit(&s);
    let r = s.begin_read();
    assert!(r.root(&FragmentKey::new(1, 1, 0, 0)).is_some());
}

#[test]
fn tightening_flushes_before_it_takes_effect() {
    // The reason `set_durability` is not a plain store. A loader that relaxes, loads, and
    // tightens has to end up with its load durable; if the tightening only affected *future*
    // commits, the last batch would sit unflushed behind a setting that claims otherwise.
    let s = store();
    s.set_durability(Durability::None).unwrap();
    commit(&s);

    s.pager().reset();
    s.set_durability(Durability::Full).unwrap();
    assert_eq!(s.pager().counts().syncs, 1, "tightening must flush what came before it");
}

#[test]
fn relaxing_does_not_flush() {
    // Nothing to make less durable. A flush here would be work done for no promise.
    let s = store();
    commit(&s);
    s.pager().reset();
    s.set_durability(Durability::None).unwrap();
    assert_eq!(s.pager().counts().syncs, 0);
}

#[test]
fn setting_the_level_it_already_has_does_nothing() {
    let s = store();
    commit(&s);
    s.pager().reset();
    s.set_durability(Durability::Full).unwrap();
    assert_eq!(s.pager().counts().syncs, 0);
}

#[test]
fn truncating_the_tail_flushes_whatever_the_setting_says() {
    // Deliberately not covered by the knob. The comment in `truncate_tail` calls a crash
    // between the meta write and the shrink harmless, and that is only true if the meta is
    // actually on disk when the file gets shorter. Nobody asked for a faster truncate.
    let s = store();
    s.set_durability(Durability::None).unwrap();

    // Free tail pages have to exist for the truncate to do anything, and enough of them that
    // the run reaches the end of the file: the root-record and freelist chains also live up
    // there, so a single freed page is not necessarily a trailing one. They become reclaimable
    // only once no transaction as old as the one that freed them can still be reading, which is
    // what the extra empty commit is for.
    let mut pages = Vec::new();
    let mut w = s.begin_write();
    for i in 0..40u32 {
        let p = w.alloc().unwrap();
        w.write(p, LeafBuilder::new().finish(p)).unwrap();
        w.set_root(FragmentKey::new(1, 1, 0, i as u64), p);
        pages.push(p);
    }
    w.commit().unwrap();

    let mut w = s.begin_write();
    for (i, p) in pages.iter().enumerate() {
        w.remove_root(&FragmentKey::new(1, 1, 0, i as u64));
        w.free(*p);
    }
    w.commit().unwrap();
    s.begin_write().commit().unwrap();

    s.pager().reset();
    let released = s.truncate_tail().unwrap();
    assert!(released > 0, "the test has to reach the code it is about");
    assert!(s.pager().counts().syncs >= 1, "truncate_tail must flush its meta unconditionally");
}

#[test]
fn the_level_is_reported_in_the_metrics() {
    // An ingest that relaxed and never tightened back is otherwise invisible: the file looks
    // exactly as healthy as it did before.
    let s = store();
    assert_eq!(s.metrics().durability, Durability::Full);
    s.set_durability(Durability::Barrier).unwrap();
    assert_eq!(s.metrics().durability, Durability::Barrier);
}

#[test]
fn the_levels_are_ordered_by_what_they_promise() {
    assert!(Durability::Full.at_least(Durability::Barrier));
    assert!(Durability::Barrier.at_least(Durability::None));
    assert!(!Durability::None.at_least(Durability::Barrier));
    assert!(Durability::Full.at_least(Durability::Full));
}

#[test]
fn the_spellings_round_trip() {
    // The labels are what a command line, a log line and a metric all use. A level whose name
    // does not parse back is a configuration file that cannot express it.
    for level in [Durability::Full, Durability::Barrier, Durability::None] {
        assert_eq!(Durability::parse(level.label()), Some(level));
    }
    assert_eq!(Durability::parse("relaxed"), None);
}
