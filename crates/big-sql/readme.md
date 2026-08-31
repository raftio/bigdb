# big-sql

SQL for a bitmap engine, which is to say: **a translation into PQL, not a second query engine.**

`translate` turns one `SELECT` into [`big_plan::ast::Call`]s — the same AST `big-plan`'s own
parser produces — plus a [`Shape`] saying how their answers become columns and rows. The planner
then resolves each call exactly as it resolves hand-written PQL: same schema lookup, same type
rules, same error codes, same `Plan`.

That is the design, and it is not a discipline anyone has to keep: it is the type of
[`Statement::calls`]. This crate cannot express anything PQL cannot, because the only thing it
can produce is PQL. There is no path by which a SQL statement reaches storage having promised
something the engine does not do.

Nothing here consults a schema, so every test in this crate runs at the speed of a parser test:
no file, no mapping, no pager. Whether `amount` exists is the planner's question, asked one layer
up — which is what keeps *this is not a statement*, *this engine does not answer that* and
*there is no such field* from arriving as the same error.

## A statement is calls, probes, and a shape

| | |
|---|---|
| `calls` | The PQL the statement means. One usually; several when the select list asks several questions of the same records. Each is planned, fanned out and merged **as if it had been written alone**, so the length of that list is what the statement costs in round trips — which is why it is capped at [`MAX_CALLS`] (16, counting `avg` as two). |
| `probes` | The searches. A quantile is the value at a rank and no plan answers that, so a bound moves until an ordinary `Count` lands on the rank. A different shape of work from a call — asked until it converges rather than once — so it is a different list. |
| `answer` | The [`Shape`], the `FORMAT`, and where in a caller's collected answers the probes' begin. |

## What translates

```sql
SELECT count(*)           FROM t WHERE amount >= 786432   -- Count(Row(amount >= 786432))
SELECT sum(amount)        FROM t WHERE amount >= 786432   -- Sum(Row(...), field=amount)
SELECT category, count(*) FROM t GROUP BY category        -- GroupBy(All(), field=category)
SELECT category, sum(amount) FROM t GROUP BY category     -- GroupBy(All(), field=category,
                                                          --         aggregate=Sum(field=amount))
SELECT category, count(*) AS n FROM t
  GROUP BY category ORDER BY n DESC LIMIT 8               -- TopN(All(), field=category, n=8)
SELECT count(DISTINCT category) FROM t WHERE amount >= 1  -- Distinct(Row(...), field=category)
SELECT DISTINCT category  FROM t                          -- Distinct(All(), field=category)
SELECT amount, price      FROM t WHERE country = 'GB'
  LIMIT 10                                                -- Project(Row(country="GB"),
                                                          --   field=amount, field=price, n=10)
SELECT *                  FROM t WHERE active = true      -- Row(active=true), rendered as ids
SELECT count(*)           FROM t
  WHERE visit = 'home' AND visit BETWEEN 100 AND 200      -- Count(Row(visit="home",
                                                          --           from=100, to=200))
```

`SELECT DISTINCT c` is not a special case: standard SQL defines it as `SELECT c GROUP BY c`, the
parser normalises it into exactly that, and the plan is the one a `GROUP BY` already produced.

`SELECT *` answers with record ids under a column called `id`, and nothing else — this engine
stores facts as bits at `(row, record)` and has no row of values to hand back. Naming the columns
instead is a projection, which reconstructs a value per record per column at a point read
apiece; the cut is therefore part of the plan rather than a view of its answer, and a `LIMIT`
between 1 and [`MAX_PROJECTION`] is required.

A window is a key and its bounds against the same time quantum column, fused out of the
conjunction into one `Row`. A field with no views by time has no window to answer, and the
refusal for that is the *planner's* — `OperatorNotAllowed`, against the schema, the same one PQL
gets for the same hole. It used to answer with the empty set instead, indistinguishable from a
window that genuinely matched nothing.

## What the shape keeps, and why it is not in the plan

A plan says what to compute; a shape says what the caller asked to see of it. The split falls
here rather than one layer down because `big-cluster` merges *by the plan*: a plan variant it has
not been taught is a distributed answer that is wrong rather than absent. A shape is applied to
the merged answer, at the coordinator, once — which is the only place several of these are right.

| Clause | Why the coordinator |
|---|---|
| `HAVING` | A group under the threshold on one node can be over it once the rest have contributed. It runs **before** any `LIMIT`, which is what SQL specifies — and is why a ranking under a `HAVING` gives its cut back to the shape: `TopN(n=3)` would pick three groups before the predicate dropped any. |
| `ORDER BY` | Absent when the answer already arrives ordered — ascending by key, or the descending ranking a `TopN` carries itself. Present only when the coordinator has to sort, which is also when every group is materialised before the cut. |
| `OFFSET`, `LIMIT`, `WITH TIES` | One decision applied in one order: skip, take, then keep whatever ties with the last taken. A `TopN` keeps its own cut only when nothing after the merge can still change which rows survive. |
| `count(DISTINCT x)` | The plan is a `Distinct`; this is the counting step, after every node has contributed to the groups. |
| `avg(x)` | `sum(x) / count(*)`, which is two plans and a division. Doing the division here is doing it where both halves are whole. |
| `FILTER (WHERE …)` | Leaves the statement's plans describing different groups, so a cell says what it holds for a group its plan said nothing about: `0` for a count or a sum, absent for an extreme. |
| `UNION ALL` | Nothing here is a set operation over records. Each branch is a whole statement with its own plans, stacked as rendered rows — which is why it needed no engine change. |
| `GROUP BY a, b` | **Not a composite key**, which this index never stored: for each value of the left column the records holding it are a set, and grouping *those* by the right is the ordinary grouping. One pass per value of the left, which is the bound the plan carries. |
| `JOIN` | See below. |

## The join

`FROM a JOIN b ON a.k = b.k`, one equality between two keyed columns. What is joined is the key's
*string*, after both sides have merged: a record is a set of bits in one table with no pointer to
another, and the row ids behind a key are per `(table, field)` and have no reason to agree.

For each shared key the join is the Cartesian product of the records holding it on each side, so
`count(*)` is `|A_s| · |B_s|` summed over shared keys, `sum(a.x)` is `sum_A(x) · |B_s|` summed the
same way, and a `min` is the smallest of one side's minima over the keys the other holds at all —
repeating a value does not make it smaller. Every one of those is arithmetic over per-key numbers
an ordinary grouping already produces, which is why a join adds no `Plan` variant and no merge
arm. The multiplication happens after both sides have merged; a test across two nodes answers 6
where multiplying at each owner would answer 3.

## Aggregates, and how an answer is written out

An aggregate can carry a condition of its own, in either of two spellings the parser folds into
one representation so they cannot drift: `count(*) FILTER (WHERE amount >= 500)` and
`countIf(amount >= 500)` are the same clause, as are `sumIf` · `minIf` · `maxIf` · `avgIf`
against their `FILTER` forms. The condition narrows that select-list entry's own row set and
nothing else, which is what lets one statement ask several differently-filtered questions of the
same records.

`uniq(x)` — with `uniqExact`, `uniqCombined`, `uniqCombined64`, `uniqHLL12` and `uniqTheta`
accepted as aliases for it — is `count(DISTINCT x)`. `topK(n)(x)` is the ranking a `TopN` already
carries, rendered as a list in one cell rather than one row per key; without `n` it ranks ten.
Both are **exact**, because the plans behind them are a grouping and a ranking rather than a
sketch, and there is nothing an approximation would buy.

`median(x)` and `quantile(p)(x)` are exact as well, and are the one thing on this surface that is
a search rather than a question — the `probes` above. The level takes at most three digits after
the point; `quantileExact` is accepted as another spelling, since every quantile here is exact.

`PREWHERE` selects exactly the set `WHERE` selects, and is folded into it: there is no row to
read early, because a record is a set of bits rather than a tuple of values. `WITH <constant> AS
<name>` binds a value the statement then uses by name. `LIMIT n WITH TIES` keeps every further
row whose ordering value the cut cannot tell apart from the last one kept.

`FORMAT` changes the bytes and the `Content-Type` and nothing about the answer — `JSON`,
`JSONCompact`, `TSV`, `TabSeparated`, `TSVWithNames`, `TabSeparatedWithNames`, `CSV`,
`CSVWithNames`. `JSON` and `JSONCompact` produce the same bytes here; the second is an accepted
spelling rather than a second encoding.

## What it refuses

Every one of these is refused by name, with a stable code, before anything runs. The list is the
feature: a surface that accepted the syntax and answered something else would be worse than one
that has no SQL at all.

**Joins past the one that is exact** — a comma between tables or a third table
(`sql_no_joins`), an outer or cross join (`sql_no_outer_joins`), `USING` or a condition that is
not one equality (`sql_join_condition`), a column that does not say which table it belongs to
(`sql_ambiguous_column`), a `WHERE` term mixing both tables under `OR` or `NOT`
(`sql_join_filter`), and `avg` or a select of joined values over a join — a pair of records has
no identity this engine stores.

**Everything else that has no set operation behind it** — subqueries, CTEs, `INTERSECT`,
`EXCEPT`, window functions, arithmetic or a function call in the select list, `LIKE`, `IS NULL`
and `= NULL`, and every write. A plain `UNION` and `UNION DISTINCT` have their own code
(`sql_union`): removing duplicate rows here would mean comparing rendered strings.

**Clauses answered in one shape and not another** — a `HAVING` naming an aggregate the answer
does not carry or with no `GROUP BY`, an `ORDER BY` naming a second aggregate or a column that
is not the grouped one, an `OFFSET` on anything but a list of groups (a record listing is paged
with the `after` cursor, which does not shift under inserts), a projection without its cut or of
a keyed column, `DISTINCT` over more columns than a grouping holds, a select list asking for
more than [`MAX_CALLS`] plans, a `FORMAT` nobody here writes, and a quantile level finer than
three digits.

See [`Refused`] for the code and the sentence each one produces — that enum *is* the list, so it
cannot drift from what the crate does.
