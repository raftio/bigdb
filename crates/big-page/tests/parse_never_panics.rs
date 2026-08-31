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

use big_page::*;
use proptest::prelude::*;

fn page_from(bytes: &[u8]) -> Page {
    let mut p = Page::zeroed();
    let n = bytes.len().min(PAGE_SIZE);
    p.as_bytes_mut()[..n].copy_from_slice(&bytes[..n]);
    p
}

// All the parsing logic lives in the header and the cell index, so the fuzz focuses there.
proptest! {
    #![proptest_config(ProptestConfig::with_cases(2000))]

    #[test]
    fn header_and_index_garbage(head in proptest::collection::vec(any::<u8>(), 512)) {
        let page = page_from(&head);
        if let Ok(leaf) = LeafPage::parse(&page) {
            for i in 0..leaf.len() {
                let _ = leaf.cell(i).map(|c| c.container());
            }
            let _ = leaf.search(0);
            let _ = leaf.total_cardinality();
        }
        if let Ok(br) = BranchPage::parse(&page) {
            for i in 0..br.len() {
                let _ = br.cell(i);
            }
            let _ = br.child_for(u64::MAX);
        }
        let _ = MetaPage::decode(&page);
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn whole_page_garbage(bytes in proptest::collection::vec(any::<u8>(), PAGE_SIZE)) {
        let page = page_from(&bytes);
        if let Ok(leaf) = LeafPage::parse(&page) {
            for i in 0..leaf.len() {
                let _ = leaf.cell(i).map(|c| c.container());
            }
        }
        let _ = MetaPage::decode(&page);
    }
}

#[test]
fn misaligned_cell_offset_is_err_not_panic() {
    let mut page = Page::zeroed();
    page.set_header(1, PageType::Leaf, 1);
    // 33 is not a multiple of 8, so the payload cast would break if this slipped through.
    page.as_bytes_mut()[PAGE_HEADER..PAGE_HEADER + 2].copy_from_slice(&33u16.to_le_bytes());
    page.seal();
    assert!(matches!(
        LeafPage::parse(&page),
        Err(PageError::CellMisaligned { index: 0, offset: 33 })
    ));
}

#[test]
fn cell_count_overflow_is_err() {
    let mut page = Page::zeroed();
    page.set_header(1, PageType::Leaf, u16::MAX);
    page.seal();
    assert!(matches!(LeafPage::parse(&page), Err(PageError::CellCountOverflow(_))));
}

#[test]
fn non_increasing_offsets_rejected() {
    let mut page = Page::zeroed();
    page.set_header(1, PageType::Leaf, 2);
    let b = page.as_bytes_mut();
    b[PAGE_HEADER..PAGE_HEADER + 2].copy_from_slice(&104u16.to_le_bytes());
    b[PAGE_HEADER + 2..PAGE_HEADER + 4].copy_from_slice(&104u16.to_le_bytes());
    page.seal();
    assert!(matches!(LeafPage::parse(&page), Err(PageError::CellOrderBroken { index: 1 })));
}

#[test]
fn wrong_page_type_rejected() {
    let page = BranchBuilder::new().finish(1);
    assert!(matches!(LeafPage::parse(&page), Err(PageError::TypeMismatch { .. })));
}

#[test]
fn checksum_catches_a_flipped_bit() {
    let mut page = LeafBuilder::new().finish(3);
    page.verify_checksum().unwrap();
    page.as_bytes_mut()[100] ^= 1;
    assert!(matches!(page.verify_checksum(), Err(PageError::ChecksumMismatch { .. })));
}
