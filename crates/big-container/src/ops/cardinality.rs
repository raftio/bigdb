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

//! How large an intersection is, without building it.
//!
//! **The one operation a grouping does and never looks at.** Counting the records of one row
//! that a filter keeps is `|a ∩ b|`, and [`crate::apply`] answers it by allocating the whole
//! intersection and then asking its length - a buffer sized, filled and dropped for a number.
//! A grouping does that once per distinct value per fragment, so the allocation is not a
//! detail: it is the shape of the loop.
//!
//! Every routine here is the corresponding one in [`crate::ops`] with the writes removed. That
//! is deliberate and it is what the tests check: `and_cardinality(a, b)` must equal
//! `and(a, b).cardinality()` for every pair of containers, because two ways of counting the
//! same set that can disagree are a wrong answer with no symptom.

use crate::container::ContainerRef;
use crate::interval::Interval;

/// How many values two containers have in common.
pub fn and_cardinality(a: ContainerRef<'_>, b: ContainerRef<'_>) -> u32 {
    use ContainerRef::{Array, Bitmap, Run};
    match (a, b) {
        // Nothing in common with nothing, and the walks below would do the right thing anyway -
        // this only saves entering them.
        _ if a.is_empty() || b.is_empty() => 0,

        (Array(x), Array(y)) => array_array(x, y),
        // Probing the bitmap is `O(|array|)` against a merge's `O(|array| + 1024)`, and the
        // array is the smaller side by construction: a container becomes a bitmap precisely
        // when it stops being worth storing as one.
        (Array(x), Bitmap(y)) | (Bitmap(y), Array(x)) => {
            x.iter().filter(|v| bit(y, **v)).count() as u32
        }
        (Array(x), Run(y)) | (Run(y), Array(x)) => {
            x.iter().filter(|v| in_runs(y, **v)).count() as u32
        }
        (Bitmap(x), Bitmap(y)) => x.iter().zip(y.iter()).map(|(p, q)| (p & q).count_ones()).sum(),
        (Run(x), Run(y)) => run_run(x, y),
        (Bitmap(x), Run(y)) | (Run(y), Bitmap(x)) => y.iter().map(|i| bits_in(x, *i)).sum(),
    }
}

fn bit(words: &[u64; crate::BITMAP_WORDS], v: u16) -> bool {
    words[v as usize / 64] >> (v % 64) & 1 == 1
}

fn in_runs(runs: &[Interval], v: u16) -> bool {
    runs.binary_search_by(|i| {
        if i.last < v {
            core::cmp::Ordering::Less
        } else if i.start > v {
            core::cmp::Ordering::Greater
        } else {
            core::cmp::Ordering::Equal
        }
    })
    .is_ok()
}

/// Two sorted arrays, walked once.
fn array_array(a: &[u16], b: &[u16]) -> u32 {
    let (mut i, mut j, mut n) = (0usize, 0usize, 0u32);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            core::cmp::Ordering::Less => i += 1,
            core::cmp::Ordering::Greater => j += 1,
            core::cmp::Ordering::Equal => {
                n += 1;
                i += 1;
                j += 1;
            }
        }
    }
    n
}

/// Two sorted, disjoint interval lists: the overlap of each pair that meets.
fn run_run(a: &[Interval], b: &[Interval]) -> u32 {
    let (mut i, mut j, mut n) = (0usize, 0usize, 0u32);
    while i < a.len() && j < b.len() {
        let (x, y) = (a[i], b[j]);
        let start = x.start.max(y.start);
        let last = x.last.min(y.last);
        if start <= last {
            n += (last as u32) - (start as u32) + 1;
        }
        // Advance whichever ends first: the other may still meet the next one along.
        if x.last < y.last {
            i += 1;
        } else {
            j += 1;
        }
    }
    n
}

/// How many bits of a bitmap fall inside one interval.
///
/// Word by word, with the two partial words at the ends masked rather than iterated - which is
/// what keeps this `O(words touched)` instead of `O(interval length)`.
fn bits_in(words: &[u64; crate::BITMAP_WORDS], run: Interval) -> u32 {
    let (start, last) = (run.start as usize, run.last as usize);
    let (first_word, last_word) = (start / 64, last / 64);
    if first_word == last_word {
        // One word, masked at both ends.
        let width = last - start + 1;
        let mask = if width == 64 { u64::MAX } else { ((1u64 << width) - 1) << (start % 64) };
        return (words[first_word] & mask).count_ones();
    }
    let head = words[first_word] & (u64::MAX << (start % 64));
    let tail = words[last_word] & (u64::MAX >> (63 - (last % 64)));
    let middle: u32 = words[first_word + 1..last_word].iter().map(|w| w.count_ones()).sum();
    head.count_ones() + middle + tail.count_ones()
}
