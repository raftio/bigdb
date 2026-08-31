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

//! `Pager::verify_bitmap` remembers that a page verified, which is only sound because
//! copy-on-write never rewrites a page in place. These tests hold that reasoning to account:
//! the memo must never turn a corrupt page into a good one, and must never survive the bytes
//! it was about.

use big_page::{bitmap_page_checksum, build_bitmap_page, Page};
use big_pager::{MemPager, MmapPager, Pager, PagerMut};

const BITMAP_WORDS: usize = 1024;

fn page_of(fill: u64) -> Page {
    build_bitmap_page(&[fill; BITMAP_WORDS])
}

fn mmap(dir: &tempfile::TempDir) -> MmapPager {
    let p = MmapPager::open_default(dir.path().join("t.db")).unwrap();
    p.grow(8).unwrap();
    p
}

#[test]
fn a_good_page_verifies_and_a_wrong_checksum_never_does() {
    let dir = tempfile::tempdir().unwrap();
    let p = mmap(&dir);
    let page = page_of(0xa5a5_a5a5_a5a5_a5a5);
    p.write(1, &page).unwrap();
    let good = bitmap_page_checksum(&page);

    assert!(p.verify_bitmap(1, p.read(1).unwrap(), good));
    // Repeat: the second call is the one served from the memo, and must agree with the first.
    assert!(p.verify_bitmap(1, p.read(1).unwrap(), good));
    assert!(!p.verify_bitmap(1, p.read(1).unwrap(), good ^ 1), "a wrong checksum must not pass");
}

#[test]
fn a_memo_never_outlives_the_bytes_it_was_about() {
    let dir = tempfile::tempdir().unwrap();
    let p = mmap(&dir);

    let first = page_of(0x1111_1111_1111_1111);
    let first_sum = bitmap_page_checksum(&first);
    p.write(3, &first).unwrap();
    assert!(p.verify_bitmap(3, p.read(3).unwrap(), first_sum), "verified, and now remembered");

    // Page 3 is recycled: same number, different contents. This is the case the memo could
    // get wrong, and the whole reason `write` has to forget.
    let second = page_of(0x2222_2222_2222_2222);
    let second_sum = bitmap_page_checksum(&second);
    p.write(3, &second).unwrap();

    assert_ne!(first_sum, second_sum);
    assert!(
        !p.verify_bitmap(3, p.read(3).unwrap(), first_sum),
        "the previous occupant's checksum must not still pass"
    );
    assert!(p.verify_bitmap(3, p.read(3).unwrap(), second_sum));
}

#[test]
fn corruption_after_a_successful_verification_is_still_caught_on_a_fresh_pager() {
    // A memo lives in the pager, so reopening the file is the strongest statement available
    // here: nothing about a previous verification may survive into a new process.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.db");
    let page = page_of(0x3333_3333_3333_3333);
    let sum = bitmap_page_checksum(&page);
    {
        let p = MmapPager::open_default(&path).unwrap();
        p.grow(8).unwrap();
        p.write(2, &page).unwrap();
        assert!(p.verify_bitmap(2, p.read(2).unwrap(), sum));
        p.sync().unwrap();
    }
    {
        let p = MmapPager::open_default(&path).unwrap();
        assert!(p.verify_bitmap(2, p.read(2).unwrap(), sum));
        assert!(!p.verify_bitmap(2, p.read(2).unwrap(), sum ^ 0xdead));
    }
}

#[test]
fn the_default_implementation_agrees_with_the_memoising_one() {
    // `MemPager` does not override `verify_bitmap`, so it exercises the trait default. Both
    // must answer the same question the same way or the memo has changed behaviour, not cost.
    let dir = tempfile::tempdir().unwrap();
    let disk = mmap(&dir);
    let mem = MemPager::new();
    mem.grow(8).unwrap();

    let page = page_of(0x4444_4444_4444_4444);
    let sum = bitmap_page_checksum(&page);
    disk.write(4, &page).unwrap();
    mem.write(4, &page).unwrap();

    for expected in [sum, sum ^ 1, 0] {
        assert_eq!(
            disk.verify_bitmap(4, disk.read(4).unwrap(), expected),
            mem.verify_bitmap(4, &mem.read(4).unwrap(), expected),
            "memoised and default disagree for checksum {expected}"
        );
    }
}
