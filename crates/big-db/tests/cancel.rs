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

//! A read that is no longer wanted stops.
//!
//! Both mechanisms are cooperative, so what these tests actually assert is that the checkpoint
//! sits on every path a scan takes - the serial one and the parallel one. A deadline of zero
//! and a flag set before the call make that deterministic: no sleeping, no timing assumption,
//! and no test that passes on a fast machine and fails on a busy one.

use big_db::*;
use big_engine::SHARD_WIDTH;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// `shards` fragments of one field, which is what decides whether the scan fans out.
fn db(shards: u64) -> Db<big_pager::MemPager> {
    let d = Db::in_memory().unwrap();
    d.create_table("tx").unwrap();
    d.create_field("tx", "amount", FieldKind::Int, 37).unwrap();

    let mut w = d.write();
    for i in 0..shards {
        w.set_int("tx", "amount", i * SHARD_WIDTH + 1, i + 1).unwrap();
    }
    w.commit().unwrap();
    d
}

/// Below the fan-out threshold, so this exercises the serial loop.
const SERIAL: u64 = 3;
/// Comfortably above it, so this exercises the scoped-thread loop.
const PARALLEL: u64 = 64;

fn scan(d: &Db<big_pager::MemPager>, read: DbRead<'_, big_pager::MemPager>) -> Result<Matches> {
    let _ = d;
    read.matching("tx", "amount", RangeOp::Gt, 0)
}

#[test]
fn a_zero_deadline_stops_a_serial_scan() {
    let d = db(SERIAL);
    let err = scan(&d, d.read().with_deadline(Duration::ZERO)).unwrap_err();
    assert!(matches!(err, DbError::QueryTimeout { .. }), "expected a timeout, got {err}");
    assert_eq!(err.code(), "query_timeout");
}

#[test]
fn a_zero_deadline_stops_a_parallel_scan() {
    let d = db(PARALLEL);
    let err = scan(&d, d.read().with_deadline(Duration::ZERO)).unwrap_err();
    assert!(
        matches!(err, DbError::QueryTimeout { .. }),
        "the scoped-thread path needs its own checkpoint; got {err}"
    );
}

#[test]
fn a_set_flag_stops_a_serial_scan() {
    let d = db(SERIAL);
    let flag = Arc::new(AtomicBool::new(true));
    let err = scan(&d, d.read().with_cancel(flag)).unwrap_err();
    assert!(matches!(err, DbError::QueryCancelled), "expected a cancellation, got {err}");
    assert_eq!(err.code(), "query_cancelled");
}

#[test]
fn a_set_flag_stops_a_parallel_scan() {
    let d = db(PARALLEL);
    let flag = Arc::new(AtomicBool::new(true));
    let err = scan(&d, d.read().with_cancel(flag)).unwrap_err();
    assert!(matches!(err, DbError::QueryCancelled), "got {err}");
}

/// The flag is read, not copied: setting it after the transaction was built still works, which
/// is the only ordering that matters in practice - the watchdog sets it while the scan runs.
#[test]
fn the_flag_is_shared_not_snapshotted() {
    let d = db(SERIAL);
    let flag = Arc::new(AtomicBool::new(false));
    let read = d.read().with_cancel(Arc::clone(&flag));
    flag.store(true, Ordering::Relaxed);
    assert!(matches!(scan(&d, read).unwrap_err(), DbError::QueryCancelled));
}

/// A cancellation beats a deadline when both fire: the client going away is the real answer,
/// and reporting it as a timeout would make an alert fire on a user pressing ctrl-c.
#[test]
fn cancellation_is_reported_ahead_of_a_timeout() {
    let d = db(SERIAL);
    let flag = Arc::new(AtomicBool::new(true));
    let read = d.read().with_deadline(Duration::ZERO).with_cancel(flag);
    assert!(matches!(scan(&d, read).unwrap_err(), DbError::QueryCancelled));
}

/// The default is unchanged: no clock, no flag, and the query runs.
#[test]
fn an_unbudgeted_read_still_answers() {
    let d = db(SERIAL);
    let rows = scan(&d, d.read()).unwrap();
    assert_eq!(rows.cardinality(), SERIAL);
}

#[test]
fn a_generous_deadline_does_not_fire() {
    let d = db(PARALLEL);
    let rows = scan(&d, d.read().with_deadline(Duration::from_secs(60))).unwrap();
    assert_eq!(rows.cardinality(), PARALLEL);
}
