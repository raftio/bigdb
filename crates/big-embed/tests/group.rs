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

//! Group commit: the same answers, out of fewer transactions.
//!
//! **What is asserted here and what is merely waited for.** Whether two writers actually share
//! a commit depends on whether they overlapped, which is the scheduler's business and not
//! something a test can demand. So every *correctness* claim below holds whatever the grouping
//! turned out to be - every good batch lands, every bad batch fails alone, the counts are each
//! caller's own - and the two claims that are *about* grouping are written as waits with a
//! deadline, the way `big-http`'s reclaim test waits on the steward's clock.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

use big_db::catalog::FieldKind;
use big_embed::*;

const PATIENCE: Duration = Duration::from_secs(20);

fn stocked(enabled: bool) -> Arc<Api<big_pager::MemPager>> {
    let api = Api::in_memory().unwrap();
    api.create_table("tx").unwrap();
    api.create_field("tx", "amount", FieldKind::Int, 32).unwrap();
    api.create_field("tx", "country", FieldKind::Set, 0).unwrap();
    api.configure_group_commit(GroupConfig { enabled, ..GroupConfig::default() });
    Arc::new(api)
}

fn count_all(api: &Api<big_pager::MemPager>) -> u64 {
    api.query("tx", "Count(All())").unwrap().as_count().expect("a count")
}

fn sum_amount(api: &Api<big_pager::MemPager>) -> u128 {
    api.query("tx", r#"Sum(All(), field="amount")"#).unwrap().as_sum().expect("a sum")
}

/// One writer, no company: the group machinery must not turn a single batch into more than one
/// transaction, and must not wait for anybody.
#[test]
fn a_lone_writer_still_costs_exactly_one_commit() {
    let api = stocked(true);
    api.import("tx", &[Fact::Int { field: "amount", record: 1, value: 100 }]).unwrap();

    let stats = api.group_stats();
    assert_eq!(stats.commits, 1, "{stats:?}");
    assert_eq!(stats.jobs, 1, "{stats:?}");
    assert_eq!(stats.isolations, 0, "nothing failed, so nothing should have been split: {stats:?}");
}

/// Switched off, nothing goes near the queue - not even to be counted.
#[test]
fn switched_off_it_does_nothing_at_all() {
    let api = stocked(false);
    api.import("tx", &[Fact::Int { field: "amount", record: 1, value: 100 }]).unwrap();
    assert_eq!(api.group_stats(), GroupStats::default());
    assert_eq!(count_all(&api), 1);
}

/// Every fact of every batch lands, whoever ends up committing it.
///
/// The count assertion is the one that holds no matter how the threads interleaved. The
/// grouping assertion underneath it is the reason the feature exists, and it is written as a
/// wait because two threads that never overlap have nothing to share.
#[test]
fn concurrent_writers_land_everything_and_share_commits() {
    const THREADS: u64 = 8;
    const ROUNDS: u64 = 60;

    let api = stocked(true);
    let start = Barrier::new(THREADS as usize);
    std::thread::scope(|scope| {
        for t in 0..THREADS {
            let api = Arc::clone(&api);
            let start = &start;
            scope.spawn(move || {
                start.wait();
                for round in 0..ROUNDS {
                    let record = t * ROUNDS + round + 1;
                    api.import("tx", &[Fact::Int { field: "amount", record, value: 7 }])
                        .unwrap_or_else(|e| panic!("thread {t} round {round}: {e}"));
                }
            });
        }
    });

    assert_eq!(count_all(&api), THREADS * ROUNDS);
    assert_eq!(sum_amount(&api), (THREADS * ROUNDS * 7) as u128);

    let stats = api.group_stats();
    assert_eq!(stats.jobs, THREADS * ROUNDS, "every batch is one job: {stats:?}");
    assert!(
        stats.commits <= stats.jobs,
        "a commit cannot carry fewer than the jobs counted against it: {stats:?}"
    );
    assert!(
        stats.commits < stats.jobs,
        "eight writers over {ROUNDS} rounds never once overlapped, which is possible but so \
         unlikely it is worth looking at: {stats:?}"
    );
}

/// **The claim this feature has to earn.** A batch the engine refuses must fail alone.
///
/// One thread always submits a batch naming a field that does not exist; the others submit good
/// ones. Whatever grouping happens, the good ones must all succeed and the bad ones must all
/// fail - and the good facts must be readable afterwards.
#[test]
fn a_bad_batch_never_takes_a_good_one_with_it() {
    const GOOD_THREADS: u64 = 7;
    const ROUNDS: u64 = 40;

    let api = stocked(true);
    let refused = AtomicU64::new(0);
    let start = Barrier::new(GOOD_THREADS as usize + 1);

    std::thread::scope(|scope| {
        // The poisoner.
        {
            let api = Arc::clone(&api);
            let refused = &refused;
            let start = &start;
            scope.spawn(move || {
                start.wait();
                for round in 0..ROUNDS {
                    let err = api
                        .import(
                            "tx",
                            &[Fact::Int { field: "nosuchfield", record: round + 1, value: 1 }],
                        )
                        .expect_err("a field that does not exist is always refused");
                    // The real error, not a stand-in for one somebody else caused.
                    assert!(
                        format!("{err}").contains("nosuchfield"),
                        "round {round} was told about somebody else's problem: {err}"
                    );
                    refused.fetch_add(1, Ordering::Relaxed);
                }
            });
        }
        for t in 0..GOOD_THREADS {
            let api = Arc::clone(&api);
            let start = &start;
            scope.spawn(move || {
                start.wait();
                for round in 0..ROUNDS {
                    let record = t * ROUNDS + round + 1;
                    api.import("tx", &[Fact::Int { field: "amount", record, value: 7 }])
                        .unwrap_or_else(|e| {
                            panic!("good thread {t} round {round} paid for somebody else: {e}")
                        });
                }
            });
        }
    });

    assert_eq!(refused.load(Ordering::Relaxed), ROUNDS);
    assert_eq!(
        count_all(&api),
        GOOD_THREADS * ROUNDS,
        "every good batch landed and no bad one did"
    );
    assert_eq!(sum_amount(&api), (GOOD_THREADS * ROUNDS * 7) as u128);
}

/// A group in which *everything* is poison commits nothing and tells everybody the truth.
#[test]
fn a_group_of_nothing_but_bad_batches_lands_nothing() {
    const THREADS: u64 = 8;

    let api = stocked(true);
    let start = Barrier::new(THREADS as usize);
    std::thread::scope(|scope| {
        for t in 0..THREADS {
            let api = Arc::clone(&api);
            let start = &start;
            scope.spawn(move || {
                start.wait();
                for round in 0..20 {
                    api.import("tx", &[Fact::Int { field: "gone", record: round + 1, value: 1 }])
                        .expect_err("thread {t} was told a missing field was fine");
                }
                let _ = t;
            });
        }
    });

    assert_eq!(count_all(&api), 0, "nothing should have reached the disk");
}

/// The answer a caller gets is about *its own* batch, never about the group it travelled in.
#[test]
fn a_delete_is_told_what_it_removed_and_not_what_the_group_did() {
    let api = stocked(true);
    api.import(
        "tx",
        &[
            Fact::Int { field: "amount", record: 1, value: 1 },
            Fact::Int { field: "amount", record: 2, value: 2 },
            Fact::Int { field: "amount", record: 3, value: 3 },
        ],
    )
    .unwrap();

    // Two of these exist and one never did. A delete reports what it actually removed.
    assert_eq!(api.delete("tx", &[1, 2, 99]).unwrap(), 2);
    assert_eq!(count_all(&api), 1);
}

/// Arrival order survives grouping, so a second write to a record still wins.
#[test]
fn the_later_write_to_a_record_still_wins() {
    let api = stocked(true);
    for value in [10u64, 20, 30] {
        api.import("tx", &[Fact::Int { field: "amount", record: 1, value }]).unwrap();
    }
    assert_eq!(sum_amount(&api), 30);
}

/// Turning it on changes what a commit costs and nothing a caller can read.
///
/// The same script twice over, once each way, compared answer for answer. This is the
/// regression gate: a difference here is a difference in the database, not in its timing.
#[test]
fn on_and_off_give_the_same_answers() {
    fn script(api: &Api<big_pager::MemPager>) -> (u64, u128, u64) {
        api.import(
            "tx",
            &[
                Fact::Int { field: "amount", record: 1, value: 100 },
                Fact::Key { field: "country", record: 1, value: "GB" },
            ],
        )
        .unwrap();
        api.import(
            "tx",
            &[
                Fact::Int { field: "amount", record: 2, value: 900 },
                Fact::Key { field: "country", record: 2, value: "US" },
            ],
        )
        .unwrap();
        api.import("tx", &[Fact::Int { field: "amount", record: 1, value: 150 }]).unwrap();
        let removed = api.delete("tx", &[2]).unwrap();
        let gb = api.query("tx", r#"Count(Row(country="GB"))"#).unwrap().as_count().unwrap();
        (gb, sum_amount(api), removed)
    }

    let off = stocked(false);
    let on = stocked(true);
    assert_eq!(script(&off), script(&on));
}

/// A group is capped, and the cap is honoured rather than merely documented.
///
/// Waited for rather than asserted outright: it takes two writers overlapping to make a group
/// larger than one, and nothing can make two threads overlap on demand.
#[test]
fn a_group_never_carries_more_jobs_than_the_cap() {
    const CAP: usize = 4;
    const THREADS: u64 = 16;

    let api = Api::in_memory().unwrap();
    api.create_table("tx").unwrap();
    api.create_field("tx", "amount", FieldKind::Int, 32).unwrap();
    api.configure_group_commit(GroupConfig {
        enabled: true,
        max_jobs: CAP,
        ..GroupConfig::default()
    });
    let api = Arc::new(api);

    let deadline = Instant::now() + PATIENCE;
    let mut record = 1u64;
    loop {
        let before = api.group_stats();
        let start = Barrier::new(THREADS as usize);
        std::thread::scope(|scope| {
            for _ in 0..THREADS {
                let api = Arc::clone(&api);
                let start = &start;
                let mine = record;
                record += 1;
                scope.spawn(move || {
                    start.wait();
                    api.import("tx", &[Fact::Int { field: "amount", record: mine, value: 1 }])
                        .unwrap();
                });
            }
        });
        let after = api.group_stats();

        let commits = after.commits - before.commits;
        let jobs = after.jobs - before.jobs;
        assert_eq!(jobs, THREADS, "every batch is one job: {after:?}");
        // The cap is per commit, so this round cannot have used fewer commits than the cap
        // allows for the jobs it carried.
        assert!(
            commits >= jobs.div_ceil(CAP as u64),
            "{jobs} jobs in {commits} commits is more than {CAP} per commit: {after:?}"
        );
        if commits < jobs {
            // Grouping happened at least once and stayed inside the cap. That is the claim.
            break;
        }
        assert!(Instant::now() < deadline, "sixteen writers never once overlapped: {after:?}");
    }
}

// ---------------------------------------------------------------------------------------
// Answering before the commit.
// ---------------------------------------------------------------------------------------

fn early(config: GroupConfig) -> Arc<Api<big_pager::MemPager>> {
    let api = Api::in_memory().unwrap();
    api.create_table("tx").unwrap();
    api.create_field("tx", "amount", FieldKind::Int, 32).unwrap();
    api.configure_group_commit(config);
    Arc::new(api)
}

fn asking_early() -> GroupConfig {
    GroupConfig { enabled: true, async_writes: true, ..GroupConfig::default() }
}

/// The whole bargain in one test: answered at once, readable only after a commit.
#[test]
fn an_early_answer_comes_before_the_commit_and_says_so() {
    let api = early(asking_early());

    let accepted = api
        .import_acked("tx", &[Fact::Int { field: "amount", record: 1, value: 100 }], Ack::Async)
        .unwrap();
    assert_eq!(accepted.count, 1);
    assert_eq!(accepted.txn, None, "there is no transaction yet, and it must not claim one");
    assert_eq!(count_all(&api), 0, "nothing is readable until something commits it");

    let stats = api.group_stats();
    assert!(stats.async_held_bytes > 0, "what would be lost is measurable: {stats:?}");

    assert_eq!(api.flush_pending(), 1);
    assert_eq!(count_all(&api), 1);
    assert_eq!(api.group_stats().async_held_bytes, 0, "and it stops being at risk");
}

/// A flush with nothing to flush is not a commit.
#[test]
fn flushing_an_empty_queue_writes_nothing() {
    let api = early(asking_early());
    assert_eq!(api.flush_pending(), 0);
    assert_eq!(api.group_stats().commits, 0, "an empty flush must not cost a transaction");
}

/// Asking to be answered early where it is not offered gets the durable answer, not an error.
///
/// The refusal belongs at the edge, which can say *why* and can name the flag. Down here the
/// only honest thing is to do the safe thing.
#[test]
fn asking_early_where_it_is_not_offered_still_commits() {
    let api = early(GroupConfig { enabled: true, ..GroupConfig::default() });
    assert!(!api.takes_async_writes());

    let accepted = api
        .import_acked("tx", &[Fact::Int { field: "amount", record: 1, value: 100 }], Ack::Async)
        .unwrap();
    assert!(accepted.txn.is_some(), "it was committed, so it has a transaction");
    assert_eq!(count_all(&api), 1, "and it is readable immediately");
}

/// Past the ceiling, `Refuse` refuses — with the code a client already backs off on.
#[test]
fn a_full_buffer_refuses_with_the_code_the_shedder_uses() {
    let api = early(GroupConfig {
        enabled: true,
        async_writes: true,
        max_async_bytes: 1,
        when_full: WhenFull::Refuse,
        ..GroupConfig::default()
    });

    // The first one fits: a job is admitted when the buffer is empty whatever its size, or a
    // batch bigger than the ceiling could never be written at all.
    api.import_acked("tx", &[Fact::Int { field: "amount", record: 1, value: 1 }], Ack::Async)
        .unwrap();
    let err = api
        .import_acked("tx", &[Fact::Int { field: "amount", record: 2, value: 2 }], Ack::Async)
        .expect_err("the buffer is over its ceiling");
    assert_eq!(err.code(), "server_busy", "{err}");
    assert_eq!(api.group_stats().async_refused, 1);

    // And it recovers: once the buffer drains, the same write is taken.
    api.flush_pending();
    api.import_acked("tx", &[Fact::Int { field: "amount", record: 2, value: 2 }], Ack::Async)
        .unwrap();
}

/// Past the ceiling, `Block` answers durably instead — backpressure, not an error.
#[test]
fn a_full_buffer_blocks_rather_than_refusing_by_default() {
    let api = early(GroupConfig {
        enabled: true,
        async_writes: true,
        max_async_bytes: 1,
        ..GroupConfig::default()
    });
    assert_eq!(GroupConfig::default().when_full, WhenFull::Block, "and that is the default");

    api.import_acked("tx", &[Fact::Int { field: "amount", record: 1, value: 1 }], Ack::Async)
        .unwrap();
    let accepted = api
        .import_acked("tx", &[Fact::Int { field: "amount", record: 2, value: 2 }], Ack::Async)
        .expect("blocked, not refused");
    assert!(accepted.txn.is_some(), "the one that waited was committed: {accepted:?}");
    assert_eq!(api.group_stats().async_refused, 0);
    // The one it waited behind went in with it.
    assert_eq!(count_all(&api), 2);
}

/// **The cost of answering early, made visible.** A batch the engine refuses has nobody left to
/// tell, so it is counted instead — and it must not take the good ones with it.
#[test]
fn an_early_answer_that_turns_out_to_be_wrong_is_counted_not_reported() {
    let api = early(asking_early());

    api.import_acked("tx", &[Fact::Int { field: "amount", record: 1, value: 1 }], Ack::Async)
        .unwrap();
    api.import_acked("tx", &[Fact::Int { field: "nosuchfield", record: 2, value: 2 }], Ack::Async)
        .expect("accepted, because nothing has looked at it yet");
    api.import_acked("tx", &[Fact::Int { field: "amount", record: 3, value: 3 }], Ack::Async)
        .unwrap();

    api.flush_pending();

    let stats = api.group_stats();
    assert_eq!(stats.async_failed, 1, "the bad one is counted: {stats:?}");
    assert_eq!(count_all(&api), 2, "and the two good ones landed anyway");
}

/// Shutdown commits what was promised. Losing it would be a bug, not a trade-off.
#[test]
fn stopping_commits_everything_that_was_acknowledged() {
    let api = early(asking_early());
    for record in 1..=5 {
        api.import_acked("tx", &[Fact::Int { field: "amount", record, value: 1 }], Ack::Async)
            .unwrap();
    }
    assert_eq!(count_all(&api), 0);

    assert_eq!(api.stop_writer(), 5, "every job it was given");
    assert_eq!(count_all(&api), 5);

    // And afterwards it takes nothing new rather than accepting what it cannot commit.
    let err = api
        .import_acked("tx", &[Fact::Int { field: "amount", record: 6, value: 1 }], Ack::Async)
        .expect_err("a stopped writer takes nothing");
    assert_eq!(err.code(), "server_busy", "{err}");
}

/// A queue deeper than one commit still drains completely.
#[test]
fn stopping_drains_more_than_one_commit_worth() {
    let api = early(GroupConfig { max_jobs: 4, ..asking_early() });
    for record in 1..=30 {
        api.import_acked("tx", &[Fact::Int { field: "amount", record, value: 1 }], Ack::Async)
            .unwrap();
    }
    assert_eq!(api.stop_writer(), 30);
    assert_eq!(count_all(&api), 30);
    assert!(api.group_stats().commits >= 30 / 4, "more than one commit was needed");
}
