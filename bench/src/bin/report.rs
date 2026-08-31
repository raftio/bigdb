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

//! The half of the comparison that is not a stopwatch: space, write amplification, and
//! whether the engines actually agree on the answer.
//!
//! Run with `cargo run -p big-bench --release --bin report`.

use big_bench::cold::Coldness;
use big_bench::engines::big::{BigEngine, Default_, FIELD, TABLE};
use big_bench::engines::fjall::FjallEngine;
use big_bench::engines::lmdb::LmdbEngine;
use big_bench::engines::redb::RedbEngine;
use big_bench::engines::sqlite::SqliteEngine;
use big_bench::*;
use std::collections::BTreeSet;
use std::time::Instant;

const RECORDS: u64 = 100_000;
const BATCH: usize = 1_000;
const VALUE_CEILING: u64 = 1 << 20;
/// The predicate every engine is asked. Roughly a quarter of the records match.
const K: u64 = VALUE_CEILING / 4 * 3;

struct Row {
    engine: &'static str,
    durability: &'static str,
    layout: String,
    ingest_ms: u128,
    query_us: u128,
    len_us: u128,
    cold_us: u128,
    coldness: &'static str,
    disk_mib: f64,
    compacted_mib: f64,
    remove_ms: u128,
    written_mib: Option<f64>,
    matched: u64,
}

/// How many times a timed measurement is repeated before a figure is reported.
///
/// Three, and the middle one is taken. The same binary on the same idle machine an hour apart
/// varied 2.7x on a single-shot ingest while writing byte-for-byte identical data, so a single
/// shot was not measuring the engine - it was measuring what else the machine felt like doing.
/// Criterion reproduces within 12% because it does this and more; the single-shot half of the
/// harness did not do it at all.
///
/// Three rather than more because a run is minutes, not microseconds. A median of three
/// discards one outlier, which is the failure mode actually observed. It does not turn these
/// into precision measurements, and nothing here should be read as one.
const REPEATS: usize = 3;

/// The middle value. Takes the sorted middle rather than a mean on purpose: a mean lets the one
/// run that got descheduled drag the reported figure with it, which is the thing being fixed.
fn median<T: Copy + Ord>(mut xs: Vec<T>) -> T {
    xs.sort();
    xs[xs.len() / 2]
}

/// One engine, one layout, repeated. Timings are medians; everything else is deterministic and
/// comes from the last repeat, which is the same as every other repeat.
fn measure<E: Engine>(durability: Durability, layout: Layout, records: &[Record]) -> Row {
    let mut runs: Vec<Row> =
        (0..REPEATS).map(|_| measure_once::<E>(durability, layout, records)).collect();
    let ingest_ms = median(runs.iter().map(|r| r.ingest_ms).collect());
    let query_us = median(runs.iter().map(|r| r.query_us).collect());
    let len_us = median(runs.iter().map(|r| r.len_us).collect());
    let remove_ms = median(runs.iter().map(|r| r.remove_ms).collect());
    let cold_us = median(runs.iter().map(|r| r.cold_us).collect());
    let last = runs.pop().unwrap();
    Row { ingest_ms, query_us, len_us, remove_ms, cold_us, ..last }
}

fn measure_once<E: Engine>(durability: Durability, layout: Layout, records: &[Record]) -> Row {
    let dir = tempfile::tempdir().unwrap();
    let mut e = E::open(dir.path(), durability);

    let t0 = Instant::now();
    for batch in records.chunks(BATCH) {
        e.ingest(batch);
    }
    let ingest_ms = t0.elapsed().as_millis();

    e.checkpoint();

    let t1 = Instant::now();
    let matched = e.count_ge(K);
    let query_us = t1.elapsed().as_micros();

    // Timed separately from `count_ge` because they are different questions with different
    // costs: one filters, the other only counts. An engine that answers `len` off cached
    // metadata and `count_ge` by scanning would look identical if the two were folded together.
    let t2 = Instant::now();
    let len = e.len();
    let len_us = t2.elapsed().as_micros();
    assert_eq!(len, records.len() as u64, "{} disagreed on len", E::name());

    // A benchmark that measures a wrong answer is measuring nothing.
    let expected = expected_count_ge(records, K);
    assert_eq!(matched, expected, "{} disagreed on count_ge", E::name());
    let (probe_id, probe_value) = records[records.len() / 3];
    assert_eq!(e.get(probe_id), Some(probe_value), "{} disagreed on get", E::name());

    let disk_mib = e.disk_bytes() as f64 / (1 << 20) as f64;
    // Read before the reopen below, not after. A reopened engine is a new handle with a fresh
    // counter, so taking this afterwards reports zero bytes written for the one engine that can
    // report bytes written at all - which is exactly the kind of silent nonsense a benchmark
    // must not print.
    let written_mib = e.bytes_written().map(|b| b as f64 / (1 << 20) as f64);

    // The cold read, after every warm measurement above it: the point is a read that finds
    // nothing in memory, and everything before this is busy putting things there. A different
    // record from the warm probe, so no part of the answer is left over from it.
    let (cold_id, cold_value) = records[records.len() / 7];
    let mut e = e.reopen(dir.path(), durability);
    let coldness =
        if big_bench::cold::drop_page_cache() { Coldness::Cold } else { Coldness::Reopened };
    let t_cold = Instant::now();
    let got = e.get(cold_id);
    let cold_us = t_cold.elapsed().as_micros();
    assert_eq!(got, Some(cold_value), "{} disagreed on a cold get", E::name());

    // Removals last, because they change what every measurement above them would say. A tenth
    // of the corpus, in one transaction, chosen by stride so the deletions are spread across
    // every shard and every page rather than concentrated at one end of the key space.
    let doomed: Vec<u64> = records.iter().step_by(10).map(|(id, _)| *id).collect();
    let t3 = Instant::now();
    let removed = e.remove(&doomed);
    let remove_ms = t3.elapsed().as_millis();
    assert_eq!(removed, doomed.len() as u64, "{} disagreed on how many it removed", E::name());
    assert_eq!(e.len(), (records.len() - doomed.len()) as u64, "{} lost count", E::name());

    // And compaction after them, which is the only order in which the row means anything: a
    // compaction with nothing freed measures the copy, not the reclaim.
    e.compact();
    e.checkpoint();
    let compacted_mib = e.disk_bytes() as f64 / (1 << 20) as f64;

    Row {
        engine: E::name(),
        durability: durability.label(),
        layout: layout.label(),
        ingest_ms,
        query_us,
        len_us,
        cold_us,
        coldness: coldness.label(),
        disk_mib,
        compacted_mib,
        remove_ms,
        written_mib,
        matched,
    }
}

fn main() {
    println!("records: {RECORDS}, batch: {BATCH}, predicate: value >= {K}");
    println!("every timing below is the median of {REPEATS} runs; byte and size figures are single-shot and deterministic");
    println!("note: relaxed means the commit survives the process, not the machine\n");

    let mut rows = Vec::new();
    for layout in [Layout::Dense, Layout::Sparse { shards: 64 }] {
        let records = workload(RECORDS, layout, VALUE_CEILING);
        for durability in [Durability::Full, Durability::Relaxed] {
            rows.push(measure::<BigEngine<Default_>>(durability, layout, &records));
            rows.push(measure::<RedbEngine>(durability, layout, &records));
            rows.push(measure::<LmdbEngine>(durability, layout, &records));
            rows.push(measure::<FjallEngine>(durability, layout, &records));
            rows.push(measure::<SqliteEngine>(durability, layout, &records));
        }
    }

    println!(
        "{:<14} {:<9} {:<11} {:>10} {:>10} {:>9} {:>8} {:>10} {:>9} {:>12} {:>12} {:>9}",
        "engine",
        "durable",
        "layout",
        "ingest(ms)",
        "query(us)",
        "cold(us)",
        "len(us)",
        "remove(ms)",
        "disk(MiB)",
        "compact(MiB)",
        "written(MiB)",
        "matched"
    );
    for r in &rows {
        let written = r.written_mib.map_or("n/a".to_string(), |m| format!("{m:.1}"));
        println!(
            "{:<14} {:<9} {:<11} {:>10} {:>10} {:>9} {:>8} {:>10} {:>9.1} {:>12.1} {:>12} {:>9}",
            r.engine,
            r.durability,
            r.layout,
            r.ingest_ms,
            r.query_us,
            r.cold_us,
            r.len_us,
            r.remove_ms,
            r.disk_mib,
            r.compacted_mib,
            written,
            r.matched
        );
    }

    // Printed once from the rows rather than assumed, because whether the page cache could be
    // dropped is a property of who is running this, not of the build.
    let coldness = rows.first().map_or("warm", |r| r.coldness);
    let caveat = match coldness {
        "cold" => Coldness::Cold.caveat(),
        "reopened" => Coldness::Reopened.caveat(),
        _ => Coldness::Warm.caveat(),
    };
    println!(
        "\nwritten(MiB) is bytes actually pushed through the pager. Only big can report it;\n\
         redb would need OS-level tracing, so it says n/a rather than guessing.\n\
         \n\
         query(us) and len(us) are warm-cache numbers. cold(us) is a single point read taken\n\
         after the engine was closed and reopened: {coldness} - {caveat}.\n\
         \n\
         Every figure above is ONE NODE: one process, one file, no fan-out and no network.\n\
         That is fair - every peer here is an embedded engine with no distributed mode - but\n\
         it means nothing on this page measures the distribution big ships."
    );

    threaded_reads();
    amplification_vs_batch();
    where_the_bytes_go();
}

/// Which page class a commit's bytes actually go to.
///
/// The write-amplification tables above say how much a commit costs; this says what it bought.
/// The distinction matters because the two candidate explanations call for opposite fixes. If
/// the b-tree paths dominate, the answer is sub-page copy-on-write or a per-container delta -
/// less rewriting per touched container. If the fixed chains dominate, no amount of container
/// cleverness helps, because a commit rewrites the root records and the catalog **whole**
/// whenever any fragment moved or any zone map shifted - which is every write to a bit-sliced
/// index - and that cost scales with how many fragments the database *has*, not how many it
/// touched.
///
/// The checklist named the first. This measures rather than assuming, on both layouts and at
/// both ends of the batch curve, because the answer plainly differs between them.
fn where_the_bytes_go() {
    // Small on purpose. This section is about the *shape* of a commit, which a batch of one
    // shows at any corpus size - and a batch of one over twenty thousand records is twenty
    // thousand commits, which made this the slowest part of the whole report for a picture that
    // did not change after the first few hundred.
    const N: u64 = 2_000;

    println!("\nbig: where a commit's pages go, by class");
    println!(
        "{:>12} {:>8} {:>10} {:>8} {:>8} {:>9} {:>8} {:>9} {:>18}",
        "layout", "batch", "pages/cmt", "data", "roots", "catalog", "free", "data/frag", "dominant"
    );

    let (mut data_total, mut fixed_total) = (0u64, 0u64);
    // The rows where a commit carries more than a single record - the shapes a real ingest has,
    // and the ones the headline bytes/record figures come from.
    let (mut batched_data, mut batched_fixed) = (0u64, 0u64);
    for layout in [Layout::Dense, Layout::Sparse { shards: 64 }, Layout::Sparse { shards: 512 }] {
        for batch in [1usize, 100, 2_000] {
            let records = workload(N, layout, VALUE_CEILING);
            let dir = tempfile::tempdir().unwrap();
            let mut e = BigEngine::<Default_>::open(dir.path(), Durability::Relaxed);

            let before = e.bytes_written().unwrap();
            let mut commits = 0u64;
            // Totals over the whole ingest, not just the last commit: a single commit's shape
            // is what the row shows, but the share of the *bytes* is what any conclusion about
            // where to optimise has to rest on, and the two differ by a lot.
            let (mut all_data, mut all_fixed) = (0u64, 0u64);
            for c in records.chunks(batch) {
                e.ingest(c);
                commits += 1;
                let b = e.db().store().metrics().last_commit;
                all_data += b.data;
                all_fixed += b.roots + b.catalog + b.freelist + b.snapshots;
            }
            data_total += all_data;
            fixed_total += all_fixed;
            let written = e.bytes_written().unwrap() - before;

            // The last commit's shape, which is the steady state: the first few are atypical
            // because the file is still being laid out.
            let b = e.db().store().metrics().last_commit;
            let pages_per_commit = written as f64 / commits as f64 / 8192.0;
            let total = b.total().max(1) as f64;
            let dominant =
                if b.data as f64 / total > 0.5 { "b-tree paths" } else { "fixed chains" };
            // The column that makes the rest of the row legible. `data` alone grows with the
            // fan-out and tells you nothing you did not already know from the layout; divided
            // by the fragments the commit actually touched, it stops moving - which is the
            // finding: the cost is a fixed path rewrite per fragment, and a sparse layout is
            // expensive because it pays that floor for a handful of records rather than for a
            // batch of them.
            let per_frag = b.data as f64 / layout.fragments_touched(batch) as f64;
            println!(
                "{:>12} {:>8} {:>10.0} {:>8} {:>8} {:>9} {:>8} {:>9.1} {:>18}",
                layout.label(),
                batch,
                pages_per_commit,
                b.data,
                b.roots,
                b.catalog,
                b.freelist,
                per_frag,
                dominant
            );
            if batch > 1 {
                batched_data += all_data;
                batched_fixed += all_fixed;
            }
            drop(e);
            drop(dir);
        }
    }

    println!(
        "\n`data` is b-tree pages - leaves, branches and the bitmap pages dense containers own.\n\
         The rest are chains a commit rewrites *whole*: the root records whenever any fragment's\n\
         root moved, the catalog whenever any zone map shifted - which every bit-sliced write\n\
         does - and the freelist on every commit without exception. Those three scale with how\n\
         many fragments the database has, not with how many this commit touched, so batching\n\
         cannot amortise them."
    );

    // Derived, never asserted. An earlier version of this report hardcoded the conclusion that
    // the fixed chains were the cost, and a performance plan was written against that sentence
    // while the `dominant` column above disagreed in six rows of nine. Prose beside a
    // measurement goes stale; prose computed from it cannot.
    let share = |d: u64, f: u64| 100.0 * f as f64 / (d + f).max(1) as f64;
    let all = share(data_total, fixed_total);
    let batched = share(batched_data, batched_fixed);
    println!(
        "\nAcross every row above the fixed chains are {all:.0}% of the bytes written, and across\n\
         only the rows where a commit carries more than one record - the shape a real ingest has,\n\
         and where the bytes/record figures above come from - they are {batched:.0}%. {}",
        if batched < 10.0 {
            "So the sparse cost is\n\
             the b-tree paths: one root-to-leaf copy-on-write rewrite per fragment a commit\n\
             touches, paid whether that fragment received one record or a thousand. Committing\n\
             less often is what reduces it, which is what `Db::ingest` and `Db::bulk_load` are\n\
             for; making the fixed chains incremental would move a few per cent."
        } else {
            "So the fixed chains are\n\
             worth attacking directly, and making them incremental would pay."
        }
    );
}

/// How point reads scale across threads.
///
/// The row that could not be measured before, because the trait had no concurrent read. It is
/// the one place `big`'s design should show most clearly in its favour or against it: reads take
/// a snapshot and never block a writer, and the fragment fan-out is already threaded, so a
/// scaling curve is a claim the engine makes and this is what checks it.
///
/// Reported as reads per second rather than as elapsed time, so the numbers can be read across a
/// row without dividing by anything.
fn threaded_reads() {
    const N: u64 = 200_000;
    const PROBES: usize = 200_000;
    const THREADS: [usize; 5] = [1, 4, 8, 16, 32];

    let records = workload(N, Layout::Dense, VALUE_CEILING);
    // Probes drawn by stride rather than in order: a sequential walk would be answered by
    // whatever the previous read pulled into cache, which measures the cache and not the engine.
    let probes: Vec<u64> = (0..PROBES).map(|i| records[(i * 7919) % records.len()].0).collect();

    println!("\nreads per second by thread count ({N} records, dense, {PROBES} point reads)");
    print!("{:>8}", "engine");
    for t in THREADS {
        print!("{:>14}", format!("{t} thread(s)"));
    }
    println!("{:>10}", "scaling");

    threaded_row::<BigEngine<Default_>>(&records, &probes, &THREADS);
    threaded_row::<RedbEngine>(&records, &probes, &THREADS);
    threaded_row::<LmdbEngine>(&records, &probes, &THREADS);
    threaded_row::<FjallEngine>(&records, &probes, &THREADS);
    threaded_row::<SqliteEngine>(&records, &probes, &THREADS);

    println!(
        "\n`scaling` is the best thread count's throughput over the single thread's. A number\n\
         near 1 means reads are serialised somewhere; the machine's core count is the ceiling,\n\
         so read this as a shape rather than as a limit of the engine."
    );
}

fn threaded_row<E: Engine>(records: &[Record], probes: &[u64], threads: &[usize]) {
    let dir = tempfile::tempdir().unwrap();
    // Relaxed: how hard the load fsynced has no bearing on how fast a read is afterwards.
    let mut e = E::open(dir.path(), Durability::Relaxed);
    for chunk in records.chunks(10_000) {
        e.ingest(chunk);
    }
    e.checkpoint();

    print!("{:>8}", E::name());
    let mut rates = Vec::new();
    for &t in threads {
        // Warmed once at this thread count, so the first measurement does not pay for whatever
        // the others got for free.
        let _ = e.threaded_reads(t, &probes[..probes.len() / 10]);
        let t0 = Instant::now();
        let found = e.threaded_reads(t, probes);
        let elapsed = t0.elapsed();
        assert_eq!(found, probes.len() as u64, "{} lost reads at {t} threads", E::name());
        let per_sec = probes.len() as f64 / elapsed.as_secs_f64();
        rates.push(per_sec);
        print!("{:>14}", format!("{:.0}k", per_sec / 1_000.0));
    }
    let best = rates.iter().copied().fold(0.0f64, f64::max);
    println!("{:>10}", format!("{:.1}x", best / rates[0].max(1.0)));

    // Engine before directory. `fjall` runs a background flusher, and deleting the files out
    // from under it prints an error into the middle of the table - which is not a failure, but
    // it is noise in a document whose whole value is that it can be read.
    drop(e);
    drop(dir);
}

/// One point on the write-amplification curve.
struct Amp {
    batch: usize,
    commits: usize,
    /// Records this batch puts into a single fragment, which is what actually gets amortised.
    /// A batch spread across many shards is a small batch as far as the engine is concerned.
    recs_per_frag: f64,
    bytes_per_record: f64,
    total_mib: u64,
    elapsed_ms: u128,
    us_per_record: f64,
}

/// Where the write amplification lives.
///
/// If the cost were the per-commit rewrite, bytes-per-record would fall as batches grow,
/// because one commit's fixed cost gets shared by more records. If it stays flat, the cost is
/// per record and batching cannot help it. Both are plausible in advance, so this reads the
/// answer off the curve rather than asserting one: a hardcoded conclusion is exactly how a
/// benchmark ends up contradicting its own output once the engine improves underneath it.
///
/// Both layouts get a curve. Running only the dense one would leave the single layout that
/// actually hurts with no measurement at all, which is how the sparse penalty stayed a
/// footnote instead of a number.
fn amplification_vs_batch() {
    const N: u64 = 10_000;
    const BATCHES: [usize; 5] = [1, 10, 100, 1_000, 10_000];

    let dense = sweep(N, Layout::Dense, &BATCHES);
    let sparse = sweep(N, Layout::Sparse { shards: 64 }, &BATCHES);
    fan_out_penalty(&dense, &sparse);
    buffered_ingest();
    buffer_sweep();
}

/// One layout's whole curve, printed as it is measured.
fn sweep(n: u64, layout: Layout, batches: &[usize]) -> Vec<Amp> {
    println!("\nbig: write amplification against batch size ({n} records, {})", layout.label());
    println!(
        "{:>10} {:>10} {:>11} {:>14} {:>12} {:>10} {:>12}",
        "batch", "commits", "recs/frag", "bytes/record", "total(MiB)", "time(ms)", "us/record"
    );

    let mut rows = Vec::new();
    for &batch in batches {
        let records = workload(n, layout, VALUE_CEILING);

        // Bytes are taken once and timings three times, because they need different treatment:
        // the byte figure is identical on every run and every machine - it is gated in
        // `crates/big-db/tests/amplification.rs` for exactly that reason - while the timing is
        // the half that moved 2.7x between two runs of this binary.
        let mut written = 0;
        let mut times = Vec::with_capacity(REPEATS);
        for _ in 0..REPEATS {
            let dir = tempfile::tempdir().unwrap();
            let mut e = BigEngine::<Default_>::open(dir.path(), Durability::Full);
            let before = e.bytes_written().unwrap();
            let t0 = Instant::now();
            for c in records.chunks(batch) {
                e.ingest(c);
            }
            times.push(t0.elapsed());
            written = e.bytes_written().unwrap() - before;
        }
        let elapsed = median(times);

        // Measured off the real ids rather than derived from `layout`, so this stays honest if
        // the workload generator ever changes shape underneath it.
        let spread = records.chunks(batch).next().map_or(1, shards_in);
        let row = Amp {
            batch,
            commits: n as usize / batch,
            recs_per_frag: batch as f64 / spread as f64,
            bytes_per_record: written as f64 / n as f64,
            total_mib: written / (1 << 20),
            elapsed_ms: elapsed.as_millis(),
            us_per_record: elapsed.as_micros() as f64 / n as f64,
        };
        println!(
            "{:>10} {:>10} {:>11.1} {:>14.0} {:>12} {:>10} {:>12.0}",
            row.batch,
            row.commits,
            row.recs_per_frag,
            row.bytes_per_record,
            row.total_mib,
            row.elapsed_ms,
            row.us_per_record
        );
        rows.push(row);
    }
    verdict(&rows);
    rows
}

/// Distinct shards a batch lands in. Every chunk of this workload has the same shape, so the
/// first one answers for all of them.
fn shards_in(chunk: &[Record]) -> usize {
    chunk.iter().map(|(id, _)| id / SHARD_WIDTH).collect::<BTreeSet<_>>().len()
}

/// Says which of the two stories the curve actually told, in the numbers it told it in.
///
/// Two fsyncs per commit is the floor for a crash-safe copy-on-write engine: data must reach
/// the disk before the meta page that points at it flips. Whether that floor is what dominates
/// is a question about the shape above, not something to state in advance.
fn verdict(rows: &[Amp]) {
    let (first, last) = match (rows.first(), rows.last()) {
        (Some(f), Some(l)) if rows.len() > 1 => (f, l),
        _ => return,
    };

    let commit_drop = first.commits as f64 / last.commits.max(1) as f64;
    let byte_drop = first.bytes_per_record / last.bytes_per_record.max(f64::MIN_POSITIVE);
    let time_drop = first.us_per_record / last.us_per_record.max(f64::MIN_POSITIVE);

    println!(
        "\nFrom batch {} to batch {}, commits fell {:.0}x; bytes/record fell {:.0}x and \
         us/record fell {:.0}x.",
        first.batch, last.batch, commit_drop, byte_drop, time_drop
    );

    // The threshold is deliberately loose. Telling "scales with commits" apart from "does not
    // scale with commits" needs an order of magnitude, not a decimal place, and a tighter test
    // would flip on noise between one run and the next.
    if byte_drop < 2.0 {
        println!(
            "Flat: batching does not help here, because the cost is paid per container written\n\
             rather than per commit. A BSI write touches one container per bit plane, so a\n\
             single record rewrites its root-to-leaf path once for each of them."
        );
    } else if byte_drop * 10.0 >= commit_drop {
        println!(
            "Per-commit dominated: the write cost tracks the commit count to within an order\n\
             of magnitude, so batching is the lever. Each commit rewrites every container it\n\
             touched in full - copy-on-write has no smaller unit - and a container costs the\n\
             same whether one record in it changed or ten thousand did."
        );
    } else {
        println!(
            "Mixed: batching recovers some of the cost but not in proportion to the commits it\n\
             removes, so a per-record floor is showing through underneath the per-commit one."
        );
    }
}

/// What spreading the same batch across shards costs, against what the model says it should.
///
/// The claim being tested is that batch size is the wrong knob: what the engine amortises is
/// records per fragment per commit, so a batch spread `f` ways should cost about `f` times
/// more per record. Printing both columns is what turns that from an assertion into a check.
fn fan_out_penalty(dense: &[Amp], sparse: &[Amp]) {
    println!("\nbig: what shard fan-out costs (same batch, dense against sparse/64)");
    println!(
        "{:>10} {:>12} {:>13} {:>13} {:>14} {:>10} {:>10}",
        "batch", "dense r/f", "sparse r/f", "dense B/rec", "sparse B/rec", "fan-out", "penalty"
    );

    let mut worst_gap = 0.0f64;
    for (d, s) in dense.iter().zip(sparse) {
        let fan_out = d.recs_per_frag / s.recs_per_frag.max(f64::MIN_POSITIVE);
        let penalty = s.bytes_per_record / d.bytes_per_record.max(f64::MIN_POSITIVE);
        println!(
            "{:>10} {:>12.1} {:>13.1} {:>13.0} {:>14.0} {:>10.1} {:>10.1}",
            d.batch,
            d.recs_per_frag,
            s.recs_per_frag,
            d.bytes_per_record,
            s.bytes_per_record,
            fan_out,
            penalty
        );
        // Only the batches that actually spread tell us anything: at batch 1 both layouts put
        // one record in one fragment, so the ratio is 1 by construction, not by measurement.
        if fan_out > 1.5 {
            worst_gap = worst_gap.max((penalty / fan_out).max(fan_out / penalty));
        }
    }

    if worst_gap == 0.0 {
        println!("\nNo batch in this sweep spread across more than one fragment; nothing to say.");
    } else if worst_gap <= 3.0 {
        println!(
            "\nPenalty tracks fan-out to within {worst_gap:.1}x across the sweep, which is the\n\
             model holding: cost follows records per fragment per commit, not records per\n\
             commit. Raising the batch size on a sparse id space buys almost nothing, because\n\
             the extra records land in new fragments rather than in fuller ones."
        );
    } else {
        println!(
            "\nPenalty and fan-out disagree by up to {worst_gap:.1}x, so records per fragment is\n\
             not the whole story here and something else is moving. Worth a look before\n\
             optimising against the model."
        );
    }
}

/// What buffering across calls is worth, when the caller does not get to choose the batch.
///
/// The curves above vary the batch because that is the only knob the comparison trait has, but
/// a real ingest usually cannot: records arrive in whatever size they arrive in. `Db::ingest`
/// separates the two, so the caller keeps handing over small batches while the engine still
/// commits large ones. The baseline row is the same records with a commit per caller batch.
fn buffered_ingest() {
    const N: u64 = 10_000;
    const CALLER_BATCH: usize = 100;
    let layout = Layout::Sparse { shards: 64 };
    let records = workload(N, layout, VALUE_CEILING);

    println!(
        "\nbig: buffered ingest against a commit per caller batch ({N} records, {}, caller \
         hands over {CALLER_BATCH} at a time)",
        layout.label()
    );
    println!(
        "{:>12} {:>10} {:>11} {:>14} {:>12} {:>10}",
        "buffer", "commits", "recs/frag", "bytes/record", "total(MiB)", "time(ms)"
    );

    let spread = records.chunks(CALLER_BATCH).next().map_or(1, shards_in);
    let baseline = commit_per_batch(&records, CALLER_BATCH);
    print_ingest_row("none", baseline, CALLER_BATCH as f64 / spread as f64, N);

    let mut best = baseline;
    for capacity in [1_000usize, 10_000] {
        let run = through_buffer(&records, CALLER_BATCH, capacity);
        let spread = records.chunks(capacity).next().map_or(1, shards_in);
        print_ingest_row(&capacity.to_string(), run, capacity as f64 / spread as f64, N);
        if run.1 < best.1 {
            best = run;
        }
    }

    let bulk = through_bulk_load(&records);
    print_ingest_row("bulk load", bulk, N as f64 / shards_in(&records) as f64, N);

    let saved = baseline.1 as f64 / (best.1 as f64).max(1.0);
    println!(
        "\nBuffering cut bytes written {saved:.0}x without the caller changing how it hands over\n\
         records, which is the point: the batch a caller can offer and the batch the engine\n\
         wants to commit are not the same number, and only one of them is negotiable."
    );

    // Read this row carefully rather than as a ranking. A buffer of 10,000 against a load of
    // 10,000 commits once, which already makes every fragment disjoint - it is a bulk load
    // with extra steps, and only available to a caller who knew the size in advance. The bulk
    // loader is slightly *behind* it here because it also pays a catalog commit and splits on a
    // page budget. The comparison that means something is the one below, where the buffer
    // cannot hold the load.
    bulk_load_against_a_smaller_buffer();
}

/// The curve a user actually has to configure against: capacity, across shard counts.
///
/// The previous version of this section measured one caller batch size, one layout and two
/// capacities - which is enough to show that buffering helps and not enough to tell anyone what
/// to set. The knob is `capacity / shards`, not `capacity`: a buffer of ten thousand is a buffer
/// of ten thousand per fragment when the ids are dense and a buffer of a hundred and fifty-six
/// when they land in sixty-four shards. This is the table that says so.
fn buffer_sweep() {
    const N: u64 = 50_000;
    const CALLER_BATCH: usize = 100;
    const CAPACITIES: [usize; 4] = [1_000, 5_000, 25_000, 50_000];
    const SHARDS: [u64; 4] = [1, 8, 64, 512];

    println!(
        "\nbig: buffered ingest, capacity against shard count ({N} records, caller hands over \
         {CALLER_BATCH} at a time)"
    );
    println!("bytes/record, with records per fragment per commit in brackets");
    print!("{:>10}", "capacity");
    for shards in SHARDS {
        print!("{:>20}", format!("{shards} shard(s)"));
    }
    println!();

    // Every cell, kept so the verdict below is read off the measurements rather than asserted
    // over them. The last time this section stated a model in prose, the model was wrong and
    // the table underneath it said so for anyone who checked.
    let mut cells: Vec<Cell> = Vec::new();

    for capacity in CAPACITIES {
        print!("{capacity:>10}");
        for shards in SHARDS {
            let layout = if shards == 1 { Layout::Dense } else { Layout::Sparse { shards } };
            let records = workload(N, layout, VALUE_CEILING);
            let run = through_buffer(&records, CALLER_BATCH, capacity);
            let spread = records.chunks(capacity).next().map_or(1, shards_in);
            let per_record = run.1 as f64 / N as f64;
            let per_frag = capacity as f64 / spread as f64;
            print!("{:>20}", format!("{per_record:.0} ({per_frag:.0})"));
            cells.push((capacity, shards, per_frag, per_record));
        }
        println!();
    }

    buffer_verdict(&cells);
}

/// One cell of the capacity x shard grid: `(capacity, shards, records per fragment per commit,
/// bytes per record)`.
type Cell = (usize, u64, f64, f64);

/// What the table above actually shows, derived from it.
///
/// The obvious model - "records per fragment per commit is the cost" - is the one to test rather
/// than to state, because two cells can agree on that figure and disagree on the cost. When they
/// do, the reason is what a fragment rewrite costs rather than how many there were: a fragment
/// holding fifty thousand records has dense containers, each owning a whole page that
/// copy-on-write rewrites in full, while a fragment holding a few hundred keeps them as arrays
/// inline in a leaf. Same number of rewrites, two orders of magnitude apart in what each one
/// costs. `where_the_bytes_go` below is the measurement that says so.
fn buffer_verdict(cells: &[Cell]) {
    // The pair whose records-per-fragment agree most closely while their costs agree least.
    let mut worst: Option<(&Cell, &Cell, f64)> = None;
    for a in cells {
        for b in cells {
            if a.0 == b.0 && a.1 == b.1 {
                continue;
            }
            let frag_ratio = (a.2 / b.2).max(b.2 / a.2);
            if frag_ratio > 1.5 {
                continue;
            }
            let cost_ratio = (a.3 / b.3).max(b.3 / a.3);
            if worst.is_none_or(|(_, _, w)| cost_ratio > w) {
                worst = Some((a, b, cost_ratio));
            }
        }
    }

    println!(
        "\nRead down a column and the cost falls with capacity; read across a row and it rises\n\
         with shard count. Both move the same underlying quantity - records per fragment per\n\
         commit, in brackets - so that is the first number to reach for when sizing a buffer:\n\
         decide how many records per fragment per commit you can afford, then multiply by\n\
         however many shards your ids actually spread across."
    );

    match worst {
        Some((a, b, ratio)) if ratio > 2.0 => println!(
            "\nBut it is not the whole model, and this table says so. capacity={} across {}\n\
             shard(s) and capacity={} across {} shard(s) put {:.0} and {:.0} records in each\n\
             fragment per commit - within {:.0}% of each other - and yet cost {:.0} and {:.0}\n\
             bytes per record, a factor of {ratio:.1}. The missing variable is not how many\n\
             fragment rewrites there were but what each one cost: a fragment holding tens of\n\
             thousands of records has dense containers, each owning a page that copy-on-write\n\
             rewrites whole, where a fragment holding a few hundred keeps them inline in a leaf.\n\
             So a buffer sized in records per fragment is the right first move and cannot be the\n\
             last one - see `where a commit's pages go` below.",
            a.0,
            a.1,
            b.0,
            b.1,
            a.2,
            b.2,
            ((a.2 / b.2).max(b.2 / a.2) - 1.0) * 100.0,
            a.3,
            b.3,
        ),
        _ => println!(
            "\nAnd on this run it is the whole model: every pair of cells that agreed on records\n\
             per fragment agreed on cost to within a factor of two."
        ),
    }
}

/// The case a bulk load is actually for: more records than any buffer will hold.
///
/// A buffer makes commits fewer; it cannot make them disjoint. Each one still carries a slice of
/// every shard, so every commit after the first finds each fragment already rooted and rewrites
/// its root-to-leaf path. Grouping by fragment first is what removes that, and it only shows up
/// once the load outgrows the buffer - which is why measuring it at `N = capacity`, as the table
/// above does, would have reported the opposite conclusion.
fn bulk_load_against_a_smaller_buffer() {
    const N: u64 = 200_000;
    const CAPACITY: usize = 10_000;
    let layout = Layout::Sparse { shards: 64 };
    let records = workload(N, layout, VALUE_CEILING);

    println!(
        "\nbig: bulk load against a buffer too small to hold the load ({N} records, {}, buffer \
         {CAPACITY})",
        layout.label()
    );
    println!(
        "{:>12} {:>10} {:>11} {:>14} {:>12} {:>10}",
        "path", "commits", "recs/frag", "bytes/record", "total(MiB)", "time(ms)"
    );

    let spread = records.chunks(CAPACITY).next().map_or(1, shards_in);
    let buffered = through_buffer(&records, 100, CAPACITY);
    print_ingest_row("buffer", buffered, CAPACITY as f64 / spread as f64, N);

    let bulk = through_bulk_load(&records);
    print_ingest_row("bulk load", bulk, N as f64 / shards_in(&records) as f64, N);

    let ratio = buffered.1 as f64 / (bulk.1 as f64).max(1.0);
    println!(
        "\nThe bulk load writes {ratio:.0}x fewer bytes. Both store the same records; the\n\
         difference is entirely that one commits in arrival order and the other does not.\n\
         The trade is memory: the whole load is held before any of it is written, which is why\n\
         a buffered ingest remains the right tool for a stream that does not end."
    );
}

/// Everything at once, grouped by fragment before anything is written.
fn through_bulk_load(records: &[Record]) -> (u64, u64, u128) {
    let dir = tempfile::tempdir().unwrap();
    let e = BigEngine::<Default_>::open(dir.path(), Durability::Full);
    let before = e.bytes_written().unwrap();
    let t0 = Instant::now();
    let mut bulk = e.db().bulk_load(TABLE).unwrap();
    for (id, value) in records {
        bulk.set_int(FIELD, *id, *value).unwrap();
    }
    bulk.finish().unwrap();
    let elapsed = t0.elapsed().as_millis();
    // Commit count is not reported by the loader; it is bounded by its page budget rather than
    // chosen, so a number here would invite a comparison the row is not making.
    (0, e.bytes_written().unwrap() - before, elapsed)
}

/// `(commits, bytes written, elapsed)` for the same records committed once per caller batch.
fn commit_per_batch(records: &[Record], batch: usize) -> (u64, u64, u128) {
    let dir = tempfile::tempdir().unwrap();
    let mut e = BigEngine::<Default_>::open(dir.path(), Durability::Full);
    let before = e.bytes_written().unwrap();
    let t0 = Instant::now();
    for c in records.chunks(batch) {
        e.ingest(c);
    }
    let elapsed = t0.elapsed().as_millis();
    (records.len().div_ceil(batch) as u64, e.bytes_written().unwrap() - before, elapsed)
}

/// The same records, handed over in the same caller batches, but committed by the buffer.
fn through_buffer(records: &[Record], batch: usize, capacity: usize) -> (u64, u64, u128) {
    let dir = tempfile::tempdir().unwrap();
    let e = BigEngine::<Default_>::open(dir.path(), Durability::Full);
    let before = e.bytes_written().unwrap();
    let t0 = Instant::now();
    let commits = {
        let mut ingest = e.db().ingest(capacity);
        for c in records.chunks(batch) {
            for (id, value) in c {
                ingest.set_int(TABLE, FIELD, *id, *value).unwrap();
            }
        }
        ingest.flush().unwrap();
        let commits = ingest.commits();
        ingest.finish().unwrap();
        commits
    };
    let elapsed = t0.elapsed().as_millis();
    (commits, e.bytes_written().unwrap() - before, elapsed)
}

fn print_ingest_row(label: &str, run: (u64, u64, u128), recs_per_frag: f64, n: u64) {
    let (commits, written, elapsed) = run;
    println!(
        "{:>12} {:>10} {:>11.1} {:>14.0} {:>12} {:>10}",
        label,
        commits,
        recs_per_frag,
        written as f64 / n as f64,
        written / (1 << 20),
        elapsed
    );
}
