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

//! DuckDB: the analytical comparison's closest rival, and the one that matters most.
//!
//! `redb` is `big`'s architectural peer in the storage table - both copy-on-write B-trees over a
//! mapped file - and DuckDB is its *market* peer here. Both are embedded, in-process, single
//! writer, one file, and both are chosen by someone who wants analytics without running a
//! server. A difference between these two columns is a difference between a bit-sliced index and
//! a vectorised column store, which is the comparison this table was added to make.

use crate::olap::{Answer, Locality, Olap};
use crate::wide::WideRecord;
use duckdb::{params, Connection};
use std::path::{Path, PathBuf};

pub struct DuckdbOlap {
    conn: Connection,
    dir: PathBuf,
}

impl DuckdbOlap {
    fn scalar<T: duckdb::types::FromSql>(&self, sql: &str, args: &[&dyn duckdb::ToSql]) -> T {
        self.conn
            .query_row(sql, args, |row| row.get(0))
            .unwrap_or_else(|e| panic!("duckdb failed to answer `{sql}`: {e}"))
    }

    /// Runs a query returning `(key, count)` pairs and parses the category back out of the key.
    fn groups(&self, sql: &str) -> Vec<(u32, u64)> {
        let mut stmt = self.conn.prepare(sql).unwrap();
        let rows = stmt
            .query_map([], |row| {
                let key: String = row.get(0)?;
                let n: i64 = row.get(1)?;
                Ok((key, n))
            })
            .unwrap();
        rows.map(|r| {
            let (key, n) = r.unwrap();
            let category = key
                .strip_prefix('c')
                .and_then(|d| d.parse::<u32>().ok())
                .unwrap_or_else(|| panic!("unexpected group key `{key}`"));
            (category, n as u64)
        })
        .collect()
    }
}

impl Olap for DuckdbOlap {
    fn name() -> &'static str {
        "duckdb"
    }

    fn locality() -> Locality {
        Locality::InProcess
    }

    fn open(dir: &Path) -> Self {
        let conn = Connection::open(dir.join("duck.db")).unwrap();
        // Unsigned for `amount` so both engines hold the same domain: `big` stores a twenty-bit
        // unsigned field, and giving DuckDB a signed one would let it answer a slightly
        // different question about a slightly different type.
        //
        // **No primary key on `id`, deliberately.** Declaring one would make DuckDB build an ART
        // index that not one of the six questions below uses - none of them is a lookup by id -
        // and then charge it for that index on every row of the load and every byte of the file.
        // That is the same rigging the storage table refuses in the other direction when it
        // makes redb pay for the secondary index its range query genuinely needs. `big` holds no
        // index over record ids either, and ClickHouse's `ORDER BY id` is a sparse sort key
        // rather than a unique one, so no index here is the honest match.
        conn.execute_batch(
            "CREATE TABLE t (
                 id       UBIGINT NOT NULL,
                 amount   UBIGINT NOT NULL,
                 category VARCHAR NOT NULL,
                 country  VARCHAR NOT NULL,
                 active   BOOLEAN NOT NULL
             );",
        )
        .unwrap();
        Self { conn, dir: dir.to_path_buf() }
    }

    /// The appender, which is DuckDB's documented bulk path and the counterpart to `big`'s
    /// `bulk_load`. Row-at-a-time `INSERT` would measure the statement overhead of a database
    /// nobody loads that way.
    fn load(&mut self, records: &[WideRecord]) {
        let mut appender = self.conn.appender("t").unwrap();
        for r in records {
            appender
                .append_row(params![
                    r.id,
                    r.amount,
                    WideRecord::category_key(r.category),
                    WideRecord::country_key(r.country),
                    r.active,
                ])
                .unwrap();
        }
        appender.flush().unwrap();
    }

    fn count_ge(&self, k: u64) -> u64 {
        let n: i64 = self.scalar("SELECT count(*) FROM t WHERE amount >= ?", params![k]);
        n as u64
    }

    fn intersect_count(&self, country: u32, active: bool, k: u64) -> u64 {
        let n: i64 = self.scalar(
            "SELECT count(*) FROM t WHERE country = ? AND active = ? AND amount >= ?",
            params![WideRecord::country_key(country), active, k],
        );
        n as u64
    }

    /// The values are all non-negative and the ground truth is `u128`, so the conversion below
    /// cannot lose anything - and asserting that rather than casting says so.
    fn sum_where(&self, k: u64) -> u128 {
        // The cast is not decoration. DuckDB widens the sum of a `UBIGINT` column rather than
        // risk overflowing it, and which 128-bit type it picks - signed or unsigned - is a
        // detail that has moved between versions. Naming `HUGEINT` pins it, so this reads the
        // same type it asked for instead of whichever one the build happened to produce. It is
        // one scalar cast at the top of the plan, not one per row.
        let total: i128 =
            self.scalar("SELECT CAST(sum(amount) AS HUGEINT) FROM t WHERE amount >= ?", params![k]);
        u128::try_from(total).expect("a sum of unsigned amounts cannot be negative")
    }

    fn group_by_count(&self) -> Vec<(u32, u64)> {
        self.groups("SELECT category, count(*) FROM t GROUP BY category ORDER BY category")
    }

    /// `ORDER BY count DESC LIMIT n`, with no tie-break clause, because the workload has no
    /// ties: `wide::category_quotas` gives every category a distinct frequency precisely so that
    /// this query has exactly one right answer in every engine here.
    fn top_n(&self, n: usize) -> Vec<(u32, u64)> {
        self.groups(&format!(
            "SELECT category, count(*) AS n FROM t GROUP BY category ORDER BY n DESC LIMIT {n}"
        ))
    }

    fn distinct(&self, k: u64) -> u64 {
        let n: i64 =
            self.scalar("SELECT count(DISTINCT category) FROM t WHERE amount >= ?", params![k]);
        n as u64
    }

    /// Writes out the WAL, so the size taken next is the database rather than the database plus
    /// whatever had not been folded in yet. The same obligation `checkpoint` carries for fjall
    /// in the storage table.
    fn checkpoint(&mut self) {
        self.conn.execute_batch("CHECKPOINT;").unwrap();
    }

    fn disk_bytes(&self) -> Answer<u64> {
        Answer::Given(crate::dir_size(&self.dir))
    }
}
