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

//! Sparse array against runs: sweep the array, not the records the runs stand for.
//!
//! This pair was deliberately left to the fallback merge, on the reasoning in `ops/mod.rs` that
//! only a handful of the twenty-seven combinations are worth hand-writing. A benchmark found
//! what that costs. `group_counts` intersects one row per distinct value against a filter, and
//! when the filter is `All()` over a dense table that filter is a single run per 65,536-record
//! block - the most compact thing the format can store. The merge then walks all 65,536 records
//! the run stands for, once per row, to intersect them with a row holding about two hundred:
//! 320ms where the same query over a bitmap-shaped filter took 3.8ms, for the same records.
//!
//! Both operands are ascending and the runs are disjoint, so one sweep with two cursors is
//! enough and costs `|array| + |runs|` rather than the cardinality the runs expand to. In the
//! case above that is 196 steps instead of 65,536.
//!
//! `Or` and `Xor` are still left to the fallback. An array output has to be ascending, which
//! forces a merge for those two whatever the right-hand side looks like - the same reason
//! `array_bitmap` only handles them when the output is already dense.

use super::SetOp;
use crate::builder::Out;
use crate::Interval;

/// `a` is the array, `b` the runs. Handles `a and b` and `a andnot b`.
///
/// Returns whether it handled the operation, so an unhandled one falls through to the merge
/// rather than silently producing nothing.
pub fn run(op: SetOp, a: &[u16], b: &[Interval], out: &mut Out) -> bool {
    // `keep` is what to do with an array value that falls inside a run; the value that falls
    // outside every run gets the opposite. Writing it once means the two cursors below cannot
    // drift apart between the two operations.
    let keep_inside = match op {
        SetOp::And => true,
        SetOp::AndNot => false,
        SetOp::Or | SetOp::Xor => return false,
    };

    let (mut i, mut j) = (0usize, 0usize);
    while i < a.len() {
        let v = a[i];
        // Past the last run: every remaining value is outside all of them.
        let Some(r) = b.get(j) else {
            if !keep_inside {
                out.push(v);
            }
            i += 1;
            continue;
        };

        if v < r.start {
            // Before this run, and the runs are ascending and disjoint, so before all of them.
            if !keep_inside {
                out.push(v);
            }
            i += 1;
        } else if v > r.last {
            // This run is entirely behind the array cursor and can never be revisited: the
            // array ascends too. Advancing `j` rather than rescanning is what makes the sweep
            // linear in the two lengths instead of quadratic.
            j += 1;
        } else {
            if keep_inside {
                out.push(v);
            }
            i += 1;
        }
    }
    true
}
