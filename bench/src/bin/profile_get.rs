//! Scratch profiler: where does a big point read actually spend its time?
//!
//! Not part of the harness. Temporary, for the P1 investigation.

use big_bench::*;
use big_db::{Db, FieldKind};
use big_pager::MmapPager;
use std::hint::black_box;
use std::time::Instant;

const TABLE: &str = "t";

fn time<F: FnMut()>(name: &str, iters: u32, mut f: F) -> f64 {
    for _ in 0..iters / 10 {
        f();
    }
    let t = Instant::now();
    for _ in 0..iters {
        f();
    }
    let ns = t.elapsed().as_nanos() as f64 / iters as f64;
    println!("{name:<44} {ns:>9.0} ns");
    ns
}

fn main() {
    // One database, one corpus, many fields of different bit depths over the same records.
    // Cost that scales with depth is per-plane work; cost that does not is fixed overhead.
    let n = 20_000u64;
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(MmapPager::open_default(dir.path().join("big.db")).unwrap()).unwrap();
    db.create_table(TABLE).unwrap();

    let depths = [1u32, 2, 4, 8, 12, 16, 20, 24, 32];
    for d in depths {
        db.create_field(TABLE, &format!("v{d}"), FieldKind::Int, d).unwrap();
    }

    let records = workload(n, Layout::Dense, 1 << 20);
    for chunk in records.chunks(1_000) {
        let mut w = db.write();
        for (id, value) in chunk {
            for d in depths {
                // Mask into range so every field stores a legal value for its own depth.
                let v = if d >= 64 { *value } else { *value & ((1u64 << d) - 1) };
                w.set_int(TABLE, &format!("v{d}"), *id, v).unwrap();
            }
        }
        w.commit().unwrap();
    }
    let probe = records[records.len() / 3].0;

    println!("\n=== get_int by bit_depth, {n} dense records, reader hoisted ===");
    let r = db.read();
    let mut prev: Option<(u32, f64)> = None;
    for d in depths {
        let f = format!("v{d}");
        let ns = time(&format!("bit_depth {d:>2}  ({} planes)", d + 1), 20_000, || {
            black_box(r.get_int(TABLE, &f, probe).unwrap());
        });
        if let Some((pd, pns)) = prev {
            let per_plane = (ns - pns) / (d - pd) as f64;
            println!("{:>44} {per_plane:>9.0} ns/plane in this step", "");
        }
        prev = Some((d, ns));
    }

    // Fixed overhead: a field that does not exist fails inside resolve, before any plane work.
    println!("\n=== fixed overhead ===");
    time("resolve miss (unknown field)", 20_000, || {
        black_box(r.get_int(TABLE, "nope", probe).is_err());
    });
    time("db.read()", 20_000, || {
        black_box(db.read());
    });

    // Is the per-plane cost the whole-page CRC that verify_base runs?
    let page = big_page::build_bitmap_page(&[0x5a5a_5a5a_5a5a_5a5a; big_container::BITMAP_WORDS]);
    time("bitmap_page_checksum over one 8 KiB page", 200_000, || {
        black_box(big_page::bitmap_page_checksum(black_box(&page)));
    });
}
