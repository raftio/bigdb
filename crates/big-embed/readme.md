# big-embed

One entry point for everything above the engine.

`big` is a bitmap index: values are stored as bits across rows, so counting and filtering
millions of records is boolean algebra over a handful of planes rather than a scan. This crate is
the facade over that — schema, ingest and queries — and it is one of the two crates in the
workspace that carry a semver guarantee.

```rust
use big_embed::{Api, Fact, FieldKind};

let api = Api::open("data.big")?;

api.create_table("tx")?;
api.create_field("tx", "amount", FieldKind::Int, 20)?;
api.create_field("tx", "country", FieldKind::Set, 0)?;

api.import("tx", &[
    Fact::Int { field: "amount", record: 1, value: 4_200 },
    Fact::Key { field: "country", record: 1, value: "GB" },
])?;

let n = api.query("tx", r#"Count(Row(country="GB"))"#)?;
# Ok::<(), big_embed::ApiError>(())
```

## What this crate decides, because nothing below it could

**Who holds the database.** `Api` owns it. Anything above holds an `Api`, not a `Db`.

**Where a transaction begins and ends.** A query is one read transaction; an import is one write
transaction over the whole batch. Neither is exposed, so no caller can leave one open across a
network round trip.

**What the schema looks like on the way out.** `schema()` returns a snapshot, not a lock guard:
serialising it must never keep a reader inside the engine while bytes go down a socket.

## Bounding a query

`QueryOptions` is a struct rather than four more `query_*` methods, because the three bounds
compose and every combination of them is legitimate:

```rust
use big_embed::QueryOptions;
use std::{sync::{Arc, atomic::AtomicBool}, time::Duration};

let cancel = Arc::new(AtomicBool::new(false));
let opts = QueryOptions {
    timeout: Some(Duration::from_secs(5)),
    cancel: Some(cancel.clone()),
    ..Default::default()
};
```

`Default` is the old behaviour exactly — the engine's own memory ceilings, no clock, and nobody
able to interrupt — so the type was added without changing a caller.

There is deliberately **no equivalent for writes**. A write is bounded by the batch the client
sent, which the edge already caps, and abandoning one half-applied would need the transaction to
be reasoned about rather than simply dropped.

## One process per file

`Api::open` takes an exclusive `flock` on the file. A second `Api` on the same path fails at
`open` rather than corrupting anything later. `Api::in_memory()` has no file and no lock.

## Stability

This crate and `big-http` are the published surface. Everything they return is nameable from
here — `Value`, `Metrics`, `Durability`, `FieldKind` and the rest are re-exported, because a type
a caller cannot name is a type they cannot hold.

**Which of the two you want is a question of where your code runs, not what it can do.**
`big-embed` is this database inside your own process, reached by a function call; `big-http` is
the same database over a socket. Same planner, same answers, same refusals. The name says
`embed` because that is the whole difference — there is no server in between.

`Api::db()`, which hands out the raw `Db`, is behind the **`unstable`** feature. `Db` belongs to a
crate with no version guarantee, so reaching for it is an explicit opt-out rather than the
default. See `../../docs/versioning.md`.
