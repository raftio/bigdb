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

//! Container boolean algebra, against `BTreeSet<u16>` as the oracle.
//!
//! **Not a byte parser, on purpose.** Parsing a container out of hostile bytes is already
//! covered by `parse_page`, which reaches `cell.container()` and `cell.bitmap()`. What is not
//! covered is the far larger surface underneath: three representations, and therefore six
//! implementations of every binary operation, chosen by a dispatch that depends on both
//! operands. `array x array` and `array x bitmap` are different code, and a fuzzer that only
//! fed bytes to the parser would never run either.
//!
//! The oracle is the whole point. "It did not panic" would pass on an intersection that
//! silently returned the wrong set, which is the failure that actually matters here - a wrong
//! answer to a query is worse than a crash, because nothing reports it.
//!
//! `optimize` is applied to the inputs so that all three representations turn up. Feeding only
//! arrays would test one sixth of the dispatch table.

use arbitrary::Arbitrary;
// `CAPS` is the physical ceiling - the largest array and run a container may hold - which is
// the right one here. `big-btree`'s `WRITE_CAPS` is tighter and about write cost rather than
// fit, so using it would leave the widest arrays and runs untested.
use big_btree::CAPS;
use big_container::{apply, optimize, Container, ContainerRef, SetOp};
use libfuzzer_sys::fuzz_target;
use std::collections::BTreeSet;

#[derive(Arbitrary, Debug)]
struct Input {
    a: Vec<u16>,
    b: Vec<u16>,
    op: Op,
    /// Whether to push each side towards a denser representation before operating, so the
    /// dispatch lands on a different pair of arms.
    densify: (bool, bool),
}

#[derive(Arbitrary, Debug, Clone, Copy)]
enum Op {
    And,
    Or,
    Xor,
    AndNot,
}

impl Op {
    fn set_op(self) -> SetOp {
        match self {
            Op::And => SetOp::And,
            Op::Or => SetOp::Or,
            Op::Xor => SetOp::Xor,
            Op::AndNot => SetOp::AndNot,
        }
    }

    fn oracle(self, a: &BTreeSet<u16>, b: &BTreeSet<u16>) -> BTreeSet<u16> {
        match self {
            Op::And => a.intersection(b).copied().collect(),
            Op::Or => a.union(b).copied().collect(),
            Op::Xor => a.symmetric_difference(b).copied().collect(),
            Op::AndNot => a.difference(b).copied().collect(),
        }
    }
}

/// A container and the set it stands for, kept side by side.
fn build(values: &[u16], densify: bool) -> (Container, BTreeSet<u16>) {
    let set: BTreeSet<u16> = values.iter().copied().collect();
    let c = Container::from_values(set.iter().copied());
    let c = if densify { optimize(c.as_ref(), CAPS).into_owned() } else { c };
    (c, set)
}

fn to_set(c: ContainerRef<'_>) -> BTreeSet<u16> {
    c.iter().collect()
}

fuzz_target!(|input: Input| {
    // Bounded so one input cannot cost the fuzzer a second. A container holds at most 65,536
    // values and the interesting behaviour - the representation crossovers - is all below this.
    if input.a.len() > 20_000 || input.b.len() > 20_000 {
        return;
    }

    let (ca, sa) = build(&input.a, input.densify.0);
    let (cb, sb) = build(&input.b, input.densify.1);

    // Precondition on the oracle itself: a container has to agree with the set it was built
    // from before it is worth asking it anything harder.
    assert_eq!(to_set(ca.as_ref()), sa, "container disagreed with the set it was built from");
    assert_eq!(to_set(cb.as_ref()), sb);
    assert_eq!(ca.cardinality() as usize, sa.len());
    assert_eq!(cb.cardinality() as usize, sb.len());

    let got = apply(input.op.set_op(), ca.as_ref(), cb.as_ref()).into_owned();
    let want = input.op.oracle(&sa, &sb);

    assert_eq!(
        to_set(got.as_ref()),
        want,
        "{:?} over {} and {} values disagreed with BTreeSet",
        input.op,
        sa.len(),
        sb.len()
    );
    // Cardinality is stored, not recounted, in at least one representation, so it can be wrong
    // while the iteration is right.
    assert_eq!(got.cardinality() as usize, want.len(), "cardinality disagreed with the contents");

    // `contains` takes a different path from `iter` in every representation - a binary search,
    // a bit test, or an interval search - so agreeing with the same set is a separate claim.
    for v in want.iter().take(64) {
        assert!(got.contains(*v), "contains said no about a value iter yielded");
    }

    // Optimising must not change what a container means, only how it is stored. This is the
    // invariant every write depends on: `make_item` optimises on the way to disk.
    let reshaped = optimize(got.as_ref(), CAPS).into_owned();
    assert_eq!(to_set(reshaped.as_ref()), want, "optimize changed the set it was given");
});
