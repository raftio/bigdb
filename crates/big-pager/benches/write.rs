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

//! The write path.
//!
//! Every group that can run on both pagers runs on both. `MemPager` makes `sync` a no-op, so
//! the difference between the two lines is the price of durability and nothing else.

mod common;

use big_page::LeafBuilder;
use big_pager::*;
use common::*;
use criterion::{criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion};
use std::time::Duration;

/// Fragment counts for the scaling curve. Each fragment is a real page, so 10k is an 80 MB
/// fixture; that is already enough to show the shape.
const FRAGMENTS: [u64; 4] = [0, 100, 1_000, 10_000];

/// b1. What a commit costs when the transaction changed nothing. Three metadata chains are
/// rewritten wholesale regardless, so this is the floor every other write sits on top of.
fn empty_commit(c: &mut Criterion) {
    let mut g = c.benchmark_group("b1_empty_commit");

    let store = Store::open_or_init(MemPager::new()).unwrap();
    g.bench_function("mem", |b| b.iter(|| store.begin_write().commit().unwrap()));

    #[cfg(unix)]
    {
        let dir = tempfile::tempdir().unwrap();
        let store = mmap_store(dir.path(), 0);
        g.sample_size(20).measurement_time(Duration::from_secs(10));
        g.bench_function("mmap", |b| b.iter(|| store.begin_write().commit().unwrap()));
    }
    g.finish();
}

/// b2. Cost against the number of pages the transaction dirtied, which separates the fixed
/// part of a commit from the part that scales with actual work.
fn dirty_pages(c: &mut Criterion) {
    let mut g = c.benchmark_group("b2_dirty_pages");
    for k in [1u64, 16, 256, 4096] {
        // A fresh store per batch, otherwise the file grows without bound across iterations
        // and the measurement drifts into measuring allocation instead.
        g.bench_with_input(BenchmarkId::new("mem", k), &k, |b, &k| {
            b.iter_batched(
                || Store::open_or_init(MemPager::new()).unwrap(),
                |store| write_pages(&store, k),
                BatchSize::SmallInput,
            )
        });
    }
    g.finish();
}

/// b3. The one that matters: a commit touching exactly one fragment, against how many
/// fragments already exist. Flat would mean the metadata cost is proportional to the change.
/// Rising means it is proportional to the database.
fn commit_vs_fragments(c: &mut Criterion) {
    let mut g = c.benchmark_group("b3_commit_vs_fragments");

    for n in FRAGMENTS {
        let store = with_fragments(MemPager::new(), n);
        g.bench_with_input(BenchmarkId::new("mem", n), &n, |b, _| {
            b.iter(|| touch_one_or_empty(&store, n))
        });
    }

    #[cfg(unix)]
    {
        g.sample_size(20).measurement_time(Duration::from_secs(10));
        for n in FRAGMENTS {
            let dir = tempfile::tempdir().unwrap();
            let store = mmap_store(dir.path(), n);
            g.bench_with_input(BenchmarkId::new("mmap", n), &n, |b, _| {
                b.iter(|| touch_one_or_empty(&store, n))
            });
        }
    }
    g.finish();
}

/// With no fragments there is nothing to touch, so the probe degenerates to an empty commit.
/// That is the honest zero point of the curve rather than a missing data point.
fn touch_one_or_empty<P: PagerMut>(store: &Store<P>, n: u64) {
    if n == 0 {
        store.begin_write().commit().unwrap();
    } else {
        touch_one(store, 0);
    }
}

/// b4. `cow` holds every copied page in a `BTreeMap<Pgno, Page>` until commit, so a large
/// transaction is also a large allocation. This measures the copy, not the commit.
fn cow_growth(c: &mut Criterion) {
    let mut g = c.benchmark_group("b4_cow");
    for k in [1usize, 64, 1024] {
        g.bench_with_input(BenchmarkId::new("mem", k), &k, |b, &k| {
            b.iter_batched(
                || {
                    let store = with_fragments(MemPager::new(), k as u64);
                    let pages: Vec<Pgno> =
                        (0..k as u64).map(|s| store.roots().get(&key(s)).unwrap()).collect();
                    (store, pages)
                },
                |(store, pages)| {
                    let mut w = store.begin_write();
                    for p in pages {
                        let new = w.cow(p).unwrap();
                        w.write(new, LeafBuilder::new().finish(new)).unwrap();
                    }
                    w.dirty_len()
                },
                BatchSize::SmallInput,
            )
        });
    }
    g.finish();
}

/// b5. Allocation has two paths — reuse from the freelist, or extend the file — and they are
/// not the same price. A workload that never frees only ever sees the second.
fn alloc_paths(c: &mut Criterion) {
    let mut g = c.benchmark_group("b5_alloc");

    g.bench_function("tail_1024", |b| {
        b.iter_batched(
            || Store::open_or_init(MemPager::new()).unwrap(),
            |store| {
                let mut w = store.begin_write();
                for _ in 0..1024 {
                    w.alloc_tail().unwrap();
                }
                w.commit().unwrap()
            },
            BatchSize::SmallInput,
        )
    });

    g.bench_function("freelist_1024", |b| {
        b.iter_batched(
            || {
                // Build a store, then free everything so the next allocation has somewhere to
                // draw from. Without a reader pinning the horizon the runs become reusable.
                let store = with_fragments(MemPager::new(), 1024);
                let mut w = store.begin_write();
                for s in 0..1024u64 {
                    if let Some(p) = w.remove_root(&key(s)) {
                        w.free(p);
                    }
                }
                w.commit().unwrap();
                store
            },
            |store| {
                let mut w = store.begin_write();
                for _ in 0..1024 {
                    w.alloc().unwrap();
                }
                w.commit().unwrap()
            },
            BatchSize::SmallInput,
        )
    });
    g.finish();
}

criterion_group!(benches, empty_commit, dirty_pages, commit_vs_fragments, cow_growth, alloc_paths);
criterion_main!(benches);
