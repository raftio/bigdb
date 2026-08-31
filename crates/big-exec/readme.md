# big-exec

Running a plan.

## The only crate that knows both halves

`big-db` never hears about a query language. `big-plan` never links a pager. Neither of them
should have to, and the cost of keeping it that way is one adapter — `CatalogSchema`, a newtype
over `&Catalog` that implements `big-plan`'s `Schema` trait.

It is a newtype here rather than an `impl` on either side because an `impl` would put the
dependency back: on the storage side it would make `big-db` depend on the query language, and on
the planner side it would make `big-plan` depend on the catalog. A third crate holding the
adapter is what lets both stay ignorant of each other.

## There is almost nothing to this crate, and that is the result

Execution is a fold:

- **Planning already rejected** every query that could fail for a reason a user needs explained
  — unknown field, wrong class, bad arity. Nothing here validates.
- **`Matches` already provides the algebra.** Union, intersection and difference of unmaterialised
  row sets are `big-db`'s, per shard, without naming a record. Nothing here implements set logic.

What is left is walking a `Plan` and calling the two of them. `query` is the whole entry point:
text, table, and a read handle, in; a `Value` out.

## `Value`

The one shape every query answers in — a count, an aggregate, a list of record ids, or groups of
those. It is defined here rather than in `big-plan` because it is what execution *produces*, and
a planner that named its own result type would have to be told about storage to fill it in.

## Read fan-out

`query` and `execute` take `P: Pager + Sync`, because a fragment scan spreads across threads
inside `big-db`. The bound is the whole of what this crate has to say about concurrency; the
scheduling is `DbRead`'s.
