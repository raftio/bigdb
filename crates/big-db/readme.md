# big-db

Schema, catalog, and the handle everything else hangs off.

This is where names become ids and where a fact becomes a set of bits in a set of fragments. It
is the widest crate in the engine because it is the one that has to hold all of the engine's
vocabulary at once: `Db`, `DbRead`, `DbWrite`, the catalog, and the three write paths.

## The three write paths, and why there are three

```rust
db.write()               // DbWrite  — a transaction, committed by hand
db.ingest(capacity)      // Ingest   — buffered, commits itself when full
db.bulk_load(table)      // BulkLoad — write-only, refuses to merge
```

**`DbWrite`** is the general case: open, write, commit. One transaction over the whole batch.

**`Ingest`** buffers facts and commits when a capacity is reached. It exists because of the
single largest cost in this engine: a commit rewrites the catalog and the root records for
*every* fragment the database holds, not only the ones it touched. A commit per record is the
worst case by four orders of magnitude, and buffering is the answer. `bench/results/REPORT.md`
has the grid.

**`BulkLoad`** goes further and **refuses rather than merges** when a fragment already holds
data — because merging means reading the fragment back, and not reading it back is the entire
reason this path exists. Set, bool, int and signed only: a mutex has to consult its shadow, and
a time quantum writes an extra view per granularity, so both are refused **at the call** rather
than thousands of facts later at `finish`.

## The catalog

Names to ids, for tables, fields and views, plus per-fragment metadata. Two properties are
load-bearing and easy to break:

- **Ids are not reused.** They are handed out as `max + 1`, which means dropping the highest id
  and creating a new one would rebind the old data. `drop_table` and `drop_field` return the
  `FragmentKey`s that have to be freed, so dropping is a complete operation rather than a
  catalog edit that leaks storage.
- **`FieldMeta` observes the values written to it**, so `may_contain` can rule a fragment out of
  a range query without reading it. This is the zone map, and it is what makes a range query skip
  shards instead of scanning them.

## Copy is one primitive with three names

`copy::copy_to` walks every page reachable from a consistent set of roots and writes it into a
fresh file with new page numbers. **Backup, full compaction and format migration are that same
walk**, which is why there is one implementation and not three.

The consistency comes from holding the read transaction open for the whole walk: a live reader
holds the reclaim horizon down, so no page the walk is about to visit can be handed to a
concurrent writer underneath it. Writers keep running — they simply cannot reuse anything this
reader can still see.

## Read fan-out

`DbRead` spreads a fragment scan across threads when there are enough fragments to be worth it —
below eight candidates it stays on one thread, because spawning costs tens of microseconds and a
fragment often costs less. Scoped threads, so the pager and the catalog are borrowed rather than
shared through an `Arc`, and nothing outlives the call.

Every worker checks the query's cancellation and memory budget for itself. One worker noticing
does not stop the others mid-fragment, but each stops at its own next one, so the whole scan
unwinds within one fragment's work.
