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

//! ClickHouse: the column store the rest of the market is measured against.
//!
//! It is here to set the ceiling, not to be beaten. Where DuckDB is `big`'s peer in deployment
//! shape - embedded, one file, no server - ClickHouse is the engine a team moves to when the
//! answer is "we need a real OLAP database", and knowing how far away that ceiling sits is worth
//! more than another column that agrees with the ones beside it.
//!
//! **Run it as a static binary, not in Docker.** The report's method is to stop every container
//! on the box before measuring, so bringing Docker back up to host a benchmark peer would
//! contradict the isolation the rest of the numbers depend on. `clickhouse server` from the
//! single static binary runs with Docker still down.

use crate::engines::http::{endpoint, responds, Http};
use crate::olap::{Answer, Locality, Olap};
use crate::wide::WideRecord;
use std::path::Path;
use std::time::Duration;

pub const ENDPOINT_VAR: &str = "BIG_BENCH_CLICKHOUSE";
pub const DEFAULT_ENDPOINT: &str = "http://127.0.0.1:8123";
const TABLE: &str = "big_bench";

pub struct ClickhouseOlap {
    http: Http,
}

impl ClickhouseOlap {
    /// Whether a server is up, so the report can print a reason rather than a panic.
    pub fn available() -> bool {
        responds(&endpoint(ENDPOINT_VAR, DEFAULT_ENDPOINT), "/", "SELECT 1")
    }

    fn sql(&self, sql: &str) -> String {
        self.http.post("/", sql).trim().to_string()
    }

    fn scalar<T: std::str::FromStr>(&self, sql: &str) -> T
    where
        T::Err: std::fmt::Display,
    {
        let raw = self.sql(sql);
        raw.parse().unwrap_or_else(|e| panic!("clickhouse answered `{sql}` with `{raw}`: {e}"))
    }

    /// Parses `key\tcount` lines, which is what `FORMAT TabSeparated` gives and what every
    /// grouped query here asks for.
    fn groups(&self, sql: &str) -> Vec<(u32, u64)> {
        self.sql(sql)
            .lines()
            .filter(|l| !l.is_empty())
            .map(|line| {
                let (key, count) = line
                    .split_once('\t')
                    .unwrap_or_else(|| panic!("clickhouse returned an unsplittable row `{line}`"));
                let category = key
                    .strip_prefix('c')
                    .and_then(|d| d.parse::<u32>().ok())
                    .unwrap_or_else(|| panic!("unexpected group key `{key}`"));
                (category, count.parse().expect("a count is an integer"))
            })
            .collect()
    }
}

impl Olap for ClickhouseOlap {
    fn name() -> &'static str {
        "clickhouse"
    }

    fn locality() -> Locality {
        Locality::OverHttp
    }

    /// The directory is ignored: the server owns its own storage and was started before the
    /// harness ran. Dropping the table rather than the database, so a run does not delete
    /// something the operator put there.
    fn open(_dir: &Path) -> Self {
        let http = Http::new(endpoint(ENDPOINT_VAR, DEFAULT_ENDPOINT));
        let me = Self { http };
        me.sql(&format!("DROP TABLE IF EXISTS {TABLE}"));
        // `MergeTree ORDER BY id` is the ordinary shape a user would reach for, and the one
        // that matches how every other engine here is laid out: sorted by record id, with no
        // index built by hand over `amount`. Giving ClickHouse a skip index over the column the
        // range query asks about would be the same rigging the storage table refuses when it
        // makes redb pay for its secondary index on every ingest.
        me.sql(&format!(
            "CREATE TABLE {TABLE} (
                 id       UInt64,
                 amount   UInt64,
                 category LowCardinality(String),
                 country  LowCardinality(String),
                 active   Bool
             ) ENGINE = MergeTree ORDER BY id"
        ));
        me
    }

    /// One INSERT carrying the whole corpus as tab-separated rows - ClickHouse's documented
    /// bulk path, and the counterpart to DuckDB's appender and `big`'s `bulk_load`.
    fn load(&mut self, records: &[WideRecord]) {
        let mut body = format!("INSERT INTO {TABLE} FORMAT TabSeparated\n");
        for r in records {
            body.push_str(&format!(
                "{}\t{}\t{}\t{}\t{}\n",
                r.id,
                r.amount,
                WideRecord::category_key(r.category),
                WideRecord::country_key(r.country),
                u8::from(r.active),
            ));
        }
        self.sql(&body);
    }

    fn count_ge(&self, k: u64) -> u64 {
        self.scalar(&format!("SELECT count() FROM {TABLE} WHERE amount >= {k}"))
    }

    fn intersect_count(&self, country: u32, active: bool, k: u64) -> u64 {
        let country = WideRecord::country_key(country);
        self.scalar(&format!(
            "SELECT count() FROM {TABLE} \
             WHERE country = '{country}' AND active = {} AND amount >= {k}",
            u8::from(active)
        ))
    }

    /// Cast to a 128-bit total for the same reason DuckDB's answer is widened: the ground truth
    /// is `u128`, and a sum that silently wrapped would be caught as a wrong answer rather than
    /// as the overflow it was.
    fn sum_where(&self, k: u64) -> u128 {
        self.scalar(&format!(
            "SELECT toString(sum(toUInt128(amount))) FROM {TABLE} WHERE amount >= {k}"
        ))
    }

    fn group_by_count(&self) -> Vec<(u32, u64)> {
        self.groups(&format!(
            "SELECT category, count() FROM {TABLE} GROUP BY category ORDER BY category \
             FORMAT TabSeparated"
        ))
    }

    fn top_n(&self, n: usize) -> Vec<(u32, u64)> {
        self.groups(&format!(
            "SELECT category, count() AS n FROM {TABLE} GROUP BY category ORDER BY n DESC \
             LIMIT {n} FORMAT TabSeparated"
        ))
    }

    fn distinct(&self, k: u64) -> u64 {
        // `uniqExact` rather than `uniq`: the default is an approximation, and an approximate
        // answer measured against an exact ground truth would fail the check for the right
        // reason at an unpredictable size. Every other engine here answers exactly, so this one
        // is asked to as well.
        self.scalar(&format!("SELECT uniqExact(category) FROM {TABLE} WHERE amount >= {k}"))
    }

    /// Merges the parts, so the size read next is the table settled rather than the table plus
    /// however many pieces the insert happened to leave behind. The same obligation `checkpoint`
    /// carries for an LSM in the storage table.
    fn checkpoint(&mut self) {
        self.sql(&format!("OPTIMIZE TABLE {TABLE} FINAL"));
    }

    /// The bytes this table's active parts occupy, which the server can state exactly.
    ///
    /// Not the data directory: that holds system tables, logs and metadata belonging to a server
    /// the harness did not start and does not own, and reporting it as the cost of the corpus
    /// would be a number with no meaning. This is the analogue of `dir_size` for an engine whose
    /// directory is not the harness's.
    fn disk_bytes(&self) -> Answer<u64> {
        Answer::Given(self.scalar(&format!(
            "SELECT sum(bytes_on_disk) FROM system.parts \
             WHERE table = '{TABLE}' AND active"
        )))
    }

    fn round_trip(&self) -> Option<Duration> {
        Some(self.http.round_trip("/", "SELECT 1"))
    }
}
