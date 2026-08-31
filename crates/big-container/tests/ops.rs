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

//! Differential tests against two independent oracles.
//!
//! `HashSet<u16>` and `croaring` fail differently: a hand-rolled set catches coding mistakes,
//! a real roaring implementation catches misunderstood semantics. One oracle would miss half.

use big_container::*;
use proptest::prelude::*;
use std::collections::BTreeSet;

const REPRS: [u8; 3] = [0, 1, 2];
const OPS: [SetOp; 4] = [SetOp::And, SetOp::Or, SetOp::Xor, SetOp::AndNot];

fn as_repr(vals: &BTreeSet<u16>, repr: u8) -> Container {
    let sorted: Vec<u16> = vals.iter().copied().collect();
    match repr {
        0 => Container::Array(sorted),
        1 => Container::Run(to_runs(ContainerRef::Array(&sorted))),
        _ => {
            let mut w = Box::new([0u64; BITMAP_WORDS]);
            for v in &sorted {
                w[*v as usize / 64] |= 1u64 << (*v % 64);
            }
            Container::Bitmap(w)
        }
    }
}

fn oracle_set(op: SetOp, a: &BTreeSet<u16>, b: &BTreeSet<u16>) -> BTreeSet<u16> {
    match op {
        SetOp::And => a.intersection(b).copied().collect(),
        SetOp::Or => a.union(b).copied().collect(),
        SetOp::Xor => a.symmetric_difference(b).copied().collect(),
        SetOp::AndNot => a.difference(b).copied().collect(),
    }
}

fn oracle_croaring(op: SetOp, a: &BTreeSet<u16>, b: &BTreeSet<u16>) -> BTreeSet<u16> {
    let mk = |s: &BTreeSet<u16>| {
        let mut r = croaring::Bitmap::new();
        for v in s {
            r.add(*v as u32);
        }
        r
    };
    let (x, y) = (mk(a), mk(b));
    let r = match op {
        SetOp::And => x.and(&y),
        SetOp::Or => x.or(&y),
        SetOp::Xor => x.xor(&y),
        SetOp::AndNot => x.andnot(&y),
    };
    r.iter().map(|v| v as u16).collect()
}

/// Mixes scattered points with contiguous ranges so run containers are actually exercised.
fn values() -> impl Strategy<Value = BTreeSet<u16>> {
    prop_oneof![
        proptest::collection::btree_set(any::<u16>(), 0..80),
        proptest::collection::btree_set(0u16..600, 0..200),
        proptest::collection::vec((0u16..60000, 1u16..90), 0..8).prop_map(|ranges| ranges
            .into_iter()
            .flat_map(|(s, l)| (s..s.saturating_add(l)).collect::<Vec<_>>())
            .collect()),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(400))]

    #[test]
    fn every_pair_and_op_matches_both_oracles(a in values(), b in values()) {
        for ra in REPRS {
            for rb in REPRS {
                let (ca, cb) = (as_repr(&a, ra), as_repr(&b, rb));
                for op in OPS {
                    let got: BTreeSet<u16> =
                        apply(op, ca.as_ref(), cb.as_ref()).as_ref().iter().collect();
                    prop_assert_eq!(&got, &oracle_set(op, &a, &b), "{:?} {} vs {}", op, ra, rb);
                    prop_assert_eq!(&got, &oracle_croaring(op, &a, &b), "{:?} {} vs {}", op, ra, rb);
                }
            }
        }
    }

    /// Every specialisation must agree with the path that is correct by construction.
    #[test]
    fn specialisations_agree_with_the_fallback(a in values(), b in values()) {
        for ra in REPRS {
            for rb in REPRS {
                let (ca, cb) = (as_repr(&a, ra), as_repr(&b, rb));
                for op in OPS {
                    let fast: BTreeSet<u16> =
                        apply(op, ca.as_ref(), cb.as_ref()).as_ref().iter().collect();

                    let bound = match op {
                        SetOp::And => a.len().min(b.len()),
                        SetOp::Or | SetOp::Xor => a.len() + b.len(),
                        SetOp::AndNot => a.len(),
                    };
                    let mut out = big_container::builder::Out::with_bound(bound);
                    big_container::ops::fallback::merge(op, ca.as_ref(), cb.as_ref(), &mut out);
                    let slow: BTreeSet<u16> = out.finish().as_ref().iter().collect();

                    prop_assert_eq!(fast, slow, "{:?} {} vs {}", op, ra, rb);
                }
            }
        }
    }

    /// Counting an intersection must agree with building one and asking its length.
    ///
    /// **The claim `and_cardinality` exists to make.** It is the same routines with the writes
    /// removed, so every way it could be wrong is a way of dropping or double-counting a value
    /// that nothing downstream would notice: a grouping would answer a plausible number. All
    /// nine representation pairs, because the dispatch depends on both operands and the
    /// bitmap-against-run arm counts by masking words rather than by walking values.
    #[test]
    fn counting_an_intersection_agrees_with_building_one(a in values(), b in values()) {
        let want = a.intersection(&b).count() as u32;
        for ra in REPRS {
            for rb in REPRS {
                let (ca, cb) = (as_repr(&a, ra), as_repr(&b, rb));
                let counted = and_cardinality(ca.as_ref(), cb.as_ref());
                prop_assert_eq!(counted, want, "counted wrong for {} against {}", ra, rb);
                // And against the operation it stands in for, which is the substitution the
                // caller is actually making.
                prop_assert_eq!(
                    counted,
                    apply(SetOp::And, ca.as_ref(), cb.as_ref()).as_ref().cardinality(),
                    "{} against {} disagreed with `and`",
                    ra,
                    rb
                );
                // Commutative, which the dispatch does not make obvious: three of the arms
                // normalise the operand order and three do not.
                prop_assert_eq!(counted, and_cardinality(cb.as_ref(), ca.as_ref()));
            }
        }
    }

    #[test]
    fn cardinality_and_contains_agree_across_representations(a in values()) {
        for r in REPRS {
            let c = as_repr(&a, r);
            prop_assert_eq!(c.cardinality() as usize, a.len());
            prop_assert_eq!(c.is_empty(), a.is_empty());
            for v in a.iter().take(50) {
                prop_assert!(c.contains(*v));
            }
            for v in [0u16, 1, 12345, 65535] {
                prop_assert_eq!(c.contains(v), a.contains(&v));
            }
        }
    }

    #[test]
    fn optimize_never_changes_the_set(a in values(), am in 0usize..5000, rm in 0usize..3000) {
        let caps = Caps { array_max: am, run_max: rm };
        for r in REPRS {
            let c = as_repr(&a, r);
            let got: BTreeSet<u16> = optimize(c.as_ref(), caps).as_ref().iter().collect();
            prop_assert_eq!(got, a.clone());
        }
    }

    #[test]
    fn insert_and_remove_track_a_plain_set(a in values(), v in any::<u16>()) {
        for r in REPRS {
            let mut c = as_repr(&a, r);
            let mut want = a.clone();

            prop_assert_eq!(c.insert(v), want.insert(v));
            prop_assert_eq!(c.as_ref().iter().collect::<BTreeSet<_>>(), want.clone());

            prop_assert_eq!(c.remove(v), want.remove(&v));
            prop_assert_eq!(c.as_ref().iter().collect::<BTreeSet<_>>(), want);
        }
    }

    #[test]
    fn representations_round_trip_through_each_other(a in values()) {
        for from in REPRS {
            let c = as_repr(&a, from);
            prop_assert!(c.as_ref().is_well_formed(), "repr {} malformed", from);
            prop_assert_eq!(run_count(c.as_ref()), run_count(ContainerRef::Array(
                &a.iter().copied().collect::<Vec<_>>()
            )));
        }
    }
}

/// `A | {}` must hand back A untouched, not a fresh allocation.
#[test]
fn identity_operations_borrow_instead_of_allocating() {
    let a = Container::from_values([1u16, 5, 9]);
    let empty = Container::empty();

    assert!(or(a.as_ref(), empty.as_ref()).is_borrowed());
    assert!(or(empty.as_ref(), a.as_ref()).is_borrowed());
    assert!(and(a.as_ref(), empty.as_ref()).is_borrowed());
    assert!(andnot(a.as_ref(), empty.as_ref()).is_borrowed());
    assert!(xor(a.as_ref(), empty.as_ref()).is_borrowed());

    assert_eq!(or(a.as_ref(), empty.as_ref()).as_ref().cardinality(), 3);
    assert_eq!(and(a.as_ref(), empty.as_ref()).as_ref().cardinality(), 0);
}

/// A small result must not be built as a bitmap and then shrunk afterwards.
#[test]
fn output_representation_is_predicted_not_corrected() {
    let a = Container::from_values(0u16..100);
    let b = Container::from_values(50u16..150);
    assert!(matches!(and(a.as_ref(), b.as_ref()), OpResult::Owned(Container::Array(_))));
    assert!(matches!(or(a.as_ref(), b.as_ref()), OpResult::Owned(Container::Array(_))));

    let dense_a = Container::from_values((0u16..60000).step_by(2));
    let dense_b = Container::from_values((1u16..60000).step_by(2));
    assert!(matches!(
        or(dense_a.as_ref(), dense_b.as_ref()),
        OpResult::Owned(Container::Bitmap(_))
    ));
}

#[test]
fn optimize_picks_the_smallest_representation_that_fits() {
    let caps = Caps { array_max: 4074, run_max: 2037 };

    let one_run = Container::from_values(0u16..5000);
    assert!(matches!(optimize(one_run.as_ref(), caps), OpResult::Owned(Container::Run(_))));

    let sparse = Container::from_values([1u16, 500, 9000]);
    assert!(optimize(sparse.as_ref(), caps).is_borrowed(), "already an array, stay borrowed");

    // Too many runs for a run container and too many values for an array: only a bitmap fits.
    let shredded = Container::from_values((0u16..40000).step_by(2));
    assert!(matches!(optimize(shredded.as_ref(), caps), OpResult::Owned(Container::Bitmap(_))));
}

#[test]
fn fold_applies_the_operation_across_a_sequence() {
    let cs: Vec<Container> = (0..4).map(|i| Container::from_values([i as u16, 100])).collect();
    let refs: Vec<ContainerRef<'_>> = cs.iter().map(|c| c.as_ref()).collect();

    let all = fold(SetOp::And, refs.iter().copied()).unwrap();
    assert_eq!(all.as_ref().iter().collect::<Vec<_>>(), vec![100]);

    let any = fold(SetOp::Or, refs.iter().copied()).unwrap();
    assert_eq!(any.as_ref().iter().collect::<Vec<_>>(), vec![0, 1, 2, 3, 100]);

    assert!(fold(SetOp::Or, core::iter::empty()).is_none());
}
