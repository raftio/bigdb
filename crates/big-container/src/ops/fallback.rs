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

//! Correct-by-construction path for every representation pair.
//!
//! Both iterators yield ascending values, so one merge covers all four operations. Every
//! specialisation is differential-tested against this.

use super::SetOp;
use crate::builder::Out;
use crate::ContainerRef;

pub fn merge(op: SetOp, a: ContainerRef<'_>, b: ContainerRef<'_>, out: &mut Out) {
    let (mut ia, mut ib) = (a.iter(), b.iter());
    let (mut va, mut vb) = (ia.next(), ib.next());

    loop {
        match (va, vb) {
            (Some(x), Some(y)) if x < y => {
                if op.keeps_left_only() {
                    out.push(x);
                }
                va = ia.next();
            }
            (Some(x), Some(y)) if x > y => {
                if op.keeps_right_only() {
                    out.push(y);
                }
                vb = ib.next();
            }
            (Some(x), Some(_)) => {
                if op.keeps_both() {
                    out.push(x);
                }
                va = ia.next();
                vb = ib.next();
            }
            (Some(x), None) => {
                if op.keeps_left_only() {
                    out.push(x);
                }
                va = ia.next();
            }
            (None, Some(y)) => {
                if op.keeps_right_only() {
                    out.push(y);
                }
                vb = ib.next();
            }
            (None, None) => return,
        }
    }
}
