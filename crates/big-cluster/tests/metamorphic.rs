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

//! Predicates nobody wrote, against answers nobody wrote down.
//!
//! # Two oracles, and why both
//!
//! The corpora say what a few hundred statements answer, and every one of those answers was read
//! by somebody. That is what makes them worth having and also what bounds them: they cover the
//! statements somebody thought of.
//!
//! So this generates predicates instead - nested, negated, mixed across every field class - and
//! checks each against two things that are not the engine.
//!
//! **The ground truth.** The rows are built here, so the predicate can be evaluated over them in
//! Rust and the number compared directly. This is the strongest oracle available and the reason
//! this file is not only metamorphic: it does not merely say the engine is consistent with
//! itself, it says the engine is *right*.
//!
//! **The partition.** `count(p) + count(NOT p) = count(*)` is the relation CockroachDB's TLP
//! looks for, and it is usually approximate: SQL's three-valued logic puts a row where `p` is
//! null in neither half. There is no null here, so the equation is exact - which makes it a
//! sharper instrument here than it is there, and cheap enough to check on every predicate as
//! well as its conjunctions and disjunctions.
//!
//! # What a failure looks like
//!
//! `proptest` shrinks the predicate before reporting it, so a failure arrives as the smallest
//! `WHERE` clause that still disagrees - which is usually short enough to paste into
//! `tests/logic` as a case and keep.

use std::sync::OnceLock;

use big_cluster::Cluster;
use big_embed::{Api, FieldKind, MemPager, QueryOptions, Value};
use proptest::prelude::*;

/// The columns the generator writes predicates about: one of every class the planner
/// distinguishes, because which class a column is decides what its `=` even means.
const COUNTRIES: [&str; 4] = ["GB", "US", "FR", "DE"];

/// One record, as the generator's own copy of the data.
#[derive(Clone, Copy)]
struct Rec {
    id: u64,
    amount: u64,
    /// In the units the field stores, which is what a decimal field actually holds.
    price: u64,
    balance: i64,
    country: &'static str,
    active: bool,
}

/// Two hundred records with no pattern anybody would write a query for, and no randomness
/// either: a generated dataset that changed between runs would make a shrunk counter-example
/// unreproducible, which is the one thing a shrinking test may not do.
fn rows() -> Vec<Rec> {
    (1..=200u64)
        .map(|id| Rec {
            id,
            amount: (id * 37) % 1000,
            price: (id * 53) % 2000,
            balance: (id as i64 * 41) % 201 - 100,
            country: COUNTRIES[(id % 4) as usize],
            active: id % 3 != 0,
        })
        .collect()
}

/// The database, built once. Every case runs against the same rows, which is what lets a shrunk
/// predicate be re-run by hand.
fn db() -> &'static Cluster<MemPager> {
    static DB: OnceLock<Cluster<MemPager>> = OnceLock::new();
    DB.get_or_init(|| {
        let api = Api::in_memory().unwrap();
        api.create_table("t").unwrap();
        api.create_field("t", "amount", FieldKind::Int, 16).unwrap();
        api.create_decimal("t", "price", 20, 2).unwrap();
        api.create_field("t", "balance", FieldKind::SignedInt, 16).unwrap();
        api.create_field("t", "country", FieldKind::Set, 0).unwrap();
        api.create_field("t", "active", FieldKind::Bool, 0).unwrap();
        for r in rows() {
            api.import(
                "t",
                &[
                    big_embed::Fact::Int { field: "amount", record: r.id, value: r.amount },
                    big_embed::Fact::Int { field: "price", record: r.id, value: r.price },
                    big_embed::Fact::Signed { field: "balance", record: r.id, value: r.balance },
                    big_embed::Fact::Key { field: "country", record: r.id, value: r.country },
                    big_embed::Fact::Bool { field: "active", record: r.id, value: r.active },
                ],
            )
            .unwrap();
        }
        Cluster::solo(api)
    })
}

/// A `WHERE` clause, as something that can be both written and evaluated.
///
/// The two have to come from one value or the test is comparing two readings of a string. This
/// is that value: [`Pred::sql`] writes it and [`Pred::holds`] decides it, and neither is
/// derived from the other.
#[derive(Clone, Debug)]
enum Pred {
    /// A comparison against a whole number, on the column with no scale.
    Amount(Cmp, u64),
    /// The same against a column of scale two, where the written value has to be converted
    /// before it means anything.
    Price(Cmp, u64),
    /// And against a signed column, where a negative bound is a bound.
    Balance(Cmp, i64),
    Country(usize),
    Active(bool),
    /// `IN`, which is a union of equalities.
    In(Vec<usize>),
    /// `BETWEEN`, which is two bounds.
    Between(u64, u64),
    And(Box<Pred>, Box<Pred>),
    Or(Box<Pred>, Box<Pred>),
    Not(Box<Pred>),
}

#[derive(Clone, Copy, Debug)]
enum Cmp {
    Gt,
    Ge,
    Lt,
    Le,
    Eq,
    Ne,
}

impl Cmp {
    fn sql(self) -> &'static str {
        match self {
            Self::Gt => ">",
            Self::Ge => ">=",
            Self::Lt => "<",
            Self::Le => "<=",
            Self::Eq => "=",
            Self::Ne => "!=",
        }
    }

    fn holds<T: Ord>(self, left: T, right: T) -> bool {
        match self {
            Self::Gt => left > right,
            Self::Ge => left >= right,
            Self::Lt => left < right,
            Self::Le => left <= right,
            Self::Eq => left == right,
            Self::Ne => left != right,
        }
    }
}

impl Pred {
    /// The clause, as SQL. Parenthesised at every level, because what is being tested is the
    /// engine and not this test's grasp of the precedence rules.
    fn sql(&self) -> String {
        match self {
            Self::Amount(op, v) => format!("amount {} {v}", op.sql()),
            // Written the way a person writes a price, which is the spelling that has to be
            // converted against the field's scale on the way in.
            Self::Price(op, units) => {
                format!("price {} {}.{:02}", op.sql(), units / 100, units % 100)
            }
            Self::Balance(op, v) => format!("balance {} {v}", op.sql()),
            Self::Country(i) => format!("country = '{}'", COUNTRIES[*i]),
            Self::Active(v) => format!("active = {v}"),
            Self::In(is) => {
                let keys: Vec<String> = is.iter().map(|i| format!("'{}'", COUNTRIES[*i])).collect();
                format!("country IN ({})", keys.join(", "))
            }
            Self::Between(lo, hi) => format!("amount BETWEEN {lo} AND {hi}"),
            Self::And(a, b) => format!("({} AND {})", a.sql(), b.sql()),
            Self::Or(a, b) => format!("({} OR {})", a.sql(), b.sql()),
            Self::Not(p) => format!("NOT ({})", p.sql()),
        }
    }

    /// Whether the record satisfies it, decided here rather than asked of the engine.
    fn holds(&self, r: &Rec) -> bool {
        match self {
            Self::Amount(op, v) => op.holds(r.amount, *v),
            Self::Price(op, units) => op.holds(r.price, *units),
            Self::Balance(op, v) => op.holds(r.balance, *v),
            Self::Country(i) => r.country == COUNTRIES[*i],
            Self::Active(v) => r.active == *v,
            Self::In(is) => is.iter().any(|i| r.country == COUNTRIES[*i]),
            Self::Between(lo, hi) => r.amount >= *lo && r.amount <= *hi,
            Self::And(a, b) => a.holds(r) && b.holds(r),
            Self::Or(a, b) => a.holds(r) || b.holds(r),
            Self::Not(p) => !p.holds(r),
        }
    }
}

fn cmp() -> impl Strategy<Value = Cmp> {
    prop_oneof![
        Just(Cmp::Gt),
        Just(Cmp::Ge),
        Just(Cmp::Lt),
        Just(Cmp::Le),
        Just(Cmp::Eq),
        Just(Cmp::Ne),
    ]
}

/// Leaves first, then three levels of nesting, which is deep enough to reach every rewrite the
/// lowering does - `AND NOT` becoming a difference, a chain of `AND` flattening into one
/// intersection - without generating clauses too long to read when one fails.
fn pred() -> impl Strategy<Value = Pred> {
    let leaf = prop_oneof![
        (cmp(), 0u64..1000).prop_map(|(o, v)| Pred::Amount(o, v)),
        (cmp(), 0u64..2000).prop_map(|(o, v)| Pred::Price(o, v)),
        (cmp(), -100i64..100).prop_map(|(o, v)| Pred::Balance(o, v)),
        (0usize..4).prop_map(Pred::Country),
        any::<bool>().prop_map(Pred::Active),
        proptest::collection::vec(0usize..4, 1..4).prop_map(Pred::In),
        (0u64..1000, 0u64..1000).prop_map(|(a, b)| Pred::Between(a.min(b), a.max(b))),
    ];
    leaf.prop_recursive(3, 12, 2, |inner| {
        prop_oneof![
            (inner.clone(), inner.clone()).prop_map(|(a, b)| Pred::And(a.into(), b.into())),
            (inner.clone(), inner.clone()).prop_map(|(a, b)| Pred::Or(a.into(), b.into())),
            inner.prop_map(|p| Pred::Not(p.into())),
        ]
    })
}

/// `SELECT count(*) FROM t WHERE <where>`, or the whole table when there is none.
fn count(clause: Option<&str>) -> u64 {
    let sql = match clause {
        Some(w) => format!("SELECT count(*) FROM t WHERE {w}"),
        None => "SELECT count(*) FROM t".to_string(),
    };
    let (values, _) = db()
        .local()
        .sql(&sql, &QueryOptions::default())
        .unwrap_or_else(|e| panic!("`{sql}` was refused: {e}"));
    match values.first() {
        Some(Value::Count(n)) => *n,
        other => panic!("`{sql}` answered {other:?}"),
    }
}

proptest! {
    // Enough cases to reach the interesting nestings, and few enough that the suite stays a
    // suite: each case is three queries over two hundred records with no file behind them.
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    /// **The strong one.** The rows are this test's own, so the answer is known before the
    /// engine is asked.
    #[test]
    fn every_predicate_counts_the_records_that_satisfy_it(p in pred()) {
        let clause = p.sql();
        let want = rows().iter().filter(|r| p.holds(r)).count() as u64;
        prop_assert_eq!(count(Some(&clause)), want, "\n  where: {}\n", clause);
    }

    /// **The partition.** A record either satisfies the predicate or does not, and there is no
    /// third answer here for it to fall into - so the two halves are the whole table, exactly.
    #[test]
    fn a_predicate_and_its_negation_partition_the_table(p in pred()) {
        let clause = p.sql();
        let yes = count(Some(&clause));
        let no = count(Some(&format!("NOT ({clause})")));
        prop_assert_eq!(yes + no, count(None), "\n  where: {}\n", clause);
    }

    /// Inclusion and exclusion, which is the relation a union has to satisfy however the
    /// lowering chose to spell it - and the one an `AND NOT` rewritten into a difference would
    /// break if it were rewritten wrongly.
    #[test]
    fn a_union_holds_what_both_halves_hold_less_their_overlap(a in pred(), b in pred()) {
        let (x, y) = (a.sql(), b.sql());
        let both = count(Some(&format!("({x}) AND ({y})")));
        let either = count(Some(&format!("({x}) OR ({y})")));
        prop_assert_eq!(
            either + both,
            count(Some(&x)) + count(Some(&y)),
            "\n  a: {}\n  b: {}\n",
            x,
            y
        );
    }

    /// A predicate splits under a second one the same way it splits under its own negation,
    /// which is the relation that catches a conjunction narrowing the wrong side.
    #[test]
    fn a_second_predicate_splits_the_first_one(a in pred(), b in pred()) {
        let (x, y) = (a.sql(), b.sql());
        let with = count(Some(&format!("({x}) AND ({y})")));
        let without = count(Some(&format!("({x}) AND NOT ({y})")));
        prop_assert_eq!(with + without, count(Some(&x)), "\n  a: {}\n  b: {}\n", x, y);
    }

    /// The groups of a keyed column are a partition too: every record holds exactly one country
    /// here, so the counts sum to the number of records the predicate selected.
    #[test]
    fn the_groups_of_a_predicate_sum_to_its_count(p in pred()) {
        let clause = p.sql();
        let sql = format!("SELECT country, count(*) FROM t WHERE {clause} GROUP BY country");
        let (set, _) = db().sql(&sql, &QueryOptions::default()).unwrap();
        let total: i128 = set
            .rows
            .iter()
            .map(|r| match r[1] {
                big_embed::Datum::Int(n) => n,
                ref other => panic!("a group counted {other:?}"),
            })
            .sum();
        prop_assert_eq!(total, i128::from(count(Some(&clause))), "\n  where: {}\n", clause);
    }
}
