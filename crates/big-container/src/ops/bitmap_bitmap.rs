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

//! Word at a time over `[u64; 1024]`. LLVM autovectorises this well and `count_ones` maps
//! straight to POPCNT, so there is no hand-written SIMD here on purpose.

use super::SetOp;
use crate::builder::Out;
use crate::BITMAP_WORDS;

/// Returns false when the output is an array, in which case the caller falls back.
pub fn run(op: SetOp, a: &[u64; BITMAP_WORDS], b: &[u64; BITMAP_WORDS], out: &mut Out) -> bool {
    let Some(w) = out.as_bitmap_mut() else {
        return false;
    };
    match op {
        SetOp::And => {
            for i in 0..BITMAP_WORDS {
                w[i] = a[i] & b[i];
            }
        }
        SetOp::Or => {
            for i in 0..BITMAP_WORDS {
                w[i] = a[i] | b[i];
            }
        }
        SetOp::Xor => {
            for i in 0..BITMAP_WORDS {
                w[i] = a[i] ^ b[i];
            }
        }
        SetOp::AndNot => {
            for i in 0..BITMAP_WORDS {
                w[i] = a[i] & !b[i];
            }
        }
    }
    true
}
