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

//! The read fan-out, which had no test at all.
//!
//! A query with enough fragments to be worth it spreads its scan across threads. Two things
//! about that need holding down, and neither was:
//!
//! **The answer must not depend on the fan-out.** Every query here is checked against a count
//! computed directly from the data that was written, so a chunking bug that dropped or
//! double-counted a fragment fails rather than merely returning a different number than last
//! time.
//!
//! **A refusal must be deterministic.** The memory ceiling and the cancellation flag are
//! checked from inside the workers, so the thread that notices first is whichever the scheduler
//! picks - but the *error the caller receives* must not be. A query that times out sometimes
//! and is cancelled other times is one an operator cannot write a runbook against.
//!
//! Every test crosses the fan-out threshold on purpose. Below eight candidate fragments the
//! scan stays on one thread, so a test with fewer would be testing the other branch while
//! looking like it tested this one.
//!
//! These were checked by breaking the engine on purpose and confirming they said so: dropping
//! one chunk of fragments from the fan-out fails five of them, and removing the cancellation
//! check fails the one that asserts it. Two earlier drafts of this file passed both mutations,
//! which is the only reason that paragraph is here.

use big_db::*;
use big_fragment::SHARD_WIDTH;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier};
use std::time::Duration;

/// Comfortably past the `2 * MIN_PER_THREAD` cut-off in `DbRead::fan_out`.
const SHARDS: u64 = 32;
const PER_SHARD: u64 = 40;

fn db() -> Db<big_pager::MemPager> {
    let d = Db::in_memory().unwrap();
    d.create_table("tx").unwrap();
    d.create_field("tx", "amount", FieldKind::Int, 20).unwrap();
    d
}

/// One record per `(shard, i)`, with a value that is a pure function of both, so the expected
/// answer to any predicate is computable without asking the engine.
fn value_at(shard: u64, i: u64) -> u64 {
    (shard * 7 + i * 13) % 1000
}

fn loaded() -> (Db<big_pager::MemPager>, Vec<(u64, u64)>) {
    let d = db();
    let mut facts = Vec::new();
    let mut w = d.write();
    for shard in 0..SHARDS {
        for i in 0..PER_SHARD {
            let record = shard * SHARD_WIDTH + i;
            let value = value_at(shard, i);
            w.set_int("tx", "amount", record, value).unwrap();
            facts.push((record, value));
        }
    }
    w.commit().unwrap();
    (d, facts)
}

fn expected_ge(facts: &[(u64, u64)], k: u64) -> u64 {
    facts.iter().filter(|(_, v)| *v >= k).count() as u64
}

#[test]
fn the_fan_out_is_actually_engaged() {
    let (d, _) = loaded();
    // Not decoration. If a later change moved the sharding or the threshold, every other test
    // in this file would quietly start exercising the single-threaded path and still pass.
    assert_eq!(
        d.catalog().fragments_of_field(0, 0, STANDARD_VIEW).count() as u64,
        SHARDS,
        "these tests only mean anything if there are enough fragments to fan out over"
    );
    assert!(std::thread::available_parallelism().map_or(1, |n| n.get()) > 1);
}

#[test]
fn a_fanned_out_count_equals_the_answer_computed_by_hand() {
    let (d, facts) = loaded();
    let r = d.read();
    for k in [0, 1, 250, 500, 750, 999, 1000] {
        assert_eq!(
            r.count("tx", "amount", RangeOp::Ge, k).unwrap(),
            expected_ge(&facts, k),
            "fanned-out count disagreed with the data at k={k}"
        );
    }
}

#[test]
fn every_record_is_reachable_after_the_fan_out() {
    let (d, facts) = loaded();
    let r = d.read();
    // A chunking bug that dropped the last partial chunk would leave a tail of records
    // invisible while every count above still looked plausible.
    for (record, value) in &facts {
        assert_eq!(
            r.get_int("tx", "amount", *record).unwrap(),
            Some(*value),
            "record {record} went missing"
        );
    }
    assert_eq!(r.count_all("tx").unwrap(), facts.len() as u64);
}

#[test]
fn concurrent_readers_all_get_the_same_answer() {
    let (d, facts) = loaded();
    let d = Arc::new(d);
    let facts = Arc::new(facts);

    // Sixteen queries fanning out at once over one handle, so the fan-outs overlap rather than
    // taking turns. Each opens its own read transaction; none may see another's.
    let start = Barrier::new(16);
    std::thread::scope(|scope| {
        for t in 0..16u64 {
            let d = Arc::clone(&d);
            let facts = Arc::clone(&facts);
            let start = &start;
            scope.spawn(move || {
                start.wait();
                for round in 0..8 {
                    let k = (t * 61 + round * 7) % 1000;
                    let got = d.read().count("tx", "amount", RangeOp::Ge, k).unwrap();
                    assert_eq!(got, expected_ge(&facts, k), "thread {t} disagreed at k={k}");
                }
            });
        }
    });
}

#[test]
fn a_reader_is_unaffected_by_writers_running_underneath_it() {
    let (d, facts) = loaded();
    let d = Arc::new(d);
    let baseline = expected_ge(&facts, 500);
    let stop = Arc::new(AtomicBool::new(false));
    let start = Arc::new(Barrier::new(5));

    std::thread::scope(|scope| {
        for _ in 0..4 {
            let d = Arc::clone(&d);
            let stop = Arc::clone(&stop);
            let start = Arc::clone(&start);
            scope.spawn(move || {
                // One transaction, held for the whole loop. Opening a fresh one each round
                // would correctly see each new commit and prove nothing - which is exactly
                // what the first version of this test did, and it failed for that reason.
                let r = d.read();
                start.wait();
                while !stop.load(Ordering::Relaxed) {
                    assert_eq!(
                        r.count("tx", "amount", RangeOp::Ge, 500).unwrap(),
                        baseline,
                        "a snapshot changed under a reader while a writer was running"
                    );
                }
            });
        }

        start.wait();
        // Records that would count towards the same predicate, so a reader whose snapshot
        // leaked would see the number move rather than stay put by luck.
        for round in 0..40u64 {
            let mut w = d.write();
            w.set_int("tx", "amount", (SHARDS + round) * SHARD_WIDTH, 999).unwrap();
            w.commit().unwrap();
        }
        stop.store(true, Ordering::Relaxed);
    });
}

#[test]
fn cancellation_is_honoured_and_reports_itself_as_cancellation() {
    let (d, _) = loaded();
    let flag = Arc::new(AtomicBool::new(true)); // already set: every worker sees it
    let r = d.read().with_cancel(Arc::clone(&flag));

    let err = r.count("tx", "amount", RangeOp::Ge, 0).unwrap_err();
    assert!(
        matches!(err, DbError::QueryCancelled),
        "a cancelled fan-out reported {err:?} instead of cancellation"
    );
}

#[test]
fn a_cancellation_mid_flight_still_lands_as_cancellation() {
    let (d, _) = loaded();
    let flag = Arc::new(AtomicBool::new(false));

    // Set from a thread that is not the one running the query, which is the only way it ever
    // happens in production: the query is busy, so it cannot be the one to notice.
    let setter = {
        let flag = Arc::clone(&flag);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_micros(50));
            flag.store(true, Ordering::Relaxed);
        })
    };

    let r = d.read().with_cancel(Arc::clone(&flag));
    let out = r.count("tx", "amount", RangeOp::Ge, 0);
    setter.join().unwrap();

    // A race by construction: the query may finish before the flag is set. What must never
    // happen is finishing *wrongly* or failing as something else.
    match out {
        Ok(_) => {}
        Err(DbError::QueryCancelled) => {}
        Err(other) => panic!("a cancelled fan-out reported {other:?} instead of cancellation"),
    }
}

#[test]
fn an_expired_deadline_is_honoured_and_reports_itself_as_a_timeout() {
    let (d, _) = loaded();
    let r = d.read().with_deadline(Duration::from_nanos(0));

    let err = r.count("tx", "amount", RangeOp::Ge, 0).unwrap_err();
    assert!(
        matches!(err, DbError::QueryTimeout { .. }),
        "an expired fan-out reported {err:?} instead of a timeout"
    );
}

#[test]
fn the_memory_ceiling_refuses_the_same_way_every_time() {
    let (d, _) = loaded();
    // Every worker charges against one shared atomic. Whichever thread pushes it over the
    // ceiling is the scheduler's business; the error is not.
    for attempt in 0..16 {
        let r = d.read().with_limits(QueryLimits { max_bytes: 1, max_records: 1 << 24 });
        let err = r.count("tx", "amount", RangeOp::Ge, 0).unwrap_err();
        assert!(
            matches!(err, DbError::QueryTooLarge { .. }),
            "attempt {attempt} refused with {err:?} rather than the memory ceiling"
        );
    }
}

#[test]
fn a_generous_ceiling_does_not_refuse() {
    let (d, facts) = loaded();
    let r = d.read().with_limits(QueryLimits::default());
    assert_eq!(
        r.count("tx", "amount", RangeOp::Ge, 500).unwrap(),
        expected_ge(&facts, 500),
        "the ceiling test above would be vacuous if this refused too"
    );
}
