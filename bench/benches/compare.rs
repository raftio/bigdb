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

//! Timed comparison. The counted half lives in `src/bin/report.rs`, and is the half worth
//! trusting: these numbers describe one machine on one afternoon.
//!
//! **Ingest is measured small and reads are measured large, and that split is deliberate.** An
//! ingest benchmark rebuilds its store every iteration, so a workload sized for `redb` would
//! leave criterion collecting samples for an hour - that asymmetry is itself a result, and the
//! report explains it. A read benchmark builds its store once and then times a query against it,
//! so the size costs a fixed setup rather than a per-sample one. The read sweep therefore runs to
//! two million: the `count_ge` crossover sat between the two smallest sizes it used to measure,
//! which made the old top end the start of the interesting region rather than the end of it.

use big_bench::engines::big::{BigEngine, Default_};
use big_bench::engines::fjall::FjallEngine;
use big_bench::engines::lmdb::LmdbEngine;
use big_bench::engines::redb::RedbEngine;
use big_bench::engines::sqlite::SqliteEngine;
use big_bench::*;
use criterion::{criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion};
use std::hint::black_box;
use std::time::Duration;

const VALUE_CEILING: u64 = 1 << 20;
const INGEST_RECORDS: u64 = 1_000;
const QUERY_RECORDS: u64 = 20_000;
const K: u64 = VALUE_CEILING / 4 * 3;

/// Ingest, at matched durability. `redb` also runs relaxed, which is the knob `big` does not
/// have: the gap between redb's two lines is what that knob is worth.
fn ingest(c: &mut Criterion) {
    let mut g = c.benchmark_group("ingest");
    g.sample_size(10).measurement_time(Duration::from_secs(10));

    let records = workload(INGEST_RECORDS, Layout::Dense, VALUE_CEILING);
    for batch in [100usize, 1_000] {
        bench_ingest::<BigEngine>(&mut g, Durability::Full, batch, &records);
        bench_ingest::<RedbEngine>(&mut g, Durability::Full, batch, &records);
        bench_ingest::<RedbEngine>(&mut g, Durability::Relaxed, batch, &records);
    }
    g.finish();
}

fn bench_ingest<E: Engine>(
    g: &mut criterion::BenchmarkGroup<'_, criterion::measurement::WallTime>,
    durability: Durability,
    batch: usize,
    records: &[Record],
) {
    let id = BenchmarkId::new(format!("{}/{}", E::name(), durability.label()), batch);
    g.bench_with_input(id, &batch, |b, &batch| {
        b.iter_batched(
            // A fresh directory per iteration: ingesting into a store the previous iteration
            // already filled would measure growth, not ingest.
            || {
                let dir = tempfile::tempdir().unwrap();
                let engine = E::open(dir.path(), durability);
                (dir, engine)
            },
            |(dir, mut engine)| {
                for chunk in records.chunks(batch) {
                    engine.ingest(chunk);
                }
                drop(dir);
            },
            BatchSize::PerIteration,
        )
    });
}

/// The query both engines are built to answer, swept over corpus size.
///
/// One size cannot answer this. A bitmap pays a fixed cost to touch a bit plane and then
/// counts a whole word at a time; an ordered index pays per matching row. Whether that trade
/// is worth taking is a question about how many rows there are, so the sweep is the answer and
/// a single number is not.
fn query(c: &mut Criterion) {
    let mut g = c.benchmark_group("count_ge");
    // Up to two million, because the crossover this sweep exists to find sat between the
    // smallest two sizes it used to measure - which means the old top end was the *start* of
    // the interesting region, not the end of it. `redb` publishes numbers at 1M+, so stopping
    // below that was also comparing against a scale nobody else reports.
    for n in [20_000u64, 100_000, 400_000, 1_000_000, 2_000_000] {
        let records = workload(n, Layout::Dense, VALUE_CEILING);
        let expected = expected_count_ge(&records, K);

        bench_count_ge::<BigEngine>(&mut g, n, &records, expected);
        bench_count_ge::<RedbEngine>(&mut g, n, &records, expected);
        bench_count_ge::<LmdbEngine>(&mut g, n, &records, expected);
        bench_count_ge::<FjallEngine>(&mut g, n, &records, expected);
        bench_count_ge::<SqliteEngine>(&mut g, n, &records, expected);
    }
    g.finish();
}

/// One engine at one size, loaded and then timed.
///
/// The engine is kept alive by the temporary directory it was opened in, so both are held for
/// the whole measurement rather than dropped between iterations - a `count_ge` against a store
/// that was just rebuilt would measure the rebuild.
fn bench_count_ge<E: Engine>(
    g: &mut criterion::BenchmarkGroup<'_, criterion::measurement::WallTime>,
    n: u64,
    records: &[Record],
    expected: u64,
) {
    let dir = tempfile::tempdir().unwrap();
    // Reads are measured at relaxed durability throughout: how hard a commit flushed has no
    // bearing on how fast a read is, and full durability at two million records would spend the
    // whole benchmark fsyncing.
    let mut e = E::open(dir.path(), Durability::Relaxed);
    for chunk in records.chunks(10_000) {
        e.ingest(chunk);
    }
    e.checkpoint();
    assert_eq!(e.count_ge(K), expected, "{} must answer correctly before it is timed", E::name());

    g.bench_with_input(BenchmarkId::new(E::name(), n), &n, |b, _| {
        b.iter(|| black_box(e.count_ge(K)))
    });
    // Engine before directory: `fjall`'s background flusher writes an error to stderr if its
    // files vanish while it is alive.
    drop(e);
    drop(dir);
}

/// Point lookup, where neither engine's design is the point but a regression would still show.
fn point_get(c: &mut Criterion) {
    let mut g = c.benchmark_group("get");
    let records = workload(QUERY_RECORDS, Layout::Dense, VALUE_CEILING);

    let big_dir = tempfile::tempdir().unwrap();
    let mut big = BigEngine::<Default_>::open(big_dir.path(), Durability::Full);
    let redb_dir = tempfile::tempdir().unwrap();
    let mut redb = RedbEngine::open(redb_dir.path(), Durability::Relaxed);
    for chunk in records.chunks(1_000) {
        big.ingest(chunk);
        redb.ingest(chunk);
    }
    let probe = records[records.len() / 3].0;

    g.bench_function("big", |b| b.iter(|| black_box(big.get(probe))));
    g.bench_function("redb", |b| b.iter(|| black_box(redb.get(probe))));
    bench_get::<LmdbEngine>(&mut g, &records, probe);
    bench_get::<FjallEngine>(&mut g, &records, probe);
    bench_get::<SqliteEngine>(&mut g, &records, probe);
    g.finish();
}

fn bench_get<E: Engine>(
    g: &mut criterion::BenchmarkGroup<'_, criterion::measurement::WallTime>,
    records: &[Record],
    probe: u64,
) {
    let dir = tempfile::tempdir().unwrap();
    let mut e = E::open(dir.path(), Durability::Relaxed);
    for chunk in records.chunks(1_000) {
        e.ingest(chunk);
    }
    e.checkpoint();
    g.bench_function(E::name(), |b| b.iter(|| black_box(e.get(probe))));
    drop(e);
    drop(dir);
}

criterion_group!(benches, ingest, query, point_get);
criterion_main!(benches);
