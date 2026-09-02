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

//! What the storage backend actually did to the disk, counted by the backend itself.
//!
//! # Why this is not `CountingPager`
//!
//! That decorator counts *calls into the trait*, which is the right measurement for a test: it
//! is the same number on every machine, so it can be asserted. It is the wrong measurement for
//! an operator, because a call into the trait is not an I/O and how much of one it is differs
//! per backend by more than a constant. `MmapPager::read` performs no read at all - it returns
//! a pointer into the mapping, and whether that touches the disk is a page fault this process
//! is never told about, while its `write` is a real `pwrite` of 8 KiB. Only the backend knows
//! which of its own operations cost anything, so the counting lives in the backend and the
//! trait only asks for the total ([`crate::Pager::io_stats`]).
//!
//! # What is counted, and what is deliberately not
//!
//! Reads are counted; **the bytes they move are not**, because on the only backend there is
//! they are not this process's to count. Writes carry their bytes because `write` really is a
//! syscall with a length. A field that would be structurally zero for ever is worse than a
//! missing one: it reads as an idle database rather than as an unmeasurable quantity.
//!
//! # Why reads are striped and nothing else is
//!
//! Writes, grows, truncates and flushes all happen under `Store`'s write lock, so one atomic
//! each is uncontended by construction. Reads are the opposite: every reader thread takes one
//! per page, and a bit-sliced scan takes one per page *per plane*. A single counter there is a
//! cache line every core wants to own, on a path where the whole read was measured at 640ns -
//! so the read counter is striped by page number, the same trick and the same stripe count the
//! verified-page memo uses. Summing 64 words to publish a metric costs nothing; nobody scrapes
//! in a loop.
//!
//! # Why nothing on the read path is timed
//!
//! `Instant::now` twice per read is tens of nanoseconds against a read that is hundreds, which
//! would mean paying a measurable slowdown for the privilege of measuring it. Flushes are the
//! opposite case - rare, two per commit, and milliseconds each - so [`IoCounters::sync`] times
//! them. That is also the number an operator actually asks for, because a commit that got slow
//! got slow in the flush, and it is the only place the two flush *strengths* are visible at
//! all: `Full` and `Barrier` issue the same count and differ only in what they wait for.

use big_page::{Pgno, PAGE_SIZE};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

/// Stripes over the read counter. Sixty-four for the reason `VERIFY_SHARDS` is: it is enough
/// that two threads scanning at once collide on one increment in sixty-four, and small enough
/// that the whole array is a handful of cache lines.
const READ_STRIPES: usize = 64;

/// A snapshot of one backend's I/O. Plain numbers, so a caller can subtract two of them.
///
/// Cumulative since open, never reset: a counter that resets cannot be rated, and everything
/// here is meant to be read as a rate.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct IoStats {
    /// Which backend produced these, for the label on the exported series. One process opens
    /// one backend, so this is fixed for its lifetime and costs no cardinality.
    pub backend: &'static str,
    /// Pages the backend handed to the engine, whatever they cost it.
    ///
    /// **Pages, not bytes, and not disk reads.** On a mapped backend a read is a pointer into
    /// the mapping; whether it reaches the disk is a major fault the kernel handles without
    /// telling this process, so the honest measurement is how often the engine asked. Its rate
    /// against `writes` is the read/write mix, which is the question this actually answers.
    pub reads: u64,
    /// Pages written. One `pwrite` each - copy-on-write never writes part of a page.
    pub writes: u64,
    /// Bytes written. Always `writes * PAGE_SIZE`, carried as a field so a dashboard does not
    /// have to know the page size to plot a byte rate.
    pub write_bytes: u64,
    /// Calls that extended the file. A commit does at most one, and only when the freelist
    /// could not satisfy it - so this rising while `free_pages_reusable` is non-zero means
    /// pages are being pinned faster than they are reused.
    pub grows: u64,
    /// Calls that shortened the file.
    pub truncates: u64,
    /// Flushes, of either strength: two per commit unless `Durability::None` is set, and none
    /// at all when it is. That makes this the number saying what the engine is *really*
    /// promising, as opposed to what it was configured to promise.
    ///
    /// **`Barrier` is not a smaller count.** It is a weaker *kind* of flush - `Store::flush`
    /// sends it to `sync_data` rather than `sync` - so it issues the same two, and what
    /// separates it from `Full` is how long they take rather than how many there are. That
    /// difference lands in `sync_nanos`, and on a platform where the two are the same call it
    /// does not land anywhere, because there it is not a difference.
    pub syncs: u64,
    /// Wall-clock spent inside those flushes. Divided by `syncs` it is the mean flush, and a
    /// commit that got slow almost always got slow here.
    pub sync_nanos: u64,
}

/// A backend that has not been asked yet, named so that the zeroes are attributable.
impl Default for IoStats {
    fn default() -> Self {
        Self {
            backend: "unknown",
            reads: 0,
            writes: 0,
            write_bytes: 0,
            grows: 0,
            truncates: 0,
            syncs: 0,
            sync_nanos: 0,
        }
    }
}

impl IoStats {
    /// Field-wise difference, for measuring one operation inside a longer session. `backend`
    /// is carried over rather than subtracted.
    ///
    /// Saturating, because the two snapshots are taken without a lock: a stripe read late in
    /// the earlier snapshot can exceed the same stripe read early in the later one, and a
    /// wrapped `u64` would publish a rate of eighteen quintillion.
    pub fn since(&self, earlier: &Self) -> Self {
        Self {
            backend: self.backend,
            reads: self.reads.saturating_sub(earlier.reads),
            writes: self.writes.saturating_sub(earlier.writes),
            write_bytes: self.write_bytes.saturating_sub(earlier.write_bytes),
            grows: self.grows.saturating_sub(earlier.grows),
            truncates: self.truncates.saturating_sub(earlier.truncates),
            syncs: self.syncs.saturating_sub(earlier.syncs),
            sync_nanos: self.sync_nanos.saturating_sub(earlier.sync_nanos),
        }
    }
}

/// One stripe of the read counter, alone on its cache line so that incrementing it never
/// invalidates a neighbour that a different core is incrementing.
#[repr(align(64))]
#[derive(Default)]
struct ReadStripe(AtomicU64);

/// The counters themselves. A backend owns one and calls into it from its trait methods.
///
/// `Relaxed` throughout: these are statistics, never a synchronisation point, and the
/// operations they describe are already ordered by the file underneath.
pub struct IoCounters {
    backend: &'static str,
    reads: Box<[ReadStripe]>,
    writes: AtomicU64,
    grows: AtomicU64,
    truncates: AtomicU64,
    syncs: AtomicU64,
    sync_nanos: AtomicU64,
}

impl std::fmt::Debug for IoCounters {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.snapshot().fmt(f)
    }
}

impl IoCounters {
    /// `backend` is the label every series it produces will carry, so it names the
    /// implementation rather than the file: `"mmap"`, not the path.
    pub fn new(backend: &'static str) -> Self {
        Self {
            backend,
            reads: (0..READ_STRIPES).map(|_| ReadStripe::default()).collect(),
            writes: AtomicU64::new(0),
            grows: AtomicU64::new(0),
            truncates: AtomicU64::new(0),
            syncs: AtomicU64::new(0),
            sync_nanos: AtomicU64::new(0),
        }
    }

    /// One page served. Striped by page number, which spreads a sequential scan across every
    /// stripe in turn without needing a thread-local to do it.
    pub fn read(&self, pgno: Pgno) {
        self.reads[pgno as usize % READ_STRIPES].0.fetch_add(1, Ordering::Relaxed);
    }

    /// One page written.
    pub fn write(&self) {
        self.writes.fetch_add(1, Ordering::Relaxed);
    }

    /// One call that actually extended the file. Not counted when the backend returned early
    /// because the file was already long enough - that is not I/O.
    pub fn grow(&self) {
        self.grows.fetch_add(1, Ordering::Relaxed);
    }

    /// One call that actually shortened the file.
    pub fn truncate(&self) {
        self.truncates.fetch_add(1, Ordering::Relaxed);
    }

    /// Runs a flush and times it. Wrapping rather than counting either side of the call so that
    /// a backend cannot count a flush it forgot to time, or time one it forgot to count.
    pub fn sync<R>(&self, flush: impl FnOnce() -> R) -> R {
        let started = Instant::now();
        let out = flush();
        // Saturating: a flush would have to block for 584 years to overflow, but it is free
        // here and a wrapped counter reads as a negative rate.
        let nanos = started.elapsed().as_nanos().min(u64::MAX as u128) as u64;
        self.syncs.fetch_add(1, Ordering::Relaxed);
        self.sync_nanos.fetch_add(nanos, Ordering::Relaxed);
        out
    }

    /// Everything, summed. Not atomic as a whole - the stripes are read one after another, so
    /// a scrape taken during heavy reading can land between two increments. That is the right
    /// trade for a counter published as a rate, and the alternative is a lock on the read path.
    pub fn snapshot(&self) -> IoStats {
        let writes = self.writes.load(Ordering::Relaxed);
        IoStats {
            backend: self.backend,
            reads: self.reads.iter().map(|s| s.0.load(Ordering::Relaxed)).sum(),
            writes,
            write_bytes: writes * PAGE_SIZE as u64,
            grows: self.grows.load(Ordering::Relaxed),
            truncates: self.truncates.load(Ordering::Relaxed),
            syncs: self.syncs.load(Ordering::Relaxed),
            sync_nanos: self.sync_nanos.load(Ordering::Relaxed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_spread_over_every_stripe_and_still_add_up() {
        let c = IoCounters::new("mmap");
        // Two full passes, so every stripe is hit twice and the sum has to cross all of them.
        for pgno in 0..(READ_STRIPES as Pgno * 2) {
            c.read(pgno);
        }
        assert_eq!(c.snapshot().reads, READ_STRIPES as u64 * 2);
    }

    #[test]
    fn a_flush_is_counted_and_timed_together() {
        let c = IoCounters::new("mmap");
        let out = c.sync(|| {
            std::thread::sleep(std::time::Duration::from_millis(2));
            7
        });
        assert_eq!(out, 7);
        let s = c.snapshot();
        assert_eq!(s.syncs, 1);
        assert!(s.sync_nanos >= 1_000_000, "a 2ms flush timed as {}ns", s.sync_nanos);
    }

    #[test]
    fn writes_carry_their_bytes() {
        let c = IoCounters::new("mmap");
        for _ in 0..5 {
            c.write();
        }
        let s = c.snapshot();
        assert_eq!(s.writes, 5);
        assert_eq!(s.write_bytes, 5 * PAGE_SIZE as u64);
    }

    #[test]
    fn since_subtracts_field_wise_and_keeps_the_name() {
        let c = IoCounters::new("mmap");
        c.read(0);
        let before = c.snapshot();
        c.read(1);
        c.write();
        let delta = c.snapshot().since(&before);
        assert_eq!(delta.backend, "mmap");
        assert_eq!(delta.reads, 1);
        assert_eq!(delta.writes, 1);
    }
}
