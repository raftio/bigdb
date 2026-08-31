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

//! Scratch profiler: why does a grouped aggregation over `All()` cost four hundred milliseconds?
//!
//! Not part of the harness. Temporary, for the `All()` question the analytical report raised and
//! could not answer.
//!
//! The report reached its suspect by subtraction: `distinct` and `top_n` call the same
//! `group_counts` over bitmaps of 50,000 and 200,000 records, and four times the records cost
//! nearly sixty times the time, which leaves the cost outside the grouping loop and inside
//! whatever produced the bitmap. Subtraction can say where the time is *not*. It cannot say
//! where it is, and a report should not promote it to a cause.
//!
//! So this times each stage on its own, and times the two ways of building the same bitmap
//! against each other:
//!
//! - `all()` reads the stored exists row.
//! - `matching(amount >= 0)` computes the identical set from twenty bit planes.
//!
//! Both answer "every record". If the first is slower than the second, reading is costing more
//! than computing and the suspect is confirmed. If they agree, `all()` is innocent, the cost is
//! in `group_counts` scaling with how dense its filter is, and the report needs correcting.

use big_bench::wide::{wide_workload, WideRecord, CATEGORIES};
use big_bench::Layout;
use big_db::catalog::FieldKind;
use big_db::{Db, RangeOp};
use big_pager::MmapPager;
use std::hint::black_box;
use std::time::Instant;

const TABLE: &str = "t";
const AMOUNT: &str = "amount";
const CATEGORY: &str = "category";
const BIT_DEPTH: u32 = 20;

/// Three, like every other timing in this benchmark: enough to discard one descheduled run.
/// These calls are milliseconds, so more would buy precision nobody is going to use.
const ITERS: u32 = 3;

fn time<T, F: FnMut() -> T>(mut f: F) -> f64 {
    // One warm-up, unmeasured, so the first run's page faults are not charged to the median.
    black_box(f());
    let mut runs: Vec<f64> = (0..ITERS)
        .map(|_| {
            let t = Instant::now();
            black_box(f());
            t.elapsed().as_secs_f64() * 1e6
        })
        .collect();
    runs.sort_by(f64::total_cmp);
    runs[runs.len() / 2]
}

fn main() {
    println!("Every column is microseconds, median of {ITERS}.\n");
    println!(
        "{:>9} {:>12} {:>14} {:>13} {:>15} {:>13} {:>15}",
        "records",
        "all()",
        "matching(>=0)",
        "count_all()",
        "group/all()",
        "group/pred",
        "TopN(All())"
    );

    for n in [50_000u64, 100_000, 200_000, 400_000] {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(MmapPager::open_default(dir.path().join("big.db")).unwrap()).unwrap();
        db.create_table(TABLE).unwrap();
        db.create_field(TABLE, AMOUNT, FieldKind::Int, BIT_DEPTH).unwrap();
        db.create_field(TABLE, CATEGORY, FieldKind::Set, 0).unwrap();

        let records = wide_workload(n, Layout::Dense);
        let mut load = db.bulk_load(TABLE).unwrap();
        for r in &records {
            load.set_int(AMOUNT, r.id, r.amount).unwrap();
            load.set_key(CATEGORY, r.id, &WideRecord::category_key(r.category)).unwrap();
        }
        load.finish().unwrap();

        // A fresh `read()` per iteration, not one reused across them: a `DbRead` accumulates
        // what it has charged against the memory budget, so reusing it would measure a
        // different thing on the third call than on the first - and the query path opens one
        // per query anyway.
        let t_all = time(|| db.read().all(TABLE).unwrap());
        let t_pred = time(|| db.read().matching(TABLE, AMOUNT, RangeOp::Ge, 0).unwrap());
        let t_count = time(|| db.read().count_all(TABLE).unwrap());

        // The same grouping call over the two bitmaps, with the bitmap built outside the timed
        // region. This is the half the report could only reach by subtraction.
        let bitmap_all = db.read().all(TABLE).unwrap();
        let bitmap_pred = db.read().matching(TABLE, AMOUNT, RangeOp::Ge, 0).unwrap();
        assert_eq!(
            bitmap_all.cardinality(),
            bitmap_pred.cardinality(),
            "the two bitmaps must hold the same records, or this compares two questions"
        );
        let t_group_all = time(|| db.read().group_counts(TABLE, CATEGORY, &bitmap_all).unwrap());
        let t_group_pred = time(|| db.read().group_counts(TABLE, CATEGORY, &bitmap_pred).unwrap());

        let t_topn = time(|| {
            big_exec::query(&db.read(), TABLE, &format!("TopN(All(), field={CATEGORY}, n=10)"))
                .unwrap()
        });

        // Correctness before timing, exactly as the harness does it: a profiler measuring a
        // wrong answer measures nothing either.
        let groups = db.read().group_counts(TABLE, CATEGORY, &bitmap_all).unwrap();
        assert_eq!(groups.len(), CATEGORIES as usize, "every category must appear");
        assert_eq!(groups.iter().map(|(_, c)| c).sum::<u64>(), n);

        println!(
            "{n:>9} {t_all:>12.0} {t_pred:>14.0} {t_count:>13.0} \
             {t_group_all:>15.0} {t_group_pred:>13.0} {t_topn:>15.0}"
        );
    }

    println!(
        "\n`all()` and `matching(>=0)` return the same set by different means - one reads the \
         stored exists row, the other computes it from {BIT_DEPTH} bit planes."
    );
    println!(
        "`group/all()` and `group/pred` are the same call over those two bitmaps, with the \
         bitmap built outside the timed region."
    );
    println!(
        "If `all()` dominates, the report's suspect is confirmed. If the two `group/` columns \
         differ instead, it is not."
    );
}
