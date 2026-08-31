# SQL: what was built, and the decision it reverses

`checklist.md` used to say SQL was **refused**, with a reason: *"a bitmap engine's shape is not a
relational one: there are no joins here and a SQL surface would promise them."* That reason is
still true, and this document does not argue it away. It records the one shape of SQL that
survives it, and what building that shape changed about the plan.

**The claim.** A SQL surface is honest here if every relational thing the engine cannot do is a
*named refusal at parse time* rather than a gap the user discovers by getting a wrong answer.
The refusal list is not the cost of the feature. It is the feature.

**What it buys, concretely.** The benchmark's largest caveat was *"`big` is asked in its own
query language and every rival in SQL, so each plans its own question."* The analytical harness
now runs a `big-sql` column beside the `big` one — same file, same corpus, same plan underneath,
different language at the door — so what a query language costs is a measurement rather than an
assumption.

---

## The shape

SQL is a **translation**, not a second engine.

```
   big-sql   text ──lex/parse──► Select ──lower──► Statement { table, call: Call, shape: Shape }
                                                                        │              │
   big-plan  ◄────── the same AST its own parser produces ──────────────┘              │
             plan(table, call, schema) ─► Plan ─► big-exec ─► big-cluster (unchanged)   │
                                                                                       │
   big-http  POST /sql ─► {"columns": [...], "rows": [[...]]}, rendered by ─────────────┘
```

`big-sql` depends on `big-plan` and nothing else — no pager, no `big-db` — so every parser and
lowering test runs with no file, no mapping and no pager. `translate` needs no schema at all:
whether `amount` exists is the planner's question, asked one layer up, which keeps "this is not
a statement", "this engine does not answer that" and "there is no such field" from arriving as
the same error.

`Shape` is where everything the plan does not carry lives — which columns, in which order, cut
to what length, and the counting step of `count(DISTINCT x)`. It is applied **after** the merge,
at the coordinator, because that is the only place those answers are right: two nodes holding
the same group hold one group, and counting earlier counts it twice.

## Three things the build changed

Written down rather than quietly folded in, because two of them are the plan being wrong and one
is the plan being right for a reason it had not stated.

**1. The lowering target is PQL, not `Plan`.** The plan said to lower straight to a `Plan`
against the `Schema` trait. Emitting the query language's own AST instead is strictly better,
and not for tidiness: it makes [Rule 1](#rule-1--the-surface-never-outgrows-the-engine-quietly)
*structural*. What a statement is permitted to say is bounded by what the planner will resolve,
which is now the **type** of the translation's output rather than a discipline someone has to
keep. It also deleted a second copy of three things that would have drifted — turning `12.50`
into the units a decimal field stores, deciding which operators a field class takes, and the
error codes for getting either wrong.

**2. Projecting stored values is refused, not merely limited.** The plan allowed
`SELECT amount FROM t LIMIT 100` at a documented cost. It cannot be built without breaking
[Rule 2](#rule-2--v1-adds-no-plan-variant): the records live on the node that owns them, so
materialising values needs a per-node step, and that is a fan-out shape the merge does not have.
Building it for the un-clustered case only would be exactly the "second path for the un-clustered
case would be the path nobody tests" that `architecture.md` rejects by name. So `SELECT *`
answers with record ids under a column called `id`, and a bare column is
`sql_projection_unsupported` — with a sentence saying to count it, aggregate it, or group by it.

> **This decision was reversed.** See [What v2 added](#what-v2-added). The reasoning above was
> correct about the cost and correct about Rule 2; what it got wrong was treating "this needs a
> merge arm" as a reason not to build it rather than as the price of building it. The arm was
> written and tested across two nodes, which is what Rule 2 asks for — it forbids a plan variant
> the merge has *not* been taught, not every plan variant.

**3. There is no `big sql` subcommand.** The plan listed one. `big` is the offline half of
operating a database — back it up, check it, shrink it — and has no query subcommand for PQL
either; adding one only for SQL would make SQL the privileged surface. It would also mean
`big-db` depending on crates that depend on `big-db`. Querying goes through `bigd`, in both
languages.

A fourth, smaller: **the time-quantum window is refused** (`sql_no_time_window`). PQL answers it
with `Row(visit="home", from=…, to=…)`, and SQL has no natural spelling for a window that
attaches to one keyed predicate. Inventing a pseudo-column would have been magic; the refusal
points at the PQL that works.

## The mapping

| SQL | lowers to |
|---|---|
| `SELECT count(*) FROM t WHERE …` | `Count(<rows>)` |
| `SELECT sum(f) FROM t WHERE …` | `Sum(<rows>, field=f)`, and the same for `min`, `max` |
| `SELECT f, count(*) FROM t GROUP BY f` | `GroupBy(<rows>, field=f)` |
| `SELECT f, sum(g) FROM t GROUP BY f` | `GroupBy(<rows>, field=f, aggregate=Sum(field=g))` |
| `SELECT f FROM t GROUP BY f` | `Distinct(<rows>, field=f)`, rendering only the key column |
| `… GROUP BY f ORDER BY count(*) DESC LIMIT n` | `TopN(<rows>, field=f, n=n)` |
| `SELECT count(DISTINCT f) FROM t WHERE …` | `Distinct(<rows>, field=f)` + `Shape::CountGroups` |
| `SELECT * FROM t WHERE … [LIMIT n]` | `<rows>`, rendered as record ids |
| `SELECT a, b FROM t WHERE … LIMIT n` | `Project(<rows>, field=a, field=b, n=n)` |
| `count(*) FROM a JOIN b ON a.k = b.k` | `Distinct(<a rows>, field=k)` and `Distinct(<b rows>, field=k)`, multiplied per key by the shape |
| `sum(a.x)` over that join | the left side becomes `GroupBy(…, aggregate=Sum(field=x))`; the right stays its count |
| `min(a.x)` over that join | the same, and the right side only says which keys are in the join |
| `SELECT avg(f) FROM t WHERE …` | `Sum(<rows>, field=f)` and `Count(<rows>)`, divided by the shape |
| `SELECT count(*), sum(f), max(f) FROM t` | three calls, one per aggregate, assembled into one row |
| `<agg> FILTER (WHERE c)` | that entry's call over `Intersect(<rows>, c)` |
| `… GROUP BY f ORDER BY sum(g) DESC` | `GroupBy(…, aggregate=Sum(field=g))`, sorted by the shape |
| `… GROUP BY f HAVING sum(g) >= 100.00` | the same plan; the threshold is converted to stored units and applied by the shape |
| `… GROUP BY f LIMIT n OFFSET m` | the ranking is asked for `n + m` and the shape takes the window |

`WHERE` is a structural lowering onto the set operations:

| SQL | becomes |
|---|---|
| `a AND b` | `Intersect(a, b)`, flattened — a nested tree would defeat the executor's short circuit |
| `a OR b` | `Union(a, b)`, flattened |
| `NOT a` | `Not(a)` |
| `a AND NOT b` | `Difference(a, b)` — `Not` builds the table's exists row to complement against and `Difference` does not |
| `a AND NOT b AND NOT c` | `Difference(a, Union(b, c))` |
| `f > 5`, `f = 'GB'`, `f = TRUE`, `price > 12.50` | `Row(f > 5)` and so on; the planner resolves the class and the scale |
| `f IN (x, y)` | `Union(Row(f=x), Row(f=y))`; one value stays the comparison it was |
| `f BETWEEN a AND b` | `Intersect(Row(f >= a), Row(f <= b))` |
| no `WHERE` | `All()` |
| `ORDER BY <the grouped column> [ASC]` | nothing — groups already come back in that order |

## What is refused, and what it says

Each is a parse- or lowering-time error with a stable code, and a sentence naming what exists
instead. A user who writes a join is told the engine has none, not handed a syntax error at
byte 41.

| Refused | Code |
|---|---|
| `JOIN` in any form, and a comma between tables | `sql_no_joins` |
| Subqueries, CTEs, `UNION`/`INTERSECT`/`EXCEPT` between selects | `sql_unsupported` |
| Window functions, `OVER` | `sql_unsupported` |
| `OFFSET` on anything but a list of groups | `sql_unsupported` |
| Arithmetic or an unknown function in the select list | `sql_unsupported` |
| `DISTINCT` over more than one column | `sql_unsupported` |
| `count(<column>)`, a column that is neither grouped nor aggregated, a star beside an aggregate | `sql_unsupported` |
| A `HAVING` naming an aggregate the select list did not ask for, or naming an `avg` | `sql_unsupported` |
| `LIKE` and any predicate with no set operation behind it | `sql_unsupported` |
| More than sixteen distinct plans in one select list | `sql_too_many_aggregates` |
| `ORDER BY` naming a number the answer does not hold, or any ordering of a record listing or a projection | `sql_unsupported_order` |
| `IS NULL`, `IS NOT NULL`, `= NULL` | `sql_no_nulls` |
| Projecting a stored column without a `LIMIT` of 1 to 10000 | `sql_projection_unsupported` |
| Projecting a keyed column | `operator_not_allowed`, from the planner |
| A window over a time quantum field | `sql_no_time_window` |
| `INSERT`, `UPDATE`, `DELETE`, and every DDL | `sql_read_only` |

Statuses: a refusal is `400` — not `501`, which would invite a client to retry after an upgrade
that is never coming. A missing table is still `404` and a missing field still `422`, the same
answers PQL gets, because they are the same mistakes.

## Two rules that hold the design together

### Rule 1 — the surface never outgrows the engine quietly

Every accepted statement lowers to a call the planner resolves. Every rejected one names a code
from the table above. There is no third outcome, and after change 1 above this is enforced by
the type of the translation's output rather than by care.

### Rule 2 — v1 adds no `Plan` variant

`big-cluster::merge` is directed by the plan, so a variant it has not been taught is a
distributed answer that is *wrong* rather than absent. Anything SQL wants that `Plan` lacks goes
into `Shape` and is applied after the merge, or it is refused. **The rule held**: `big-cluster`
gained one method, which plans locally and calls the existing `execute`. No fan-out code, no
merge arm, no wire change.

## What shipped, by phase

| | |
|---|---|
| **0** Decision written down first | `checklist.md`, `architecture.md` (a seventh decision, and `big-sql` in the diagram) |
| **1** `crates/big-sql`: lexer and parser | Hand-written, `MAX_DEPTH` borrowed from `big-plan` for the same stack reason |
| **2** Lowering to a `Call` and a `Shape` | Schema-free, so it tests at parser speed |
| **3** `Api::sql`, `Api::plan_sql`, `Cluster::sql` | Plan once, fan the plan out, merge as before |
| **4** `POST /sql` | Twelfth public route, role `read`, result-set JSON, `status.rs` mapping |
| **5** Docs and the benchmark | `README`, `runbook`, `CHANGELOG`, `versioning`, and a `big-sql` column in the analytical report |
| **6** Writes | Deliberately not built. `INSERT` would be a second, slower way to do what `/import` exists for |

### The tests that carry the claim

`crates/big-sql/tests/translate.rs` asks the same question in both languages and asserts the
**plans are equal** — not that SQL produced *some* plan. That is what makes "SQL is a
translation" checkable rather than descriptive. It compares plans rather than ASTs because the
two surfaces spell an equality differently and the planner resolves both to the same thing,
which is the level the claim is made at. `crates/big-api/tests/sql.rs` does the same against a
real database, comparing answers; `crates/big-http/tests/sql.rs` covers the route, the result-set
shape, and the status of every kind of refusal.

**The clustered path is tested, not measured.** Three tests in
`crates/big-http/tests/cluster.rs` run SQL over two real nodes on loopback. The one that earns
its place is `sql_is_answered_across_nodes_and_merged`: `count(DISTINCT country)` answers **3**,
and it would answer 4 if the counting happened at each owner and were summed, because both nodes
hold some of `GB`. That is the whole argument for putting the count in a `Shape` applied after
the merge rather than in a `Plan` variant, and it is now a failing test rather than a paragraph.
The other two cover a ranking cut after the merge instead of at the owners, and a refusal that
never reaches a peer because planning is pure.

The benchmark, by contrast, is **single-node throughout** and says nothing about any of this;
see [`bench/readme.md`](../bench/readme.md).

## Risks, in the order they are likely to bite

1. **The surface promises more than the engine does.** Rule 1, now structural, plus one assertion
   per refusal in `every_refusal_names_itself`. A construct that stops being refused breaks a
   test rather than starting to be answered.
2. **`Plan` grows to fit SQL and the cluster merge falls behind.** Rule 2. It held for v1; the
   next thing SQL wants is where it will be tested.
3. **Parser recursion.** Same argument and same limit as `big_plan::parse::MAX_DEPTH`, with a
   test that 300 levels of parentheses are refused rather than fatal. A fuzz target mirroring
   `parse_page` is the remaining gap.
4. **Scope creep to joins.** The refusal that will be asked for most and the one that cannot be
   granted without a different engine. `checklist.md` keeps **Joins: missing** as its own line
   for exactly this reason.

---

## What v2 added

The first version answered one question per statement, and everything it would not answer was a
named refusal. That was the right shape to start from and the wrong place to stop: a user with
one database and one table could count, aggregate or group, and could not ask two of those at
once, could not rank by anything but a count, and could not see a value they had stored.

Four things were added. Three of them cost no `Plan` variant, which is the interesting part; one
of them reverses a decision this document argued for, which is the honest part.

### 1. A statement may make several plans

`Statement::call` became `Statement::calls`. `SELECT count(*), sum(amount), avg(amount)` is three
plans — a count, a sum, and the count the average divides by, which deduplicates into the first —
each planned, fanned out and merged **exactly as if it had been written alone**. The row is
assembled at the coordinator afterwards.

This is what keeps [Rule 2](#rule-2--v1-adds-no-plan-variant) intact through the largest part of
the change. `big-cluster::sql` gained a loop and nothing else: no fan-out code, no merge arm, no
wire change. A grouped statement with several aggregates joins its plans **on the group's row
id**, not on the key — a row id is assigned once for the whole cluster, and a key is a string a
particular node may never have been told.

The cost is now the length of that list, so it is capped at sixteen distinct plans, with a
refusal that says what the number means. `avg` costs two.

### 2. Everything the plan cannot carry moved into the shape

`HAVING` over any of the aggregates the select list asked for, `OFFSET`, and every ordering
`TopN` cannot do — descending by key, ascending by count, either direction over a `sum`, `min` or
`max`. All of them are applied to the merged answer, at the coordinator, in the order SQL
specifies: filter, sort, skip, cut.

Two things are worth stating rather than leaving to be discovered:

- **An ordering the plan cannot carry means every group is materialised before the cut.** `TopN`
  ranks and truncates in one pass and still does, whenever the ordering is the count ranking and
  nothing after the merge can drop a row it counted. `ORDER BY sum(x) DESC LIMIT 10` cannot use
  it, and holds every group at the coordinator first. That is the honest price of the clause, and
  it is in the `Shape::Groups` doc comment where someone reading the code will find it.
- **A `HAVING` threshold on a decimal column is converted by the planner's own conversion.**
  `HAVING sum(price) >= 100.00` on a two-place field compares against `10000`, because
  `WHERE price >= 100.00` does. A second implementation of that conversion is the one that would
  drift, so `Shape::resolve` calls `big_plan::to_units` — and it is a separate step from
  `translate`, which stays a pure function of the text with no schema in sight.

### 3. `FILTER (WHERE ...)` — and the case it forced into the open

A filtered aggregate is that entry's call over `Intersect(<the statement's rows>, <the filter>)`.
Nearly free, given the first change.

What it forced into the open is that **under a `FILTER` the statement's plans no longer describe
the same groups**. A group every record of which the filter rejects is missing from that plan's
answer entirely, while the others still know it exists. Two consequences, both of which the code
now names:

- The group set has to come from the `WHERE`, not from any filter, so a grouped statement with a
  filtered aggregate also plans the unfiltered `Distinct` — purely to say which groups exist.
  Without it the answer would have no row for such a group, which is a *missing* group rather
  than an empty one, and no client could see the difference.
- A cell has to know what its plan would have answered over no records. That is `Shape::Absent`:
  zero for a count and for a sum, absent for a `min` or a `max`. Standard SQL would make the sum
  `null`; this engine has no absent total and `SELECT sum(x)` over an empty `WHERE` has always
  answered `0`, so matching the rest of the engine wins over matching the standard. A client
  reading two spellings of the same nothing should not get two answers.

### 4. Projecting stored values, which reverses change 2 above

`SELECT amount, price FROM t WHERE … LIMIT 100` now answers with the values. The old reasoning
was right about the cost and wrong about what to do with it.

The cost is real and unchanged: a value in a bit-sliced field is not stored anywhere as a number,
so reconstructing one is a point read per record per column. **The number of records is therefore
the whole of what a projection costs, and that is why it is part of the plan.** `Plan::Project`
carries a mandatory limit of 1 to 10000; a statement without one is refused by name, with a
sentence saying so. A cut applied by a shape would be a cut applied after paying for it.

This is the one change that added a `Plan` variant, and Rule 2 is the reason it was safe to:
`big-cluster::merge` was taught the arm, and a test runs the projection across two real nodes on
loopback. Each owner is asked for the whole page — the first `n` records overall can all live on
one node — and the coordinator interleaves by record id and cuts once. A merge that concatenated
would answer with one node's records followed by the other's, which for a cut page is the wrong
rows in the wrong order, and nothing in the result set could show it.

Keyed columns are still refused, and by the planner rather than by the parser, because the
planner is the layer that knows what kind of field it is: *"`a projection` is not allowed on
`country`, which is a keyed field"*. `SELECT *` still answers with record ids, because a star
would have to include exactly those columns.

`Project` is a PQL call as well as a SQL one — `Project(All(), field=qty, field=price, n=10)` —
which it had to be. The translation can only emit the query language, and that is the type of
`Statement::calls` rather than a rule anyone keeps.

### What is still not there, and why

Two clauses were planned for this round and are not in it, because reading the code turned each
one from "add a clause" into something larger. Both are in `checklist.md` under their own lines.

**`LIKE 'prefix%'` on a keyed column** needs the keys that match the prefix, and key dictionaries
are per-node and partial — a node can hold records under a row whose string it has never been
told, which is exactly why `Group::key` is an `Option`. Resolving a prefix locally would answer
with fewer records than exist and no client could see it. Only the schema leader holds the whole
dictionary, so the clause needs a synchronous request to the leader on the read path: a new
failure mode for one clause, in an engine where every other read survives the leader being down.
That is a decision about availability, not a parser change.

**A window over a time quantum field** was refused in v1 for the wrong reason. The stated reason
was that SQL has no natural spelling for it. The real one is that the planner cannot tell a time
quantum field from any other keyed field — `FieldClass` collapses both to `Keyed` — and
`Rows::KeyBetween` against a plain `Set` field reads day views that do not exist and answers
empty. PQL can already be written into that hole today. Giving SQL a spelling means giving
`FieldClass` the distinction first, which is a fix to the query language, not an addition to this
surface.

---

## Joins

`checklist.md` carried **Joins: missing** as its own line from the beginning, and called it one
of "the two largest" — *a question about what the engine is for, and it has not been answered*.
Half of it is answered now, and the half that is not is worth naming as precisely as the half
that is.

### What a join can be here

A record is a set of bits in one table. There is no pointer to another table and no row of
values to carry one, so **nothing here pairs records**. What two tables share is the *string* a
keyed column was interned from — and even the row ids behind those strings are per
`(table, field)` and have no reason to agree.

So take the join key `s`. On the left it selects a set of records `A_s`; on the right, `B_s`.
The inner join is `⋃_s A_s × B_s`, and every aggregate over that product is arithmetic over two
numbers each side already knows how to produce:

| | |
|---|---|
| `count(*)` | `Σ_s \|A_s\| · \|B_s\|` |
| `sum(a.x)` | `Σ_s sum_A(x) · \|B_s\|` — each of A's records repeats once per partner |
| `min(a.x)` | the smallest `min_A(x)` over the keys B holds at all — repeating a value does not make it smaller |
| `count(DISTINCT a.k)` | how many keys both sides hold |
| `GROUP BY a.k` | the same, per key instead of folded |

Those are exact, not approximations. And `|A_s|` per key is what `Distinct(rows, field=k)`
already answers; `sum_A(x)` per key is what `GroupBy(rows, field=k, aggregate=Sum(field=x))`
already answers.

**So a join is two ordinary single-table plans and a `Shape`.** Each is planned, fanned out and
merged exactly as it would be written alone. `big-cluster` gained nothing: no `Plan` variant, no
merge arm, no wire change. [Rule 2](#rule-2--v1-adds-no-plan-variant) held for the feature the
whole document was written to say could not be built.

### Why the arithmetic cannot happen any earlier

`crates/big-http/tests/cluster.rs` has the test that makes this checkable rather than
descriptive. `GB` holds two orders on node `a` and one on `b`, and two shops split one apiece.
The join has `(2+1) · (1+1) = 6` rows. Multiplying at each owner and summing the products gives
`2·1 + 1·1 = 3` — a plausible number, off by half, that nothing in the result set could reveal.

That is the same argument `count(DISTINCT x)` made for living in a shape, one step further on:
a per-node product is not a share of the product.

### What the statement has to say

With two tables in scope, **every column must be qualified**. `translate` resolves names before
it has ever seen a schema, so `sum(amount)` over a join names nothing this crate can decide, and
picking the left table because it was written first would be guessing. Refused as
`sql_ambiguous_column`, with a sentence saying so.

A `WHERE` is split by the table each term names and applied to that side alone. Only an `AND`
can be split: `a.x = 1 OR b.y = 2` selects pairs where *either* side matched, which neither side
can be narrowed to on its own, so it is `sql_join_filter` rather than a filter quietly applied to
one of them.

### What is refused, and why each reason outlives the decision

| Refused | Code | Why |
|---|---|---|
| `LEFT`/`RIGHT`/`FULL`/`CROSS`/`NATURAL JOIN` | `sql_no_outer_joins` | An outer join produces a row for a record with no partner. What is computed here is arithmetic over per-key counts; there is no row to null out half of. |
| A comma between tables | `sql_no_joins` | A cross join has no key to pair on. |
| A third table | `sql_no_joins` | Would need a key all three share, which is a different question. |
| `ON` that is not one equality, and `USING` | `sql_join_condition` | Two conditions pair on a composite key this index never stored. `USING` names one column for two tables that each keep their own dictionary. |
| A bare column with two tables in scope | `sql_ambiguous_column` | Nothing here to guess with. |
| A `WHERE` term mixing both tables under `OR` or `NOT` | `sql_join_filter` | Neither side can be filtered to it. |
| `SELECT a.x, b.y FROM a JOIN b …` | `sql_unsupported` | A pair of records has no identity this engine stores. |
| `avg` over a join | `sql_unsupported` | A ratio of two paired numbers, which is a cell holding two cells. Write the sum and the count. |
| `FILTER` over a join | `sql_unsupported` | It would leave the two sides disagreeing about which keys are in the join, and a key one side dropped is a pairing that never happened rather than a group with a zero in it. |

### What is still missing

Rows of a join, and outer joins — and both for the same reason, which is the one this document
started with. Aggregates over a join are arithmetic over sets. A *row* of a join is a pair of
records, and a pair of records is a thing this engine does not store and has no id for. That is
not engineering time; it is the shape of the index.

---

## What v3 added: the ClickHouse surface

The question this round started from was "what does ClickHouse answer that this does not", and
the honest first half of the answer is that **most of it cannot be answered here and never
will**. ClickHouse has about a thousand expression functions and this engine has no expression
evaluator, by design: a record is a set of bits, and there is nowhere for `toYear(ts) * 2` to
run. Window functions, `LIMIT BY`, `groupArray`, `ARRAY JOIN` and the statistics that need `Σx²`
are all refused, and they are refused for one reason wearing several names — each of them asks
for **a row**, and a row of values is the thing this index does not store.

What was left was larger than it looked. Five pieces.

### 1. Vocabulary

`countIf(cond)` is `count(*) FILTER (WHERE cond)`, which this surface already had — so it parses
into the same place, and a test asserts the two spellings answer identically rather than merely
both answering. `PREWHERE` is accepted and folded into `WHERE`, because here it selects exactly
the same set: there is no row to avoid reading, and an intersection is an intersection whichever
clause spelled it. A bare boolean column is a predicate, which is how `countIf(active)` has to
read. `WITH <constant> AS <name>` binds a value; a `WITH` whose body is a select is a real CTE
and stays the subquery refusal it was.

Two mappings are deliberately **not** faithful, and say so. `uniq` and its five approximate
cousins all answer the exact distinct count, and `topK` the exact ranking. The plans behind them
are a `Distinct` and a `TopN` — a grouping that walks each fragment once and counts overlaps
without building them — so there is nothing an approximation would buy. A client porting from
ClickHouse gets a different number wherever its sketch was wrong.

`FORMAT` needed somewhere to live, and it is not the shape. A shape says what the answer *is*;
a format says how those bytes are spelled. So `Statement` carries an `Answer { shape, format }`,
the route declares the matching content type, and TSV, CSV and JSON are written from the same
cells rather than each re-deriving them.

### 2. `UNION ALL`

Each branch is a whole statement with its own plans; what makes them one answer is that the rows
are written one after the other. No `Plan` variant, no merge arm, no wire change. Branches are
lowered on their own and rebased onto one flat call list, which is what keeps everything
downstream seeing a statement with some calls and a shape.

A plain `UNION` is refused rather than answered as `ALL`: it removes duplicate rows, and a row
here is a rendered answer rather than a stored tuple.

**This turned up a latent bug.** `Shape::Groups` drove its group list from every answer in the
statement, which was right while a statement made plans for one shape — and wrong the moment a
union put another branch's plans in the same list. The second branch of a stacked `GROUP BY`
picked up the first branch's keys and rendered them with a zero. The driver is a fact about the
shape, so it is recorded there now, the way `Shape::Join` already recorded its two.

### 3. Time windows, and the class that could not see them

The old refusal said SQL had no natural spelling for a window. The real reason was worse.

`FieldClass` collapsed `Set`, `Mutex` and `TimeQuantum` into one `Keyed`. So
`Row(f="k", from=…, to=…)` resolved against any keyed field at all — and `Rows::KeyBetween`
reads the views by day that **only a time quantum field writes**. Against a plain set field
there are none, so the read returned an *empty set*. Not an error: a window that matched
nothing, which is exactly what a window that cannot be answered looks like from the outside. The
planner could not refuse it because the planner could not see the difference, and PQL could be
written straight into the same hole.

`FieldClass::Keyed` now carries which kind it is, a window requires one with views to read, and
both spellings of the hole are tested.

And the feature was unreachable. A time quantum field could be created over HTTP and filled, and
every fact landed as a plain key: `Fact` had `Int`, `Signed`, `Key` and `Bool` and no `Time`. So
the day views were never written and a window had nothing to read. The import line now spells
one `key@seconds` — the field's kind deciding how a value is read, which was already true of
every other kind on that route.

The spelling is a key and its bounds written against the same column, because a time quantum
field carries a key *and* a time: `visit = 'home' AND visit BETWEEN <seconds> AND <seconds>`.

### 4. `GROUP BY a, b`

Refused before as "a composite key this index never stored", which is true and not the whole of
it. **For each value of the left column the records holding it are a set, and grouping those by
the right column is the ordinary grouping the engine already does.** Nothing anywhere
materialises a pair of rows.

That makes the cost the number of values on the left, which is why `Plan::GroupByPair` carries
the bound itself rather than having one applied to its answer. Past it the executor refuses by
name rather than truncating: an answer holding fewer groups than exist is one nothing in it
could reveal.

This is the second change to add a `Plan` variant, and [Rule 2](#rule-2--v1-adds-no-plan-variant)
is again the reason it was safe to — `big-cluster` was taught the merge arm, and a test runs it
across two nodes. `GB/a` holds two records on one and one on the other: merging by concatenation
would answer with two rows for that pair, and merging by the left key alone would fuse `GB/a`
with `GB/b`. It is one row, and it counts three.

`SELECT DISTINCT a, b` is that grouping with the counts not rendered, which is what standard SQL
says it is — the same relationship `SELECT DISTINCT c` already had with `GROUP BY c`.

### 5. Exact quantiles, which are a search

A quantile is the value at a rank, and **no plan answers that**. What a plan answers is how many
records hold a value at or below a bound. So the bound moves until the count lands on the rank,
and every step is an ordinary `Count` — fanned out and merged like any other, which is why an
exact quantile needed no `Plan` variant and no merge arm.

It is a different *shape* of work from everything else here, and it is carried as one.
`Statement` gained `probes` beside `calls`: a call is asked once and a probe is asked until it
converges, and a caller that could not tell them apart would have no way to know how many round
trips a statement costs.

The clustered test says why this cannot be a plan. Node `a` holds 10, 20 and 30; node `b` holds
40 and 50. Their medians are 20 and 40, and neither of those — nor anything derived from the
pair alone — is the answer. The median of all five is 30, which only a search over the merged
counts finds.

The price is one round trip per step, about the bit depth of the range, plus three to find the
range and the population. That is what it costs to hold no values in memory; ClickHouse's
`quantileExact` holds every one it saw.

### What the refusals now have in common

The first version of this document said the refusal list was the feature. It still is, and the
list has become easier to justify because the reasons have collapsed into one.

Outer joins, rows of a join, `LIMIT BY`, window functions, `groupArray`, `SELECT *` with values,
`DISTINCT ON` — every one of them asks for **a row**. A record here is a set of bits at
`(row, record)`; there is no row of values to hand back, no identity for a pair of records, and
no order among them to window over. `SELECT *` answering with ids and a projection costing a
point read apiece are the same fact seen from two other angles.

That is not engineering time. It is the shape of the index, and it is the one thing on this page
that a later version will not reverse.
