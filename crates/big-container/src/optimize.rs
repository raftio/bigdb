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

//! Representation choice at the boundary where a container is written to a page.
//!
//! Run once there, never after every operation: an intermediate result that spends its whole
//! life in RAM does not need to be in its smallest form.

use crate::{Container, ContainerRef, Interval, OpResult, ARRAY_BREAK_EVEN, BITMAP_BYTES};

/// Physical ceilings, which come from the page layout and so are passed in rather than known here.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Caps {
    pub array_max: usize,
    pub run_max: usize,
}

/// Number of maximal contiguous runs, which is what a run encoding would cost.
pub fn run_count(c: ContainerRef<'_>) -> usize {
    match c {
        ContainerRef::Run(r) => r.len(),
        _ => {
            let mut runs = 0usize;
            let mut prev: Option<u16> = None;
            for v in c.iter() {
                if prev.is_none_or(|p| v != p + 1) {
                    runs += 1;
                }
                prev = Some(v);
            }
            runs
        }
    }
}

pub fn to_runs(c: ContainerRef<'_>) -> Vec<Interval> {
    let mut out: Vec<Interval> = Vec::new();
    for v in c.iter() {
        match out.last_mut() {
            Some(last) if last.last + 1 == v => last.last = v,
            _ => out.push(Interval::new(v, v)),
        }
    }
    out
}

/// Picks the smallest representation that also fits the page, and borrows when already optimal.
pub fn optimize<'a>(c: ContainerRef<'a>, caps: Caps) -> OpResult<'a> {
    let card = c.cardinality() as usize;
    let runs = run_count(c);

    let array_ok = card <= caps.array_max.min(ARRAY_BREAK_EVEN);
    let run_ok = runs <= caps.run_max;

    let array_bytes = if array_ok { card * 2 } else { usize::MAX };
    let run_bytes = if run_ok { runs * 4 } else { usize::MAX };
    let bitmap_bytes = BITMAP_BYTES;

    let best = array_bytes.min(run_bytes).min(bitmap_bytes);

    // Ties go to the current representation so an already-optimal container stays borrowed.
    match c {
        ContainerRef::Array(_) if array_ok && array_bytes == best => return OpResult::Borrowed(c),
        ContainerRef::Run(_) if run_ok && run_bytes == best => return OpResult::Borrowed(c),
        ContainerRef::Bitmap(_) if bitmap_bytes == best => return OpResult::Borrowed(c),
        _ => {}
    }

    OpResult::Owned(if best == array_bytes {
        Container::Array(c.iter().collect())
    } else if best == run_bytes {
        Container::Run(to_runs(c))
    } else {
        let mut w = Box::new([0u64; crate::BITMAP_WORDS]);
        for v in c.iter() {
            w[v as usize / 64] |= 1u64 << (v % 64);
        }
        Container::Bitmap(w)
    })
}
