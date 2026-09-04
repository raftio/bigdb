# bigdb — a Go client

```
go get github.com/raftio/bigdb/clients/go
```

```go
import (
    "context"

    bigdb "github.com/raftio/bigdb/clients/go"
)

c, err := bigdb.New("127.0.0.1:7654")
defer c.Close()

c.CreateTable(ctx, "tx")
c.CreateField(ctx, "tx", "amount", "int")
c.Import(ctx, "tx", []bigdb.Fact{{Field: "amount", Record: 0, Value: 1250}})

res, err := c.SQL(ctx, "SELECT count(*) FROM tx")
```

Go 1.23 or later. **No dependencies** — there is no `go.sum`, and CI fails if one appears.

## database/sql

```go
import (
    "database/sql"
    _ "github.com/raftio/bigdb/clients/go/sqldriver"
)

db, _ := sql.Open("bigdb", "bigdb://alice:s3cret@127.0.0.1:7654/sales")
row := db.QueryRow("SELECT count(*) FROM tx WHERE country = ?", "gb")
```

Placeholders are `?`. Not `$1` or `%s`, and the reason is the lexer rather than convention:
`?` is not a token in this dialect, so an unbound one is a syntax error the server reports
clearly — whereas `%` **is** a token (`Tok::Arith`), and a `%s` marker would collide with
modulo and, worse, with a `%` inside `LIKE 'a%b'`.

`sql.DB` pools connections, which is how you get parallelism; a single `bigdb.Client` holds one
connection and serialises calls on it.

## Delivery, said plainly

An allocating `INSERT` sent twice writes two records, and no client can undo that from the
outside.

| | |
|---|---|
| Delivery | at-least-once |
| Duplicates | possible, and silent — nothing errors |
| Retry | only what provably never reached the server |
| Resume | none; there is no offset to record |

If you need exactly-once, you need the record id — which means choosing it yourself and using
`Import`, where writing the same bit twice is the same as writing it once.

## Retry rules

| Failure | Retried | Why |
|---|---|---|
| connect refused | yes | nothing was sent |
| the write did not finish | yes | the server holds fewer bytes than `Content-Length`; it cannot parse a request from them |
| `503`, and the cluster's `stale_route` / `range_moving` / `owner_unreachable` / `not_serving` / `schema_leader_unreachable` / `server_busy` / `busy_authenticating` | yes | the work is somewhere else, or not now; the delay is at least what `Retry-After` asked for |
| written in full, then timeout / EOF / reset | **only when the operation is idempotent** | indistinguishable from a server that committed and then died |
| `504 query_timeout` | no | a query that has already run out of time will run out of time again |
| `500 partially_applied` | no | some of it landed; sending it again turns one thing to check into two |
| any other `4xx`/`5xx` | no | the server understood it and said no; it will say no again |

Which operations are idempotent is fixed per route, in `ops.go`, and never guessed from the
method:

| idempotent | not |
|---|---|
| `/query`, `/import`, `/delete`, `/records`, `/schema`, `/health`, `/ready` | `/sql`, and every DDL route |

`/import` is on the left because a fact is one bit set at an address the caller chose. `/sql` is
on the right because an allocating `INSERT` writes twice. DDL is on the right because in a
cluster a schema change that got far enough may already have come back as `partially_applied`.

Errors carry both readings:

```go
if errors.Is(err, bigdb.ErrNotFound) { ... }

var se *bigdb.ServerError
if errors.As(err, &se) && se.Code == "unknown_table" { ... }
```

There is no `405` here and `no_such_route` is a `404`, so a typo in a target and a table that
genuinely does not exist look identical apart from `se.Code`.

## Connections

One connection, held open. The server allows a thousand requests and five seconds of idle per
connection; this retires its own at **nine hundred** and at **three seconds**, so the connection
is always replaced *between* requests rather than closed by the server underneath one. That race
is the reason — for a non-idempotent write there is no safe recovery from it, so it is made
unreachable instead of handled.

Through `database/sql` the same two rules are enforced by `driver.Validator.IsValid` and
`driver.SessionResetter.ResetSession`, which the pool already calls at exactly the right two
moments.

## Timeouts

Pass a `context.Context` with a deadline; without one, `WithTimeout` (30s by default) applies.

**A trap worth writing down.** The default matches the server's `read_timeout` and
`write_timeout`. If an operator raises `--query-timeout` above 30s, raise the client's deadline
to match — otherwise every long query becomes a `TransportError{Sent: true}`, which is the worst
way for a slow query to fail: the answer exists and the client threw away its only chance to
read it. A real `504 query_timeout` is different and much better: it is the server saying
definitively that the statement did not complete.

## Values

The types that have a spelling in this dialect:

| Go | written as |
|---|---|
| `string` | `'...'`, with `'` doubled |
| `bool` | `TRUE` / `FALSE` |
| `int`…`int64`, `uint`…`uint64` | digits |
| `float32`, `float64` | fixed notation, never an exponent |
| `bigdb.Decimal` | as written, checked against the lexer's grammar |
| `bigdb.Keyed{Key, At}` | `'key@seconds'` |
| `time.Time` | RFC 3339 text |

Refused, on purpose:

- **`nil`.** This dialect has no `NULL` literal at all, so there is nothing honest to write.
  Leave the column out.
- **`[]byte`.** No spelling here, and in Go it is far too easy to reach for where a string was
  meant.
- **A float with no fixed spelling.** `1e300` is 301 digits and `units` is a `u64`, so it is
  refused rather than rounded — writing a different number than the caller sent is worse than
  saying no.
- **`_record_id` as a column**, however it is capitalised. Naming it would turn off the
  allocation this client relies on.

## The 8 MiB ceiling

`big_http::MAX_BODY` is 8 MiB, checked against `Content-Length` before the body is read. This
client refuses at 7, leaving the same 1 MiB margin `bigctl import` and `big-message` leave, and
the refusal names **which line** crossed it — a producer that sent a million needs to know which
one, not that there was one.

`ImportStream` chunks for you and takes a callback:

```go
c.ImportStream(ctx, "tx", facts, func(sent int, r *bigdb.WriteResult) error {
    return checkpoint(sent) // /import is idempotent, so this is a real resume point
})
```

For statements, `sqldriver.ExecBatch` merges an `INSERT` template over many rows. It is worth
doing: the server commits once per request, and the same four million facts took 3.36s as 250
statements and 1.81s as 5.

## Databases

`WithDatabase("sales")` means one thing on every call, and the client works for that rather than
relying on the server to.

The parameter alone would not do it. `?database=` scopes the data routes — `/sql`, `/query`,
`/import`, `/delete`, `/records` — but the DDL routes ignore it and read the path alone. A
client that only sent the parameter would `CreateTable("orders")` in the **default** database
and then `Import("orders")` into `sales.orders`, with a `200` at every step and the two never
meeting.

So this client folds the database into the table name: every route is given `sales.orders`,
which works on all of them and is documented to win over a disagreeing parameter. The parameter
is sent as well, because the route table documents it and the permission check reads it.

A name you qualify yourself is more specific than the client's default, and wins:

```go
c, _ := bigdb.New(addr, bigdb.WithDatabase("sales"))
c.Records(ctx, "orders")        // sales.orders
c.Records(ctx, "other.orders")  // other.orders — yours wins
c.Records(ctx, "orders", bigdb.InDatabase("other"))
```

Related: `/schema` does not report which database a table belongs to, so two tables with the
same name in two databases come back identical and `Schema.Table` returns the first. Ask with
the database set rather than sorting it out on this side.

## Why not net/http

`net/http` knows whether a request reached the wire and discards the fact before returning:
when a request is not going to be retried, `transport.go` unwraps `nothingWrittenError` and
`transportReadFromServerError` down to the underlying error, so what you get is a `*url.Error`
around a `*net.OpError`. "Nothing was written" is then indistinguishable from "written in full,
then silence", and that one bit is what the whole retry policy above rests on.

Its own retrying is fine, to be fair: it only happens on a reused connection, the
nothing-written case is exactly the safe one, and the two ambiguous errors are gated behind
`isReplayable()`, which a POST is not. `net/http` will not duplicate your `INSERT`. It just
cannot tell you which failure you had.

If you need a proxy or HTTP/2 more than you need that distinction, `WithDoer` takes any
implementation — report every failure as `Sent: true`, which is the conservative reading, and
accept losing the ability to retry a write that never left the machine.

## Testing this client

```
make check   # gofmt, go vet, go test -race -cover, and the zero-dependency assertion
make e2e     # builds `big` and runs the integration tests
```

The integration tests are behind `-tags integration` and skip with instructions when no `big`
binary is found. They deliberately do not build one: a `go test` that shells out to cargo takes
minutes on a cold tree and fails outright without Rust.

## Releasing

This is a nested module, so a release tag carries its path:

```
git tag clients/go/v0.1.0
```

A bare `v0.1.0` would publish the repository root, which has no Go in it.
