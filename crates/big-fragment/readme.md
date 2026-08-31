# big-fragment

Fragment coordinates, and row access within one.

A **fragment** is one b-tree. A **row** is a contiguous span of container keys inside it. That
is the whole idea: because a row occupies a known, contiguous key range, reading one is a range
scan rather than a lookup per chunk, and unioning two rows is a merge of two scans.

## The coordinate

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

## The constants are derived, not chosen twice

`SHARD_WIDTH_EXPONENT` is 20 and `CONTAINER_EXPONENT` is 16, so `CONTAINERS_PER_ROW` is
`1 << 4`. That last one is **computed** rather than written as `16`, because a literal there
would let a later change to the shard width break the layout silently instead of failing to
compile. `SHARD_WIDTH_EXPONENT` is part of the wire format: any two peers exchanging data have
to agree on it.

## Why the fan-out penalty lives here

A commit rewrites the root record for every fragment the database holds, not only the ones it
touched. So a write that spreads across many fragments pays for all of them — which is exactly
what the sparse-layout column in `bench/results/REPORT.md` is measuring. The coordinate above is
where that spread is decided.

## Layout

- `coords` — `RecordId`, `ShardId`, `RowId`, the width constants, and the arithmetic that turns
  a `(row, record)` pair into a bit position and then into a container key
- `read` / `write` — `FragmentRead` and `FragmentWrite`, row access over one fragment's tree
- `rowset` — `RowSet`: rows held unmaterialised as containers per slot, so two can be combined
  with `SetOp` without either one naming a record
