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

//! A pager decorator that tallies I/O instead of performing any of its own.
//!
//! Timing a commit measures the disk. Counting one measures the engine, and the count is the
//! same on every machine, which is what makes it assertable in a test rather than merely
//! reportable in a benchmark.
//!
//! It lives in this crate rather than alongside the comparison benchmarks because the engine's
//! own regression tests are its main users - `tests/amplification.rs` here, and the page-count
//! assertion in `big-db`. Moving it out would mean those tests depending on a crate that
//! depends on them. Feature-gated for the same reason `crash-injection` is: test support that
//! must not reach a release build.

use crate::error::Result;
use crate::pager::{Pager, PagerMut};
use big_page::{Page, Pgno, PAGE_SIZE};
use std::sync::atomic::{AtomicU64, Ordering};

/// A snapshot of the tally. Plain numbers, so a caller can subtract two of them.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct PagerCounts {
    pub reads: u64,
    pub writes: u64,
    pub grows: u64,
    pub truncates: u64,
    pub syncs: u64,
}

impl PagerCounts {
    /// What `writes` costs on disk. The only derived number worth having: everything else is
    /// a count of calls, and this is a count of bytes.
    pub fn bytes_written(&self) -> u64 {
        self.writes * PAGE_SIZE as u64
    }

    /// Field-wise difference, for measuring one operation inside a longer session.
    pub fn since(&self, earlier: &Self) -> Self {
        Self {
            reads: self.reads - earlier.reads,
            writes: self.writes - earlier.writes,
            grows: self.grows - earlier.grows,
            truncates: self.truncates - earlier.truncates,
            syncs: self.syncs - earlier.syncs,
        }
    }
}

/// Wraps any pager and counts what passes through. `Relaxed` throughout: these are statistics,
/// never a synchronisation point, and the operations they describe are already ordered by the
/// pager underneath.
#[derive(Default)]
pub struct CountingPager<P> {
    inner: P,
    reads: AtomicU64,
    writes: AtomicU64,
    grows: AtomicU64,
    truncates: AtomicU64,
    syncs: AtomicU64,
}

impl<P> CountingPager<P> {
    pub fn new(inner: P) -> Self {
        Self {
            inner,
            reads: AtomicU64::new(0),
            writes: AtomicU64::new(0),
            grows: AtomicU64::new(0),
            truncates: AtomicU64::new(0),
            syncs: AtomicU64::new(0),
        }
    }

    pub fn inner(&self) -> &P {
        &self.inner
    }

    pub fn counts(&self) -> PagerCounts {
        PagerCounts {
            reads: self.reads.load(Ordering::Relaxed),
            writes: self.writes.load(Ordering::Relaxed),
            grows: self.grows.load(Ordering::Relaxed),
            truncates: self.truncates.load(Ordering::Relaxed),
            syncs: self.syncs.load(Ordering::Relaxed),
        }
    }

    /// Zeroes the tally so a fixture's own I/O does not land in the measurement.
    pub fn reset(&self) {
        self.reads.store(0, Ordering::Relaxed);
        self.writes.store(0, Ordering::Relaxed);
        self.grows.store(0, Ordering::Relaxed);
        self.truncates.store(0, Ordering::Relaxed);
        self.syncs.store(0, Ordering::Relaxed);
    }
}

impl<P: Pager> Pager for CountingPager<P> {
    type Ref<'a>
        = P::Ref<'a>
    where
        Self: 'a;

    fn read(&self, pgno: Pgno) -> Result<Self::Ref<'_>> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        self.inner.read(pgno)
    }

    fn page_count(&self) -> u64 {
        self.inner.page_count()
    }

    fn capacity(&self) -> Option<u64> {
        self.inner.capacity()
    }

    /// Forwarded, not defaulted: a wrapper that silently recomputed would undo the inner
    /// pager's memo without saying so, and this wrapper is what the benchmarks measure.
    fn verify_bitmap(&self, pgno: Pgno, page: &Page, expected: u32) -> bool {
        self.inner.verify_bitmap(pgno, page, expected)
    }
}

impl<P: PagerMut> PagerMut for CountingPager<P> {
    fn write(&self, pgno: Pgno, page: &Page) -> Result<()> {
        self.writes.fetch_add(1, Ordering::Relaxed);
        self.inner.write(pgno, page)
    }

    fn grow(&self, page_count: u64) -> Result<()> {
        self.grows.fetch_add(1, Ordering::Relaxed);
        self.inner.grow(page_count)
    }

    fn truncate(&self, page_count: u64) -> Result<()> {
        self.truncates.fetch_add(1, Ordering::Relaxed);
        self.inner.truncate(page_count)
    }

    fn sync(&self) -> Result<()> {
        self.syncs.fetch_add(1, Ordering::Relaxed);
        self.inner.sync()
    }
}
