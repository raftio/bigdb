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

#![cfg(unix)]

use big_page::{FragmentKey, LeafBuilder};
use big_pager::*;

fn open(path: &std::path::Path) -> MmapPager {
    MmapPager::open(path, 1 << 20).unwrap()
}

#[test]
fn state_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.big");
    let k = FragmentKey::new(1, 2, 0, 3);

    let root = {
        let store = Store::init(open(&path)).unwrap();
        let mut w = store.begin_write();
        let p = w.alloc().unwrap();
        w.write(p, LeafBuilder::new().finish(p)).unwrap();
        w.set_root(k, p);
        w.commit().unwrap();
        p
    };

    let store = Store::load(open(&path)).unwrap();
    assert_eq!(store.meta().txn_id, 1);
    assert_eq!(store.roots().get(&k), Some(root));
    assert_eq!(store.pager().read(root).unwrap().pgno(), root);
}

/// A file with bytes in it that this engine did not write is refused, not initialised.
///
/// **This is the destructive case, and it looked like the harmless one.** A database is at least
/// two pages, so anything shorter holds zero *pages* - and `open_or_init` used to ask about
/// pages, which made somebody else's short file indistinguishable from a path nothing had
/// created. It was then initialised, over the top. Anything larger was already caught by the
/// magic in the meta page; this is the range where there was no magic to check.
#[test]
fn a_file_too_short_to_be_a_database_is_refused_rather_than_overwritten() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("someone-elses.txt");
    let original = b"not a database, and shorter than two pages".to_vec();
    std::fs::write(&path, &original).unwrap();

    // `MmapPager` is not `Debug` - it holds a mapping - so the error is taken out on its own.
    let why = MmapPager::open(&path, 1 << 20).err();

    assert!(
        matches!(why, Some(StoreError::NotADatabase { bytes: 42 })),
        "refused by length: {why:?}"
    );
    assert_eq!(std::fs::read(&path).unwrap(), original, "and the file is untouched");
}

/// An empty path *is* a database nobody has created yet, and creating it is the point.
///
/// The other half of the rule above. Without this the fix would have turned every first start
/// into a failure, which is a worse bug than the one it fixes.
#[test]
fn an_empty_path_is_still_created_on_the_spot() {
    let dir = tempfile::tempdir().unwrap();

    // A path with nothing at it.
    let fresh = dir.path().join("new.big");
    Store::open_or_init(MmapPager::open(&fresh, 1 << 20).unwrap()).unwrap();

    // And a file that exists but holds nothing, which has nothing to lose either.
    let touched = dir.path().join("touched.big");
    std::fs::write(&touched, b"").unwrap();
    Store::open_or_init(MmapPager::open(&touched, 1 << 20).unwrap()).unwrap();
}

/// The exclusive lock is what makes the unsafe sound, so a second handle must be refused.
#[test]
fn second_handle_cannot_open_the_same_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.big");
    let _held = open(&path);
    assert!(matches!(MmapPager::open(&path, 1 << 20), Err(StoreError::Locked)));
}

#[test]
fn lock_is_released_on_drop() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.big");
    drop(open(&path));
    open(&path);
}

/// Mapped but past EOF must be an error, never a page we touch.
#[test]
fn page_inside_mapping_but_past_eof_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let p = open(&dir.path().join("t.big"));
    p.grow(2).unwrap();
    assert_eq!(p.page_count(), 2);
    assert!(matches!(p.read(2), Err(StoreError::OutOfBounds { .. })));
    assert!(matches!(p.write(2, &big_page::Page::zeroed()), Err(StoreError::OutOfBounds { .. })));
}

/// Running out of mapsize is a hard error, not a silent remap.
#[test]
fn exhausting_mapsize_is_an_error_not_a_remap() {
    let dir = tempfile::tempdir().unwrap();
    let p = MmapPager::open(dir.path().join("t.big"), 8192 * 4).unwrap();
    p.grow(4).unwrap();
    assert!(matches!(p.grow(5), Err(StoreError::MapSizeExhausted { need: 5, mapsize: 4 })));
    assert_eq!(p.capacity(), Some(4));
}

#[test]
fn a_commit_that_outgrows_mapsize_fails_before_touching_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::init(MmapPager::open(dir.path().join("t.big"), 8192 * 4).unwrap()).unwrap();
    let before = store.meta();

    let mut w = store.begin_write();
    let mut last = Ok(0);
    for _ in 0..8 {
        last = w.alloc();
        if last.is_err() {
            break;
        }
    }
    assert!(matches!(last, Err(StoreError::MapSizeExhausted { .. })));
    drop(w);
    assert_eq!(store.meta(), before, "a failed txn must leave meta alone");
}

#[test]
fn mapped_pages_are_aligned_for_payload_cast() {
    let dir = tempfile::tempdir().unwrap();
    let p = open(&dir.path().join("t.big"));
    p.grow(8).unwrap();
    for pgno in 0..8 {
        let addr = p.read(pgno).unwrap().as_bytes().as_ptr() as usize;
        assert_eq!(addr % 8, 0, "page {pgno} is not 8-aligned");
    }
}
