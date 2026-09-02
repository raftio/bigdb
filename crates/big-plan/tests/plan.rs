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

//! Parsing and planning, with no database anywhere.
//!
//! The whole reason `big-plan` depends on no other crate: these run in milliseconds and a
//! failure here is never ambiguous about whether storage was involved.

use big_plan::*;
use proptest::prelude::*;
use std::collections::BTreeMap;

/// A schema that is a map, so a planning test states its world in one line.
struct Fake(BTreeMap<(&'static str, &'static str), FieldClass>);

impl Fake {
    fn tx() -> Self {
        Self(BTreeMap::from([
            (("tx", "amount"), FieldClass::Integer { scale: 0 }),
            (("tx", "price"), FieldClass::Integer { scale: 2 }),
            (("tx", "country"), FieldClass::Keyed(Keyed::Set)),
            (("tx", "active"), FieldClass::Boolean),
            (("tx", "rate"), FieldClass::Float { bits: 64 }),
            (("tx", "ratio"), FieldClass::Float { bits: 32 }),
            (("tx", "day"), FieldClass::Temporal { unit: TimeUnit::Days }),
            (("tx", "seen"), FieldClass::Temporal { unit: TimeUnit::Seconds }),
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

fn planned(text: &str) -> Result<Plan> {
    plan("tx", &parse(text)?, &Fake::tx())
}

#[test]
fn a_comparison_resolves_to_a_typed_predicate() {
    let Plan::Rows { rows: Rows::Compare { field, op, value }, .. } =
        planned("Row(amount > 500)").unwrap()
    else {
        panic!("expected a comparison")
    };
    assert_eq!((field.as_str(), op, value), ("amount", CmpOp::Gt, 500));
}

/// `=` is ambiguous in the grammar and is only resolved once the schema says what the field
/// is. The same syntax must land on three different plans.
#[test]
fn equals_resolves_by_field_class() {
    assert!(matches!(
        planned(r#"Row(country="GB")"#).unwrap(),
        Plan::Rows { rows: Rows::Key { .. }, .. }
    ));
    assert!(matches!(
        planned("Row(active=true)").unwrap(),
        Plan::Rows { rows: Rows::Bool { value: true, .. }, .. }
    ));
    assert!(matches!(
        planned("Row(amount=7)").unwrap(),
        Plan::Rows { rows: Rows::Compare { op: CmpOp::Eq, .. }, .. }
    ));
}

#[test]
fn nesting_survives_the_round_trip() {
    let p = planned(r#"Count(Intersect(Row(amount > 10), Not(Row(country="GB"))))"#).unwrap();
    let Plan::Count { rows: Rows::Intersect(parts), .. } = p else { panic!("expected a count") };
    assert_eq!(parts.len(), 2);
    assert!(matches!(parts[1], Rows::Not(_)));
}

#[test]
fn sum_names_its_field() {
    let Plan::Sum { field, .. } = planned(r#"Sum(All(), field="amount")"#).unwrap() else {
        panic!("expected a sum")
    };
    assert_eq!(field, "amount");
}

#[test]
fn queries_that_do_not_describe_anything_real_are_refused() {
    type Check = fn(&PlanError) -> bool;
    let cases: [(&str, Check); 6] = [
        ("Row(nope > 1)", |e| matches!(e, PlanError::UnknownField { .. })),
        ("Nonsense(All())", |e| matches!(e, PlanError::UnknownCall(_))),
        // An ordering on a keyed field has no meaning.
        (r#"Row(country > "GB")"#, |e| matches!(e, PlanError::OperatorNotAllowed { .. })),
        // A number where a key belongs.
        ("Row(country = 5)", |e| matches!(e, PlanError::OperatorNotAllowed { .. })),
        ("Difference(All())", |e| matches!(e, PlanError::Arity { .. })),
        // Counting a count is not a bitmap.
        ("Count(Count(All()))", |e| matches!(e, PlanError::UnknownCall(_))),
    ];
    for (text, want) in cases {
        let err = planned(text).expect_err(text);
        assert!(want(&err), "{text} gave {err:?}");
    }
}

#[test]
fn malformed_text_is_a_parse_error_not_a_plan_error() {
    assert!(matches!(parse("Row(amount > )"), Err(PlanError::Unexpected { .. })));
    assert!(matches!(parse(r#"Row(country="GB)"#), Err(PlanError::UnterminatedString { .. })));
    assert!(matches!(parse("All() extra"), Err(PlanError::TrailingInput { .. })));
    assert!(matches!(
        parse("Row(amount > 99999999999999999999999)"),
        Err(PlanError::NumberTooLarge { .. })
    ));
}

proptest! {
    /// Whatever the input, the parser answers rather than panicking. It is the first thing a
    /// network-facing surface will hand untrusted bytes to.
    #[test]
    fn parsing_never_panics(s in ".{0,60}") {
        let _ = parse(&s);
    }

    /// Any comparison a user can write against an integer field must plan, whatever the
    /// numbers, and must come back carrying exactly what was written.
    #[test]
    fn integer_comparisons_round_trip(v in any::<u64>(), which in 0usize..6) {
        let op = [">", ">=", "<", "<=", "==", "!="][which];
        let text = format!("Row(amount {op} {v})");
        let Plan::Rows { rows: Rows::Compare { value, .. }, .. } = planned(&text).unwrap() else {
            panic!("expected a comparison for {text}")
        };
        prop_assert_eq!(value, v);
    }
}

/// A decimal field stores an integer. What the user writes has to be rewritten into those
/// units, or `price > 5` on a two-place field silently means `> 0.05`.
#[test]
fn a_written_value_is_rewritten_into_the_units_the_field_stores() {
    let units = |text: &str| {
        let Plan::Rows { rows: Rows::Compare { value, .. }, .. } = planned(text).unwrap() else {
            panic!("expected a comparison for {text}")
        };
        value
    };

    assert_eq!(units("Row(price > 5)"), 500, "a whole number is still scaled");
    assert_eq!(units("Row(price > 5.25)"), 525);
    assert_eq!(units("Row(price > 5.2)"), 520, "fewer digits than the field holds is fine");
    // A plain integer field is unaffected.
    assert_eq!(units("Row(amount > 5)"), 5);

    // More precision than the field can hold is refused, not rounded away.
    assert!(matches!(
        planned("Row(price > 5.256)"),
        Err(PlanError::TooPrecise { written: 3, scale: 2, .. })
    ));
}

#[test]
fn a_point_that_does_not_start_a_fraction_is_not_part_of_the_number() {
    assert!(parse("Row(amount > 1)").is_ok());
    assert!(matches!(parse("Row(amount > 1.)"), Err(PlanError::Unexpected { .. })));
}

/// The parser is recursive descent, so nesting depth is call depth.
///
/// Before [`MAX_DEPTH`] existed, `Count(Union(Union(...` at ten thousand levels - seventy
/// kilobytes, well inside `big-http`'s eight megabyte body cap - overflowed the stack and
/// aborted the process. A stack overflow is not a panic: it cannot be caught, cannot be turned
/// into a `Result`, and on `bigd` takes every other in-flight request down with it. Any client
/// able to POST a query could do it.
///
/// So this is a denial-of-service regression test, not a taste in queries.
mod depth {
    use big_plan::{parse, PlanError};

    fn nested(depth: usize) -> String {
        format!("Count({}All(){})", "Union(".repeat(depth), ")".repeat(depth))
    }

    #[test]
    fn ordinary_nesting_still_parses() {
        // Nothing a person writes comes close to the limit, and the limit must not change that.
        assert!(parse(&nested(8)).is_ok());
        assert!(parse(&nested(100)).is_ok());
    }

    #[test]
    fn nesting_past_the_limit_is_refused_rather_than_fatal() {
        let err = parse(&nested(10_000)).unwrap_err();
        assert!(matches!(err, PlanError::TooDeep { .. }), "expected a depth refusal, got {err:?}");
        assert_eq!(err.code(), "query_too_deep");
    }

    /// Unbalanced, and never closed. It is a parse error either way - the point is that the
    /// error arrives instead of the process dying, and that the depth guard is what stops it
    /// rather than the input eventually running out.
    ///
    /// It has to be nested *calls*: a run of bare `(` fails at the first one, because `(` does
    /// not start an expression. That is why this is `Union(` repeated and not `(` repeated -
    /// the first version of this test used bare parens and was refused at depth two, proving
    /// nothing.
    #[test]
    fn an_unterminated_nest_returns_instead_of_overflowing() {
        let err = parse(&"Union(".repeat(50_000)).unwrap_err();
        assert!(matches!(err, PlanError::TooDeep { .. }), "got {err:?}");
    }

    /// Depth is per nest, not per query: a wide query is not a deep one, and capping the wrong
    /// one would refuse queries that are entirely reasonable.
    #[test]
    fn width_is_not_depth() {
        let args: Vec<String> = (0..2_000).map(|_| "All()".to_string()).collect();
        assert!(parse(&format!("Count(Union({}))", args.join(","))).is_ok());
    }
}

/// A float takes every numeric literal, including the negative fractional one no other class
/// does, and the threshold reaches the plan unrounded: rounding it needs the field's width and
/// the operator together, and only the storage layer has the first.
#[test]
fn a_float_comparison_carries_the_written_threshold() {
    let Ok(Plan::Rows { rows: Rows::CompareFloat { field, op, bits }, .. }) =
        planned("Row(rate > -12.50)")
    else {
        panic!("expected a float comparison");
    };
    assert_eq!(field, "rate");
    assert_eq!(op, CmpOp::Gt);
    assert_eq!(f64::from_bits(bits), -12.5);

    // An integer literal is a perfectly good float bound.
    assert!(matches!(
        planned("Row(ratio <= 5)"),
        Ok(Plan::Rows { rows: Rows::CompareFloat { .. }, .. })
    ));
}

/// The refusal the lexer used to make, now made where the field is known.
///
/// `-12.50` is a legal literal - a float field holds it - so the lexer cannot refuse it any
/// more. Against a decimal field there is still nowhere for it to go, and this is the arm that
/// can tell.
#[test]
fn a_negative_decimal_is_refused_by_the_field_rather_than_by_the_lexer() {
    assert!(matches!(planned("Row(price > -12.50)"), Err(PlanError::NegativeDecimal { .. })));
    // The same literal against a float is simply a bound.
    assert!(planned("Row(rate > -12.50)").is_ok());
}

/// A date is compared against a written date and comes out as the signed comparison it is: the
/// stored value is a count from the epoch, so nothing below the planner needs a temporal case.
#[test]
fn a_date_comparison_becomes_a_signed_one() {
    let Ok(Plan::Rows { rows: Rows::CompareSigned { field, op, value }, .. }) =
        planned("Row(day >= \"2024-01-15\")")
    else {
        panic!("expected a signed comparison");
    };
    assert_eq!((field.as_str(), op, value), ("day", CmpOp::Ge, 19_737));

    // Seconds, not days, for a timestamp - the unit is the whole of what tells them apart.
    let Ok(Plan::Rows { rows: Rows::CompareSigned { value, .. }, .. }) =
        planned("Row(seen >= \"2024-01-15 10:30:00\")")
    else {
        panic!("expected a signed comparison");
    };
    assert_eq!(value, 1_705_314_600);
}

/// A string that is not a date the field can hold is named as such, rather than being read as a
/// key or rounded into one.
#[test]
fn a_date_that_is_not_one_is_refused_by_name() {
    for text in [
        "Row(day >= \"2024-02-30\")",
        "Row(day >= \"yesterday\")",
        // A time of day against a `DATE` is refused rather than truncated to midnight: dropping
        // it would silently answer about a different instant from the one that was written.
        "Row(day >= \"2024-01-15 10:30:00\")",
    ] {
        assert!(matches!(planned(text), Err(PlanError::BadDate { .. })), "{text}");
    }
    // A number is not a date either, and says so as an operator refusal rather than a bad one.
    assert!(matches!(planned("Row(day >= 5)"), Err(PlanError::OperatorNotAllowed { .. })));
}

/// `min` and `max` are questions about an ordering, which a date has. `sum` is a question about
/// addition, which it does not.
#[test]
fn a_date_orders_but_does_not_add() {
    assert!(planned("Min(All(), field=\"day\")").is_ok());
    assert!(planned("Max(All(), field=\"seen\")").is_ok());
    assert!(matches!(
        planned("Sum(All(), field=\"day\")"),
        Err(PlanError::OperatorNotAllowed { .. })
    ));
}

/// The two aggregate gates agree. They did not: a bare `Sum` over a signed field was answered
/// and the same call inside a `GroupBy` was refused, because the two checks listed different
/// classes. Both go through `aggregable` now, so this holds for every class at once.
#[test]
fn an_aggregate_is_allowed_the_same_way_bare_and_grouped() {
    for field in ["amount", "price", "rate"] {
        let bare = planned(&format!("Sum(All(), field=\"{field}\")"));
        let grouped = planned(&format!(
            "GroupBy(All(), field=\"country\", aggregate=Sum(field=\"{field}\"))"
        ));
        assert_eq!(bare.is_ok(), grouped.is_ok(), "{field} disagreed");
        assert!(bare.is_ok(), "{field} should aggregate");
    }
    // And a class that aggregates neither way still refuses both.
    assert!(planned("Sum(All(), field=\"country\")").is_err());
    assert!(planned("GroupBy(All(), field=\"country\", aggregate=Sum(field=\"country\"))").is_err());
}
