# big-plan

Query text in, a checked plan out. No storage anywhere.

## The dependency points the other way, and that is the design

**This crate depends on no other `big` crate.** What a planner needs to know about a database is
narrow — does this field exist, and what class of thing is it — so that knowledge arrives
through the `Schema` trait, which the *storage* side implements (`big-exec::CatalogSchema`).

The payoff is concrete: parser and planner tests run at unit-test speed with no file, no
mapping, and no pager. A query language is the part of a database most likely to churn, and this
is what keeps churning it from costing a rebuild of everything underneath.

## The three stages

1. **`parse`** — text to `ast::Call`. Recursive descent, no dependencies, no lexer generator.
2. **`plan`** — the AST resolved against a `Schema`: names become ids, and a comparison against
   a field acquires the field's class and width.
3. The result is a `Plan` that the executor can run without any further checking. **Everything
   that could fail for a reason a user needs explained has already failed by this point**, which
   is why `big-exec` is a hundred lines: execution is a fold, not a validator.

## PQL

```
Count(All())
Count(Row(country="GB"))
Intersect(Row(country="GB"), Row(amount > 1000))
Sum(Row(country="GB"), field=amount)
TopN(All(), field=country, n=10)
```

Function calls all the way down — `All`, `Row`, `Union`, `Intersect`, `Difference`, `Not`,
`Count`, `Sum`, `Min`, `Max`, `Distinct`, `TopN`, `GroupBy` — so adding an operator is a match
arm rather than precedence table surgery. A comparison is an argument to `Row`, not an operator
of its own: `Row(amount > 1000)`, never `Range(...)`.

## Untrusted input

`parse` takes a `&str` that arrives over HTTP. It is one of the three entry points in this
engine that a stranger can reach directly, and it is fuzzed from the workspace `fuzz/` crate —
`cargo +nightly fuzz run parse_pql`.
