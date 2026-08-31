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

//! Two sorted `[u16]` slices: a plain linear merge, no iterator indirection.

use super::SetOp;
use crate::builder::Out;

pub fn run(op: SetOp, a: &[u16], b: &[u16], out: &mut Out) {
    let (mut i, mut j) = (0usize, 0usize);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            core::cmp::Ordering::Less => {
                if op.keeps_left_only() {
                    out.push(a[i]);
                }
                i += 1;
            }
            core::cmp::Ordering::Greater => {
                if op.keeps_right_only() {
                    out.push(b[j]);
                }
                j += 1;
            }
            core::cmp::Ordering::Equal => {
                if op.keeps_both() {
                    out.push(a[i]);
                }
                i += 1;
                j += 1;
            }
        }
    }
    if op.keeps_left_only() {
        for v in &a[i..] {
            out.push(*v);
        }
    }
    if op.keeps_right_only() {
        for v in &b[j..] {
            out.push(*v);
        }
    }
}
