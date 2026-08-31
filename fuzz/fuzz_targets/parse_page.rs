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

//! Every entry point that touches bytes off disk must return `Result`, never panic.

use big_page::*;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let mut page = Page::zeroed();
    let n = data.len().min(PAGE_SIZE);
    page.as_bytes_mut()[..n].copy_from_slice(&data[..n]);

    if let Ok(leaf) = LeafPage::parse(&page) {
        for i in 0..leaf.len() {
            if let Ok(cell) = leaf.cell(i) {
                let _ = cell.container();
                let _ = cell.bitmap(&page);
            }
        }
        let _ = leaf.search(u64::from_le_bytes(page.as_bytes()[..8].try_into().unwrap()));
        let _ = leaf.total_cardinality();
    }

    if let Ok(branch) = BranchPage::parse(&page) {
        for i in 0..branch.len() {
            let _ = branch.cell(i);
        }
        let _ = branch.child_for(u64::MAX);
    }

    for stride in [16usize, 24, 64] {
        for ty in [PageType::Freelist, PageType::RootRecords, PageType::Snapshots] {
            if let Ok(chain) = ChainPage::parse(&page, ty, stride) {
                let _ = chain.next();
                for e in chain.entries() {
                    core::hint::black_box(e);
                }
            }
        }
    }

    let _ = MetaPage::decode(&page);
    let _ = bitmap_container(&page);
    let _ = page.verify_checksum();
});
