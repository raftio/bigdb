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

use big_container::{ContainerRef, ContainerType, Interval};
use big_page::*;

fn array_cell(vals: &[u16]) -> ContainerRef<'_> {
    ContainerRef::Array(vals)
}

#[test]
fn roundtrip_array_and_run() {
    let a: Vec<u16> = (0..100u16).map(|i| i * 3).collect();
    let r = vec![Interval::new(10, 20), Interval::new(100, 300)];

    let mut b = LeafBuilder::new();
    b.push_container(1, array_cell(&a)).unwrap();
    b.push_container(2, ContainerRef::Run(&r)).unwrap();
    b.push_bitmap_ptr(3, 65536, 42, 0xDEAD_BEEF).unwrap();
    let page = b.finish(7);

    assert_eq!(page.pgno(), 7);
    assert_eq!(page.page_type().unwrap(), PageType::Leaf);
    page.verify_checksum().unwrap();

    let leaf = LeafPage::parse(&page).unwrap();
    assert_eq!(leaf.len(), 3);

    let c0 = leaf.cell(0).unwrap();
    assert_eq!(c0.key, 1);
    assert_eq!(c0.cardinality, 100);
    match c0.container().unwrap().unwrap() {
        ContainerRef::Array(got) => assert_eq!(got, &a[..]),
        other => panic!("{other:?}"),
    }

    let c1 = leaf.cell(1).unwrap();
    assert_eq!(c1.cardinality, 11 + 201);
    match c1.container().unwrap().unwrap() {
        ContainerRef::Run(got) => assert_eq!(got, &r[..]),
        other => panic!("{other:?}"),
    }

    let c2 = leaf.cell(2).unwrap();
    assert_eq!(c2.ty, ContainerType::BitmapPtr);
    assert_eq!(c2.bitmap_pgno, 42);
    assert_eq!(c2.bitmap_checksum, 0xDEAD_BEEF);
    assert!(c2.container().unwrap().is_none());
}

#[test]
fn search_matches_binary_search_convention() {
    let mut b = LeafBuilder::new();
    for k in [10u64, 20, 30, 40] {
        b.push_container(k, array_cell(&[1, 2, 3])).unwrap();
    }
    let page = b.finish(1);
    let leaf = LeafPage::parse(&page).unwrap();

    assert_eq!(leaf.search(30), Ok(2));
    assert_eq!(leaf.search(5), Err(0));
    assert_eq!(leaf.search(25), Err(2));
    assert_eq!(leaf.search(99), Err(4));
}

#[test]
fn every_payload_lands_on_align_8() {
    let mut b = LeafBuilder::new();
    // Odd lengths, to force the builder to pad between cells.
    for k in 0..40u64 {
        let vals: Vec<u16> = (0..(k as u16 % 7) + 1).collect();
        b.push_container(k, array_cell(&vals)).unwrap();
    }
    let page = b.finish(1);
    let leaf = LeafPage::parse(&page).unwrap();
    for i in 0..leaf.len() {
        let cell = leaf.cell(i).unwrap();
        let off = cell.payload.as_ptr() as usize - page.as_bytes().as_ptr() as usize;
        assert_eq!(off % CELL_ALIGN, 0, "cell {i} payload lệch align");
    }
}

#[test]
fn builder_refuses_to_overflow_the_page() {
    let big = vec![0u16; ARRAY_MAX_ELEMS];
    let mut b = LeafBuilder::new();
    assert!(b.push_container(1, array_cell(&big)).is_some(), "một cell tối đa phải vừa");
    assert!(b.push_container(2, array_cell(&[1])).is_none(), "cell thứ hai phải bị từ chối");
    let page = b.finish(1);
    LeafPage::parse(&page).unwrap();
}

#[test]
fn full_page_of_small_cells_still_parses() {
    let mut b = LeafBuilder::new();
    let mut n = 0u64;
    while b.push_container(n, array_cell(&[1, 2])).is_some() {
        n += 1;
    }
    let page = b.finish(1);
    let leaf = LeafPage::parse(&page).unwrap();
    assert_eq!(leaf.len() as u64, n);
    assert_eq!(leaf.total_cardinality().unwrap(), n * 2);
    for i in 0..leaf.len() {
        leaf.cell(i).unwrap().container().unwrap().unwrap();
    }
}

#[test]
fn total_cardinality_reads_only_headers() {
    let mut b = LeafBuilder::new();
    b.push_container(1, array_cell(&[1, 2, 3])).unwrap();
    b.push_bitmap_ptr(2, 65536, 9, 0).unwrap();
    let page = b.finish(1);
    let leaf = LeafPage::parse(&page).unwrap();
    assert_eq!(leaf.total_cardinality().unwrap(), 3 + 65536);
}

#[test]
fn dense_page_roundtrips_through_the_parent_cell() {
    let mut words = [0u64; big_container::BITMAP_WORDS];
    for (i, w) in words.iter_mut().enumerate() {
        *w = (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    }
    let dense = build_bitmap_page(&words);

    let mut b = LeafBuilder::new();
    b.push_dense(5, 77, &dense).unwrap();
    let page = b.finish(1);

    let cell = LeafPage::parse(&page).unwrap().cell(0).unwrap();
    assert_eq!(cell.bitmap_pgno, 77);
    assert!(cell.container().unwrap().is_none(), "dense has no inline payload");

    match cell.bitmap(&dense).unwrap() {
        ContainerRef::Bitmap(got) => assert_eq!(got, &words),
        other => panic!("{other:?}"),
    }
    assert_eq!(cell.cardinality, words.iter().map(|w| w.count_ones()).sum::<u32>());
}

/// The one parent-to-child integrity link: a bitmap page has no trailer of its own.
#[test]
fn parent_cell_checksum_catches_a_corrupt_dense_page() {
    let dense = build_bitmap_page(&[0xFFu64; big_container::BITMAP_WORDS]);
    let mut b = LeafBuilder::new();
    b.push_dense(5, 77, &dense).unwrap();
    let page = b.finish(1);
    let cell = LeafPage::parse(&page).unwrap().cell(0).unwrap();

    let mut rotten = dense.clone();
    rotten.as_bytes_mut()[4096] ^= 1;
    assert!(matches!(cell.bitmap(&rotten), Err(PageError::ChecksumMismatch { .. })));
    assert!(cell.bitmap(&dense).is_ok());
}
