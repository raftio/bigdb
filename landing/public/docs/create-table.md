# Create a table

A table here is a name, a storage engine, and the fields you declare on it. The engine it is
created under is chosen once and decides which questions it can answer cheaply — and which it
refuses outright.

> Creating schema needs an `admin` token. Writing facts afterwards needs only `write`, and reading
> needs `read`.

## 1. Choose the engine

What a table writes for every fact. Chosen once, at creation, and never changed — so this is the
decision to make before the name.

### bitmap

- **Writes** — Bitmaps and bit-sliced indexes.
- **Answers well** — Filters and counts, which are an intersection and a popcount rather than a scan.
- **Costs** — Values are not stored. An integer rebuilds from its bit planes; a keyed or boolean
  column has no value to hand back, and asking for one is refused when the plan is built.

### bitmap+columnar (default)

- **Writes** — Both, for every fact.
- **Answers well** — Everything either half is good at; the planner reads whichever answers the
  question cheaper.
- **Costs** — Two copies of every fact on the write path, and the disk to hold them.

### columnar

- **Writes** — Column segments, plus the existence row.
- **Answers well** — Aggregates and projections that read a column end to end.
- **Costs** — No index to intersect, so a filter scans. Time windows and named views are refused by
  name: they live in an index this table does not keep.

The existence row is kept under every engine, columnar included: it is one bit per record, and it is
what `NOT`, `count(*)` and the record cursor stand on.

## 2. Create it

One statement, with or without a column list. The list is optional because a table with no fields
is a real thing here, and because there is no `ALTER`: adding a field to a table that already
exists is still the field route's job.

```sql
CREATE TABLE events
```

A name and nothing else. No `ENGINE` clause means the server's default — `bitmap+columnar`, the
engine that answers the widest range of questions well, rather than any particular one.

```sql
CREATE TABLE events ENGINE = 'bitmap+columnar'
```

Quoted because `+` is not a character the lexer has a token for. `ENGINE = columnar` is fine bare.

```sql
CREATE TABLE events (country TEXT, amount INT, price DECIMAL(10, 2))
```

A column list, which is optional. Each column becomes exactly the field
`POST /table/{t}/field/{f}` would have created — same kind, same depth, same scale in `/schema` —
so this is one statement to write rather than a second way to have a field.

```json
{"columns":["table"],"rows":[[3]]}
```

What `POST /sql` answers: the table id in a one-cell result set, so a SQL client reads a result set
rather than a second response shape.

```sql
CREATE TABLE events (amount INT(11), price DECIMAL(10), country TEXT NOT NULL)
```

Three refusals, each by name. `INT(11)` is a display width where it was written and would be eleven
*bits* here, which stops at 2047 — so it is refused rather than reinterpreted. `DECIMAL(10)` is SQL
for ten digits and no fraction, and reading it as a scale would make `price > 5` a question about
ten-billionths. `NOT NULL` is a promise about rows, and a fact at `(field, record)` is not a row.

Nothing is created until the whole list has been read: every kind, depth and scale is decided while
the statement is parsed, so a list with a bad type in it leaves no table behind. It is still not a
transaction — the table and each field are separate changes below this line — so what can fail
part-way is a node going quiet, reported as a partial schema change naming what did land.

The statement is classified before anything is planned, because the two kinds go to different
places: a query is planned where it arrives, a schema change goes to the leader and then to every
node. That is what keeps a `CREATE TABLE` from being applied on whichever machine the client
happened to reach.

`POST /sql` is authorised as `read`, and this statement raises that to `admin` once the parser has
classified it — a read-only token cannot create a table by writing it in SQL.

Everything else is still refused by name: `INSERT`, `UPDATE`, `DELETE`, `DROP TABLE`, and every
other `CREATE`. This surface writes no rows, and the schema it changes is a table's and its fields'.

### Changing it afterwards

`ALTER TABLE` adds and drops fields — the two changes the engine below can make to one.

```sql
ALTER TABLE events ADD COLUMN region TEXT, DROP COLUMN legacy
```

```json
{"columns":["fields"],"rows":[[2]]}
```

How many fields changed, not an id: two changes have no single id to answer with. `COLUMN` is
optional, the clauses run in the order written, and every one of them is judged before the first is
applied — so `ADD b, DROP nope` creates nothing.

There is no third clause. `MODIFY`, `ALTER COLUMN` and `CHANGE` are refused because a field's kind
is how every fact in it was routed and its bit depth is how many bitmaps hold a value: neither can
change without rewriting every fact ever written to it. `RENAME` is refused because names are what
resolve a fact all the way down and nothing below renames one. `ALTER TABLE ... ENGINE =` is
refused because the facts already there were written under the old engine. Each of those means a
second field or table, the facts copied into it, and the first dropped — three statements, because
it is three changes.

## 3. Declare its fields

A kind decides how a fact is routed and what a second write to the same record means. Every column
in the list above is one of these, and so is every field the route creates.

Two spellings, one field. A column list names a kind the way SQL names a type; the route names it
directly, and it is the route you reach for when the table already exists.

```http
POST /table/events/field/country?kind=set
POST /table/events/field/amount?kind=int&bit_depth=32
POST /table/events/field/price?kind=decimal&scale=2
```

| kind | written in SQL as | what a record holds | parameters |
| --- | --- | --- | --- |
| `set` | `TEXT`, `VARCHAR`, `CHAR`, `STRING` | Many keys per record. A second write adds. | — |
| `mutex` | `MUTEX` | One key per record. A second write replaces the first. | — |
| `bool` | `BOOL`, `BOOLEAN` | Two rows, true and false. | — |
| `int` | `TINYINT`, `SMALLINT`, `INT`, `BIGINT` | An unsigned integer, bit-sliced. | bit_depth (32), or `UINT(bits)` |
| `signed` | `SIGNED`, `INT SIGNED`, `BIGINT SIGNED` | A signed integer, in the same planes under a bias. | bit_depth (32), or `SIGNED(bits)` |
| `decimal` | `DECIMAL(p, s)`, `NUMERIC(p, s)` | An integer compared as a value with digits after the point. | scale (required), bit_depth |
| `timequantum` | `TIMEQUANTUM`, `TIMESTAMP`, `DATETIME` | A key, plus a bitmap view per granularity, so a window reads only the days in it. | — |

`bit_depth` is how many bits a value may occupy, and every plane is one more bitmap to intersect
during a range query — a wider field is a slower one, so declare what the data needs rather than the
maximum. A value wider than the depth is refused on write, never wrapped.

The SQL integer names carry the widths they have always carried: `SMALLINT` is 16 bits and `BIGINT`
is 64. `UINT(bits)` and `SIGNED(bits)` exist for the depths no SQL name has, which is most of the
useful ones. A decimal's precision buys its depth — `DECIMAL(10, 2)` is 34 bits, enough to hold
every ten-digit number and no more.

## 4. Write a fact, then count it

Writing is the other thing this surface does not do. Facts go in over the import route — one per
line, field, record, value — and SQL is how you check they landed.

```http
POST /table/events/import

country 1 vn
amount 1 4200
```

How the value is read is the field's kind to decide: a number for an integer, `true` or `false` for
a boolean, `key@seconds` for a time quantum, a string for the rest.

```sql
SELECT count(*) FROM events WHERE country = 'vn'
```

```json
{"columns":["count"],"rows":[[1]]}
```

The column is named `count` because that is what the select list asked for. A filter is an
intersection and the count is a popcount over it — no rows are read to answer this.

The whole batch is resolved against the schema before any of it is written, so a batch that turns
out to be malformed does not land halfway.

`INSERT` is refused by name rather than translated into this route: an insert promises a row, and
what lands here is a fact at `(field, record)`. A body goes in one request the server bounds at
8 MiB; a whole file is `bigi`'s job.

To see the table itself rather than its records, `GET /schema` answers with every table, its engine
and its fields — which is also what the console's workbench autocompletes from.

## What creating a table will not do

Each of these is a refusal with a name and a reason, raised where you can still act on it rather
than discovered later as a wrong answer.

- **The engine is fixed at creation.** Creating the same table again under the same engine returns
  the same id. Under a different one it is refused — `table_redefined`, because handing back a
  bitmap-only table to somebody who wrote `columnar` is handing them a table they did not ask for.
  Changing an engine means creating a second table and copying into it.
- **A statement is not a transaction.** A column list and a multi-clause `ALTER` are each one
  statement, not one change: the table and every field still go to the schema leader separately, and
  nothing rolls back. What a statement buys is one round of judgement before the first change goes
  out — so the failure it can leave behind is a node that went quiet, never a `DECIMAL` somebody
  forgot to give a scale or an `ADD` that landed beside a `DROP` naming a field that was never
  there.
- **A field's kind and depth are fixed once written.** `MODIFY`, `ALTER COLUMN`, `CHANGE` and
  `RENAME` are refused by name: the kind is how every fact in the field was routed and the depth is
  how many bitmaps hold a value, so changing either means rewriting every fact ever written to it.
  That is a second field, a copy, and a drop — three statements, because it is three changes rather
  than one hidden inside a `MODIFY` that would look free.
- **Names are flat, and short.** There are no schemas or namespaces to qualify a name with — one
  global namespace per deployment. The console holds you to lowercase letters, digits and
  underscores; the server's own limit is 104 bytes, which is what is left of a 128-byte catalog
  entry after its header.
- **A decimal without a scale is refused.** Not defaulted to zero: a decimal with no scale is an
  integer wearing a different name, and `price > 5` means `> 500` on a field with two of them.
- **A schema change is applied everywhere or reported.** One DDL goes to the schema leader and then
  to every node, so a partial success is the cluster's to report rather than something a client
  discovers later as a table that exists on three machines out of four.

## Next steps

- Open the console: https://bigdb.cloud/deployments
- What bigdb refuses: https://bigdb.cloud/#refusals
