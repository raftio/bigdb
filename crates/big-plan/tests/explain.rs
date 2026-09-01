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

//! The plan printer, which the test corpora are written against.
//!
//! A printer that a few hundred expected outputs depend on has to be checked at the level those
//! outputs are read at - the exact lines - and it has to be checked for the one property that
//! makes it usable as an oracle at all: **two plans that differ print differently**. The last
//! test here is the one that carries that claim, and it is the reason the signed marker exists.

use std::collections::BTreeMap;

use big_plan::{explain, parse, plan, FieldClass, Keyed, Plan, Rows, Schema};

struct Fake(BTreeMap<(&'static str, &'static str), FieldClass>);

impl Fake {
    fn tx() -> Self {
        Self(BTreeMap::from([
            (("tx", "amount"), FieldClass::Integer { scale: 0 }),
            (("tx", "price"), FieldClass::Integer { scale: 2 }),
            (("tx", "balance"), FieldClass::Signed),
            (("tx", "country"), FieldClass::Keyed(Keyed::Set)),
            (("tx", "city"), FieldClass::Keyed(Keyed::Set)),
            (("tx", "visit"), FieldClass::Keyed(Keyed::Time)),
            (("tx", "active"), FieldClass::Boolean),
        ]))
    }
}

impl Schema for Fake {
    fn has_table(&self, table: &str) -> bool {
        self.0.keys().any(|(t, _)| *t == table)
    }
    fn field_class(&self, table: &str, field: &str) -> Option<FieldClass> {
        self.0.iter().find(|((t, f), _)| *t == table && *f == field).map(|(_, c)| *c)
    }
    fn fields(&self, table: &str) -> Vec<String> {
        self.0.keys().filter(|(t, _)| *t == table).map(|(_, f)| (*f).to_string()).collect()
    }
}

/// The lines a query prints, with the indentation of the literal in the test stripped.
fn lines(text: &str) -> String {
    let call = parse(text).unwrap_or_else(|e| panic!("`{text}` did not parse: {e}"));
    explain(&plan("tx", &call, &Fake::tx()).unwrap_or_else(|e| panic!("`{text}`: {e}")))
}

/// Asserts the printed tree, written in the test the way it prints.
fn prints(text: &str, want: &str) {
    let want = want.trim_matches('\n').trim_end();
    assert_eq!(lines(text), want, "\n  query: {text}\n");
}

#[test]
fn a_bare_aggregate_is_a_head_line_and_its_bitmap() {
    prints(
        "Count(Row(amount >= 500))",
        "\
Count tx
└── amount >= 500",
    );
    prints(
        "Sum(All(), field=amount)",
        "\
Sum tx.amount
└── all",
    );
}

#[test]
fn a_set_operation_nests_its_parts_under_it() {
    prints(
        "Count(Intersect(Row(country=\"GB\"), Row(active=true)))",
        "\
Count tx
└── intersect
    ├── country = 'GB'
    └── active = true",
    );
    prints(
        "Count(Union(Row(country=\"GB\"), Difference(All(), Row(active=true))))",
        "\
Count tx
└── union
    ├── country = 'GB'
    └── minus
        ├── all
        └── active = true",
    );
    prints(
        "Count(Not(Row(country=\"GB\")))",
        "\
Count tx
└── not
    └── country = 'GB'",
    );
}

/// The elbows have to keep saying which subtree a line belongs to however deep it goes, which
/// is what the carried `│` is for.
#[test]
fn a_nested_operation_carries_its_parents_bar() {
    prints(
        "Count(Intersect(Union(Row(country=\"GB\"), Row(city=\"LDN\")), Row(active=true)))",
        "\
Count tx
└── intersect
    ├── union
    │   ├── country = 'GB'
    │   └── city = 'LDN'
    └── active = true",
    );
}

/// The one place the printer folds: a grouping's aggregate is always built over a placeholder
/// bitmap, so it says what it computes on the head line rather than spending three lines saying
/// `all` in the middle.
#[test]
fn a_grouping_folds_its_aggregate_onto_the_head_line() {
    prints(
        "GroupBy(All(), field=country)",
        "\
GroupBy tx.country -> count
└── all",
    );
    prints(
        "GroupBy(Row(active=true), field=country, aggregate=Sum(field=amount))",
        "\
GroupBy tx.country -> sum(amount)
└── active = true",
    );
    prints(
        "GroupByPair(All(), left=country, right=city, n=100, aggregate=Max(field=amount))",
        "\
GroupByPair tx.(country, city) left_max=100 -> max(amount)
└── all",
    );
}

/// The fold is a match that can fail rather than an assumption, so a plan the planner does not
/// currently build still prints - in full, as a child.
#[test]
fn an_aggregate_the_fold_declines_prints_as_a_child() {
    let printed = explain(&Plan::GroupBy {
        table: "tx".to_string(),
        rows: Rows::All,
        field: "country".to_string(),
        // Not the placeholder: a bitmap the planner never puts here today.
        aggregate: Box::new(Plan::Sum {
            table: "tx".to_string(),
            rows: Rows::Bool { field: "active".to_string(), value: true },
            field: "amount".to_string(),
        }),
    });
    assert_eq!(
        printed,
        "\
GroupBy tx.country
├── all
└── Sum tx.amount
    └── active = true"
    );
}

/// The one number that is not a number: a ranking the planner was given no cut for is built
/// whole, and `usize::MAX` is how it says so.
#[test]
fn an_uncut_ranking_says_so_rather_than_printing_a_sentinel() {
    let printed = explain(&Plan::TopN {
        table: "tx".to_string(),
        rows: Rows::All,
        field: "country".to_string(),
        n: usize::MAX,
    });
    assert_eq!(
        printed,
        "\
TopN tx.country n=unbounded
└── all"
    );
}

#[test]
fn the_plans_that_carry_a_number_print_it() {
    prints(
        "TopN(All(), field=country, n=2)",
        "\
TopN tx.country n=2
└── all",
    );
    prints(
        "Project(All(), field=amount, field=price, n=10)",
        "\
Project tx.(amount, price) limit=10
└── all",
    );
    prints(
        "Distinct(All(), field=country)",
        "\
Distinct tx.country
└── all",
    );
}

/// An absent bound prints as absent. Standing in a sentinel would read as a date somebody
/// chose, and the two are different plans.
#[test]
fn a_time_window_prints_its_bounds_and_the_ones_it_does_not_have() {
    prints(
        "Count(Row(visit=\"login\", from=1000, to=2000))",
        "\
Count tx
└── visit = 'login' in [1000, 2000]",
    );
    prints(
        "Count(Row(visit=\"login\", from=1000))",
        "\
Count tx
└── visit = 'login' in [1000, ..]",
    );
    prints(
        "Count(Row(visit=\"login\", to=2000))",
        "\
Count tx
└── visit = 'login' in [.., 2000]",
    );
}

/// **The property the corpora rest on.** A printer that maps two plans onto one string is a
/// printer that lets a bug through with the test still green, so the pairs here are the ones
/// that come closest to colliding: a signed bound against an unsigned one, and a scale carried
/// by the field rather than written in the query.
#[test]
fn plans_that_differ_print_differently() {
    let pairs = [
        // The bound is an `i64` on one side and a `u64` on the other, and both are positive.
        ("Count(Row(amount >= 500))", "Count(Row(balance >= 500))"),
        // `price` has scale 2, so the same written number is a different bound.
        ("Count(Row(price >= 5))", "Count(Row(amount >= 5))"),
        ("Count(Intersect(Row(active=true)))", "Count(Union(Row(active=true)))"),
        ("Count(Difference(All(), Row(active=true)))", "Count(Not(Row(active=true)))"),
        ("GroupBy(All(), field=country)", "Distinct(All(), field=country)"),
        ("TopN(All(), field=country, n=2)", "TopN(All(), field=country, n=3)"),
        ("Count(Row(visit=\"a\", from=1))", "Count(Row(visit=\"a\", to=1))"),
    ];
    for (a, b) in pairs {
        assert_ne!(lines(a), lines(b), "\n  these two print the same:\n  {a}\n  {b}\n");
    }
}

/// The signed marker, which the pair above needs and nothing else would have asked for.
#[test]
fn a_signed_bound_says_so() {
    prints(
        "Count(Row(balance >= 500))",
        "\
Count tx
└── balance >= 500 [signed]",
    );
}
