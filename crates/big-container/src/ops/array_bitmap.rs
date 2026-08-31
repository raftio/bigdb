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

//! Sparse array against a dense bitmap: probe rather than merge.

use super::SetOp;
use crate::builder::Out;
use crate::BITMAP_WORDS;

fn has(b: &[u64; BITMAP_WORDS], v: u16) -> bool {
    b[v as usize / 64] >> (v % 64) & 1 == 1
}

/// `a` is the array, `b` the bitmap. Handles both `a op b` and `a andnot b`.
pub fn run(op: SetOp, a: &[u16], b: &[u64; BITMAP_WORDS], out: &mut Out) -> bool {
    match op {
        SetOp::And => {
            for v in a {
                if has(b, *v) {
                    out.push(*v);
                }
            }
            true
        }
        SetOp::AndNot => {
            for v in a {
                if !has(b, *v) {
                    out.push(*v);
                }
            }
            true
        }
        // Union and symmetric difference are only cheap when the output is already dense;
        // otherwise the ascending-order requirement of an array output forces a merge.
        SetOp::Or => match out.as_bitmap_mut() {
            Some(w) => {
                w.copy_from_slice(b);
                for v in a {
                    w[*v as usize / 64] |= 1u64 << (*v % 64);
                }
                true
            }
            None => false,
        },
        SetOp::Xor => match out.as_bitmap_mut() {
            Some(w) => {
                w.copy_from_slice(b);
                for v in a {
                    w[*v as usize / 64] ^= 1u64 << (*v % 64);
                }
                true
            }
            None => false,
        },
    }
}

/// `a` is the bitmap, `b` the array. Only reached for the non-commutative direction.
pub fn run_reversed(op: SetOp, a: &[u64; BITMAP_WORDS], b: &[u16], out: &mut Out) -> bool {
    match (op, out.as_bitmap_mut()) {
        (SetOp::AndNot, Some(w)) => {
            w.copy_from_slice(a);
            for v in b {
                w[*v as usize / 64] &= !(1u64 << (*v % 64));
            }
            true
        }
        _ => false,
    }
}
