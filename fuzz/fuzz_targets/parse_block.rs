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

//! A column block is bytes off disk, so decoding one must return `Result` and never panic.
//!
//! The same contract `parse_page` holds, and it needs its own target for the same reason: a
//! block's header carries counts and widths that nothing above has bounded, and every one of
//! them feeds an index or an allocation. The first bug this found was a running offset added to
//! a count without a check - a debug overflow, and in release a backwards slice.

use big_engine::columnar::block::PartRef;
use big_engine::columnar::Block;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Split the input into a header and a body, the two places a part's bytes really come from:
    // the leaf cell, and the page it may point at.
    let cut = data.first().copied().unwrap_or(0) as usize;
    let rest = data.get(1..).unwrap_or(&[]);
    let cut = cut.min(rest.len());
    let (header, body) = rest.split_at(cut);

    // One part on its own, which is what a scalar block is.
    let _ = Block::decode(&[PartRef { header, body }]);

    // And a two-part block, which is the list shape - where the counts in the first part decide
    // how the second is cut up, and therefore where an unchecked offset would land.
    let half = header.len() / 2;
    let _ = Block::decode(&[
        PartRef { header: &header[..half], body },
        PartRef { header: &header[half..], body },
    ]);
});
