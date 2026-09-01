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

//! What the query front end costs, in isolation from storage.
//!
//! **This binary exists because the analytical table could not answer the question.** Put a
//! `big` column next to a `big-sql` one and the difference between them moves by more than the
//! difference itself between runs - on a 2 vCPU host the sign flips - so the honest reading of
//! that table is "no signal", and a reader is entitled to ask what the front end actually costs
//! rather than being told it is small.
//!
//! So: no file, no pager, no read transaction. Text in, `Plan` out, in a loop. Whatever this
//! prints is the *ceiling* on what parsing and planning could contribute to a query, because
//! everything else a query does is strictly more than this.
//!
//! ```sh
//! cargo run -p big-bench --release --bin frontend
//! ```

use big_db::catalog::FieldKind;
use big_db::Db;
use std::time::Instant;

/// Enough that a per-iteration cost in the hundreds of nanoseconds is measured rather than
/// rounded, and few enough that the whole run is a second.
const ITERATIONS: u32 = 100_000;

/// The six questions the analytical benchmark asks, in both languages.
const QUESTIONS: [(&str, &str, &str); 6] = [
    ("count_ge", "SELECT count(*) FROM t WHERE amount >= 786432", "Count(Row(amount >= 786432))"),
    (
        "intersect",
        "SELECT count(*) FROM t WHERE country = 'n07' AND active = true AND amount >= 786432",
        "Count(Intersect(Row(country=\"n07\"), Row(active=true), Row(amount >= 786432)))",
    ),
    (
        "sum",
        "SELECT sum(amount) FROM t WHERE amount >= 786432",
        "Sum(Row(amount >= 786432), field=amount)",
    ),
    (
        "group_by",
        "SELECT category, count(*) FROM t GROUP BY category ORDER BY category",
        "GroupBy(All(), field=category)",
    ),
    (
        "top_n",
        "SELECT category, count(*) AS n FROM t GROUP BY category ORDER BY n DESC LIMIT 10",
        "TopN(All(), field=category, n=10)",
    ),
    (
        "distinct",
        "SELECT count(DISTINCT category) FROM t WHERE amount >= 786432",
        "Distinct(Row(amount >= 786432), field=category)",
    ),
];

fn main() {
    let db = Db::in_memory().expect("an in-memory database");
    db.create_table("t").unwrap();
    db.create_field("t", "amount", FieldKind::Int, 20).unwrap();
    db.create_field("t", "category", FieldKind::Set, 0).unwrap();
    db.create_field("t", "country", FieldKind::Set, 0).unwrap();
    db.create_field("t", "active", FieldKind::Bool, 0).unwrap();
    let catalog = db.catalog();
    let schema = big_exec::CatalogSchema(&catalog);

    println!("# What the query front end costs\n");
    println!(
        "Text to `Plan`, {ITERATIONS} times each, no storage touched. Both languages resolve \
         through the same planner, so the difference between the columns is the difference \
         between the two parsers and nothing else.\n"
    );
    println!("| question | PQL | SQL | difference |");
    println!("|---|---|---|---|");

    let (mut pql_total, mut sql_total) = (0u128, 0u128);
    for (name, sql, pql) in QUESTIONS {
        // Both are checked to produce the same plan before either is timed. A timing of two
        // things that are not the same thing is not a comparison.
        let from_pql = big_plan::plan("t", &big_plan::parse(pql).unwrap(), &schema).unwrap();
        let statement = question(sql);
        // One question per statement here, so one call. A statement with several would not be
        // comparable against a single PQL plan, which is the whole point of this table.
        let ask = &statement.calls[0];
        let from_sql = big_plan::plan(&ask.table, &ask.call, &schema).unwrap();
        assert_eq!(from_pql, from_sql, "`{name}` does not plan the same way in both languages");

        let pql_ns = time(ITERATIONS, || {
            let call = big_plan::parse(pql).unwrap();
            std::hint::black_box(big_plan::plan("t", &call, &schema).unwrap());
        });
        let sql_ns = time(ITERATIONS, || {
            let s = question(sql);
            let ask = &s.calls[0];
            std::hint::black_box(big_plan::plan(&ask.table, &ask.call, &schema).unwrap());
        });
        pql_total += pql_ns;
        sql_total += sql_ns;

        println!(
            "| `{name}` | {pql_ns}ns | {sql_ns}ns | {:+}ns |",
            sql_ns as i128 - pql_ns as i128
        );
    }

    println!(
        "| **total** | **{pql_total}ns** | **{sql_total}ns** | **{:+}ns** |",
        sql_total as i128 - pql_total as i128
    );

    // The comparison that decides the question. A query in the analytical table takes single-
    // digit milliseconds; if planning is a thousandth of that, no amount of it explains a
    // difference between engines.
    let slowest = sql_total.max(pql_total);
    println!(
        "\nSix questions planned end to end: {}µs in PQL, {}µs in SQL. The analytical benchmark's \
         six queries take about 21,000µs together, so the whole front end is {:.2}% of a run \
         and the difference between the two languages is {:.3}% of it.",
        pql_total / 1000,
        sql_total / 1000,
        slowest as f64 / 21_000_000.0 * 100.0,
        (sql_total as f64 - pql_total as f64).abs() / 21_000_000.0 * 100.0,
    );
}

/// Translates one statement and takes the query half, which is what `POST /sql` does.
///
/// Inside the timed closure on purpose: the arm is part of what the front end costs, and lifting
/// it out would measure a translation nobody performs. Every entry in `QUESTIONS` is a `SELECT`,
/// so the other arm is a typo in that table rather than a case to carry.
fn question(sql: &str) -> big_sql::lower::Statement {
    match big_sql::translate(sql).unwrap() {
        big_sql::Sql::Query(s) => s,
        other => panic!("`{sql}` is not a question: {other:?}"),
    }
}

/// Median of three, matching what the analytical harness reports.
fn time(iterations: u32, mut f: impl FnMut()) -> u128 {
    let mut runs: Vec<u128> = (0..3)
        .map(|_| {
            let start = Instant::now();
            for _ in 0..iterations {
                f();
            }
            start.elapsed().as_nanos() / u128::from(iterations)
        })
        .collect();
    runs.sort_unstable();
    runs[1]
}
