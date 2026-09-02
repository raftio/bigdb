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

//! The analytical corpus, written as `big serve` import lines.
//!
//! **Why this exists.** Every measurement in this crate until now has called `big-db` as a
//! library, which is the engine and is not the product: a load that arrives over the wire pays
//! for a line parse, a fact batch, one commit per request and a round trip, and none of those
//! appear in an in-process number. That gap matters most in exactly the comparison the OLAP
//! report makes, where ClickHouse is reached over HTTP and `big` is not - the two tables are
//! kept separate for that reason, and the missing column has always been `big` over HTTP.
//!
//! So this prints the same corpus the rest of the harness measures, in the format
//! `POST /table/{t}/import` takes, and `bigctl` sends it. Same records, same values, same
//! permutation; the only difference is the road they travel.
//!
//! ```text
//! facts 20000000 | bigctl import t - --addr 127.0.0.1:7654
//! facts 20000000 --rows | curl --data-binary @- 'http://…/?query=INSERT INTO t FORMAT TSV'
//! ```
//!
//! `--rows` is the same corpus one record per line instead of one fact per line, which is what
//! a row store is loaded with. Nothing in this repo calls it: `scripts/bench server` measures
//! `big serve` alone, because two engines loaded back to back on one box share a page cache and a
//! disk queue and the second is measured through what the first left behind. It is kept for
//! loading a rival by hand, on its own box, which is the only way that comparison is worth
//! printing.
//!
//! Written to a pipe rather than a file on purpose: twenty million records is eighty million
//! lines and gigabytes of text, and a benchmark that has to stage that on disk first is
//! measuring the staging.

use big_bench::wide::{wide_stream, WideRecord, CATEGORIES, COUNTRIES};
use big_bench::Layout;
use std::io::{BufWriter, ErrorKind, Write};

fn main() {
    let mut args = std::env::args().skip(1).filter(|a| !a.starts_with("--"));
    let n: u64 = args
        .next()
        .map(|a| a.parse().expect("first argument is a record count"))
        .unwrap_or(1_000_000);
    let shards: u64 =
        args.next().map(|a| a.parse().expect("second argument is a shard count")).unwrap_or(0);
    let layout = if shards == 0 { Layout::Dense } else { Layout::Sparse { shards } };
    let rows = std::env::args().any(|a| a == "--rows");

    // Built once. A `format!` per line would put the generator's own allocation into a
    // measurement of the server, which is the mistake this whole binary exists to avoid making
    // in the other direction.
    let categories: Vec<String> = (0..CATEGORIES).map(WideRecord::category_key).collect();
    let countries: Vec<String> = (0..COUNTRIES).map(WideRecord::country_key).collect();

    let stdout = std::io::stdout();
    let mut out = BufWriter::with_capacity(1 << 20, stdout.lock());

    for r in wide_stream(n, layout) {
        let wrote = if rows {
            // One row per record: id, amount, category, country, active. TSV because it is the
            // one format every row store here reads without a schema description attached.
            writeln!(
                out,
                "{}\t{}\t{}\t{}\t{}",
                r.id,
                r.amount,
                categories[r.category as usize],
                countries[r.country as usize],
                u8::from(r.active),
            )
        } else {
            // `field record value`, the format `POST /import` reads. Four facts a record, the
            // same four every other engine in this harness is given.
            writeln!(out, "amount {} {}", r.id, r.amount)
                .and_then(|()| {
                    writeln!(out, "category {} {}", r.id, categories[r.category as usize])
                })
                .and_then(|()| writeln!(out, "country {} {}", r.id, countries[r.country as usize]))
                .and_then(|()| writeln!(out, "active {} {}", r.id, r.active))
        };
        if let Err(e) = wrote {
            // The reader going away first is how a pipeline ends, not a failure worth a
            // backtrace: `facts 1000000 | head` should be quiet.
            if e.kind() == ErrorKind::BrokenPipe {
                return;
            }
            eprintln!("facts: {e}");
            std::process::exit(1);
        }
    }
    if let Err(e) = out.flush() {
        if e.kind() != ErrorKind::BrokenPipe {
            eprintln!("facts: {e}");
            std::process::exit(1);
        }
    }
}
