# big-engine

The storage engines, and the registry that names them.

Everything that decides *what a table writes for every fact it takes* lives here, one module per
engine. What sits below — `big-container`, `big-page`, `big-pager`, `big-btree` — is not an
engine and stays outside: all three engines use all four.

| module     | code | name              | bitmaps | columns |
|------------|------|-------------------|---------|---------|
| `bitmap`   | 0    | `bitmap`          | yes     | no      |
| `hybrid`   | 1    | `bitmap+columnar` | yes     | yes     |
| `columnar` | 2    | `columnar`        | no      | yes     |

The code is the byte the catalog stores. It is part of the file format: codes are never reused
and never renumbered, and zero has to stay `bitmap` because every file written before the engine
byte existed carries a zero there.

## The registry

`base/engine.rs` holds the `Engine` trait and `ENGINES`, the list of the ones this build has.
`TableEngine` is a handle into that list — a pointer-wide `Copy` value, not an enum, so the code,
the name and the capabilities have exactly one definition and cannot drift from a second copy.
`from_u8`, `parse`, `all`, `names` and the error message that lists the engines all read
`ENGINES` rather than repeating it.

**Adding an engine** is a module with a unit struct implementing `Engine`, and one line in
`ENGINES`. **Nothing in `big-db` changes**, and that is what `Engine::place` is for: the write
path used to ask `has_bitmap()`/`has_columns()` at nine separate setters and buffer the halves
itself, so a fourth engine was nine edits in someone else's crate. Now `big-db` owns the buffers
and hands them over as a `Sink`; the engine says what to put in them. `hybrid` is then literally
the other two called in turn.

Three questions in `big-db` still ask an engine what it keeps — a point read from a column or
from bit planes, a predicate from an index or from a scan, and whether a bulk load can write
everything the table stores. Those are the planner reading a *description*; a new engine answers
them rather than editing them.

What none of it buys is a genuinely new *format*. `has_bitmap` and `has_columns` describe the two
kinds of tree this build knows how to maintain, and an engine storing something that is neither
needs code in `big-db` too. The registry buys the identity, the naming and the routing.

## `bitmap`

A **fragment** is one b-tree. A **row** is a contiguous span of container keys inside it. That is
the whole idea: because a row occupies a known, contiguous key range, reading one is a range scan
rather than a lookup per chunk, and unioning two rows is a merge of two scans.

The coordinate:

```rust
FragmentKey { table: u32, field: u32, view: u32, shard: u64 }
```

Defined in `big-page` — it is part of the byte layout, so it lives with the layout — and
re-exported here, which is where it acquires meaning. Each field exists because something needs
to partition on it independently:

- **table**, **field** — the obvious two
- **view** — the same field written more than once under different conventions. A time quantum
  field writes one view per granularity so a range of days can be read without touching the days
  outside it; a mutex field keeps a shadow view of what each record currently holds.
- **shard** — a range of record ids. Sharding is what makes a write touch a bounded amount of
  the tree and what lets a read fan out across threads.

The catalog maps names to ids; by the time a `FragmentKey` exists, nothing about it is a string.

- `read` / `write` — `FragmentRead` and `FragmentWrite`, row access over one fragment's tree
- `rowset` — `RowSet`: rows held unmaterialised as containers per slot, so two can be combined
  with `SetOp` without either one naming a record

### `bitmap::field` — the five conventions

Nothing in `field` stores anything. A field type is a *mapping* — given a value and a record,
which bits in which rows of a fragment does it turn on — and the fragment underneath does not
know which convention produced the bits it holds. That separation is what lets five field kinds
share one b-tree implementation.

**`bsi` — bit-sliced integers.** A `u64` stored as one row per bit: value `5` at depth 3 sets the
record's bit in plane 0 and plane 2. A range query is then a boolean circuit over the planes
rather than a scan of values, which is the trade this engine is built around — `count` over
millions of records touches `bit_depth` rows, but a point read has to reconstruct the value from
one plane per bit where a b-tree would do a single descent. Row 0 is `EXISTS_ROW`, so "which
records have this field at all" is one more row rather than a separate structure.

**`signed`** (in `big-db`) is a `bsi` with the sign handled above it. It is a distinct field kind
rather than a flag because writing a signed value into an unsigned field stores a different number
silently instead of failing.

**`set`** — the simple case: one row per interned key, and a write only ever turns bits on.
Nothing to serialise against, so a set write can wait with the rest of the batch.

**`mutex`** — a set where each record holds at most one value. Enforcing that means finding the
value being replaced, which means a **read**, which is why a mutex write cannot be buffered with
the others: it keeps a shadow view of what each record currently holds and has to consult it at
the moment of the write.

**`quantum`** — a keyed value that also happened at a time. The fact goes into the standard view
exactly as `set` would, and additionally into one view per declared `Granularity`. Those extra
views are the entire point: a query for a range of days reads the day views it asks about instead
of every record ever written. `DEFAULT_GRANULARITY` is `[Day]`.

`civil_from_days` is Howard Hinnant's algorithm. Days-to-date is the only calendar arithmetic
this engine needs, and it is thirty lines — a date/time crate would be a dependency an order of
magnitude larger than the thing it replaces, in a module whose entire job is naming views.

## `columnar`

The other half of what a table can store. A fragment answers *which records*; a segment answers
*what a record holds* by keeping the values themselves. Neither can cheaply do the other's job.

A segment is one b-tree, addressed by the same `FragmentKey` as the field's bitmaps and differing
only in the view. That is deliberate and it is most of the design: root records, the backup walk,
`drop_table`, `drop_field` and the cluster's fragment addressing all speak `FragmentKey`, and
every one of them reaches a segment without being taught what one is.

A block is `BLOCK_RECORDS` = 1024 consecutive records, and the number is not arbitrary: 1024
values at the full 64-bit width is 8192 bytes, exactly one page. So a scalar block reaches one
page and never two, and the common cases — a boolean, a low-cardinality key, a narrow integer —
encode small enough to sit inside the leaf cell with no page at all.

The b-tree is the mark index. A column store normally needs a side structure mapping block number
to file offset; here the tree already maps a key to a cell, so what other engines build, this one
gets from the tree it already had.

A scalar block is one cell. A **list** block — a set field, where one record may hold many values
— has no bound of that kind, so it spills into further cells at the same block under an
increasing `part`, at key `(block << PART_BITS) | part`. That makes a block a *contiguous span of
keys* and reading one a range scan: the same shape `coords::row_ckeys` uses for a row, chosen
over a chain of pages because a chain would be a second kind of page ownership that the free
walk, the scrub and the copy would each have to learn.

## `base/coords`

Shard and record arithmetic, in `base` rather than inside `bitmap`, because it is not one
engine's: the columnar block number is computed from a record's offset within its shard, and
`big-cluster` partitions ownership on the same shard id.

`SHARD_WIDTH_EXPONENT` is 20 and `CONTAINER_EXPONENT` is 16, so `CONTAINERS_PER_ROW` is `1 << 4`.
That last one is **computed** rather than written as `16`, because a literal there would let a
later change to the shard width break the layout silently instead of failing to compile.
`SHARD_WIDTH_EXPONENT` is part of the wire format: any two peers exchanging data have to agree
on it.

## Why the fan-out penalty lives here

A commit rewrites the root record for every fragment the database holds, not only the ones it
touched. So a write that spreads across many fragments pays for all of them — which is exactly
what the sparse-layout column in `bench/results/REPORT.md` is measuring. The coordinate above is
where that spread is decided.
