# Architecture

Every layer the design calls for. A dashed line is something that does not exist yet, so what
is missing shows up in the shape rather than only in a footnote. There is one left, and it is at
the bottom.

```
   client     ┌──────────────────────────────────────────────────────────────┐
              │ big-cli   bigc: one subcommand per route, links no engine     │  cannot answer
              │           a statement travels as bytes; errors come back whole│  what bigd cannot
              └───────────────────────────────┬──────────────────────────────┘
                                              │  HTTP, the same twelve routes
   ingest     ┌───────────────────────────────▼──────────────────────────────┐
              │ big-ingest  bigi: one file, one route, many requests         │  idempotent by
              │             whole lines under the body cap, in order         │  construction, so
              │             a byte offset written down after each ack        │  a resend repeats
              └───────────────────────────────┬──────────────────────────────┘
                                              │  HTTP, line format
   edge       ┌───────────────────────────────▼──────────────────────────────┐
              │ big-http  HTTP/1.1, twelve routes, hand written, no framework│  bearer tokens,
              │           bounded worker pool, socket and query deadlines    │  three roles
              │           no TLS - terminate at a reverse proxy              │
              └───────────────────────────────┬──────────────────────────────┘
                                              │
   cluster    ┌───────────────────────────────▼──────────────────────────────┐
              │ big-cluster  ranges from a config file; which copy serves one│  CP: a range
              │              is agreed, and moves when that copy stops       │  fails over in
              │              plan out, Matches back, merged by the plan      │  ~1s rather than
              │              one schema leader owns every row key            │  answering from
              │              write to every copy, read from the one serving  │  a stale copy
              └───────────────────────────────┬──────────────────────────────┘
                                              │  the local share; peers get
                                              │  the same plan over /internal
   facade     ┌───────────────────────────────▼──────────────────────────────┐
              │ big-api    schema, batch import, query                       │  owns the transaction boundary
              └───────────────────────────────┬──────────────────────────────┘
                                              │
   query      ┌───────────────────────────────▼──────────────────────────────┐
              │ big-sql   one-table SELECT ──translates to──► PQL            │  no joins, and
              │           refuses everything else, by name, before it runs   │  it says so
              ├──────────────────────────────────────────────────────────────┤
              │ big-plan  parse, resolve, type check ──plan──► big-exec      │  one planner,
              │           no dependencies                 fans out per shard │  two surfaces
              └───────────────────────────────┬──────────────────────────────┘
                                              │  DbRead, one read txn
   data       ┌───────────────────────────────▼──────────────────────────────┐
              │ big-db     catalog, Matches, per_fragment / per_segment      │  one key, not five objects
              │            FragmentKey = (table, field, view, shard)         │  a segment is one view
              │            engine per table: bitmap │ both │ columnar        │  over its bitmaps
              └───────────────────────────────┬──────────────────────────────┘
                                              │
   storage    ┌───────────────────────────────▼──────────────────────────────┐
              │ big-engine  base │ bitmap │ columnar │ hybrid                │
              │ big-btree │ big-pager │ big-page │ big-container │ big-keys  │
              │ no WAL — the meta page flip is the only atomic point         │
              └──────────────────────────────────────────────────────────────┘

   ops        ┌──────────────────────────────────────────────────────────────┐
              │ GET /metrics  Prometheus text: pager gauges + server counters│
              │               + the row-key dictionary, which is the one     │
              │               allocation that grows with cardinality         │
              │ GET /health   liveness     GET /ready   readiness            │
              │ POST /admin/backup  a compact copy, taken while serving      │
              │ one JSON line per request on stderr, with a request id       │
              │ stable error codes, and a status chosen per error variant    │
              └──────────────────────────────────────────────────────────────┘
                                              ╎  no UI, no distributed tracing
```

## Nine decisions worth stating

**The data layer is one key, not a tree of objects.** A fragment is addressed by
`(table, field, view, shard)` and nothing holds a pointer to anything else. Renaming a table
touches one catalog record and no data at all, and "every fragment of one field" is a prefix
scan rather than a walk through four nested maps.

**There is no write-ahead log.** A commit writes its pages, fsyncs, then flips the meta page
and fsyncs again. Until that flip every byte written is unreachable garbage; after it, all of
it is live. There is no state in between, so there is nothing to replay on open and no
recovery path to get wrong.

**Backup, compaction and format migration are one operation.** Each is: walk everything
reachable from one consistent set of roots, and write it into a fresh file with new page
numbers. `big_btree::visit_tree` is that walk, `big_btree::copy_tree` moves a tree through it,
and `big_db::copy` drives it over every fragment inside one read transaction. Building it once
means a backup cannot silently skip a page class that compaction handles, and it means the
compaction path is exercised by every backup test.

**Security stops at the edge, and transport security stops before it.** The server
authenticates with bearer tokens read from a file and authorises with three roles - `read`,
`write`, `admin` - which are about *verbs*, not about rows. There is no TLS and there will not
be: a TLS stack is a larger dependency than the entire engine, and a hand-written one is out of
the question, so termination belongs to a reverse proxy. What keeps that from being an excuse
is that `bigd` **refuses** to bind anywhere but loopback without a token file. A warning is the
right shape for something recoverable; a public port with no authentication is not one.

**One tenant per process.** Table names are a flat global namespace with no notion of an owner,
so a credential that can read one table can read every table. Isolation means one `bigd` per
tenant against one file per tenant, which the exclusive file lock already pushes toward. Stated
here so that nobody assumes otherwise from the presence of roles. It is not a dead end: a
tenant id would enter the catalog as a new record kind, which is additive and needs no format
version bump - the same escape hatch the id high-water counters used.

**SQL is a translation, not a second engine.** *(One statement is not a query: `CREATE TABLE`,
which is classified before anything is planned and sent to the schema leader like any other
schema change. It raises the route's role to `admin` on the way - see the route table in
`big-http`.)* `big-sql` emits `big-plan`'s own AST, so a
`SELECT` is resolved by the planner that resolves hand-written PQL - same schema lookup, same
type rules, same error codes, same `Plan`s. Nothing below the planner can tell which surface a
query arrived through. The objection this had to answer is the one `checklist.md` used to record
as a refusal: a SQL surface on an engine with no joins promises joins. It does not promise them
here, because what a statement is permitted to say is bounded by what the planner will resolve -
that is the *type* of the translation's output, not a discipline anyone has to keep - and
everything outside that boundary is refused at the keyword with a code and a sentence saying what
exists instead. The refusal list is the feature.

Where SQL asks for something a plan does not carry - `count(DISTINCT x)`, a `HAVING`, an
`OFFSET`, `WITH TIES`, an ordering by a total rather than a count, the division an `avg` is,
**and the whole of a join** - that lives in a `Shape` applied *after* the merge, at the
coordinator, which is the only place those answers are right. Where it asks for something no
plan answers at all - a quantile, which is the value at a rank - the coordinator *searches*: it
moves a bound and asks an ordinary `Count` until the count lands on the rank. A statement
therefore carries two lists, calls and probes, because a call is asked once and a probe is asked
until it converges. A join is the clearest case: `FROM a JOIN b ON a.k = b.k` is
two ordinary single-table groupings, one per table, and what makes it a join is that for each key
both sides hold, the answer is the product of their per-key numbers. Multiplying at each owner
and summing would be a plausible number that is wrong, so the arithmetic waits until both sides
have been merged. Where it asks several questions at once, it makes several plans, each fanned out and
merged exactly as if it had been written alone; the row is assembled afterwards. Neither of those
taught the layers below anything, which is the point: **a statement's cost is the number of plans
in it, and its reach is bounded by what one plan can be.** The one exception is deliberate and
tested - `Plan::Project` reads stored values back, and the merge was taught its arm rather than
left to guess, because a variant the merge has not been taught is a distributed answer that is
wrong rather than absent.

**There is a second way to answer a query, and it is a table's own choice which one it gets.**
A table declares a storage engine at creation - `bitmap`, `bitmap+columnar`, or `columnar` - and
that decides whether its facts are written as bit rows, as column segments, or as both. A segment
is addressed by the *same* `FragmentKey` as its bitmaps and differs only in the view, which is why
backup, compaction, `drop_table`, the freelist and the cluster's fragment addressing all reach
segments without being taught what one is. The block format is in `big-engine::columnar`: 1024 records to
a block, because 1024 values at the full width is exactly one page, so a scalar block never needs
a second one; four encodings chosen per block by measuring all of them; and **no general-purpose
compressor**, because the whole engine ships two dependencies and a compression library would be
the largest thing in the tree.

This is the one decision that contradicts something else on this page. *SQL* is still a
translation and not a second engine - both surfaces resolve through one planner - but **a scan
is a second engine, below the planner**, and pretending otherwise would be the stale claim this
document exists to avoid. Two paths to one number is how a database goes quietly wrong, so what
holds them together is not care inside either path: `crates/big-db/tests/engines.rs` asks every
verb of every engine over the same data and requires the answers to be identical. A verb one
engine cannot answer is named there rather than skipped - a time window is the only one, because
the per-day views a time quantum field writes are an index construct and a segment records which
keys a record holds, never when it held them.

The routing rule is deliberately the dumbest defensible one: if there is an index, a predicate
uses it; otherwise it scans. **The index is not always cheaper and the rule knowingly ignores
that** - a bit-sliced index costs one read per bit plane whatever the predicate selects, and one
measured `Eq` over five thousand records of a 13-bit column took sixteen page reads through the
index and one through the segment. Choosing per query needs a cost model, a cost model needs
statistics this engine does not keep, and a rule that guesses is worse than one that is simple:
the simple one is predictable, and the guessing one is a cliff nobody sees coming. The figures are
written down in that test so whoever builds the cost model starts from a measurement.

**The command-line client links none of the engine, and that is what keeps it honest.**
`big-cli` has an empty `[dependencies]` section. It cannot link `big-sql` or `big-plan`, so it
cannot validate a statement before sending it and cannot grow an offline query path - both of
which would be a second surface drifting from the first. A statement travels as bytes; an error
comes back as the server's own code and the server's own sentence, printed without rewording, so
`sql_no_joins` on a terminal *is* the string `bigd` chose. Every subcommand is exactly one route,
which means a feature request for the client is a feature request for the server. `big` stays
what it was - the offline half, which takes the exclusive lock - and `bigc` is a third binary
rather than a subcommand of it, because a query subcommand there would be the one command that
fails whenever the database is actually being served.

**Distribution is built, and the engine had decided most of it.** A record id names
its shard by a shift, and the client picks the record id, so placement needs no agreement.
`Matches` is a map from shard to row set whose `and`/`or` are shard-wise merges, so combining
two nodes' answers is the operator that already combines two fragments'. What is left is
ownership and the row-key namespace, and both are settled the same way: **shard ownership is a
range map in a config file, and interning routes to one named schema leader.** There is no
membership layer beyond the agreement's own heartbeats, which already have to say who is
answering. A query that cannot reach the copy serving a range fails naming that range rather
than answering partially, for the same reason an unknown container type is an error rather than
a skip: a count missing one node's contribution looks exactly like a correct count. **That is
the CAP choice, and it is CP** - an answer here is an aggregate, so a stale one is a wrong
number with no symptom, while a refusal is an outage that lasts as long as a failover. A range
with a copy fails over on its own, by an agreement over one small value: which copy serves it.
`bigd --cluster` is the flag; a daemon without it is a cluster of one, running the same
coordinator, because a second path for the un-clustered case would be the path nobody tests.
The full design, including what it deliberately does not give - no rebalancing, no cluster-wide
snapshot, no quorum reads - is in [docs/clustering.md](docs/clustering.md).

The consequences are worth spelling out, because they are what the operator actually deals
with:

- **A backup is taken online.** The walk holds a read transaction, which holds the reclaim
  horizon down, so a concurrent writer can commit but cannot reuse a page the walk still
  needs. Copying the file with `cp` while a writer is live is *not* safe and never will be.
- **A backup is an ordinary database file.** Restoring is opening it. There is no restore
  format and no conversion step.
- **Compaction is the only thing that returns space.** Free pages in the interior of a file
  are reused by the next allocation, so a file does not grow without bound - but it never
  shrinks either, except for whatever `truncate_tail` finds sitting at the very end. A
  compaction is the copy path plus an atomic rename.
- **The format upgrade policy is dump/reload, never migrate in place.** A format change ships
  a copy tool built with both codecs: it reads with the old reader and writes with the new
  writer. That costs 2x the file size in free space during the upgrade, and buys never having
  a second recovery path that can lose data. No such tool exists yet and no `MIN_READ_VERSION`
  has been added, because only one format version has ever existed - a range check with no
  caller is machinery, not a guarantee. What the policy does require today is already in
  place: a file from another version reports that fact rather than collapsing into "this file
  is damaged". New *catalog record kinds* are exempt from all of this - unknown kinds are
  skipped on load - so the schema can grow without a version bump, which is how the id
  high-water records were added.
- **New container types and field kinds are additive forwards and loud backwards.** A file
  written by a newer build and opened by an older one fails with the type it did not recognise -
  `UnknownContainerType`, `unknown_field_kind` - rather than skipping the thing it cannot read.
  That distinction is the whole reason they need no dump/reload: skipping would make a newer file
  indistinguishable from one that is simply missing a container or a field, which is the same
  mistake the meta page used to make and the two call for opposite actions. `BitmapDelta` and
  `SignedInt` were both added this way. A change to an *existing* type's meaning is not exempt
  and never will be: nothing about it would be loud.
