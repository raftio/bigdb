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

//! `big` in the analytical comparison, answering through its own query language.
//!
//! **Through the query language on purpose.** The storage comparison calls `DbRead` methods
//! directly, which is right there: the question is what the storage costs, and a parser between
//! the harness and the pager would only add noise. Here the question is what the *engine*
//! answers, and every rival is being handed a string of SQL and asked to plan it. Reaching past
//! `big`'s planner into the executor would give it a head start no rival gets, and would also
//! stop measuring the thing a user of `big` actually experiences.

use crate::olap::{Answer, Locality, Olap};
use crate::wide::WideRecord;
use big_db::catalog::FieldKind;
use big_db::Db;
use big_exec::Value;
use big_pager::MmapPager;
use big_sql::Sql;
use std::path::{Path, PathBuf};

pub const TABLE: &str = "t";
pub const AMOUNT: &str = "amount";
pub const CATEGORY: &str = "category";
pub const COUNTRY: &str = "country";
pub const ACTIVE: &str = "active";
/// Amounts stay under 2^20 and the engine refuses anything wider than declared.
pub const BIT_DEPTH: u32 = 20;

pub struct BigOlap {
    db: Db<MmapPager>,
    dir: PathBuf,
}

impl BigOlap {
    /// Runs one query through parser, planner and executor, exactly as the HTTP daemon would.
    fn query(&self, text: &str) -> Value {
        big_exec::query(&self.db.read(), TABLE, text)
            .unwrap_or_else(|e| panic!("big failed to answer `{text}`: {e}"))
    }

    /// Turns a `Distinct`, `TopN` or `GroupBy` answer into the `(category, count)` pairs the
    /// ground truth is written in.
    ///
    /// The engine returns interned keys - `c000`, `c001` - because that is what was stored;
    /// parsing the number back out is the harness's job, not evidence of anything.
    pub(crate) fn groups(value: &Value, prefix: &str) -> Vec<(u32, u64)> {
        value
            .as_groups()
            .expect("a grouped query must answer with groups")
            .iter()
            .map(|g| {
                let key = g.key.as_deref().expect("every group here was written with a key");
                let n = key
                    .strip_prefix(prefix)
                    .and_then(|d| d.parse::<u32>().ok())
                    .unwrap_or_else(|| panic!("unexpected group key `{key}`"));
                (n, g.value.as_count().expect("the aggregate here is a count"))
            })
            .collect()
    }
}

impl Olap for BigOlap {
    fn name() -> &'static str {
        "big"
    }

    fn locality() -> Locality {
        Locality::InProcess
    }

    fn open(dir: &Path) -> Self {
        let db = Db::open(MmapPager::open_default(dir.join("big.db")).unwrap()).unwrap();
        db.create_table(TABLE).unwrap();
        db.create_field(TABLE, AMOUNT, FieldKind::Int, BIT_DEPTH).unwrap();
        db.create_field(TABLE, CATEGORY, FieldKind::Set, 0).unwrap();
        db.create_field(TABLE, COUNTRY, FieldKind::Set, 0).unwrap();
        db.create_field(TABLE, ACTIVE, FieldKind::Bool, 0).unwrap();
        Self { db, dir: dir.to_path_buf() }
    }

    /// `bulk_load`, which is the path its own documentation points a first load of known size
    /// at. Every rival here is given its bulk path too - DuckDB gets the appender, ClickHouse
    /// gets one multi-row insert - so this is matching them rather than favouring itself.
    fn load(&mut self, records: &[WideRecord]) {
        let mut load = self.db.bulk_load(TABLE).unwrap();
        for r in records {
            load.set_int(AMOUNT, r.id, r.amount).unwrap();
            load.set_key(CATEGORY, r.id, &WideRecord::category_key(r.category)).unwrap();
            load.set_key(COUNTRY, r.id, &WideRecord::country_key(r.country)).unwrap();
            load.set_bool(ACTIVE, r.id, r.active).unwrap();
        }
        load.finish().unwrap();
    }

    fn count_ge(&self, k: u64) -> u64 {
        self.query(&format!("Count(Row({AMOUNT} >= {k}))")).as_count().unwrap()
    }

    fn intersect_count(&self, country: u32, active: bool, k: u64) -> u64 {
        let country = WideRecord::country_key(country);
        self.query(&format!(
            "Count(Intersect(Row({COUNTRY}=\"{country}\"), Row({ACTIVE}={active}), \
             Row({AMOUNT} >= {k})))"
        ))
        .as_count()
        .unwrap()
    }

    fn sum_where(&self, k: u64) -> u128 {
        self.query(&format!("Sum(Row({AMOUNT} >= {k}), field={AMOUNT})")).as_sum().unwrap()
    }

    fn group_by_count(&self) -> Vec<(u32, u64)> {
        // No `aggregate=`: a `GroupBy` without one counts each group, which is the question.
        let v = self.query(&format!("GroupBy(All(), field={CATEGORY})"));
        let mut groups = Self::groups(&v, "c");
        // The engine orders groups by interned key and the ground truth by category number.
        // Zero-padded keys make those the same order, but sorting says so rather than relying
        // on it.
        groups.sort_unstable();
        groups
    }

    fn top_n(&self, n: usize) -> Vec<(u32, u64)> {
        let v = self.query(&format!("TopN(All(), field={CATEGORY}, n={n})"));
        Self::groups(&v, "c")
    }

    fn distinct(&self, k: u64) -> u64 {
        let v = self.query(&format!("Distinct(Row({AMOUNT} >= {k}), field={CATEGORY})"));
        // `Distinct` answers with every group and its count; the scalar the comparison asks for
        // is how many groups survived the predicate. Groups with no surviving record must not
        // be counted, and the filter says so rather than trusting the executor to omit them.
        v.as_groups().unwrap().iter().filter(|g| g.value.as_count().unwrap_or(0) > 0).count() as u64
    }

    fn checkpoint(&mut self) {
        self.db.store().truncate_tail().unwrap();
    }

    fn disk_bytes(&self) -> Answer<u64> {
        Answer::Given(crate::dir_size(&self.dir))
    }
}

/// The same engine, asked in SQL.
///
/// **This column exists to answer one question: what does the front end cost?** Every rival here
/// is handed a string of SQL and plans it; `big` is handed PQL, and it has always been fair to
/// wonder how much of its margin was simply the absence of a parser and planner on the hot path.
/// The two `big` columns differ in nothing but the language the question is written in - same
/// file, same corpus, same plan underneath - so the gap between them *is* the answer, measured
/// rather than argued.
///
/// It should be small, and the reason is structural rather than hopeful: `big-sql` translates
/// into the query language rather than interpreting anything, so the extra work per query is one
/// pass over a short string. If the gap is not small, that is a result too, and a more
/// interesting one.
pub struct BigSqlOlap(BigOlap);

impl BigSqlOlap {
    /// Translates, plans and runs - which is what `POST /sql` does, minus the socket.
    fn query(&self, sql: &str) -> Value {
        let statement =
            big_sql::translate(sql).unwrap_or_else(|e| panic!("big refused `{sql}`: {e}"));
        // `translate` answers with either half of the SQL surface, and only one of them is a
        // question. The schema this column measures against is built through the catalog in
        // `open`, so a DDL arriving here is the harness asking the wrong thing rather than the
        // engine refusing a right one.
        let Sql::Query(statement) = statement else {
            panic!("the benchmark asks queries, and `{sql}` is a schema change")
        };
        let read = self.0.db.read();
        // The benchmark asks one question per statement, so there is one call. A statement with
        // several would be measuring a different thing from the `big` column beside it.
        let [ask] = statement.calls.as_slice() else {
            panic!("the benchmark asks one question per statement, and `{sql}` asks several")
        };
        let plan = big_plan::plan(&ask.table, &ask.call, &big_exec::CatalogSchema(read.catalog()))
            .unwrap_or_else(|e| panic!("big could not plan `{sql}`: {e}"));
        big_exec::execute(&read, &plan)
            .unwrap_or_else(|e| panic!("big failed to answer `{sql}`: {e}"))
    }
}

impl Olap for BigSqlOlap {
    fn name() -> &'static str {
        "big-sql"
    }

    fn locality() -> Locality {
        Locality::InProcess
    }

    fn open(dir: &Path) -> Self {
        Self(BigOlap::open(dir))
    }

    fn load(&mut self, records: &[WideRecord]) {
        self.0.load(records);
    }

    fn count_ge(&self, k: u64) -> u64 {
        self.query(&format!("SELECT count(*) FROM {TABLE} WHERE {AMOUNT} >= {k}"))
            .as_count()
            .unwrap()
    }

    fn intersect_count(&self, country: u32, active: bool, k: u64) -> u64 {
        let country = WideRecord::country_key(country);
        self.query(&format!(
            "SELECT count(*) FROM {TABLE} WHERE {COUNTRY} = '{country}' AND {ACTIVE} = {active} \
             AND {AMOUNT} >= {k}"
        ))
        .as_count()
        .unwrap()
    }

    fn sum_where(&self, k: u64) -> u128 {
        self.query(&format!("SELECT sum({AMOUNT}) FROM {TABLE} WHERE {AMOUNT} >= {k}"))
            .as_sum()
            .unwrap()
    }

    fn group_by_count(&self) -> Vec<(u32, u64)> {
        let v = self.query(&format!(
            "SELECT {CATEGORY}, count(*) FROM {TABLE} GROUP BY {CATEGORY} ORDER BY {CATEGORY}"
        ));
        let mut groups = BigOlap::groups(&v, "c");
        groups.sort_unstable();
        groups
    }

    fn top_n(&self, n: usize) -> Vec<(u32, u64)> {
        let v = self.query(&format!(
            "SELECT {CATEGORY}, count(*) AS n FROM {TABLE} GROUP BY {CATEGORY} \
             ORDER BY n DESC LIMIT {n}"
        ));
        BigOlap::groups(&v, "c")
    }

    fn distinct(&self, k: u64) -> u64 {
        // `count(DISTINCT ...)` plans as a `Distinct` and the counting is the shape's job, which
        // over one node is this line. The filter for empty groups is the one the PQL column
        // applies, for the same reason.
        let v = self.query(&format!(
            "SELECT count(DISTINCT {CATEGORY}) FROM {TABLE} WHERE {AMOUNT} >= {k}"
        ));
        v.as_groups().unwrap().iter().filter(|g| g.value.as_count().unwrap_or(0) > 0).count() as u64
    }

    fn checkpoint(&mut self) {
        self.0.checkpoint();
    }

    fn disk_bytes(&self) -> Answer<u64> {
        self.0.disk_bytes()
    }
}
