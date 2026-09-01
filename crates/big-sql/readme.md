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

Naming columns is a projection, which reconstructs a value per record per column at a point read
apiece; the cut is therefore part of the plan rather than a view of its answer, and it bounds
the reads rather than trimming them afterwards. A `LIMIT` is optional, and leaving it out asks
for every matching record at that price.

`SELECT *` is a projection of every column the table declares, in declaration order. The list is
not in the statement — nothing there knows the table — so both halves of the answer are filled in
against the schema out of one expansion, `expanded_columns`: the plan reads the columns and the
header names them, and a plan reading three under a header of four would be an answer that is
wrong rather than absent. A column the star cannot reach is not named by it: on a table that
keeps no values a keyed or boolean column has no read back from a record at all, and where
nothing at all can be read `*` falls back to record ids under `_record_id`. That fall back is the
only way to see a record id from SQL; `GET /table/{t}/records` is the route that lists them.

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
| A decimal's scale | A decimal field stores an integer — `12.50` at scale two is 1250 — and everything up to the merge works in those units, which is what keeps all of it exact. The point goes back where the answer is written, so a value read back is the value that was written. `WHERE price = 12.50` converts on the way in; this is the same conversion on the way out. |
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

**Joins past the ones that are exact** — any number of tables are joined when they are one
*star* around one shared key. What is refused is a comma between tables, or a table two joins
would key differently (`sql_no_joins`); an outer or cross join (`sql_no_outer_joins`); `USING`
or a condition that is not one equality between the table being joined in and one already in
`FROM` (`sql_join_condition`); a column that does not say which table it belongs to
(`sql_ambiguous_column`); a `WHERE` term naming two tables under `OR` or `NOT`
(`sql_join_filter`); and `avg` or a select of joined values over a join — a pair of records has
no identity this engine stores.

**Everything else that has no set operation behind it** — subqueries, CTEs, `INTERSECT`,
`EXCEPT`, window functions, arithmetic in the select list, `LIKE`, `IS NULL` and `= NULL`. A
plain `UNION` and `UNION DISTINCT` have their own code (`sql_union`): removing duplicate rows
here would mean comparing rendered strings.

**The computation this dialect has no evaluator for**, each named by what was asked for rather
than by the evaluator that is missing: `CASE WHEN`, `if` and `coalesce` choose between two
values per record, and there are no values per record until something reads them back — a
condition selects a *set*, and `count(*) FILTER (WHERE …)` is how one statement asks about
several. `CAST`, `toInt64` and `toString` convert between representations that do not convert: a
keyed column is a string in a dictionary and an integer column is bit planes. `argMin`, `argMax`,
`stddev`, `varPop` and `corr` need each record's value revisited against a running total, and
this engine holds bits at `(row, record)` rather than values to revisit. A `FLOAT`, `DOUBLE`,
`DATE`, `UUID`, `JSON` or `BLOB` column names nothing this engine stores
(`sql_unknown_column_type`).

**The statements above a table, and the writes with no operation behind them** — a *session*
(`sql_use_unsupported`: a database arrives with the request, so `USE` has nothing to leave
behind), a **materialised** view (`sql_no_materialized_views`: a table plus a promise to keep it
current, where a plain view is a name for a statement), and `UPDATE`,
`TRUNCATE`, `MERGE`, `REPLACE`, `ALTER VIEW` and `CREATE INDEX` (`sql_read_only`), each of which is either a
row this engine does not store or an index a bitmap already is. `DELETE FROM` shares that code
and has its own sentence: a record is the bits set for it across every field, and
`POST /table/{t}/delete` takes the ids to clear. `INSERT … SELECT` would write an answer back as
facts, and an answer here is counts and keys *about* records rather than records to copy.

**Clauses answered in one shape and not another** — a `HAVING` naming an aggregate the answer
does not carry or with no `GROUP BY`, an `ORDER BY` naming a second aggregate or a column that
is not the grouped one, an `OFFSET` on anything but a list of groups (a record listing is paged
with the `after` cursor, which does not shift under inserts), a projection without its cut or of
a keyed column, `DISTINCT` over more columns than a grouping holds, a select list asking for
more than [`MAX_CALLS`] plans, a `FORMAT` nobody here writes, and a quantile level finer than
three digits.

See [`Refused`] for the code and the sentence each one produces — that enum *is* the list, so it
cannot drift from what the crate does.

## What it writes

`INSERT INTO t (country, amount) VALUES ('GB', 100), ('US', 900)` — the facts
`POST /table/{t}/import` writes, said as a statement.

**It carries literals, not facts.** What a value *means* is the field's kind to decide — `'GB'`
is a key to intern, `12.50` is 1250 units on a decimal of scale two, `now@1750000000` is a key
and a moment — and deciding any of that here would be this crate reading a schema. So an
[`Insert`] holds the literals exactly as written and `big_api::fact` turns each into a fact
against the `FieldInfo` it is for. That is the same function the import route reads its lines
with, so the two write paths cannot come to disagree about what a value means.

**The record id is a column called `_record_id`, and writing it is optional.** A fact is a bit
at `(row, record)`: the id is the address it is written to rather than a key generated for it,
and `shard_of(record)` is also which node owns it. An ETL that already has its own ids writes
them straight through — `INSERT INTO t (_record_id, country) VALUES (7, 'GB')` — and two
statements writing one id write two facts about one record, exactly as two import lines do.

Underscored because `id` belongs to whoever is writing the table. Taking the most natural column
name in SQL for the engine's own coordinate would mean a table could not have an `id` field of
its own, which is a name almost every schema wants; so the reserved one is spelled where nothing
else will be, and a *field* called `_record_id` is refused instead (`sql_id_column`) — it could
never be written through this surface, and would answer nothing while the record sat there.

**A statement that leaves the column out is given ids by the schema leader.** "One past the
highest" has one right answer per cluster, and two coordinators computing it independently would
compute the same number and write two records into one — which, unlike a refusal, nothing
downstream could see. So it is allocated where keys are interned, before any fact is sent
anywhere, and a leader that cannot be reached stops the statement rather than guessing. The run
is contiguous and in the order the rows were written, and it starts above every id that already
exists — including ones written by hand or through the import route, which never pass through
the leader at all.

A statement carries at most [`MAX_INSERT_ROWS`] rows, because the text is lexed and then parsed
into literals before the first fact is written — the batch is resident twice over. Volume goes
through the import route, which holds one line at a time.

**A value with more digits than its field keeps is refused, never rounded** (`too_precise`):
`VALUES (12.523)` into a field of scale two would have to drop a digit, and dropping one
silently answers a question nobody asked. It is the planner's own refusal, the one
`WHERE price = 12.523` already gives — one code and one sentence, whichever half of a statement
wrote the number.

## The schema, changed and asked about

`CREATE TABLE [IF NOT EXISTS] t (…) [ENGINE = …]`, `ALTER TABLE t ADD`/`DROP COLUMN`, and
`DROP TABLE [IF EXISTS] t` — the changes the `/table` routes make, written as statements and
applied as exactly those changes, so no node learns a second way to be told about a field.

`IF NOT EXISTS` leaves a table that is already there **exactly as it is, fields included**. That
is not the same as a `CREATE` that happens to be idempotent: an identical declaration interns to
the same table below, but a column the table does not have yet would be *created*. "Make sure
this exists" and "leave it alone if it does" are two requests, and only the second is safe to
run against a table somebody has since altered.

`DESCRIBE t` — with `DESC t`, `DESCRIBE TABLE t` and `SHOW COLUMNS FROM t` as the same question
— answers one row per field. `SHOW TABLES` lists the tables with their engine and field count.
`SHOW CREATE TABLE t` answers with the statement that recreates the table, rendered by
[`render::create_table`], which is the inverse of the parser's own type table and lives beside it
so the two cannot drift. All three read the catalog every node already holds: no plan, no
fan-out, and a `read` token is enough.

Each kind of statement is a variant of [`Sql`], and that is also how the edge decides what a
statement costs: a schema change needs `admin`, an `INSERT` needs `write`, and a `SELECT` or a
`DESCRIBE` reads. A further kind cannot be added without that decision being made.

## What a statement would do

`EXPLAIN [PLAN | SHAPE] <statement>` answers with what the statement means and runs none of it —
one row per line, under a column called `explain`. A query is planned but never executed, so
`EXPLAIN SELECT nope FROM t` still reports the unknown field; a schema change, a write and a
listing are already wholly in the parse tree, so explaining one reads no catalog at all and
cannot be used to ask whether a table exists.

`PLAN` and `SHAPE` name the two halves a query has — the plans its calls resolve to, and the
columns and clauses its answer takes. Naming a half of a statement that has one description is
refused (`sql_explain_half`), and so is `EXPLAIN EXPLAIN`, which is not a statement in any
dialect and so is a syntax error rather than a refusal.

**`EXPLAIN` is one wrapper around every kind rather than a flag on each**, which is why it
inherits the whole refusal list: `EXPLAIN DELETE FROM t` is `sql_read_only` and
`EXPLAIN SELECT * FROM t, u` is `sql_no_joins`, with no arm anywhere deciding that twice. It is
also not a reserved word — only the leading token position is special, so a table or a column may
still be called `explain`.
