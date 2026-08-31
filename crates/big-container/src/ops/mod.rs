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

//! Set operations without a combinatorial explosion of hand-written functions.
//!
//! 3 representations squared times 4 operations is 36 combinations. Normalising the operand
//! order for the commutative ones brings that to 27, and only a handful are worth specialising:
//! everything else routes through an iterator merge that is always correct.
//!
//! Correct, but not always affordable, and which handful is "worth it" is a measurement rather
//! than a judgement. The merge walks values, so a run container costs what it *expands to*
//! rather than what it stores - and the densest data produces the most compact runs, which
//! makes the engine slowest on the shape it stores best. `array_run` exists because a benchmark
//! priced that at 85x on a grouped aggregation; see the report.

pub mod array_array;
pub mod array_bitmap;
pub mod array_run;
pub mod bitmap_bitmap;
pub mod cardinality;
pub mod fallback;

use crate::builder::Out;
use crate::{Container, ContainerRef, OpResult};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SetOp {
    And,
    Or,
    Xor,
    AndNot,
}

impl SetOp {
    pub fn is_commutative(self) -> bool {
        !matches!(self, Self::AndNot)
    }

    /// Keep a value present only in the left operand.
    pub fn keeps_left_only(self) -> bool {
        matches!(self, Self::Or | Self::Xor | Self::AndNot)
    }

    /// Keep a value present only in the right operand.
    pub fn keeps_right_only(self) -> bool {
        matches!(self, Self::Or | Self::Xor)
    }

    /// Keep a value present in both.
    pub fn keeps_both(self) -> bool {
        matches!(self, Self::And | Self::Or)
    }

    /// Upper bound on the result, known before any work happens.
    fn bound(self, la: usize, lb: usize) -> usize {
        match self {
            Self::And => la.min(lb),
            Self::Or | Self::Xor => la + lb,
            Self::AndNot => la,
        }
    }
}

/// Array before Run before Bitmap, so a commutative operation only has to cover 6 pairs.
fn rank(c: &ContainerRef<'_>) -> u8 {
    match c {
        ContainerRef::Array(_) => 0,
        ContainerRef::Run(_) => 1,
        ContainerRef::Bitmap(_) => 2,
    }
}

pub fn and<'a>(a: ContainerRef<'a>, b: ContainerRef<'a>) -> OpResult<'a> {
    apply(SetOp::And, a, b)
}

pub fn or<'a>(a: ContainerRef<'a>, b: ContainerRef<'a>) -> OpResult<'a> {
    apply(SetOp::Or, a, b)
}

pub fn xor<'a>(a: ContainerRef<'a>, b: ContainerRef<'a>) -> OpResult<'a> {
    apply(SetOp::Xor, a, b)
}

pub fn andnot<'a>(a: ContainerRef<'a>, b: ContainerRef<'a>) -> OpResult<'a> {
    apply(SetOp::AndNot, a, b)
}

pub fn apply<'a>(op: SetOp, a: ContainerRef<'a>, b: ContainerRef<'a>) -> OpResult<'a> {
    // Identity cases hand back an operand untouched. This is the only place borrowing wins,
    // and it is exactly why `OpResult` exists instead of a Cow in every variant.
    if let Some(r) = shortcut(op, a, b) {
        return r;
    }

    let (op, a, b) =
        if op.is_commutative() && rank(&a) > rank(&b) { (op, b, a) } else { (op, a, b) };

    let mut out = Out::with_bound(op.bound(a.cardinality() as usize, b.cardinality() as usize));
    let handled = match (a, b) {
        (ContainerRef::Array(x), ContainerRef::Array(y)) => {
            array_array::run(op, x, y, &mut out);
            true
        }
        (ContainerRef::Array(x), ContainerRef::Bitmap(y)) => array_bitmap::run(op, x, y, &mut out),
        (ContainerRef::Array(x), ContainerRef::Run(y)) => array_run::run(op, x, y, &mut out),
        (ContainerRef::Bitmap(x), ContainerRef::Array(y)) => {
            array_bitmap::run_reversed(op, x, y, &mut out)
        }
        (ContainerRef::Bitmap(x), ContainerRef::Bitmap(y)) => {
            bitmap_bitmap::run(op, x, y, &mut out)
        }
        _ => false,
    };
    if !handled {
        fallback::merge(op, a, b, &mut out);
    }
    OpResult::Owned(out.finish())
}

fn shortcut<'a>(op: SetOp, a: ContainerRef<'a>, b: ContainerRef<'a>) -> Option<OpResult<'a>> {
    let (ea, eb) = (a.is_empty(), b.is_empty());
    if !ea && !eb {
        return None;
    }
    Some(OpResult::Borrowed(match op {
        SetOp::And => {
            if ea {
                a
            } else {
                b
            }
        }
        SetOp::Or | SetOp::Xor => {
            if ea {
                b
            } else {
                a
            }
        }
        SetOp::AndNot => a,
    }))
}

/// Folds a sequence with one operation. Returns `None` for an empty sequence, because the
/// identity element differs per operation and guessing it here would be wrong.
pub fn fold<'a>(op: SetOp, mut items: impl Iterator<Item = ContainerRef<'a>>) -> Option<Container> {
    let mut acc = items.next()?.to_owned();
    for it in items {
        acc = apply(op, acc.as_ref(), it).into_owned();
    }
    Some(acc)
}
