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

#![no_main]

//! The b-tree, driven by a generated program and checked against `BTreeMap`.
//!
//! **Not a byte fuzzer.** Feeding random bytes at a page is `parse_page`'s job. The bugs that
//! live here are not in parsing a page but in the sequence of writes that produce one: a split
//! that drops the last item, a merge that leaves a stale separator, a remove that frees a page
//! something still points at. None of those are reachable by mutating bytes; they need a
//! *history*.
//!
//! **`big-btree` already has a `BTreeMap` model test** — `tree_tracks_a_btreemap`, under
//! proptest. This is not a second copy of it. Three things differ, and they are the reason it
//! is worth the build:
//!
//! - **Coverage-guided rather than randomly sampled.** Proptest draws from a distribution
//!   somebody chose; libFuzzer keeps the inputs that reached new code, which is how a split at
//!   an unusual fill level gets found.
//! - **The tree is checked for being a tree.** `visit_tree` after every step asserts the walk
//!   terminates and that no page is reachable twice — a double free waiting to happen, and the
//!   one class of bug a contents-only oracle cannot see. The proptest compares contents at the
//!   end and would pass on a tree with a duplicated page.
//! - **After every step, not at the end.** A two-hundred step program that fails at the end
//!   says the bug is somewhere in two hundred steps, which is barely a report.
//!
//! Keys are deliberately narrow so they collide and cluster. A uniformly random 64-bit key
//! never lands in the same leaf twice, so it exercises the descent and almost nothing else;
//! real container keys arrive at a fixed stride within a fragment, which is what makes splits
//! and merges happen at all.

use arbitrary::Arbitrary;
use big_btree::{collect, find, put_container, remove, visit_tree, LeafItem};
use big_container::Container;
use big_page::{ContainerKey, FragmentKey};
use big_pager::{MemPager, Pager, Pgno, Store};
use libfuzzer_sys::fuzz_target;
use std::collections::{BTreeMap, BTreeSet};

const KEY: FragmentKey = FragmentKey { table: 1, field: 1, view: 0, shard: 0 };

#[derive(Arbitrary, Debug)]
enum Step {
    /// `key` is narrow on purpose: keys collide and land in the same leaves.
    Put { key: u8, values: Vec<u16> },
    Remove { key: u8 },
    Lookup { key: u8 },
}

#[derive(Arbitrary, Debug)]
struct Program {
    steps: Vec<Step>,
    /// Spreads the narrow keys over the real key space without spreading them evenly, so leaves
    /// fill unevenly the way a fragment's do.
    stride: u8,
}

fn key_of(k: u8, stride: u8) -> ContainerKey {
    k as ContainerKey * (stride as ContainerKey + 1)
}

/// Walks every page reachable from the root, which is also the check that it is still a tree.
fn assert_is_a_tree<P: Pager>(pager: &P, root: Pgno) {
    let mut seen = BTreeSet::new();
    visit_tree(pager, root, &mut |pgno, _| {
        assert!(seen.insert(pgno), "page {pgno} is reachable twice from one root");
        Ok(())
    })
    .expect("a tree the writer just built must be walkable");
}

fuzz_target!(|program: Program| {
    // Bounded so one input stays cheap. Long programs are not where the bugs are - a split
    // needs a handful of items, not thousands - and a slow target finds fewer of them per hour.
    if program.steps.len() > 200 {
        return;
    }

    let store = Store::init(MemPager::new()).unwrap();
    let mut root: Option<Pgno> = None;
    let mut oracle: BTreeMap<ContainerKey, BTreeSet<u16>> = BTreeMap::new();

    for step in &program.steps {
        // One commit per step, so every write goes through the copy-on-write path rather than
        // accumulating in one transaction's dirty set.
        let mut w = store.begin_write();
        match step {
            Step::Put { key, values } => {
                if values.len() > 4_000 {
                    continue;
                }
                let ckey = key_of(*key, program.stride);
                let set: BTreeSet<u16> = values.iter().copied().collect();
                let container = Container::from_values(set.iter().copied());
                root = Some(put_container(&mut w, root, ckey, container.as_ref()).unwrap());
                oracle.insert(ckey, set);
            }
            Step::Remove { key } => {
                let ckey = key_of(*key, program.stride);
                if let Some(r) = root {
                    root = Some(remove(&mut w, r, ckey).unwrap());
                    oracle.remove(&ckey);
                }
            }
            Step::Lookup { .. } => {}
        }
        if let Some(r) = root {
            w.set_root(KEY, r);
        }
        w.commit().unwrap();

        let Some(r) = root else { continue };

        if let Step::Lookup { key } = step {
            let ckey = key_of(*key, program.stride);
            assert_eq!(
                find(store.pager(), r, ckey).unwrap().is_some(),
                oracle.contains_key(&ckey),
                "the tree and the map disagreed about whether key {ckey} is present"
            );
        }

        // `remove` always keeps a root page, so an emptied tree is an empty tree and not an
        // absent one. Walking it must still work.
        assert_is_a_tree(store.pager(), r);

        let items: Vec<LeafItem> = collect(store.pager(), r).unwrap();
        let keys: Vec<ContainerKey> = items.iter().map(|i| i.key).collect();
        let mut sorted = keys.clone();
        sorted.sort_unstable();
        assert_eq!(keys, sorted, "a leaf walk came back out of order");
        sorted.dedup();
        assert_eq!(keys.len(), sorted.len(), "a key appears twice in the tree");
        assert_eq!(
            keys,
            oracle.keys().copied().collect::<Vec<_>>(),
            "the tree's keys diverged from the map's"
        );
    }
});
