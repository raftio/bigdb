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

//! Crash injection. The child process aborts at a failpoint inside `commit`; the parent then
//! reopens the file and checks what survived.

#![cfg(unix)]

use big_page::{FragmentKey, LeafBuilder};
use big_pager::*;
use std::process::Command;

const KEY: FragmentKey = FragmentKey { table: 1, field: 2, view: 0, shard: 3 };

fn child_binary() -> Command {
    Command::new(std::env::current_exe().unwrap())
}

/// Runs `crash_child` in a fresh process, aborting at `at`. Returns the database path.
fn run_child(at: &str) -> tempfile::TempDir {
    run_child_at(at, Durability::Full)
}

/// The same, with the child committing under a chosen durability level.
fn run_child_at(at: &str, durability: Durability) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.big");

    {
        let store = Store::init(MmapPager::open(&path, 1 << 20).unwrap()).unwrap();
        store.begin_write().commit().unwrap();
    }

    let status = child_binary()
        .args(["--exact", "crash_child", "--ignored", "--nocapture"])
        .env("BIG_CRASH_AT", at)
        .env("BIG_DB", &path)
        .env("BIG_DURABILITY", durability.label())
        .status()
        .unwrap();
    assert!(!status.success(), "child was supposed to abort at {at}");
    dir
}

/// Nothing reached the meta page, so the previous one still describes a complete tree.
#[test]
fn crash_between_data_fsync_and_meta_write_keeps_the_old_tree() {
    let dir = run_child("after_data_sync");
    let store = Store::load(MmapPager::open(dir.path().join("t.big"), 1 << 20).unwrap()).unwrap();

    assert_eq!(store.meta().txn_id, 1, "the aborted txn must not be visible");
    assert!(store.roots().get(&KEY).is_none());
}

/// The meta write landed in the page cache, so the commit took effect. The data was already
/// fsynced before it, which is exactly why that is safe rather than torn.
#[test]
fn crash_after_meta_write_leaves_a_complete_committed_tree() {
    let dir = run_child("after_meta_write");
    let store = Store::load(MmapPager::open(dir.path().join("t.big"), 1 << 20).unwrap()).unwrap();

    assert_eq!(store.meta().txn_id, 2);
    let root = store.roots().get(&KEY).expect("root must be present");
    let page = store.pager().read(root).unwrap();
    page.verify_checksum().expect("the page it points at must be intact");
}

/// What a relaxed commit still owes: a file that opens.
///
/// This is the half of the durability knob that can honestly be tested here. A process abort is
/// not a power cut - the kernel keeps every write the process made and hands them to whoever
/// opens the file next - so what this proves is that dropping the flushes did not break
/// *ordering*, not that the bytes reached the platter. That second claim cannot be tested
/// without cutting power to real hardware, and the levels are documented accordingly.
///
/// Both failpoints, both relaxed levels. Losing the aborted transaction is allowed; a meta page
/// naming pages that were never written is not, and would show up here as a load error or a
/// checksum failure rather than as missing data.
#[test]
fn a_relaxed_commit_still_leaves_a_file_that_opens() {
    for level in [Durability::Barrier, Durability::None] {
        for at in ["after_data_sync", "after_meta_write"] {
            let dir = run_child_at(at, level);
            let store = Store::load(MmapPager::open(dir.path().join("t.big"), 1 << 20).unwrap())
                .unwrap_or_else(|e| {
                    panic!("{} crashed at {at} would not open: {e}", level.label())
                });

            // Whichever way the commit fell, what the meta names has to be intact. A root that
            // is present but points at an unwritten page is the failure this guards.
            if let Some(root) = store.roots().get(&KEY) {
                let page = store.pager().read(root).unwrap();
                page.verify_checksum().unwrap_or_else(|e| {
                    panic!("{} crashed at {at}: root page is damaged: {e}", level.label())
                });
            }
        }
    }
}

/// Not a test: the body a crashing child runs. Ignored so a normal run skips it.
#[test]
#[ignore]
fn crash_child() {
    let Ok(path) = std::env::var("BIG_DB") else {
        return;
    };
    let store = Store::load(MmapPager::open(&path, 1 << 20).unwrap()).unwrap();
    if let Some(d) = std::env::var("BIG_DURABILITY").ok().and_then(|v| Durability::parse(&v)) {
        store.set_durability(d).unwrap();
    }
    let mut w = store.begin_write();
    let p = w.alloc().unwrap();
    w.write(p, LeafBuilder::new().finish(p)).unwrap();
    w.set_root(KEY, p);
    w.commit().unwrap();
    panic!("failpoint did not fire");
}
