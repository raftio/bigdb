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

//! DataFusion over Parquet: the shape the modern analytical stack actually arrives in.
//!
//! It earns its column by being the entrant with **no storage engine of its own**. Every other
//! engine in either table owns its file format and decides what its pages look like; this one is
//! a query engine pointed at Parquet, which is how a great deal of analytical work is now built
//! - files in object storage, a planner over the top, nothing durable in between.
//!
//! That is also its limit, and the limit is stated rather than worked around. There is no
//! transaction here, so there is nothing to make durable and nothing to remove; the report
//! prints `n/a` with the reason. Arrow and Parquet come through DataFusion's own re-exports
//! rather than as separate dependencies, so there is no way for their versions to drift apart
//! from the one it was built against.

use crate::olap::{Answer, Locality, Olap};
use crate::wide::WideRecord;
use datafusion::arrow::array::{
    Array, ArrayRef, BooleanArray, Int64Array, StringArray, StringViewArray, UInt64Array,
};
use datafusion::arrow::datatypes::{DataType, Field, Schema};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::parquet::arrow::ArrowWriter;
use datafusion::prelude::{ParquetReadOptions, SessionContext};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub struct DatafusionOlap {
    ctx: SessionContext,
    runtime: tokio::runtime::Runtime,
    dir: PathBuf,
}

impl DatafusionOlap {
    /// Runs one SQL statement and returns the batches it produced.
    fn sql(&self, sql: &str) -> Vec<RecordBatch> {
        self.runtime
            .block_on(async {
                let df = self.ctx.sql(sql).await?;
                df.collect().await
            })
            .unwrap_or_else(|e| panic!("datafusion failed to answer `{sql}`: {e}"))
    }

    /// Reads a single-row, single-column answer as a `u128`.
    ///
    /// The width is decided by the planner - `count` comes back signed, `sum` over an unsigned
    /// column comes back unsigned - so this matches on what actually arrived instead of asking
    /// for a cast in the SQL and measuring the cast.
    fn scalar(&self, sql: &str) -> u128 {
        let batches = self.sql(sql);
        let batch = batches.first().unwrap_or_else(|| panic!("`{sql}` returned no rows"));
        let column = batch.column(0);
        match column.data_type() {
            DataType::Int64 => {
                let v = column.as_any().downcast_ref::<Int64Array>().unwrap().value(0);
                u128::try_from(v).expect("no answer in this comparison is negative")
            }
            DataType::UInt64 => {
                u128::from(column.as_any().downcast_ref::<UInt64Array>().unwrap().value(0))
            }
            other => panic!("`{sql}` answered with an unexpected type {other:?}"),
        }
    }

    /// Reads a string column, whichever of Arrow's two string layouts it arrived in.
    ///
    /// DataFusion reads Parquet `Utf8` back as `Utf8View` by default - a view array keeps short
    /// strings inline and long ones behind an offset, which is faster and is a different Rust
    /// type. Handling both here rather than turning the setting off is the honest choice: the
    /// alternative is making the engine read its own files in a slower mode so that this
    /// harness can downcast to the type it expected first.
    fn strings(column: &ArrayRef) -> Vec<&str> {
        let n = column.len();
        if let Some(a) = column.as_any().downcast_ref::<StringArray>() {
            (0..n).map(|i| a.value(i)).collect()
        } else if let Some(a) = column.as_any().downcast_ref::<StringViewArray>() {
            (0..n).map(|i| a.value(i)).collect()
        } else {
            panic!(
                "a key column came back as {:?}, which is neither string layout",
                column.data_type()
            )
        }
    }

    fn groups(&self, sql: &str) -> Vec<(u32, u64)> {
        let mut out = Vec::new();
        for batch in self.sql(sql) {
            let keys = Self::strings(batch.column(0));
            let counts = batch.column(1).as_any().downcast_ref::<Int64Array>().unwrap();
            for (i, key) in keys.iter().enumerate() {
                let category = key
                    .strip_prefix('c')
                    .and_then(|d| d.parse::<u32>().ok())
                    .unwrap_or_else(|| panic!("unexpected group key `{key}`"));
                out.push((category, counts.value(i) as u64));
            }
        }
        out
    }
}

impl Olap for DatafusionOlap {
    fn name() -> &'static str {
        "datafusion"
    }

    fn locality() -> Locality {
        Locality::InProcess
    }

    fn open(dir: &Path) -> Self {
        // A current-thread runtime would serialise the whole engine and measure something no
        // DataFusion user would ever run; the multi-threaded one is the default its own
        // documentation starts from.
        let runtime = tokio::runtime::Runtime::new().unwrap();
        Self { ctx: SessionContext::new(), runtime, dir: dir.to_path_buf() }
    }

    /// Writes the corpus as one Parquet file and registers it.
    ///
    /// One file rather than many: partitioning is a tuning decision this harness has no basis to
    /// make on DataFusion's behalf, and a partition layout chosen to suit the six questions
    /// below would be the rigging the storage table's rules already forbid.
    fn load(&mut self, records: &[WideRecord]) {
        let schema = Arc::new(Schema::new(vec![
            Field::new("id", DataType::UInt64, false),
            Field::new("amount", DataType::UInt64, false),
            Field::new("category", DataType::Utf8, false),
            Field::new("country", DataType::Utf8, false),
            Field::new("active", DataType::Boolean, false),
        ]));

        let columns: Vec<ArrayRef> = vec![
            Arc::new(records.iter().map(|r| r.id).collect::<UInt64Array>()),
            Arc::new(records.iter().map(|r| r.amount).collect::<UInt64Array>()),
            // `Some` on every value: Arrow builds a string array from an iterator of options,
            // because a column that cannot be null is a special case of one that can. The
            // schema above already declares these non-nullable, so nothing is actually optional
            // here.
            Arc::new(
                records
                    .iter()
                    .map(|r| Some(WideRecord::category_key(r.category)))
                    .collect::<StringArray>(),
            ),
            Arc::new(
                records
                    .iter()
                    .map(|r| Some(WideRecord::country_key(r.country)))
                    .collect::<StringArray>(),
            ),
            Arc::new(records.iter().map(|r| Some(r.active)).collect::<BooleanArray>()),
        ];
        let batch = RecordBatch::try_new(schema.clone(), columns).unwrap();

        let path = self.dir.join("t.parquet");
        let file = std::fs::File::create(&path).unwrap();
        let mut writer = ArrowWriter::try_new(file, schema, None).unwrap();
        writer.write(&batch).unwrap();
        writer.close().unwrap();

        self.runtime
            .block_on(self.ctx.register_parquet(
                "t",
                path.to_str().unwrap(),
                ParquetReadOptions::default(),
            ))
            .unwrap();
    }

    fn count_ge(&self, k: u64) -> u64 {
        self.scalar(&format!("SELECT count(*) FROM t WHERE amount >= {k}")) as u64
    }

    fn intersect_count(&self, country: u32, active: bool, k: u64) -> u64 {
        let country = WideRecord::country_key(country);
        self.scalar(&format!(
            "SELECT count(*) FROM t \
             WHERE country = '{country}' AND active = {active} AND amount >= {k}"
        )) as u64
    }

    fn sum_where(&self, k: u64) -> u128 {
        self.scalar(&format!("SELECT sum(amount) FROM t WHERE amount >= {k}"))
    }

    fn group_by_count(&self) -> Vec<(u32, u64)> {
        let mut groups =
            self.groups("SELECT category, count(*) FROM t GROUP BY category ORDER BY category");
        // Batches arrive in whatever order the plan produced them, and `ORDER BY` orders within
        // the result rather than guaranteeing one batch; sorting here rather than trusting that.
        groups.sort_unstable();
        groups
    }

    fn top_n(&self, n: usize) -> Vec<(u32, u64)> {
        self.groups(&format!(
            "SELECT category, count(*) AS n FROM t GROUP BY category ORDER BY n DESC LIMIT {n}"
        ))
    }

    fn distinct(&self, k: u64) -> u64 {
        self.scalar(&format!("SELECT count(DISTINCT category) FROM t WHERE amount >= {k}")) as u64
    }

    /// The Parquet file was closed when it was written; there is nothing deferred here to flush.
    fn checkpoint(&mut self) {}

    fn disk_bytes(&self) -> Answer<u64> {
        Answer::Given(crate::dir_size(&self.dir))
    }
}
