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

//! Backup: the whole database into a fresh file, while the original stays open.
//!
//! The copy is a real file and not a reference, so the test that matters is that the original
//! can be thrown away entirely and the copy still answers every question the same way.

use big_db::*;
use big_pager::MemPager;

fn seeded() -> Db<MemPager> {
    let d = Db::in_memory().unwrap();
    d.create_table("tx").unwrap();
    d.create_field("tx", "amount", FieldKind::Int, 37).unwrap();
    d.create_field("tx", "country", FieldKind::Set, 0).unwrap();
    d.create_field("tx", "tier", FieldKind::Mutex, 0).unwrap();
    d.create_field("tx", "active", FieldKind::Bool, 0).unwrap();

    let mut w = d.write();
    for r in 0..3000u64 {
        // Spread across shards so the copy has many fragments to walk, not one.
        let record = r * 977;
        w.set_int("tx", "amount", record, r * 13).unwrap();
        w.set_key("tx", "country", record, if r.is_multiple_of(3) { "vn" } else { "jp" }).unwrap();
        w.set_key("tx", "tier", record, if r.is_multiple_of(5) { "gold" } else { "silver" })
            .unwrap();
        w.set_bool("tx", "active", record, r.is_multiple_of(2)).unwrap();
    }
    w.commit().unwrap();
    d
}

/// Everything an outside caller can ask, so two databases can be compared without reaching
/// into either one's pages.
fn fingerprint(d: &Db<MemPager>) -> Vec<String> {
    let r = d.read();
    let mut out = vec![
        format!("all={}", r.all("tx").unwrap().cardinality()),
        format!("sum={}", r.sum("tx", "amount").unwrap()),
        format!("ge={}", r.count("tx", "amount", RangeOp::Ge, 20_000).unwrap()),
        format!("lt={}", r.count("tx", "amount", RangeOp::Lt, 500).unwrap()),
        format!("vn={}", r.matching_key("tx", "country", "vn").unwrap().cardinality()),
        format!("gold={}", r.matching_key("tx", "tier", "gold").unwrap().cardinality()),
        format!("on={}", r.matching_bool("tx", "active", true).unwrap().cardinality()),
    ];
    // A handful of point reads: aggregates alone would not catch a value landing on the
    // wrong record.
    for r_i in [0u64, 1, 7, 999, 2999] {
        out.push(format!("v{r_i}={:?}", r.get_int("tx", "amount", r_i * 977).unwrap()));
    }
    let all = r.all("tx").unwrap();
    out.push(format!("groups={:?}", r.group_counts("tx", "country", &all).unwrap()));
    out
}

#[test]
fn a_copy_answers_every_question_the_original_does() {
    let src = seeded();
    let before = fingerprint(&src);

    let copy = src.copy_to(MemPager::new()).unwrap();
    assert_eq!(fingerprint(&copy), before);
}

#[test]
fn a_copy_carries_the_schema_not_only_the_data() {
    let src = seeded();
    let copy = src.copy_to(MemPager::new()).unwrap();

    // Encoded and compared inside the block: `catalog()` hands out a lock guard, and holding
    // one across the commit below would wait on a writer that is waiting on this reader.
    let (a, b) = { (src.catalog().encode(), copy.catalog().encode()) };
    assert_eq!(a, b, "the copy must describe itself identically");

    // And it must still be writable: a restored file is a database, not a museum piece.
    let mut w = copy.write();
    w.set_int("tx", "amount", 42, 7).unwrap();
    w.commit().unwrap();
    assert_eq!(copy.read().get_int("tx", "amount", 42).unwrap(), Some(7));
}

#[test]
fn a_copy_reclaims_what_the_original_is_still_holding() {
    // Churn the same records repeatedly. Copy-on-write leaves the superseded pages on the
    // freelist, so the source ends up much larger than the data it holds; the copy allocates
    // into an empty freelist and comes out the size of the live tree.
    let src = seeded();
    for round in 0..6u64 {
        let mut w = src.write();
        for r in 0..3000u64 {
            w.set_int("tx", "amount", r * 977, r * 13 + round + 1).unwrap();
        }
        w.commit().unwrap();
    }

    let copy = src.copy_to(MemPager::new()).unwrap();

    let (fat, lean) = (src.store().metrics().page_count, copy.store().metrics().page_count);
    assert!(lean < fat, "a compacting copy must be smaller: {lean} is not below {fat}");
    assert_eq!(copy.store().metrics().free_pages_reusable, 0, "nothing to reclaim in a fresh copy");
}

#[test]
fn the_source_is_readable_and_unchanged_throughout() {
    let src = seeded();
    let before = fingerprint(&src);
    let pages_before = src.store().metrics().page_count;

    src.copy_to(MemPager::new()).unwrap();

    assert_eq!(fingerprint(&src), before);
    assert_eq!(src.store().metrics().page_count, pages_before, "a backup writes nothing here");
}

#[test]
fn a_copy_of_an_empty_database_is_a_valid_empty_database() {
    let src = Db::in_memory().unwrap();
    src.create_table("tx").unwrap();
    src.create_field("tx", "amount", FieldKind::Int, 8).unwrap();

    let copy = src.copy_to(MemPager::new()).unwrap();

    assert_eq!(copy.read().all("tx").unwrap().cardinality(), 0);
    let mut w = copy.write();
    w.set_int("tx", "amount", 1, 5).unwrap();
    w.commit().unwrap();
    assert_eq!(copy.read().get_int("tx", "amount", 1).unwrap(), Some(5));
}

#[cfg(unix)]
#[test]
fn backup_to_writes_a_file_that_opens_on_its_own() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("backup.big");

    let source_path = dir.path().join("live.big");
    let src = Db::open_path(&source_path).unwrap();
    src.create_table("tx").unwrap();
    src.create_field("tx", "amount", FieldKind::Int, 20).unwrap();
    let mut w = src.write();
    for r in 0..500u64 {
        w.set_int("tx", "amount", r, r * 3).unwrap();
    }
    w.commit().unwrap();

    src.backup_to(&path).unwrap();
    drop(src); // the original is gone; the backup has to stand alone

    let restored = Db::open_path(&path).unwrap();
    let r = restored.read();
    assert_eq!(r.all("tx").unwrap().cardinality(), 500);
    assert_eq!(r.get_int("tx", "amount", 499).unwrap(), Some(1497));
}

#[cfg(unix)]
#[test]
fn backup_refuses_to_overwrite_an_existing_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("taken.big");
    std::fs::write(&path, b"not a database").unwrap();

    let src = Db::in_memory().unwrap();
    src.create_table("tx").unwrap();

    let err = src.backup_to(&path).unwrap_err();
    assert!(
        matches!(err, DbError::BackupDestinationExists(_)),
        "a backup must never silently clobber a destination, got {err:?}"
    );
    assert_eq!(std::fs::read(&path).unwrap(), b"not a database");
}

#[cfg(unix)]
#[test]
fn compaction_shrinks_the_file_and_keeps_every_answer() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("churn.big");

    // Rewriting the same records over and over is what leaves a file larger than its data:
    // every commit copies the pages it touches and leaves the originals to the freelist.
    let fingerprint = {
        let d = Db::open_path(&path).unwrap();
        d.create_table("tx").unwrap();
        d.create_field("tx", "amount", FieldKind::Int, 32).unwrap();
        for round in 0..8u64 {
            let mut w = d.write();
            for r in 0..2000u64 {
                w.set_int("tx", "amount", r, r + round).unwrap();
            }
            w.commit().unwrap();
        }
        let r = d.read();
        (r.all("tx").unwrap().cardinality(), r.sum("tx", "amount").unwrap())
    };

    let bytes_before = std::fs::metadata(&path).unwrap().len();
    let report = big_db::copy::compact_path(&path).unwrap();
    let bytes_after = std::fs::metadata(&path).unwrap().len();

    assert!(report.pages_reclaimed() > 0, "churn must leave something to reclaim: {report:?}");
    assert!(bytes_after < bytes_before, "{bytes_after} is not below {bytes_before}");
    assert!(!path.with_extension("compacting").exists(), "the temp file must be gone");

    let d = Db::open_path(&path).unwrap();
    let r = d.read();
    assert_eq!((r.all("tx").unwrap().cardinality(), r.sum("tx", "amount").unwrap()), fingerprint);
}

#[cfg(unix)]
#[test]
fn compaction_refuses_while_the_database_is_open_elsewhere() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("busy.big");
    let held = Db::open_path(&path).unwrap();
    held.create_table("tx").unwrap();

    // The exclusive lock is the whole guard here: rewriting a file another handle has mapped
    // would pull the mapping out from under it.
    let err = big_db::copy::compact_path(&path).unwrap_err();
    assert!(
        matches!(err, DbError::Store(big_pager::StoreError::Locked)),
        "expected a lock refusal, got {err:?}"
    );
    assert!(!path.with_extension("compacting").exists());
}

/// A database whose containers carry deltas copies exactly.
///
/// The case a copy is most likely to get wrong, and the reason it is worth its own test: a
/// `BitmapDelta` cell is the only kind whose meaning is split across two pages. Its base page is
/// reachable only through the cell, and the cell's checksum covers that page as written - so a
/// copy that forgot the base would leave a cell pointing at nothing, and one that rewrote the
/// base would invalidate the checksum. Neither shows up in the cell count or the cardinality.
#[test]
fn a_database_with_delta_cells_copies_exactly() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("copy.big");

    let d = Db::in_memory().unwrap();
    d.create_table("t").unwrap();
    d.create_field("t", "v", FieldKind::Int, 20).unwrap();

    // Enough records for the containers to go dense, then written one at a time so that each
    // one accumulates a delta rather than being rewritten whole.
    let mut w = d.write();
    for i in 0..20_000u64 {
        w.set_int("t", "v", i, i.wrapping_mul(2_654_435_761) % (1 << 20)).unwrap();
    }
    w.commit().unwrap();
    for i in 20_000..20_016u64 {
        let mut w = d.write();
        w.set_int("t", "v", i, i.wrapping_mul(2_654_435_761) % (1 << 20)).unwrap();
        w.commit().unwrap();
    }

    d.backup_to(&path).unwrap();
    let copy = Db::open_path(&path).unwrap();

    assert_eq!(copy.read().count_all("t").unwrap(), 20_016);
    let (a, b) = (d.read(), copy.read());
    for i in (0..20_016u64).step_by(97) {
        assert_eq!(b.get_int("t", "v", i).unwrap(), a.get_int("t", "v", i).unwrap(), "record {i}");
    }
    // The whole answer to a range query, not a sample: a copy that lost one bit of one plane
    // would still pass a spot check on the records it happened to look at.
    assert_eq!(
        b.range("t", "v", RangeOp::Ge, 500_000).unwrap(),
        a.range("t", "v", RangeOp::Ge, 500_000).unwrap()
    );
}

// ---------------------------------------------------------------------------
// Scrub
// ---------------------------------------------------------------------------

/// Flips one byte of the page a root points at, leaving its checksum saying what it said.
///
/// Written straight through the pager rather than through a transaction, which is the whole
/// point: this is what a bad block looks like from the engine's side. Copy-on-write means
/// nothing in normal operation ever rewrites a page in place, so a page whose bytes changed
/// under a valid checksum can only have come from underneath.
fn rot_one_page(d: &Db<MemPager>) -> u32 {
    use big_pager::{Pager, PagerMut};
    let pgno = {
        let r = d.store().begin_read();
        let (_, root) = r.roots().iter().next().expect("the seeded database has a tree");
        *root
    };
    let mut page = (*d.store().pager().read(pgno).unwrap()).clone();
    // Somewhere in the payload, well away from the header and the trailer, and a flip rather
    // than an assignment so the test cannot accidentally write the byte that was already there.
    page.0[4096] ^= 0xff;
    d.store().pager().write(pgno, &page).unwrap();
    pgno
}

/// The hole this closes: nothing on the query path recomputes a branch or leaf checksum, so a
/// rotted page answers a *different number* rather than an error.
#[test]
fn a_scrub_finds_a_page_that_rotted_under_a_valid_checksum() {
    let d = seeded();

    let clean = d.scrub().expect("a fresh database has to scrub clean");
    assert!(clean.pages > 0, "the scrub reached no tree pages at all");
    assert!(clean.total() >= clean.pages);

    rot_one_page(&d);
    let found = d.scrub();
    assert!(found.is_err(), "the scrub walked over a corrupted page and called it fine");
}

/// A backup used to *launder* corruption: the copy is written through the ordinary commit
/// path, so a rotted page was copied and then sealed under a fresh, valid checksum computed
/// over the rotted bytes. Every later check of the copy would have called it intact.
#[test]
fn a_backup_refuses_to_copy_a_page_that_does_not_match_its_checksum() {
    let d = seeded();
    d.copy_to(MemPager::new()).expect("a clean database has to copy");

    rot_one_page(&d);
    assert!(
        d.copy_to(MemPager::new()).is_err(),
        "the copy carried corruption forward and gave it a valid checksum"
    );
}
