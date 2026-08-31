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

//! Comparing `big` against other embedded engines.
//!
//! The rule this crate exists to enforce: every engine answers the *same* question, at the
//! *same* durability, over the *same* records. An engine that is allowed to skip an fsync its
//! rival performs is not being measured, it is being flattered.

#![deny(unsafe_code)]

pub mod cold;
pub mod engines;
pub mod olap;
pub mod wide;

use std::path::Path;

/// How hard the engine is made to work before it calls a commit done.
///
/// Every engine here implements both. `big` used to implement only `Full`, and the harness
/// reported that rather than hiding it by quietly running everything relaxed; it now has a
/// knob, and `Relaxed` maps to the level that makes the same trade its rivals do - survives the
/// process, not the machine.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Durability {
    /// Committed data survives power loss.
    Full,
    /// Committed data survives a process crash, not necessarily a power cut.
    Relaxed,
}

impl Durability {
    pub fn label(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Relaxed => "relaxed",
        }
    }
}

/// One fact: a record and the value of its single integer field.
pub type Record = (u64, u64);

/// How record ids are spread out.
///
/// `big` shards every 2^20 records and keeps metadata per fragment, so the same number of
/// records costs differently depending on how many shards they land in. Its rivals index a
/// key without caring where it sits, so this axis is invisible to them - which is exactly why
/// it belongs in a comparison rather than only in `big`'s own benchmarks.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Layout {
    /// Ids 0..n. One shard per million records.
    Dense,
    /// Ids spread across `shards` shards, the shape a sparse global id space really has.
    Sparse { shards: u64 },
}

impl Layout {
    /// How many fragments a commit of `batch` records touches under this layout.
    ///
    /// The denominator the amplification tables were missing. A commit's cost turns out to be
    /// roughly constant *per fragment it touches* - the copy-on-write rewrite of that
    /// fragment's root-to-leaf path - so a per-record figure conflates two different things
    /// and a per-commit figure conflates two others. This is the one that stays still.
    pub fn fragments_touched(self, batch: usize) -> u64 {
        match self {
            Self::Dense => 1,
            Self::Sparse { shards } => shards.min(batch as u64).max(1),
        }
    }

    pub fn label(self) -> String {
        match self {
            Self::Dense => "dense".to_string(),
            Self::Sparse { shards } => format!("sparse/{shards}"),
        }
    }
}

/// Records per shard in `big`. Duplicated here rather than imported so the workload generator
/// does not depend on the engine it is measuring.
pub const SHARD_WIDTH: u64 = 1 << 20;

/// Deterministic workload. A real RNG would make runs incomparable across machines for no
/// gain: what matters is that the values are not sorted and that every engine sees the same
/// ones.
pub fn workload(n: u64, layout: Layout, value_ceiling: u64) -> Vec<Record> {
    (0..n)
        .map(|i| {
            let id = match layout {
                Layout::Dense => i,
                Layout::Sparse { shards } => (i % shards) * SHARD_WIDTH + i / shards,
            };
            // Knuth's multiplicative hash: cheap, deterministic, and not monotonic in `i`,
            // so a range query cannot be accidentally answered by a prefix.
            let value = i.wrapping_mul(2_654_435_761) % value_ceiling;
            (id, value)
        })
        .collect()
}

/// Ground truth for `count_ge`, computed without any engine, so a wrong answer is a failed
/// benchmark rather than a fast one.
pub fn expected_count_ge(records: &[Record], k: u64) -> u64 {
    records.iter().filter(|(_, v)| *v >= k).count() as u64
}

/// What every engine in the comparison must be able to do.
///
/// Deliberately small. Anything richer would start favouring whichever engine happens to
/// expose it natively, which is how storage benchmarks usually end up meaningless.
// `is_empty` would be a method no benchmark calls, on a trait whose whole point is to stay
// small enough that no engine is favoured by what it happens to expose.
#[allow(clippy::len_without_is_empty)]
pub trait Engine: Sized {
    fn name() -> &'static str;

    /// Opens an engine in an empty directory.
    fn open(dir: &Path, durability: Durability) -> Self;

    /// Writes `batch` in exactly one transaction. Batch size is a benchmark axis, so the
    /// engine must not decide it internally.
    fn ingest(&mut self, batch: &[Record]);

    /// The value stored for `id`.
    fn get(&self, id: u64) -> Option<u64>;

    /// How many records hold a value of at least `k`.
    fn count_ge(&self, k: u64) -> u64;

    /// How many records the engine holds. Every peer has a cheap answer to this, which is why
    /// it belongs on the trait rather than being derived from a scan: an engine that has to
    /// count by reading everything is being measured on exactly that.
    fn len(&self) -> u64;

    /// Removes `ids` in exactly one transaction, and reports how many were actually there.
    ///
    /// Batched for the same reason `ingest` is: removal cost is per transaction in a
    /// copy-on-write engine and per key in an LSM, and letting each engine choose its own batch
    /// size would measure the choice rather than the engine.
    fn remove(&mut self, ids: &[u64]) -> u64;

    /// Returns free space to the filesystem, the way an operator would before measuring a file.
    ///
    /// Distinct from `checkpoint`, which only flushes what is deferred. This is the expensive
    /// whole-file rewrite, and the size taken after it is the `compacted size` row.
    fn compact(&mut self);

    /// Runs one point read per entry of `probes`, spread across `threads` threads, and returns
    /// how many found a value.
    ///
    /// On the trait rather than in the harness, because concurrency is an engine-level property
    /// and imposing one shape would measure the shape. `big`, `redb`, `lmdb` and `fjall` are all
    /// safe to read from several threads through one handle; SQLite is not, and opens a
    /// connection per thread - which is what its users do and therefore what it should be
    /// measured doing.
    ///
    /// Point reads rather than range queries: a range query over the same predicate returns the
    /// same answer from every thread, so an engine that memoised it would look infinitely
    /// scalable. Distinct probes cannot be shared.
    fn threaded_reads(&self, threads: usize, probes: &[u64]) -> u64;

    /// Closes and reopens against the same directory, so nothing this process cached survives.
    ///
    /// The half of a cold read that always works. Dropping the OS page cache needs root;
    /// dropping the engine's own memory - a mapping, a block cache, a page cache of its own -
    /// needs only this. Takes `self` by value because the old handle has to be *closed* first
    /// rather than merely unused: `big` and `lmdb` hold their files open and a second open
    /// against the same path would be a different measurement or an outright failure.
    ///
    /// Must be idempotent about schema: it is called against a directory that already has one.
    fn reopen(self, dir: &Path, durability: Durability) -> Self;

    /// Flushes whatever the engine defers, so a size taken afterwards is honest. An LSM
    /// measured before compaction looks smaller than it is.
    fn checkpoint(&mut self);

    /// Bytes the engine occupies on disk, after `checkpoint`.
    fn disk_bytes(&self) -> u64;

    /// Bytes the engine wrote, if it can say. Only `big` can, through `CountingPager`; the
    /// others would need OS-level tracing, so they return `None` rather than a guess.
    fn bytes_written(&self) -> Option<u64> {
        None
    }
}

/// Fans `probes` across `threads` scoped threads, calling `read` on each.
///
/// Shared by every engine whose handle is safe to read from concurrently. Scoped threads so the
/// engine is borrowed rather than shared through an `Arc`: nothing outlives the call, and an
/// `Arc` would put an atomic refcount in the middle of the thing being measured.
pub fn fan_out_reads<F>(threads: usize, probes: &[u64], read: F) -> u64
where
    F: Fn(u64) -> bool + Sync,
{
    if threads <= 1 {
        return probes.iter().filter(|p| read(**p)).count() as u64;
    }
    let chunk = probes.len().div_ceil(threads);
    std::thread::scope(|scope| {
        let handles: Vec<_> = probes
            .chunks(chunk)
            .map(|part| scope.spawn(|| part.iter().filter(|p| read(**p)).count() as u64))
            .collect();
        handles.into_iter().map(|h| h.join().expect("a read thread panicked")).sum()
    })
}

/// Total size of every file under `dir`, which is what "how much disk does this cost" means
/// to anyone operating it.
pub fn dir_size(dir: &Path) -> u64 {
    fn walk(dir: &Path, total: &mut u64) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for e in entries.flatten() {
            match e.file_type() {
                Ok(t) if t.is_dir() => walk(&e.path(), total),
                Ok(_) => *total += e.metadata().map(|m| m.len()).unwrap_or(0),
                Err(_) => {}
            }
        }
    }
    let mut total = 0;
    walk(dir, &mut total);
    total
}
