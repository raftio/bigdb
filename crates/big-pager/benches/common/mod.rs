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

//! Fixtures shared by the benchmarks.
//!
//! Every store here is built the same way whichever pager backs it, so a `MemPager` number and
//! an `MmapPager` number differ only by durability. That subtraction is the point of the suite.

#![allow(dead_code)] // each bench binary uses a different subset

use big_page::LeafBuilder;
use big_pager::*;
use std::path::Path;

/// Fragments are laid out across shards of one field, which is the shape a real table has.
pub fn key(shard: u64) -> FragmentKey {
    FragmentKey::new(1, 1, 0, shard)
}

/// A store carrying `n` fragments, each one leaf page, committed in a single transaction.
///
/// One transaction rather than `n` on purpose: the fixture should cost what it costs, not
/// `n` times the per-commit overhead the benchmarks are trying to isolate.
pub fn with_fragments<P: PagerMut>(pager: P, n: u64) -> Store<P> {
    let store = Store::open_or_init(pager).unwrap();
    if n == 0 {
        return store;
    }
    let mut w = store.begin_write();
    for shard in 0..n {
        let p = w.alloc().unwrap();
        w.write(p, LeafBuilder::new().finish(p)).unwrap();
        w.set_root(key(shard), p);
    }
    // One catalog entry per fragment, which is what `big-db` keeps: a zone map and a bit
    // depth per fragment. Leaving it out would understate the per-commit cost by 5x, since a
    // catalog entry is 128 bytes against a root record's 24.
    w.set_catalog(catalog_entries(n as usize));
    w.commit().unwrap();
    store
}

/// Rewrites the root of fragment `shard` in one transaction: the smallest unit of real work,
/// and the probe used to ask what a commit costs when nothing else changed.
pub fn touch_one<P: PagerMut>(store: &Store<P>, shard: u64) {
    let mut w = store.begin_write();
    let old = w.root(&key(shard)).expect("fixture must have this fragment");
    let new = w.cow(old).unwrap();
    w.write(new, LeafBuilder::new().finish(new)).unwrap();
    w.set_root(key(shard), new);
    w.commit().unwrap();
}

/// Allocates and writes `pages` fresh pages in one transaction, then commits.
pub fn write_pages<P: PagerMut>(store: &Store<P>, pages: u64) {
    let mut w = store.begin_write();
    for _ in 0..pages {
        let p = w.alloc().unwrap();
        w.write(p, LeafBuilder::new().finish(p)).unwrap();
    }
    w.commit().unwrap();
}

/// A catalog of `entries` fixed-width records, standing in for schema plus per-fragment
/// metadata without pulling `big-db` into this crate's dev-dependencies.
pub fn catalog_entries(entries: usize) -> Vec<Vec<u8>> {
    (0..entries)
        .map(|i| {
            let mut b = vec![0u8; CATALOG_ENTRY_BYTES];
            b[0] = 1;
            b[4..12].copy_from_slice(&(i as u64).to_le_bytes());
            b
        })
        .collect()
}

/// An mmap-backed store in a throwaway directory. The `TempDir` is returned alongside it
/// because dropping it deletes the file out from under the mapping.
#[cfg(unix)]
pub fn mmap_store(dir: &Path, n: u64) -> Store<MmapPager> {
    with_fragments(MmapPager::open_default(dir.join("bench.big")).unwrap(), n)
}
