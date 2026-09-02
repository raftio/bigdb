# big-message

Messages from a running program into a bigdb table.

`bigctl import` loads a **file**. This loads a **stream**: a program producing events as it runs,
with no file to point at and no record ids of its own.

[`examples/producer/`](../../examples/producer/) runs all of this under `docker compose` in one
command, if you would rather see it than read it.

```rust
use big_message::{Config, Producer, Value};

let mut producer = Producer::open(
    "127.0.0.1:7654",
    "tx",
    &["amount", "country"],
    None,
    Config::default(),
)?;

producer.send(&[Value::Int(1250), Value::Text("GB")])?;
producer.send(&[Value::Int(900), Value::Text("US")])?;

let flushed = producer.close()?;   // Flushed { inserted: 2 }
```

## The record id never appears

A record id is not a key the data chose. It is the address a bit is written at, and
`shard_of(record)` is also which node in a cluster owns it — the engine's own coordinate. So it
is not in this API: a caller cannot set one, cannot read one, and cannot be told one.

That is done by writing through `POST /sql` with an `INSERT` that names no `_record_id`, which
the schema leader allocates for. The server answers `{"columns":["inserted"],"rows":[[n]]}` —
the ids are not in the response either. A column called `_record_id`, in any case, is refused
when the producer opens.

## What that costs

`POST /table/{t}/import` is idempotent because the caller chooses the id: every fact is a `set`
at an address, so sending a chunk twice writes the same bits twice, which is writing them once.
That is why `bigctl import` can retry a transport failure and resume from a byte offset.

**An allocating `INSERT` has none of that.** Sending it twice writes two records. So:

| | |
|---|---|
| Delivery | at-least-once |
| Duplicates | possible, and silent — nothing errors |
| Retry | only what provably never reached the server |
| Resume | none; there is no offset to record |

The one place the uncertainty lives is `Error::Unknown`: the request was written in full and the
outcome is not known. It is never retried, it stops the producer, and it is the caller's to
resolve. There is a test, `two_flushes_of_the_same_messages_create_two_sets_of_records`, that
pins this rather than leaving it to be discovered in production.

If you need exactly-once, you need the record id — which means `bigctl import`, or a client that
chooses ids and posts to `/table/{t}/import`.

## Throughput, measured

Four million facts into one release-build server on one machine, both routes, same data:

| | requests | elapsed | facts/s |
|---|---|---|---|
| `bigctl import` | 12 chunks of 7 MiB | 0.74 s | ~5.4 M |
| `big-message` | 5 statements of ~400,000 rows | 1.43 s | ~2.8 M |
| `big-message`, when `big_sql::MAX_INSERT_ROWS` was 10,000 | 250 statements | 3.36 s | ~1.19 M |

The third row is why that ceiling was raised to a million. It made no statement cheaper — it made
250 of them where 5 would do, and the server commits once per request.

What is left is not the request count: this sends **fewer** requests than the import route and is
still about twice as slow. The rest is the SQL path itself — the text is lexed into tokens and
parsed into literals before the first fact is written, where an import line is read one at a time
and turned straight into a fact. That is inherent to writing through statements, and no amount of
batching removes it.

**A seven-megabyte statement costs the server real memory.** The run above left it at about
550 MB resident; seven megabytes of the smallest rows there are — `(1),(2),(3)…` — measured
239 MB against a 3.4 MB baseline, an amplification of roughly thirty-eight times over the text.
`big_http::MAX_BODY` bounds this, but it bounds it *per request in flight*, so the worst case is
that figure times the worker pool. Lowering `max_bytes` is the knob that lowers it.

So this crate is for a stream that has no file. If you have a file, `bigctl import` is faster,
idempotent, and costs the server almost nothing to hold.

## Retry rules

| Failure | Retried | Why |
|---|---|---|
| `connect()` refused | yes | nothing was sent |
| write did not finish | yes | the server holds fewer bytes than `Content-Length`; it cannot parse a statement from them |
| `503 server_busy` | yes | `big_http::shed` writes it from the accepting thread without giving the connection a worker, so the body was never read |
| written in full, then timeout / EOF / reset | **no** → `Error::Unknown` | indistinguishable from a server that committed and then died |
| `4xx` refusal | no | the server understood it and said no; it will say no again |

## Connections

One connection, held open across flushes. The server allows a thousand requests and five seconds
of idle per connection; this retires its own at nine hundred and at three seconds, so the
connection is always replaced *between* requests rather than closed by the server underneath
one. That race is the reason — for a non-idempotent write there is no safe recovery from it, so
it is made unreachable instead of handled.

## Dependencies

None. `[dependencies]` is empty and stays empty — `cargo tree -p big-message --edges normal`
prints one line. The engine appears only under `[dev-dependencies]`, where the tests bind a real
server to talk to.

## Values

`Value` says what kind of **literal** to write, not what kind of field to write into — field
kinds live in the schema, on the server, and `big_embed::fact::from_literal` decides whether a
literal fits one.

| Variant | For |
|---|---|
| `Int(u64)` | `INT`, and a `DECIMAL` already in stored units |
| `Signed(i64)` | `SIGNED` |
| `Decimal(&str)` | `DECIMAL` — `"12.50"` is 1250 units at scale 2, converted on the server |
| `Float(f64)` | `FLOAT32`, `FLOAT64` |
| `Text(&str)` | `SET`, `MUTEX`, `DATE` (`"2024-01-15"`), `DATETIME` |
| `Bool(bool)` | `BOOL` |
| `Keyed { key, at }` | `TIMEQUANTUM`, written `key@unix_seconds` |

Two limits worth knowing, both from `big_sql::lex`: there is **no exponent notation**, and a
number is `units / 10^scale` with `units` a `u64`. So `1e300` and `NaN` have no spelling and are
refused here — where the message that carried one can still be named — rather than at the
server, where one value refuses a batch of eight thousand. Ordinary magnitudes are unaffected.

## Batching

Bounded by bytes (`7 MiB`, under `big_http::MAX_BODY`), by time (`linger`, 200 ms), and by rows
(`900_000`, under `big_sql::MAX_INSERT_ROWS`).

**Bytes is the one an ordinary batch reaches.** The row cap is a backstop for rows so narrow that
seven megabytes still holds an unreasonable number of them — a single small integer per row fits
close to two million. Raising `max_rows` on its own changes nothing; `max_bytes` is the knob.

For a stream that goes quiet, `send` can only notice time when it is called. A loop that blocks
on its source with a timeout should check `due()` and `flush()`:

```rust
loop {
    match source.next_with_timeout(Duration::from_millis(50)) {
        Some(message) => producer.send(&message.values())?,
        None if producer.due() => { producer.flush()?; }
        None => {}
    }
}
```

## Reading back

`Reader` answers one question, and it is the one a writer that cannot see record ids needs:

```rust
let mut reader = Reader::open("127.0.0.1:7654", None, &Config::default());
let already: Vec<String> = reader.seen("tx", "msg_id", &["1700-0", "1700-1"])?;
```

Which of these keys the table already holds in that field. Only useful when the caller put the
key there — a table with no such column has nothing to match on, because the record ids are the
server's and are never handed out. `contrib/big-message-redis` uses it to make a restart write
nothing it already wrote.

It is one call rather than "send any statement": a caller writing its own SQL is a caller this
crate cannot keep from disagreeing with what `Producer` writes, and the escaping — the one place
here where a mistake is a security bug rather than a failure — would have had two callers.

Retry here is the **opposite** of retry in `Producer`: a `SELECT` is idempotent, so it is sent
again after a failure of any kind, including the one a flush must never repeat.
