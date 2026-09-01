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

//! Write amplification, gated.
//!
//! The benchmark's timing columns wobble by up to 2.7x between runs on an idle machine. Its
//! byte columns do not wobble at all: the same figures came out of two VPS runs and a macOS run,
//! to the byte. That makes bytes-written the one thing the benchmark measures that can be
//! asserted rather than reported, and asserting it is free - no flakiness, no dedicated
//! hardware, no criterion.
//!
//! So this is not a benchmark. It is a test with the same workload as
//! `bench/src/bin/report.rs`, run under `cargo test` on every push, and the numbers below are
//! the numbers in [`bench/results/REPORT.md`](../../../bench/results/REPORT.md). If a change
//! moves either, both move together or this fails.
//!
//! **The equality is on purpose.** A ceiling would let an improvement land silently and leave
//! the report quoting a figure the engine no longer produces - which is exactly how that report
//! ended up printing a conclusion its own data contradicted. Making a better number fail too
//! costs one line to update and buys a report that cannot go stale.
//!
//! It lives here rather than in `bench/` because `bench/` pulls in rival engines and CI does not
//! build it. This needs none of them: write amplification is `big`'s own property, and measuring
//! it against a rival would only add a reason for the gate not to run.

use big_db::*;
use big_pager::{CountingPager, MemPager};

const N: u64 = 10_000;
const VALUE_CEILING: u64 = 1 << 20;
const BIT_DEPTH: u32 = 20;
/// Duplicated from `big_engine::bitmap` rather than imported, for the same reason the benchmark
/// duplicates it: the workload must not be defined in terms of the thing it is measuring.
const SHARD_WIDTH: u64 = 1 << 20;

/// How record ids are spread out. The axis `big` is sensitive to and its rivals are not.
#[derive(Clone, Copy)]
enum Layout {
    /// Ids 0..n, so a batch of consecutive records lands in one shard and one fragment.
    Dense,
    /// Ids spread across `shards`, the shape a global id space really has. The same batch now
    /// puts `batch / shards` records into each fragment, and pays per fragment.
    Sparse { shards: u64 },
}

impl Layout {
    fn label(self) -> String {
        match self {
            Self::Dense => "dense".to_string(),
            Self::Sparse { shards } => format!("sparse/{shards}"),
        }
    }
}

/// The benchmark's workload, to the byte. Deterministic on purpose: a real RNG would make the
/// figures incomparable between machines for no gain, and comparability is the whole point.
fn workload(n: u64, layout: Layout) -> Vec<(u64, u64)> {
    (0..n)
        .map(|i| {
            let id = match layout {
                Layout::Dense => i,
                Layout::Sparse { shards } => (i % shards) * SHARD_WIDTH + i / shards,
            };
            // Knuth's multiplicative hash: cheap, deterministic, and not monotonic in `i`.
            (id, i.wrapping_mul(2_654_435_761) % VALUE_CEILING)
        })
        .collect()
}

/// Bytes the pager wrote per record, committing every `batch` records.
fn bytes_per_record(records: &[(u64, u64)], batch: usize) -> u64 {
    let d = Db::open(CountingPager::new(MemPager::new())).unwrap();
    d.create_table_with("t", TableEngine::Bitmap).unwrap();
    d.create_field("t", "v", FieldKind::Int, BIT_DEPTH).unwrap();

    // After the schema, so the two catalog commits that declaring a table costs are not charged
    // to the records.
    let before = d.store().pager().counts().bytes_written();
    for chunk in records.chunks(batch) {
        let mut w = d.write();
        for (id, v) in chunk {
            w.set_int("t", "v", *id, *v).unwrap();
        }
        w.commit().unwrap();
    }
    let written = d.store().pager().counts().bytes_written() - before;
    // Rounded, not truncated, so this is the same number the report prints rather than one that
    // differs from it by one in a way nobody could explain from the two sources.
    (written as f64 / records.len() as f64).round() as u64
}

/// `(batch, bytes per record)`.
///
/// To change one of these: run `cargo run -p big-bench --release --bin report`, confirm the
/// figure moved for a reason you can name, and update **both** this table and the write
/// amplification section of `bench/results/REPORT.md` in the same commit.
///
/// **Moved once, deliberately, on 2026-08-28**, from `2,929 / 238 / 19` dense. Three changes
/// landed together and the gate is what measured them: a per-container delta, leaf merging, and
/// an inline ceiling chosen for rewrite cost rather than for stored size. Dense improved 1.7x at
/// batch 100 and 1.3x at batch 1,000. Batch 10,000 got **one byte worse** - a single commit into
/// an empty file gains nothing from any of the three and pays the inline ceiling's extra space -
/// and that is recorded rather than smoothed over, because a change that is a win at four batch
/// sizes and a rounding loss at one is a trade and not a free lunch.
///
/// Sparse did not move at all. Its fragments hold too few records for a container to be dense,
/// so there is nothing to delta and nothing to merge; the cost there is the per-fragment path,
/// which is a different problem and still open.
///
/// **Moved again, deliberately, on 2026-09-01**, from `26,299 / 2,272 / 110` sparse. That open
/// problem, closed: a bit-sliced value used to write a *clear* for every zero plane, and
/// `Bsi::group_all` skips the ones for records the fragment has never held - they were
/// erasing bits that were never set. `BulkLoad` had always dropped them wholesale on the same
/// reasoning; this is the per-record version of it, so a stream gets it too.
///
/// **Dense did not move, and that is the shape of the win rather than a disappointment.** A
/// dense batch puts its records in one container, which the *set* bits rewrite regardless, so
/// dropping the clears removes no work. A sparse batch puts each record in a container of its
/// own, so the clears were naming containers nothing else touched - and `write_bits` reads and
/// rewrites every container either half names. 2.4x at batch 100, 2.1x at batch 1,000, and
/// nothing at 10,000, where one commit into an empty file has nothing to clear anyway.
///
/// To read the six current figures without running the whole report:
/// `cargo test --test amplification -- --ignored --nocapture measured_amplification`.
const DENSE: [(usize, u64); 3] = [(100, 1_712), (1_000, 177), (10_000, 20)];
const SPARSE_64: [(usize, u64); 3] = [(100, 10_960), (1_000, 1_098), (10_000, 110)];

fn check(layout: Layout, expected: &[(usize, u64)]) {
    let records = workload(N, layout);
    for &(batch, want) in expected {
        let got = bytes_per_record(&records, batch);
        assert_eq!(
            got,
            want,
            "\n{} at batch={batch}: {got} bytes/record, expected {want}.\n\
             {}\n\
             If this is an improvement, update this table and bench/results/REPORT.md together.\n\
             If it is not, a commit made every write in the engine cost {:.1}x what it did.",
            layout.label(),
            if got > want { "This is a regression." } else { "This is an improvement." },
            got as f64 / want as f64,
        );
    }
}

#[test]
fn dense_write_amplification_has_not_moved() {
    check(Layout::Dense, &DENSE);
}

#[test]
fn sparse_write_amplification_has_not_moved() {
    check(Layout::Sparse { shards: 64 }, &SPARSE_64);
}

#[test]
fn batching_still_amortises() {
    // The shape, not the numbers - so that a legitimate update to the table above cannot
    // quietly invert what the engine does. Bytes per record must fall as batches grow; if it
    // ever stops falling, the cost has moved from per-commit to per-record and the advice in
    // the report ("commit less often") is wrong.
    for expected in [&DENSE, &SPARSE_64] {
        for pair in expected.windows(2) {
            let (small, large) = (pair[0], pair[1]);
            assert!(
                large.1 < small.1,
                "batch {} costs {} bytes/record, batch {} costs {} - batching stopped helping",
                small.0,
                small.1,
                large.0,
                large.1
            );
        }
    }
}

#[test]
fn the_sparse_penalty_is_still_there() {
    // The finding the whole sparse column exists to carry: at the same batch size, spreading
    // the same records across 64 shards costs multiples more, because a batch divided by 64 is
    // a small batch as far as a fragment is concerned. A change that made this vanish would be
    // remarkable and must not land unnoticed.
    for (&(batch, dense), &(_, sparse)) in DENSE.iter().zip(SPARSE_64.iter()) {
        assert!(
            sparse > dense * 4,
            "batch {batch}: sparse {sparse} vs dense {dense} - the fan-out penalty has changed \
             shape, which is a finding, not a passing test"
        );
    }
}

/// `Ingest::with_flush_fraction`, gated like everything else here.
///
/// The knob commits only the fullest fraction of the buffered shards and lets the rest keep
/// accumulating, so a fragment is committed having gathered records from several buffer-fulls
/// rather than one. It exists because a commit's cost has a floor of roughly four to six pages
/// **per fragment it touches** - the copy-on-write rewrite of that fragment's root-to-leaf path
/// - paid whether the fragment received a thousand records or one.
///
/// **The figures below are a trade and are asserted as one.** The knob buys bytes with commits,
/// and a commit costs two fsyncs. Wall clock over 60,000 records at full durability on the
/// development machine: 64 shards went from 147ms to 175ms (**worse**), 256 shards from 322ms to
/// 276ms and 4,096 shards from 4,822ms to 3,369ms (**better**). So it is off by default, and the
/// dense row below is the one that matters most: it asserts the knob is free to leave on for a
/// workload that cannot benefit from it.
mod flush_fraction {
    use super::*;

    const CAPACITY: usize = 1_000;

    pub(super) fn bytes_per_record(records: &[(u64, u64)], fraction: f64) -> u64 {
        let d = Db::open(CountingPager::new(MemPager::new())).unwrap();
        d.create_table_with("t", TableEngine::Bitmap).unwrap();
        d.create_field("t", "v", FieldKind::Int, BIT_DEPTH).unwrap();
        let before = d.store().pager().counts().bytes_written();

        let mut i = d.ingest(CAPACITY).with_flush_fraction(fraction);
        for (id, v) in records {
            i.set_int("t", "v", *id, *v).unwrap();
        }
        i.finish().unwrap();

        let written = d.store().pager().counts().bytes_written() - before;
        (written as f64 / records.len() as f64).round() as u64
    }

    /// `(shards, default bytes/record, staggered at 0.25)`.
    ///
    /// To change these: re-measure, confirm the figure moved for a reason you can name, and
    /// update this table and the knob's doc comment together.
    /// **Moved deliberately on 2026-09-01**, from `2,272/1,574 · 10,302/7,295 · 45,130/29,767`.
    /// `Bsi::group_all` does not write a clear for every zero plane of a record the fragment has
    /// never held. 2.1x at 64 shards, 2.4x at 256, 2.7x at 1,024 - the wider the fan-out the
    /// more of it, because the clears were naming containers no set bit touched and a fragment
    /// holding few records has more such containers per record.
    ///
    /// The knob's own ratio barely moved: 1.4x before, 1.3x after. It buys the same thing it
    /// always did, on a smaller number.
    const GRID: [(u64, u64, u64); 3] =
        [(64, 1_098, 822), (256, 4_301, 3_191), (1_024, 16_726, 11_249)];

    #[test]
    fn staggering_reduces_bytes_by_the_measured_amount() {
        for (shards, plain, staggered) in GRID {
            let records = workload(N, Layout::Sparse { shards });
            assert_eq!(bytes_per_record(&records, 1.0), plain, "sparse/{shards}, knob off");
            assert_eq!(
                bytes_per_record(&records, 0.25),
                staggered,
                "sparse/{shards}, knob at 0.25"
            );
        }
    }

    #[test]
    fn staggering_is_free_on_a_dense_workload() {
        // The claim that makes the knob safe to reach for without knowing the id distribution:
        // a single-shard ingest has nothing to stagger, so the policy must not cost it a byte.
        let records = workload(N, Layout::Dense);
        assert_eq!(
            bytes_per_record(&records, 0.25),
            bytes_per_record(&records, 1.0),
            "a dense ingest paid for a knob it cannot benefit from"
        );
    }

    /// The shape, not the numbers - so a legitimate update to the grid cannot quietly invert it.
    #[test]
    fn staggering_never_costs_bytes() {
        for (shards, _, _) in GRID {
            let records = workload(N, Layout::Sparse { shards });
            let plain = bytes_per_record(&records, 1.0);
            let staggered = bytes_per_record(&records, 0.25);
            assert!(
                staggered <= plain,
                "sparse/{shards}: staggering wrote {staggered} bytes/record against {plain}"
            );
        }
    }
}

// ------------------------------------------------------------------------------------------
// What the engines cost, relative to each other
// ------------------------------------------------------------------------------------------

/// The same workload under each engine, as bytes written per record.
fn bytes_per_record_under(engine: TableEngine, records: &[(u64, u64)], batch: usize) -> u64 {
    let d = Db::open(CountingPager::new(MemPager::new())).unwrap();
    d.create_table_with("t", engine).unwrap();
    d.create_field("t", "v", FieldKind::Int, BIT_DEPTH).unwrap();

    let before = d.store().pager().counts().bytes_written();
    for chunk in records.chunks(batch) {
        let mut w = d.write();
        for (id, v) in chunk {
            w.set_int("t", "v", *id, *v).unwrap();
        }
        w.commit().unwrap();
    }
    (d.store().pager().counts().bytes_written() - before) / records.len() as u64
}

/// What the second half of the default engine costs, in the one unit that does not wobble.
///
/// **This is the number that decides whether `bitmap+columnar` is the right default**, so it is
/// asserted rather than reported. The columns are not free and pretending otherwise in a doc
/// comment would be exactly the kind of stale claim the gate above exists to prevent.
///
/// Equality for the same reason every figure here is equality: a change that makes the columns
/// cheaper should update this line, not slip past a ceiling.
#[test]
fn what_each_engine_costs_to_write() {
    let records = workload(N, Layout::Dense);
    let batch = 100;

    let bitmap = bytes_per_record_under(TableEngine::Bitmap, &records, batch);
    let both = bytes_per_record_under(TableEngine::BitmapColumnar, &records, batch);
    let columnar = bytes_per_record_under(TableEngine::Columnar, &records, batch);

    assert_eq!(
        (bitmap, both, columnar),
        (1712, 1864, 488),
        "\ndense at batch={batch}, bytes/record: bitmap {bitmap}, \
         bitmap+columnar {both}, columnar {columnar}.\n\
         If this is a deliberate change, update this line and `bench/results/REPORT.md`\n\
         together - and re-read whether `bitmap+columnar` is still the right default."
    );

    // The two properties the engine choice is supposed to have. Stated as inequalities because
    // these are what must stay true whatever the figures move to.
    assert!(both > bitmap, "adding columns has to cost something, or the choice is not real");
    assert!(columnar < bitmap, "columns alone have to be cheaper than an index, or why choose");
}

// ------------------------------------------------------------------------------------------
// The cost model: what a commit is actually paying for
// ------------------------------------------------------------------------------------------

/// Where a commit's pages go, and what that says about which fix is the right one.
///
/// The tables above measure bytes per *record*, which is the number a user feels. This module
/// measures the thing underneath it: **a commit's cost is set by how many fragments it touches,
/// not by how many records it carries.** Every figure here is a page count from
/// [`big_pager::Metrics::last_commit`], which splits a commit by what the pages were - and that
/// split is the whole point, because "the commit wrote 1088 pages" admits two explanations that
/// call for opposite fixes.
///
/// **This exists to say which of the two it is.** The fixed chains a commit rewrites whole - root
/// records, catalog, freelist - are the obvious suspect and are the wrong one: they are under 1%
/// of a commit at any realistic fan-out. What is left is the copy-on-write rewrite of one
/// root-to-leaf path per fragment, paid whether that fragment received a thousand records or one.
///
/// So this is the baseline a part-based engine has to beat, stated as three properties rather
/// than as a wish: a commit is flat in batch size, linear in field count, and does not spend its
/// bytes on the fixed chains. An engine that collapses a shard's fields into one tree per commit
/// breaks the second, which is what makes the first stop mattering.
mod cost_model {
    use super::*;
    use big_pager::metrics::CommitBreakdown;

    /// Fields per record, at the same layout the sparse table above uses.
    const SHARDS: u64 = 64;

    /// What the last commit of the run wrote, by class.
    ///
    /// The *last* commit rather than the total, because it is the steady state: by then every
    /// fragment exists and every write is the read-modify-write this module is about. A total
    /// would average that against the first commit, which builds its trees bottom-up and is the
    /// one case the engine already does well.
    pub(super) fn last_commit(fields: usize, batch: usize) -> (CommitBreakdown, usize) {
        let d = Db::open(CountingPager::new(MemPager::new())).unwrap();
        d.create_table_with("t", TableEngine::Bitmap).unwrap();
        let names: Vec<String> = (0..fields).map(|i| format!("f{i}")).collect();
        for name in &names {
            d.create_field("t", name, FieldKind::Int, BIT_DEPTH).unwrap();
        }

        let records = workload(N, Layout::Sparse { shards: SHARDS });
        for chunk in records.chunks(batch) {
            let mut w = d.write();
            for (id, v) in chunk {
                for name in &names {
                    w.set_int("t", name, *id, *v).unwrap();
                }
            }
            w.commit().unwrap();
        }
        let m = d.store().metrics();
        (m.last_commit, m.fragments)
    }

    /// `(fields, data pages one steady-state commit writes)`.
    ///
    /// To change these: confirm the figure moved for a reason you can name, and say in the same
    /// commit which of the three properties below it changes. A number here moving without one
    /// of those tests failing means this table is being updated to match the code rather than
    /// the code being measured against it.
    /// **Moved deliberately on 2026-09-01**, from `320 / 576 / 1,088 / 2,112`. The shape is what
    /// matters, not the four numbers: it was `256 x fields + 64` and it is now `64 x fields +
    /// 64`. **The per-field cost of a commit fell fourfold**; the fixed part did not move,
    /// which is the property `the_fixed_chains_are_not_where_the_bytes_go` asserts separately.
    ///
    /// The cause is one change: a bit-sliced value no longer writes a clear for every zero
    /// plane of a record the fragment has never held. Those clears named containers that no set
    /// bit named, and `write_bits` reads and rewrites every container either half names - so
    /// each field was paying for its own zeros, once per commit, for ever.
    ///
    /// It gets better with more fields, which is the direction that matters: 2.5x at one field
    /// and 3.7x at eight.
    const PAGES_BY_FIELD: [(usize, u64); 4] = [(1, 128), (2, 192), (4, 320), (8, 576)];

    /// **The finding.** Ten times the records in one commit, to the page identical.
    ///
    /// Not "roughly the same" - byte-identical, at every field count, which is what makes it a
    /// structural property rather than a measurement. A commit pays for the path it rewrites in
    /// each fragment it touches; the records riding along in that path are free until they
    /// overflow a container.
    ///
    /// This is the sentence a part-based engine exists to falsify. Until one lands, it is also
    /// the reason the advice in `bench/results/REPORT.md` is "commit less often" rather than
    /// anything about the shape of the data.
    #[test]
    fn a_commit_costs_the_same_at_100_records_as_at_1000() {
        for (fields, _) in PAGES_BY_FIELD {
            let (small, _) = last_commit(fields, 100);
            let (large, _) = last_commit(fields, 1_000);
            assert_eq!(
                small.data, large.data,
                "{fields} field(s): a commit of 100 records wrote {} data pages and one of \
                 1,000 wrote {}.\nIf these have come apart, a commit's cost has moved from \
                 per-fragment to per-record - which is the thing part-based engines are for, \
                 so say so here rather than deleting this test.",
                small.data, large.data,
            );
        }
    }

    /// The cost is linear in the field count, because each field is its own tree in each shard.
    ///
    /// **This is the line a part-based engine cuts.** A part holding every field of one shard is
    /// one tree per commit instead of `fields` of them, so what is linear here becomes flat -
    /// and at eight fields that is the difference between 2,112 pages and something near 320.
    #[test]
    fn a_commit_scales_with_the_field_count() {
        for (fields, want) in PAGES_BY_FIELD {
            let (c, frags) = last_commit(fields, 1_000);
            assert_eq!(
                c.data,
                want,
                "\n{fields} field(s), {frags} fragments: {} data pages, expected {want}.\n\
                 That is {:.2} pages per fragment.",
                c.data,
                c.data as f64 / frags as f64,
            );
        }
        // The shape, so a legitimate update to the table cannot quietly invert it: doubling the
        // fields has to roughly double the pages, or the trees are no longer per field and the
        // paragraph above is describing an engine that no longer exists.
        for pair in PAGES_BY_FIELD.windows(2) {
            let ((f0, p0), (f1, p1)) = (pair[0], pair[1]);
            assert!(
                p1 > p0,
                "{f1} fields wrote {p1} pages against {f0} fields' {p0} - fields stopped costing"
            );
        }
    }

    /// **The hypothesis this kills.** The fixed chains are not where the bytes go.
    ///
    /// Root records and the catalog are each rewritten *whole* on every commit that touches
    /// anything, and both grow with how many fragments the database holds rather than with how
    /// many it touched - which makes them the obvious thing to blame and an expensive thing to
    /// rebuild around. They are under 1% of a commit here.
    ///
    /// Asserted rather than reported because the wrong fix is a large one: making the root
    /// records incremental is a format change touching backup, the freelist and the cluster,
    /// and it would buy the single digit this test measures.
    #[test]
    fn the_fixed_chains_are_not_where_the_bytes_go() {
        for (fields, _) in PAGES_BY_FIELD {
            let (c, frags) = last_commit(fields, 1_000);
            let fixed = c.roots + c.catalog + c.freelist + c.snapshots;
            assert!(
                fixed * 20 < c.data,
                "\n{fields} field(s), {frags} fragments: fixed chains {fixed} pages against \
                 {} data pages ({:.1}%).\nIf the fixed chains have become the cost, the fix is \
                 an incremental root-record chain and not a new engine - which is a different \
                 project, so change this test deliberately.",
                c.data,
                100.0 * fixed as f64 / c.data as f64,
            );
        }
    }
}

/// Scratch: prints the six figures the gated tables hold, for updating them after a deliberate
/// change. Ignored, so it never runs in CI - `cargo test --test amplification -- --ignored
/// --nocapture measured_amplification` is how it is read.
#[test]
#[ignore]
fn measured_amplification() {
    for (layout, name) in [(Layout::Dense, "DENSE"), (Layout::Sparse { shards: 64 }, "SPARSE_64")] {
        let records = workload(N, layout);
        let figures: Vec<String> = [100usize, 1_000, 10_000]
            .iter()
            .map(|&batch| format!("({batch}, {})", bytes_per_record(&records, batch)))
            .collect();
        println!("const {name}: [(usize, u64); 3] = [{}];", figures.join(", "));
    }

    let grid: Vec<String> = [64u64, 256, 1_024]
        .iter()
        .map(|&shards| {
            let records = workload(N, Layout::Sparse { shards });
            format!(
                "({shards}, {}, {})",
                flush_fraction::bytes_per_record(&records, 1.0),
                flush_fraction::bytes_per_record(&records, 0.25)
            )
        })
        .collect();
    println!("const GRID: [(u64, u64, u64); 3] = [{}];", grid.join(", "));

    let pages: Vec<String> = [1usize, 2, 4, 8]
        .iter()
        .map(|&fields| format!("({fields}, {})", cost_model::last_commit(fields, 1_000).0.data))
        .collect();
    println!("const PAGES_BY_FIELD: [(usize, u64); 4] = [{}];", pages.join(", "));
}
