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

//! Every analytical adapter, checked against ground truth before anyone reports a timing.
//!
//! `measure` already asserts each answer, so these tests are thin on purpose: their job is to
//! run it in CI, where a broken adapter should fail the build rather than wait to be noticed as
//! an implausible row in a report.

use big_bench::engines::big_olap::BigOlap;
use big_bench::olap::{measure, Questions};
use big_bench::wide::wide_workload;
use big_bench::Layout;

/// The smallest corpus the wide workload allows - 256 categories need `256*257/2` records to
/// hold strictly distinct frequencies. Small on purpose: this is a correctness test, and the
/// report is where size is a variable.
const N: u64 = 32_896;

#[test]
fn big_answers_every_analytical_question_correctly() {
    let dir = tempfile::tempdir().unwrap();
    let records = wide_workload(N, Layout::Dense);
    let m = measure::<BigOlap>(dir.path(), &records, Questions::default());
    assert_eq!(m.name, "big");
    assert!(m.disk_bytes.given().is_some_and(|b| *b > 0), "big must report a file size");
}

#[test]
fn a_sparse_layout_changes_the_cost_and_not_the_answers() {
    let dir = tempfile::tempdir().unwrap();
    let records = wide_workload(N, Layout::Sparse { shards: 4 });
    // Same assertions inside `measure`; the point is that spreading records across fragments
    // must not change a single answer.
    measure::<BigOlap>(dir.path(), &records, Questions::default());
}

#[cfg(feature = "duckdb-peer")]
#[test]
fn duckdb_answers_every_analytical_question_correctly() {
    let dir = tempfile::tempdir().unwrap();
    let records = wide_workload(N, Layout::Dense);
    let m = measure::<big_bench::engines::duckdb::DuckdbOlap>(
        dir.path(),
        &records,
        Questions::default(),
    );
    assert!(m.disk_bytes.given().is_some_and(|b| *b > 0), "duckdb must report a file size");
}

#[cfg(feature = "datafusion-peer")]
#[test]
fn datafusion_answers_every_analytical_question_correctly() {
    let dir = tempfile::tempdir().unwrap();
    let records = wide_workload(N, Layout::Dense);
    measure::<big_bench::engines::datafusion::DatafusionOlap>(
        dir.path(),
        &records,
        Questions::default(),
    );
}

/// The server peers are started by hand, so their tests skip rather than fail when nothing is
/// listening.
///
/// Skipping is right here and would be wrong anywhere else in this harness: a server that is
/// absent has not answered wrongly, it has not answered. A server that *is* up and disagrees
/// with the ground truth still fails, loudly, inside `measure`.
#[cfg(feature = "http-peers")]
#[test]
fn clickhouse_answers_every_analytical_question_correctly() {
    use big_bench::engines::clickhouse::ClickhouseOlap;
    if !ClickhouseOlap::available() {
        eprintln!("skipped: no clickhouse listening");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let records = wide_workload(N, Layout::Dense);
    measure::<ClickhouseOlap>(dir.path(), &records, Questions::default());
}
