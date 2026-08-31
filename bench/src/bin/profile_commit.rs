//! Scratch profiler: what fraction of commits rewrite each fixed chain, and how big is it?
//!
//! Not part of the harness. Temporary, for the P2 investigation. Every figure here is a page
//! count, so it is deterministic and machine-independent.

use big_bench::engines::big::{BigEngine, Default_};
use big_bench::*;

const N: u64 = 10_000;
const VALUE_CEILING: u64 = 1 << 20;

fn main() {
    println!(
        "{:>12} {:>6} {:>8} {:>10} {:>9} {:>9} {:>9} {:>7} {:>9}",
        "layout", "batch", "commits", "cat dirty", "catalog", "roots", "data", "free", "total"
    );
    for layout in [Layout::Dense, Layout::Sparse { shards: 64 }, Layout::Sparse { shards: 512 }] {
        for batch in [1usize, 100, 1_000] {
            let records = workload(N, layout, VALUE_CEILING);
            let dir = tempfile::tempdir().unwrap();
            let mut e = BigEngine::<Default_>::open(dir.path(), Durability::Relaxed);

            let (mut commits, mut cat_dirty) = (0u64, 0u64);
            let (mut cat, mut roots, mut data, mut free, mut total) =
                (0u64, 0u64, 0u64, 0u64, 0u64);
            for c in records.chunks(batch) {
                e.ingest(c);
                let b = e.db().store().metrics().last_commit;
                commits += 1;
                cat_dirty += (b.catalog > 0) as u64;
                cat += b.catalog;
                roots += b.roots;
                data += b.data;
                free += b.freelist;
                total += b.total();
            }
            println!(
                "{:>12} {:>6} {:>8} {:>9.0}% {:>9} {:>9} {:>9} {:>7} {:>9}",
                layout.label(),
                batch,
                commits,
                100.0 * cat_dirty as f64 / commits as f64,
                cat,
                roots,
                data,
                free,
                total,
            );
            drop(e);
            drop(dir);
        }
    }
}
