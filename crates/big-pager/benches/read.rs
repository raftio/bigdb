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

//! The read path.
//!
//! There is no buffer pool to warm and no deserialisation step, so what is left to measure is
//! the page fault, the bounds check, and whatever the reader registry costs under contention.

mod common;

use big_pager::*;
use common::*;
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use std::hint::black_box;

const PAGES: u64 = 4_096;

/// Deterministic stand-in for a random access pattern. A real RNG would be one more
/// dependency for something whose only requirement is not to be sequential.
fn scramble(i: u64, n: u64) -> u64 {
    // An odd stride coprime with any power of two visits every slot exactly once.
    (i.wrapping_mul(2_654_435_761) % n).max(1)
}

/// b6. Sequential against scattered, on a mapping the process has already touched.
fn read_patterns(c: &mut Criterion) {
    let mut g = c.benchmark_group("b6_read");

    let store = with_fragments(MemPager::new(), PAGES);
    g.bench_function("mem_sequential", |b| {
        b.iter(|| {
            let t = store.begin_read();
            for p in 1..PAGES {
                black_box(t.read(p as Pgno).unwrap().pgno());
            }
        })
    });

    #[cfg(unix)]
    {
        let dir = tempfile::tempdir().unwrap();
        let store = mmap_store(dir.path(), PAGES);

        g.bench_function("mmap_sequential", |b| {
            b.iter(|| {
                let t = store.begin_read();
                for p in 1..PAGES {
                    black_box(t.read(p as Pgno).unwrap().pgno());
                }
            })
        });

        g.bench_function("mmap_scattered", |b| {
            b.iter(|| {
                let t = store.begin_read();
                for i in 1..PAGES {
                    black_box(t.read(scramble(i, PAGES) as Pgno).unwrap().pgno());
                }
            })
        });

        // A mapping this process has never faulted in. The file is still in the OS cache, so
        // this is the cost of populating page tables, not the cost of reaching the disk.
        // Measuring the latter honestly needs the cache purged, which needs root.
        //
        // Its own file, and the store that built it is dropped first: the pager takes an
        // exclusive lock, so a second handle on a file another one still holds is `Locked`.
        // `PerIteration` for the same reason - a batch of setups would hold several at once.
        let cold = tempfile::tempdir().unwrap();
        let cold_path = cold.path().join("cold.big");
        drop(with_fragments(MmapPager::open_default(&cold_path).unwrap(), PAGES));

        g.bench_function("mmap_fresh_map", |b| {
            b.iter_batched(
                || MmapPager::open_default(&cold_path).unwrap(),
                |pager| {
                    for p in 1..PAGES {
                        black_box(pager.read(p as Pgno).unwrap().pgno());
                    }
                },
                criterion::BatchSize::PerIteration,
            )
        });
    }
    g.finish();
}

/// b8. `begin_read` registers the reader in a mutex-guarded map so the freelist knows what it
/// may not reclaim. Cheap per transaction, but every reader in the process contends for it.
fn reader_registry(c: &mut Criterion) {
    let mut g = c.benchmark_group("b8_readers");
    let store = with_fragments(MemPager::new(), 64);

    g.bench_function("begin_read_single", |b| b.iter(|| black_box(store.begin_read().txn_id())));

    for threads in [2usize, 4, 8] {
        g.bench_with_input(BenchmarkId::new("begin_read_threads", threads), &threads, |b, &t| {
            b.iter(|| {
                std::thread::scope(|s| {
                    for _ in 0..t {
                        s.spawn(|| {
                            for _ in 0..64 {
                                black_box(store.begin_read().txn_id());
                            }
                        });
                    }
                })
            })
        });
    }
    g.finish();
}

criterion_group!(benches, read_patterns, reader_registry);
criterion_main!(benches);
