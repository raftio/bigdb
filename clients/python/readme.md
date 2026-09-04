# bigdb, from Python

```python
import bigdb

with bigdb.Client("127.0.0.1:7654") as db:
    db.create_table("tx")
    db.create_field("tx", "amount", kind="int", bit_depth=20)
    db.create_field("tx", "country", kind="set")

    db.import_facts("tx", [
        bigdb.Fact("amount", 0, 1250), bigdb.Fact("country", 0, "GB"),
        bigdb.Fact("amount", 1,  900), bigdb.Fact("country", 1, "US"),
    ])

    print(db.sql("SELECT country, sum(amount) FROM tx GROUP BY country").rows)
```

The same thing awaiting:

```python
from bigdb import AsyncClient

async with AsyncClient("127.0.0.1:7654") as db:
    await db.import_facts("tx", [bigdb.Fact("amount", 0, 1250)])
    print((await db.sql("SELECT count(*) FROM tx")).scalar())
```

And through PEP 249, which is what `pandas.read_sql` and most BI tooling expect:

```python
import bigdb, pandas as pd

conn = bigdb.connect("127.0.0.1:7654")
frame = pd.read_sql("SELECT country, sum(amount) FROM tx GROUP BY country", conn)
```

## Install

```
pip install bigdb          # from clients/python: pip install -e '.[dev]' for the tests
```

Python 3.10 or newer. **No dependencies**, and there will not be any: `[project.dependencies]` is
empty, and CI checks both that the wheel declares nothing and that importing the package pulls in
nothing outside the standard library. An empty manifest is the only form of that claim a build
can check, which is the same argument `contrib/big-message` makes about its `Cargo.toml`.

## What it covers

The data plane and the schema - what a program talks to.

| | |
|---|---|
| `sql(statement)` | `POST /sql`. One statement; there is no session and no `USE`. |
| `query(table, pql)` | `POST /table/{t}/query`, in the query language. |
| `import_facts` / `import_stream` | `POST /table/{t}/import`. The idempotent write. |
| `delete_records(table, ids)` | `POST /table/{t}/delete`. |
| `records` / `iter_records` | `GET /table/{t}/records`, with the cursor followed for you. |
| `schema()` | `GET /schema`. |
| `create_table` / `drop_table` / `create_field` / `drop_field` | the DDL routes. |
| `create_database` / `drop_database` | the database routes. |
| `health()` / `ready()` | the two probes, neither authenticated. |

Not the operator surface - `/metrics`, `/verify`, `/repair`, `/admin/backup`, `/cluster/*`. That
is `bigctl`'s, and it wants a person reading the answers.

## Connecting

```python
bigdb.Client("127.0.0.1:7654")                                  # plaintext
bigdb.Client("https://db.example:7654", user="a", password="b") # TLS, Basic auth
bigdb.Client("https://db.example:7654", ca_file="ca.pem")       # a private CA
```

**TLS is chosen by the scheme and never guessed.** A bare `host:port` is plaintext, `https://` is
TLS, and a `ca_file` on a plaintext address is refused - the same three rules `bigctl` follows,
because a client that inferred encryption from a port number would connect in the clear to a
server the caller believed was encrypted.

Auth is HTTP Basic, which is base64 rather than encryption. On loopback that is silent; on any
other plaintext address the client warns once. Silence it with
`Config(warn_on_plaintext_credentials=False)` if you know what the network is.

## Writes are at-least-once. This is the part to read.

| | |
|---|---|
| Delivery | at-least-once |
| Duplicates | possible, and silent - nothing errors |
| Retry | only what provably never reached the server |
| Resume | for `import_facts` only, via `import_stream(on_chunk=...)` |

Failures are split by what you can do about them, not by whose fault they are:

- **`bigdb.NotSent`** - the request provably never arrived. The server holds fewer bytes than
  `Content-Length` promised and cannot parse a statement from them. Always retried for you.
- **`bigdb.Unknown`** - the request was written in full and the answer never came. A read
  timeout, a reset and an EOF are all indistinguishable from a server that committed and then
  died. **Never retried except on an idempotent route**, and handed to you to resolve.

`POST /table/{t}/import` is idempotent because *you* choose the record id: a fact is a bit set at
an address, so sending a chunk twice writes the same bits twice, which is writing them once. An
allocating `INSERT` over `/sql` has none of that - sending it twice writes two records. So if you
need exactly-once, choose the ids yourself and use `import_facts`.

Retries, in full:

| failure | retried |
|---|---|
| `NotSent` | yes, whatever the route |
| `Unknown` / `ProtocolError` | only on an idempotent route |
| any 503 (`server_busy`, `stale_route`, `owner_unreachable`, …) | yes - nothing was written |
| `504 query_timeout` | no; the query has to change, not the luck |
| `500 partially_applied` | no - catch `bigdb.PartiallyApplied` and repair |
| any other 4xx | no; the server understood it and said no |

## Large imports

`import_facts` sends one request. For more facts than fit in a body, `import_stream` chunks them
under the cap and tells you where each chunk started, which is a resume point:

```python
def checkpoint(offset, result):
    open("progress", "w").write(str(offset))

db.import_stream("tx", millions_of_facts(), on_chunk=checkpoint)
```

The default cap is 7 MiB against the server's `MAX_BODY` of 8, the same one-megabyte margin
`bigctl import` leaves. A body past the cap is refused before a socket is opened, and for facts
the refusal names *which* one pushed it over.

## Connections

One `Client` holds one keep-alive connection and is **not thread-safe** - use a client per
thread, or the async one. The server allows a thousand requests and five seconds of idle per
connection; this retires its own at nine hundred and at three, so the connection is always
replaced *between* requests rather than closed by the server underneath one. For a
non-idempotent write there is no safe recovery from that race, so it is made unreachable rather
than handled.

## Two traps worth knowing

**A slow query needs `io_timeout` raised with it.** The socket timeout defaults to 30 seconds to
match the server's own `read_timeout`. If an operator raised `--query-timeout` past that, raise
`Config(io_timeout=...)` too - otherwise a long query fails as `Unknown`, which is the worst way
for a slow query to fail, instead of as the `504 query_timeout` it actually is.

**Reaching a table outside the default database.** Two spellings, both of which work on every
route; a qualified path wins if they disagree.

```python
db.records("sales.orders")                  # qualified in the path
db.records("orders", database="sales")      # named by parameter
db.sql("SELECT count(*) FROM orders", database="sales")
bigdb.Client(addr, database="sales")        # or set it once, for every call
```

## Values

`Fact` values are written as text and the field's kind on the server decides how they are read:
a number for `int`, `true`/`false` for `bool`, `key@unix_seconds` for `timequantum`, text for
`set` and `mutex`.

SQL parameters go through one escaper, and it refuses what the dialect cannot spell rather than
letting the server refuse a batch of eight thousand rows without naming which one:

- **no exponent form** - a literal is `units / 10^scale` with `units` a `u64`, so `1e300` is
  refused rather than rounded to something that fits;
- **no `NULL`** - the dialect has no null literal; leave the column out;
- `_record_id` is refused as a column name, in any case, because the id is the server's to
  allocate.

`paramstyle` is `qmark`. `?` is the one marker the lexer can never produce in a valid statement
outside a string or a comment; `%` is a real token there, so `pyformat` would collide with modulo
and with `LIKE 'a%b'`.

## Field kinds are spelled two ways

`create_field(kind=...)` takes what the route accepts; `/schema` answers with what the engine
calls it. They are not the same list, and this client does not normalise them - inventing a third
vocabulary neither side speaks would be worse than the asymmetry.

| write | read back |
|---|---|
| `signed` | `signedint` |
| `int` `decimal` `set` `mutex` `bool` `timequantum` `float32` `float64` `date` `datetime` | unchanged |

`bigdb.FIELD_KINDS_WRITE` and `bigdb.FIELD_KINDS_READ` are both exported.

## Developing

```
make dev      # install the dev dependencies
make check    # ruff format --check, ruff check, mypy --strict, pytest
make e2e      # build `big`, then run only the integration tests
make cov      # the suite with a coverage report
```

The suite is in three layers. Most of it needs nothing: escaping, error mapping, response
decoding and request building are pure and are tested against bodies copied out of the server's
own tests. The transport layer runs against a scripted byte-level fake server, and
`test_transport_conformance.py` runs **one** table of cases against both the sync and the async
transport - anything that behaves differently there is a bug in one of them rather than a
difference in design. The integration tests need a real `big serve` and are skipped, with a
reason that names the fix, when there is not one.
