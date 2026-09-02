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

//! The analytical report: `big` against the engines sold into the same market.
//!
//! Separate from `report`, which measures storage. That one asks what a commit costs and what a
//! file weighs; this one asks what the engine answers. They share a workload generator and a
//! rule - every answer is checked against a ground truth computed without any engine - and
//! nothing else.
//!
//! ```sh
//! cargo run -p big-bench --release --features olap-peers --bin olap
//! cargo run -p big-bench --release --features olap-peers --bin olap -- 1000000
//! ```
//!
//! Peers that were not compiled in, and servers that are not listening, are named as such. A row
//! that is missing for a stated reason is worth more than a row that is quietly absent.

use big_bench::engines::big_olap::{BigOlap, BigSqlOlap, Bitmap, Columnar};
use big_bench::olap::{measure, Answer, Locality, Measured, Olap, Questions, REPEATS};
use big_bench::wide::{self, wide_workload, WideRecord};
use big_bench::Layout;
use std::time::Duration;

/// Large enough that a group-by is real work and small enough that a run is minutes rather than
/// an afternoon. The storage report's summary uses 100,000; this one asks harder questions of
/// the same corpus shape, so it doubles it.
const DEFAULT_RECORDS: u64 = 200_000;

fn main() {
    let n: u64 = std::env::args()
        .nth(1)
        .map(|a| a.parse().expect("the first argument is a record count"))
        .unwrap_or(DEFAULT_RECORDS);
    let q = Questions::default();
    let records = wide_workload(n, Layout::Dense);

    header(n, q);

    // Both `big` columns, next to each other. They are the same engine over the same corpus and
    // differ only in the language the question arrives in, so the distance between them is what
    // the SQL front end costs - and the distance from either to a rival is what the engine does.
    let mut in_process = vec![
        run::<BigOlap>(&records, q),
        run::<BigSqlOlap>(&records, q),
        // The part-based engines, on the same corpus and the same questions. They are `big` with
        // a different write schedule rather than different storage, so what this table measures
        // of them is what the schedule costs a *reader*: a compacted part-based table should
        // answer like the default engine, and any distance is read amplification the merge did
        // not fully pay off.
        // Both halves of the default engine on their own, so the table shows what each brings:
        // one answers from an index and the other from a scan, and the default is both at once.
        run::<BigOlap<Columnar>>(&records, q),
        run::<BigOlap<Bitmap>>(&records, q),
    ];
    let mut over_http = Vec::new();
    let mut absent: Vec<String> = Vec::new();

    #[cfg(feature = "duckdb-peer")]
    in_process.push(run::<big_bench::engines::duckdb::DuckdbOlap>(&records, q));
    #[cfg(not(feature = "duckdb-peer"))]
    absent.push("duckdb: not compiled in (build with `--features duckdb-peer`)".to_string());

    #[cfg(feature = "datafusion-peer")]
    in_process.push(run::<big_bench::engines::datafusion::DatafusionOlap>(&records, q));
    #[cfg(not(feature = "datafusion-peer"))]
    absent
        .push("datafusion: not compiled in (build with `--features datafusion-peer`)".to_string());

    #[cfg(feature = "http-peers")]
    {
        use big_bench::engines::clickhouse::{
            ClickhouseOlap, DEFAULT_ENDPOINT as CH, ENDPOINT_VAR as CHV,
        };
        use big_bench::engines::http::endpoint;

        if ClickhouseOlap::available() {
            over_http.push(run::<ClickhouseOlap>(&records, q));
        } else {
            absent.push(format!("clickhouse: nothing listening at {}", endpoint(CHV, CH)));
        }
    }
    #[cfg(not(feature = "http-peers"))]
    absent.push("clickhouse: not compiled in (build with `--features http-peers`)".to_string());

    table("in-process", &in_process);
    if over_http.is_empty() {
        println!("\n## Over HTTP\n\nNo server peer ran.");
    } else {
        // **Every `big` engine appears here, not just the default one.** Which engine a table is
        // created under decides what it can answer without scanning, and that is precisely the
        // question a comparison against a column store is asking - a single reference column
        // answers it for one engine and silently invites the reader to assume the rest.
        //
        // They are still not being *ranked* against the server: they pay no round trip and it
        // does, which is why the two tables are separate and why the round trip is printed on its
        // own row rather than subtracted here.
        //
        // `big-sql` is left out. It is the first column's engine asked in a different language,
        // so it would be a second row for one engine rather than another engine.
        let mut with_reference: Vec<_> = in_process
            .iter()
            .filter(|m| m.name != <BigSqlOlap as big_bench::olap::Olap>::name())
            .cloned()
            .collect();
        with_reference.extend(over_http.iter().cloned());
        table("over HTTP", &with_reference);
    }

    if !absent.is_empty() {
        println!("\n## Not run\n");
        for reason in &absent {
            println!("- {reason}");
        }
    }
}

fn header(n: u64, q: Questions) {
    println!("# big analytical benchmark\n");
    println!(
        "{n} records, dense ids, {} categories, {} countries, amounts under {}.",
        wide::CATEGORIES,
        wide::COUNTRIES,
        wide::VALUE_CEILING
    );
    println!(
        "Questions: `amount >= {}`, country `{}`, active `{}`, top {}.",
        q.k,
        WideRecord::country_key(q.country),
        q.active,
        q.n
    );
    println!(
        "\nEvery answer below was checked against a ground truth computed without any engine; a \
         wrong answer aborts the run rather than being printed. Query timings are medians of \
         {REPEATS}; the load is a single shot."
    );
    println!(
        "\nThe load row excludes the settle that follows it - DuckDB's `CHECKPOINT`, \
         ClickHouse's `OPTIMIZE ... FINAL` - the same split the storage report uses, where a \
         checkpoint is charged to the size that is measured after it rather than to the write \
         that preceded it. An engine that defers work therefore shows a cheaper load here and \
         pays for it in the size row."
    );
    println!(
        "\nEvery engine here answers from **one node**: one process, one file, no shard fan-out, \
         no merge and no network. `big serve --cluster` is not started at any point. The peers are \
         single-node too - DuckDB and DataFusion in-process, ClickHouse one server over loopback \
         - so the comparison is not distorted by it, but no number below says anything about a \
         cluster: a fanned-out query pays a plan encode, a round trip per owner and a merge, and \
         none of that is measured here."
    );
    println!(
        "\n`big` is asked in its own query language and every other engine in SQL, so each one \
         plans its own question rather than being handed a loop to run. The `big-sql` column is \
         the same engine over the same file asked in SQL, so the caveat that used to follow \
         from that - that `big` was being spared a parser the others paid for - is now a \
         column rather than an argument. What it costs is measured on its own by \
         `--bin frontend`, because the difference here is smaller than this host's noise."
    );
}

fn run<E: Olap>(records: &[WideRecord], q: Questions) -> Measured {
    let dir = tempfile::tempdir().expect("a temporary directory");
    eprintln!("measuring {} ...", E::name());
    let m = measure::<E>(dir.path(), records, q);
    // Held until here so the engine's files still exist while `disk_bytes` is read.
    drop(dir);
    m
}

fn table(heading: &str, rows: &[Measured]) {
    println!("\n## {heading}\n");
    print!("| |");
    for r in rows {
        print!(" {} |", r.name);
    }
    println!();
    print!("|---|");
    for _ in rows {
        print!("---|");
    }
    println!();

    row("load", rows, |m| ms(m.load));
    row("`count_ge`", rows, |m| us(m.count_ge.1));
    row("intersect", rows, |m| us(m.intersect.1));
    row("`sum` over predicate", rows, |m| us(m.sum_where.1));
    row("`group_by` (256 groups)", rows, |m| us(m.group_by.1));
    row("`top_n`", rows, |m| us(m.top_n.1));
    row("`distinct`", rows, |m| us(m.distinct.1));
    row("size", rows, |m| match &m.disk_bytes {
        Answer::Given(b) => mib(*b),
        Answer::NotApplicable(_) => "n/a".to_string(),
    });
    if rows.iter().any(|r| r.round_trip.is_some()) {
        row("round trip (subtract this)", rows, |m| {
            m.round_trip.map(us).unwrap_or_else(|| "—".to_string())
        });
    }

    for r in rows {
        if let Answer::NotApplicable(why) = &r.disk_bytes {
            println!("\n- **{} size is `n/a`** — {why}.", r.name);
        }
        if r.locality == Locality::OverHttp {
            println!(
                "- **{}** answers over HTTP, so every timing in its column includes a round \
                 trip. The last row is that round trip measured on its own.",
                r.name
            );
        }
    }
}

fn row(label: &str, rows: &[Measured], f: impl Fn(&Measured) -> String) {
    print!("| {label} |");
    for r in rows {
        print!(" {} |", f(r));
    }
    println!();
}

fn ms(d: Duration) -> String {
    format!("{}ms", d.as_millis())
}

fn us(d: Duration) -> String {
    format!("{}µs", d.as_micros())
}

fn mib(bytes: u64) -> String {
    format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
}
