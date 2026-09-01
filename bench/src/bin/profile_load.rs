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

//! Where does the time in a load actually go?
//!
//! The OLAP report says how long a corpus took to load and nothing about *which part* of the
//! load that was. The number it produces is the sum of four things that respond to entirely
//! different fixes, and a summary that adds them together cannot tell you which one to reach
//! for:
//!
//! - **The caller.** Building the facts at all - a `format!` per key is the caller's cost, not
//!   the engine's, and it is charged to the engine by every timer that wraps the loop.
//! - **Buffering.** What [`Ingest`] does per fact before anything is written.
//! - **The replay and the commit.** Walking the buffer into fragments and rewriting the
//!   containers it touched.
//! - **Durability.** Two fsyncs per commit, which on macOS are `F_FULLFSYNC` and cost
//!   milliseconds each.
//!
//! So each line below moves exactly one of them. The distance between two adjacent lines is
//! what that one variable costs, which is the only form of this measurement anybody can act on.
//!
//! ```text
//! cargo run --release -p big-bench --bin profile_load -- 2000000
//! ```
//!
//! [`Ingest`]: big_db::Ingest

use big_bench::wide::{wide_stream, WideRecord};
use big_bench::Layout;
use big_db::catalog::{FieldKind, TableEngine};
use big_db::{Db, Durability};
use big_pager::MmapPager;
use std::hint::black_box;
use std::time::Instant;

const TABLE: &str = "t";
const AMOUNT: &str = "amount";
const CATEGORY: &str = "category";
const COUNTRY: &str = "country";
const ACTIVE: &str = "active";
const BIT_DEPTH: u32 = 20;

/// Facts per record: an amount, two keys and a flag. The OLAP corpus's shape.
const FACTS_PER_RECORD: u64 = 4;

/// What one record costs `bulk_load` in memory: the fact, and again in the plan it groups the
/// facts into. Measured generously on purpose - being wrong low here is an OOM kill.
const BULK_BYTES_PER_RECORD: u64 = 300;

/// A ceiling whatever the machine, so a large box does not spend an hour on the one path whose
/// answer stops being interesting once it is clearly the fastest.
const BULK_CEILING: u64 = 100_000_000;

/// How many records `bulk_load` may be given here.
///
/// It holds the entire load in memory before it writes any of it - that is the trade it exists
/// to make, not an oversight - so it is the one path a streaming corpus does not rescue. Asking
/// the machine rather than hard-coding a constant, because the constant that is safe on a
/// 4 GiB droplet wastes a 64 GiB one, and the failure when it is too high is the OOM killer
/// arriving twenty minutes into a run.
///
/// Half of what is available, not all: the engine wants its own page cache, and a benchmark
/// that pushes the box into swap is measuring swap.
fn bulk_max() -> u64 {
    let budget = available_bytes() / 2;
    (budget / BULK_BYTES_PER_RECORD).min(BULK_CEILING)
}

/// Memory this process can expect to get, in bytes. Zero when it cannot be determined, which
/// callers read as "no ceiling I can justify".
fn available_bytes() -> u64 {
    #[cfg(target_os = "linux")]
    {
        // MemAvailable rather than MemFree: the kernel's own estimate of what is obtainable
        // without swapping, which is the question being asked.
        if let Ok(text) = std::fs::read_to_string("/proc/meminfo") {
            for line in text.lines() {
                if let Some(rest) = line.strip_prefix("MemAvailable:") {
                    if let Some(kb) = rest.split_whitespace().next() {
                        if let Ok(kb) = kb.parse::<u64>() {
                            return kb * 1024;
                        }
                    }
                }
            }
        }
        0
    }
    #[cfg(not(target_os = "linux"))]
    {
        // No portable equivalent of MemAvailable, so the total is the honest answer here and
        // the halving above is what keeps it from being a reckless one.
        std::process::Command::new("sysctl")
            .args(["-n", "hw.memsize"])
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(0)
    }
}

#[derive(Clone, Copy)]
struct Run {
    secs: f64,
    commits: u64,
    bytes: u64,
}

/// The fastest of `repeat` runs, not the mean.
///
/// A load is throughput, and everything noise does to throughput is subtract it: a descheduled
/// run, a page cache eviction and a neighbour on the same host all make a run slower and none of
/// them makes one faster. The mean of those is a measurement of the machine's mood. The minimum
/// is the closest this can get to what the code costs, and it is the statistic that stays
/// comparable when the same command is run again an hour later on a busier box.
fn best_of(repeat: u32, mut f: impl FnMut() -> Run) -> Run {
    let mut best = f();
    for _ in 1..repeat {
        let r = f();
        if r.secs < best.secs {
            best = r;
        }
    }
    best
}

fn open(dir: &std::path::Path, engine: TableEngine, dur: Durability) -> Db<MmapPager> {
    let db = Db::open(MmapPager::open_default(dir.join("big.db")).unwrap()).unwrap();
    db.set_durability(dur).unwrap();
    db.create_table_with(TABLE, engine).unwrap();
    db.create_field(TABLE, AMOUNT, FieldKind::Int, BIT_DEPTH).unwrap();
    db.create_field(TABLE, CATEGORY, FieldKind::Set, 0).unwrap();
    db.create_field(TABLE, COUNTRY, FieldKind::Set, 0).unwrap();
    db.create_field(TABLE, ACTIVE, FieldKind::Bool, 0).unwrap();
    db
}

/// Key strings built once instead of once per fact.
///
/// `WideRecord::category_key` is a `format!`, so the loop as the report writes it allocates two
/// strings per record before the engine is even called. Whether that matters is exactly the
/// kind of thing this binary exists to answer rather than assume.
struct Keys {
    categories: Vec<String>,
    countries: Vec<String>,
}

impl Keys {
    fn build() -> Self {
        Self {
            categories: (0..big_bench::wide::CATEGORIES).map(WideRecord::category_key).collect(),
            countries: (0..big_bench::wide::COUNTRIES).map(WideRecord::country_key).collect(),
        }
    }
}

/// The `Ingest` path: what any engine that keeps columns is loaded through.
fn ingest(n: u64, layout: Layout, capacity: usize, dur: Durability, keys: Option<&Keys>) -> Run {
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path(), TableEngine::default(), dur);
    let t = Instant::now();
    let mut load = db.ingest(capacity);
    for r in wide_stream(n, layout) {
        load.set_int(TABLE, AMOUNT, r.id, r.amount).unwrap();
        match keys {
            Some(k) => {
                load.set_key(TABLE, CATEGORY, r.id, &k.categories[r.category as usize]).unwrap();
                load.set_key(TABLE, COUNTRY, r.id, &k.countries[r.country as usize]).unwrap();
            }
            None => {
                load.set_key(TABLE, CATEGORY, r.id, &WideRecord::category_key(r.category)).unwrap();
                load.set_key(TABLE, COUNTRY, r.id, &WideRecord::country_key(r.country)).unwrap();
            }
        }
        load.set_bool(TABLE, ACTIVE, r.id, r.active).unwrap();
    }
    // Flushed before the count is read: `finish` consumes the loader, so asking it afterwards
    // is not possible and asking before would miss the last commit.
    load.flush().unwrap();
    let commits = load.commits();
    load.finish().unwrap();
    let secs = t.elapsed().as_secs_f64();
    Run { secs, commits, bytes: db.store().metrics().page_count * 4_096 }
}

/// Buffering with no commit at all, to separate what `Ingest` costs per fact from what the
/// engine costs per commit. The buffer is leaked rather than finished: the point is the half
/// that never reaches the disk.
fn buffer_only(n: u64, layout: Layout, keys: Option<&Keys>) -> f64 {
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path(), TableEngine::default(), Durability::None);
    let t = Instant::now();
    let mut load = db.ingest(usize::MAX);
    for r in wide_stream(n, layout) {
        load.set_int(TABLE, AMOUNT, r.id, r.amount).unwrap();
        match keys {
            Some(k) => {
                load.set_key(TABLE, CATEGORY, r.id, &k.categories[r.category as usize]).unwrap();
                load.set_key(TABLE, COUNTRY, r.id, &k.countries[r.country as usize]).unwrap();
            }
            None => {
                load.set_key(TABLE, CATEGORY, r.id, &WideRecord::category_key(r.category)).unwrap();
                load.set_key(TABLE, COUNTRY, r.id, &WideRecord::country_key(r.country)).unwrap();
            }
        }
        load.set_bool(TABLE, ACTIVE, r.id, r.active).unwrap();
    }
    let secs = t.elapsed().as_secs_f64();
    std::mem::forget(load);
    secs
}

/// The corpus and nothing else: what generating a record costs, so it can be taken off every
/// row below. It used to be a `Vec` built once before the timer started; it is now computed per
/// record inside the loop, which is what lets a billion of them exist at all - and which puts
/// its cost inside every measurement, so it has to be measured too.
fn generate_only(n: u64, layout: Layout) -> f64 {
    let t = Instant::now();
    for r in wide_stream(n, layout) {
        black_box(r);
    }
    t.elapsed().as_secs_f64()
}

/// The `bulk_load` path: fragment-major, one bottom-up build per fragment, nothing read back.
/// Bitmap-only, because that is all it writes.
fn bulk(n: u64, layout: Layout, dur: Durability, keys: Option<&Keys>) -> Run {
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path(), TableEngine::Bitmap, dur);
    let t = Instant::now();
    let mut load = db.bulk_load(TABLE).unwrap();
    for r in wide_stream(n, layout) {
        load.set_int(AMOUNT, r.id, r.amount).unwrap();
        match keys {
            Some(k) => {
                load.set_key(CATEGORY, r.id, &k.categories[r.category as usize]).unwrap();
                load.set_key(COUNTRY, r.id, &k.countries[r.country as usize]).unwrap();
            }
            None => {
                load.set_key(CATEGORY, r.id, &WideRecord::category_key(r.category)).unwrap();
                load.set_key(COUNTRY, r.id, &WideRecord::country_key(r.country)).unwrap();
            }
        }
        load.set_bool(ACTIVE, r.id, r.active).unwrap();
    }
    load.finish().unwrap();
    let secs = t.elapsed().as_secs_f64();
    Run { secs, commits: 0, bytes: db.store().metrics().page_count * 4_096 }
}

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    // TSV so two runs can be subtracted from each other. See `scripts/bench ab`.
    let tsv = argv.iter().any(|a| a == "--tsv");
    let mut positional = argv.iter().filter(|a| !a.starts_with("--"));

    let n: u64 = positional
        .next()
        .map(|a| a.parse().expect("first argument is a record count"))
        .unwrap_or(1_000_000);
    let shards: u64 = positional
        .next()
        .map(|a| a.parse().expect("second argument is a shard count"))
        .unwrap_or(0);
    let repeat: u32 = positional
        .next()
        .map(|a| a.parse().expect("third argument is a repeat count"))
        .unwrap_or(3)
        .max(1);
    let layout = if shards == 0 { Layout::Dense } else { Layout::Sparse { shards } };

    let keys = Keys::build();
    let facts = n * FACTS_PER_RECORD;

    let gen = (0..repeat).map(|_| generate_only(n, layout)).fold(f64::MAX, f64::min);
    let raw = (0..repeat).map(|_| buffer_only(n, layout, None)).fold(f64::MAX, f64::min);
    let pre = (0..repeat).map(|_| buffer_only(n, layout, Some(&keys))).fold(f64::MAX, f64::min);

    let mut rows: Vec<(&str, Run)> = vec![
        ("corpus only, no engine at all", Run { secs: gen, commits: 0, bytes: 0 }),
        ("buffer only, keys formatted per fact", Run { secs: raw, commits: 0, bytes: 0 }),
        ("buffer only, keys precomputed", Run { secs: pre, commits: 0, bytes: 0 }),
        (
            "ingest, cap 100k, full durability",
            best_of(repeat, || ingest(n, layout, 100_000, Durability::Full, None)),
        ),
        (
            "ingest, cap 100k, no fsync",
            best_of(repeat, || ingest(n, layout, 100_000, Durability::None, None)),
        ),
        (
            "ingest, cap 1M, full durability",
            best_of(repeat, || ingest(n, layout, 1_000_000, Durability::Full, None)),
        ),
        (
            "ingest, cap 1M, no fsync",
            best_of(repeat, || ingest(n, layout, 1_000_000, Durability::None, None)),
        ),
        (
            "ingest, cap 1M, no fsync, keys precomputed",
            best_of(repeat, || ingest(n, layout, 1_000_000, Durability::None, Some(&keys))),
        ),
    ];

    // See `bulk_max`. Nothing streams this one out of trouble.
    let bulk_max = bulk_max();
    if n <= bulk_max {
        rows.push((
            "bulk_load (bitmap only), full durability",
            best_of(repeat, || bulk(n, layout, Durability::Full, None)),
        ));
        rows.push((
            "bulk_load (bitmap only), no fsync",
            best_of(repeat, || bulk(n, layout, Durability::None, Some(&keys))),
        ));
    }

    if tsv {
        for (label, r) in &rows {
            println!("{label}\t{:.4}\t{:.0}", r.secs, facts as f64 / r.secs);
        }
        return;
    }

    println!(
        "# load profile\n\n{n} records x {FACTS_PER_RECORD} facts, layout {}, best of {repeat}\n",
        layout.label(),
    );
    println!(
        "{:<46} {:>9} {:>13} {:>11} {:>8} {:>10}",
        "path", "wall", "facts/s", "records/s", "commits", "file"
    );
    for (label, r) in &rows {
        println!(
            "{label:<46} {:>8.2}s {:>13} {:>11} {:>8} {:>10}",
            r.secs,
            thousands((facts as f64 / r.secs) as u64),
            thousands((n as f64 / r.secs) as u64),
            if r.commits == 0 { "-".to_string() } else { r.commits.to_string() },
            if r.bytes == 0 { "-".to_string() } else { mib(r.bytes) },
        );
    }
    if n > bulk_max {
        println!(
            "{:<46} {:>9}  holds the whole load in memory by design; this box fits about {}",
            "bulk_load (bitmap only)",
            "skipped",
            thousands(bulk_max),
        );
    }

    println!(
        "\nRead the gaps, not the rows. `corpus only` is what generating the records costs and\n\
         is inside every row below it; `buffer only` to `ingest` is what committing costs;\n\
         `full durability` to `no fsync` is what the two fsyncs per commit cost; `cap 100k` to\n\
         `cap 1M` is what records-per-fragment-per-commit buys; and `bulk_load` is the ceiling\n\
         a stream cannot reach, because it knows the whole load before it writes any of it."
    );
}

fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn mib(bytes: u64) -> String {
    format!("{:.0} MiB", bytes as f64 / (1024.0 * 1024.0))
}
