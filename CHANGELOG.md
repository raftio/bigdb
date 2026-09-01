# Changelog

Format per [Keep a Changelog](https://keepachangelog.com/en/1.1.0/). Versioning per
[docs/versioning.md](docs/versioning.md), which is narrower than "SemVer" on its own: the promise
covers `big-api`, `big-http` and the on-disk format, and explicitly does not cover the
fourteen internal crates.

Entries describe what changed for someone using the engine. A refactor that no caller can
observe does not get a line.

## [Unreleased]

Nothing has been released yet. `0.1.0` is the version in the manifests, not a version anyone
can install. What is below is the state the first release will describe, not a diff against a
predecessor.

### SQL

- **`avg` over a join is answered.** One cell rather than two and a division: each half is
  scaled by the same per-key product, and that product is inside both sums rather than outside
  the fraction - so `Σ sum_s·Π / Σ count_s·Π` is the average of the join and the mean of the
  per-key means is a different number. `ORDER BY` an average over a join follows; `HAVING` on one
  stays refused, because an average is fractional and that comparison is not.
- **A join takes any number of tables**, so long as they are one *star* around the key they
  share: `FROM t JOIN u ON t.k = u.k JOIN v ON t.k = v.k`. Still one grouped count per table and
  a product per key, so still no `Plan` variant and no merge arm. A table two joins would key
  differently is a *chain* — it would have to be grouped by both columns at once — and stays
  `sql_no_joins`, as does a `FROM` wider than the fan-out cap.
- **Rows can be written in SQL**: `INSERT INTO t (country, amount) VALUES ('GB', 100)`, which
  writes the facts `POST /table/{t}/import` writes. A statement carries at most 10,000 rows;
  volume goes through the import route. Needs a `write` token.
- **Record ids are allocated when a statement does not name one.** The column is `_record_id` —
  underscored so that `id` stays free for a field of yours — and writing it is optional: an ETL
  that has its own ids passes them straight through. Allocation is the schema leader's, where
  key interning already happens, because "one past the highest" has one right answer per cluster
  and two coordinators computing it independently would write two records into one. A leader
  that cannot be reached stops the statement rather than guessing. `SELECT *` answers under the
  same name, and a *field* called `_record_id` is refused (`sql_id_column`).
- **`DROP TABLE [IF EXISTS]`** and **`CREATE TABLE IF NOT EXISTS`**. `IF NOT EXISTS` leaves a
  table that is already there exactly as it is, fields included, which is what makes a setup
  script re-runnable against a table somebody has since altered.
- **The catalog answers in SQL**: `DESCRIBE t` (also `DESC`, `DESCRIBE TABLE`,
  `SHOW COLUMNS FROM`), `SHOW TABLES`, and `SHOW CREATE TABLE t`, which answers with the
  statement that would recreate the table. All three read what `GET /schema` reads and need only
  a `read` token.
- **Named refusals for the SQL this engine cannot answer**, in place of a syntax error or a
  blanket "this surface writes no rows": `CASE WHEN` and `if` (`sql_unsupported`), `CAST` and
  `toString`, `argMin`/`stddev`/`corr`, `INSERT … SELECT`, `DELETE FROM` (`sql_read_only`),
  `CREATE`/`DROP`/`ALTER DATABASE` and `USE` (`sql_no_database`), and views
  (`sql_no_views`). Each says what the engine's shape makes impossible and what exists instead.
- A value written with more digits than its decimal field keeps is refused rather than rounded,
  with the same `too_precise` code and sentence `WHERE price = 12.523` already gives.
- **A decimal reads back as the value it is.** `SELECT price`, `sum(price)`, `min`, `max`, a
  quantile and a grouped total over a field of scale two now answer `12.50` where they answered
  `1250` — the same conversion `WHERE price = 12.50` already made on the way in, made on the way
  out. Exact: the answer carries the stored integer and the field's scale, and no value passes
  through a float. `POST /table/{t}/query` still answers in stored units, because its statements
  name fields rather than columns and its answers carry no schema to place the point with.
- `POST /sql` raises the role the statement needs: `admin` for a schema change, `write` for an
  `INSERT`, and the route's own `read` for a `SELECT` or a `DESCRIBE`.

### Storage

- **A table declares a storage engine at creation**: `bitmap`, `bitmap+columnar` (the default a
  table gets when the caller says nothing), or `columnar`. It decides whether facts are written
  as bit rows, as column segments, or as both, and it is fixed for the table's life - creating
  the same table again under a different engine is refused rather than ignored. Set it with
  `POST /table/{t}?engine=…`, `bigc create table t --engine …`, or `Api::create_table_with`;
  read it back from `GET /schema`.

  Measured, per record written, on a dense ten-thousand-record load: `bitmap` 1712 bytes,
  `bitmap+columnar` 1864 (+8.9%), `columnar` 488 (-71%). Asserted in
  `crates/big-db/tests/amplification.rs` so the figures and the code cannot drift apart.
- **`CREATE TABLE` is the one schema change `/sql` takes**: `CREATE TABLE t` or
  `CREATE TABLE t ENGINE = columnar`. Quote the name that contains a `+`
  (`ENGINE = 'bitmap+columnar'`); the others may be bare. No column list - a field kind may be a
  set, a mutex or a time quantum, none of which a SQL type names, so fields are still declared
  with `POST /table/{t}/field/{f}`. A column list is refused by name with `sql_no_column_list`
  rather than as a syntax error, and everything else that writes still answers `sql_read_only`.

  **It needs an `admin` token.** `POST /sql` is authorised as `read`, and the check runs before
  any body is decoded - so the route's role is a floor and a schema change raises it once the
  statement has been classified. A read-only token gets the same `403` it gets from
  `POST /table/{t}`.
- **A projection can read a keyed column.** `SELECT country FROM t` was refused by name because
  a bitmap records which records hold a key and never which key a record holds. On a table that
  stores its values it is now answered, as the string the key was interned from. Integer columns
  still render as JSON numbers and absent cells as `null`, so no client that could read a
  projection is affected.
- **A table with no index answers by scanning its columns.** Every verb but one - ranges, key and
  boolean lookups, sums, extremes, counts and groupings - is answered from segments when there is
  no index to read. A time window (`Row(f="k", from=…, to=…)`) is **refused by name** with
  `engine_cannot_answer` rather than answered emptily: the per-day views it reads are an index
  construct, and "no records in that window" is a different fact from "this table cannot see
  time".
- Copy-on-write b-tree of roaring containers over 8 KiB pages. No WAL: a commit writes pages,
  fsyncs, flips the meta page, fsyncs again, so there is no intermediate state to replay.
- Checksums on every page except the bitmap pages a leaf cell points at, which have no room for
  one without either shrinking the bitmap or growing the page.
- `flock(LOCK_EX|LOCK_NB)` against a second process on the same file.
- Three durability levels — `full`, `barrier`, `none` — settable at runtime, with tightening
  flushing everything written under the looser setting before it returns.
- Deletes, drop table, drop field, online backup, and full compaction. Backup, compaction and
  format migration are one walk over every page reachable from a consistent set of roots.
- Snapshots with expiry and pinning, and a reclaim horizon that a live reader holds down.

### Query

- PQL: `All`, `Row`, `Union`, `Intersect`, `Difference`, `Not`, `Count`, `Sum`, `Min`, `Max`,
  `Distinct`, `TopN`, `GroupBy`, `Project`.
- SQL over one or two tables, at `POST /sql`, answering with a result set of columns and rows.
  `count`, `sum`, `min`, `max`, `avg`, `count(DISTINCT x)`, `median`/`quantile`, `topK`,
  `SELECT DISTINCT`, `GROUP BY` one or two columns, `HAVING`, `ORDER BY`, `LIMIT`, `OFFSET`,
  `WITH TIES`, `FILTER (WHERE ...)`, `UNION ALL`, `PREWHERE`, `WITH <constant> AS`, `FORMAT`,
  an inner join, a time window, and a projection of stored values under a required cut. A
  translation into PQL rather than a second engine: both surfaces resolve through one planner,
  so a schema mistake answers with the same code either way.
- ClickHouse's spellings where they mean something here: `countIf`/`sumIf`/`minIf`/`maxIf`/
  `avgIf` are `FILTER (WHERE ...)` under another name, `PREWHERE` selects what `WHERE` selects,
  and a bare boolean column is a predicate. `uniq` and its five approximate cousins map to the
  **exact** distinct count and `topK` to the **exact** ranking - the plans behind them are a
  grouping and a ranking rather than a sketch.
- A select list may hold several aggregates. Each is a plan of its own, fanned out and merged as
  if it had been written alone, and the row is assembled at the coordinator - so `big-cluster`
  gained nothing for any of it. Capped at sixteen distinct plans per statement.
- `UNION ALL` stacks two answers with the same number of columns, named by the first. A plain
  `UNION` removes duplicate rows, which here would mean comparing two rendered answers, and is
  refused.
- `GROUP BY a, b`: not a composite key, but one grouping per value of the left column. The plan
  carries that bound and the executor refuses past it rather than truncating.
- Exact quantiles, by search rather than by plan: the bound moves until the count at or below it
  lands on the rank, and every step is an ordinary `Count`. One round trip per step; this holds
  no values in memory where ClickHouse's `quantileExact` holds every one it saw.
- An inner join between two tables on a keyed column, with each table's own `WHERE`. For each
  key both sides hold, the join is the Cartesian product of the records holding it, so every
  aggregate over it is arithmetic over per-key numbers an ordinary grouping already produces -
  two ordinary plans and a shape, and the arithmetic happens after both sides have merged.
- Time windows: `visit = 'home' AND visit BETWEEN <seconds> AND <seconds>`, answered from the
  views by day a time quantum field writes.
- Selecting stored values, previously refused. `SELECT amount, price FROM t WHERE ... LIMIT 100`
  reads bit-sliced columns back under a required `LIMIT` of 1 to 10000.
- `HAVING`, `OFFSET`, `WITH TIES` and every ordering `TopN` cannot carry are applied by the
  coordinator after the merge, in the order SQL specifies. A `HAVING` threshold on a decimal
  column is converted to stored units by the planner's own conversion.

### Fixed before release

- **A window over a keyed column with no views by time answered with the empty set.**
  `FieldClass` collapsed `Set`, `Mutex` and `TimeQuantum` into one, so `Row(f="k", from=, to=)`
  resolved against any keyed field - and `Rows::KeyBetween` reads views only a time quantum
  field writes. Against a plain set field there are none, so the read returned no records:
  indistinguishable from a window that genuinely matched nothing. Reachable from PQL as well as
  from SQL. The class now carries which kind it is and the planner refuses the rest by name.
- **A time quantum field could never be given a time.** It could be created and filled over
  HTTP, and every fact landed as a plain key - `Fact` had no `Time` variant - so the day views
  were never written and a window had nothing to read. The import line now spells one
  `key@seconds`.
- **A statement's timeout did not bound the statement.** Plans run in sequence and each was
  handed the full wall-clock budget, so a sixteen-plan statement could hold a worker and a
  socket for sixteen times the configured timeout.
- Five field kinds: bit-sliced int, signed, set, mutex and time quantum.
- Grouping walks each fragment once rather than once per distinct value, and counts each row's
  overlap with the filter without building it. `Distinct`, `TopN`, `GroupBy`, `count(DISTINCT x)`
  and joins all run on that loop; it is 3-5× faster on a laptop, and the multiple grows with the
  corpus because what changed is the shape of the loop rather than its constant.
- Read fan-out across threads for queries with enough fragments to pay for the spawn.
- Per-query memory ceiling, wall-clock timeout, and cooperative cancellation.

### Client

- `bigc`: a command-line client for a running `bigd`. One subcommand per public route - `sql`,
  `query`, `records`, `import`, `delete`, `schema`, `create`, `drop`, `verify`, `repair`,
  `health`, `ready`, `metrics` - plus `shell`, a loop over both query surfaces with `sql>` and
  `pql>` prompts and no guessing between them.
- Output follows its destination: aligned columns to a terminal, TSV to a pipe, `--format`
  overrides both, and `--format json` prints the server's body untouched.
- Exit codes are part of the surface: `0` answered, `1` refused, `2` usage, `3` nothing
  listening.
- The client links no engine crate. It cannot validate a statement or answer one offline, so a
  refusal reaches the terminal as the server's own code and sentence.
- A bearer token is read from a mode-600 file and never taken as a flag.

### Edge

- `bigd`: HTTP/1.1, a fixed worker pool with a bounded queue and `503` past it, read and write
  timeouts on every socket.
- Bearer tokens with `read` / `write` / `admin` roles from a mode-`600` file. Binding anywhere
  but loopback without one is refused unless `--insecure-no-auth` is passed.
- `/health`, `/ready` and Prometheus `/metrics`.

### Clustering

- **`bigd --cluster <file>`**: more than one node. Static shard ownership from a `cluster.toml`,
  one named schema leader, and a coordinator on every node - the one that received the request
  plans the query, fans the *plan* out to each owner, and merges what comes back. Ranges must be
  disjoint and total; a gap or an overlap is a startup failure naming the shards, because there
  is no protocol here that could resolve one safely. `--node <name>` says which entry this
  daemon is, defaulting to the one whose address it was told to bind.
- A daemon **without** `--cluster` is a cluster of one, running the same coordinator. `Server`
  holds a `Cluster`, never an `Api`, so the un-clustered configuration is the general one with
  no peers rather than a second path nobody tests.
- **Row keys route to the schema leader.** A node is told what a key means and refuses a mapping
  that contradicts one it already holds. Hashing keys to row ids and per-node id ranges were
  both rejected, the first because dense row ids are what the storage layer charges for and the
  second because its failure mode is two row ids for one string, which nothing downstream can
  notice.
- **An unreachable owner fails the whole query** - `503`, `owner_unreachable`, and the shard
  range. Never a partial answer: a count missing one node's contribution looks exactly like a
  correct count.
- A batch spanning two owners is two commits. If one lands and another does not, the response
  says which (`partially_applied`, `500`). There is no transaction across nodes and there is no
  documentation claiming otherwise.
- **Replicas, and a range that fails over on its own.** `replica = "a"` says this node holds
  exactly what `a` holds. Every write goes to every copy and every read goes to the one
  serving the range; when that one stops answering, the nodes agree on another and the range
  keeps working, in about a second rather than after somebody edits a file.
- **The CAP choice is CP, and it is written down.** An answer here is an aggregate, so a stale
  one is a wrong number with no symptom, while a refusal is an outage that lasts as long as a
  failover. `docs/clustering.md` opens with the reasoning and what a partition costs each way.
- **A write that cannot reach a copy stands** and names it, rather than failing. Refusing would
  mean one machine nobody reads from can stop a range being written to - a hole failing over
  does not plug. What makes it safe is that the agreement marks that copy **behind**, and a
  copy that is behind will not be promoted, so nothing ever reads from a copy that missed a
  write. If every other copy of a range is behind, the range does not move and says so.
- **`POST /repair`** catches those copies up and clears the mark, without stopping anything. It
  moves the schema, then the row keys, then only the fragments that differ - and the comparison
  is by cardinality, which under this design is a *proof* rather than a hint: a copy that is
  behind holds a subset, and a subset with the same count is the same set.
- **A node that loses touch with the agreement stops serving its range** before anything could
  be promoted in its place, so two nodes never answer for one range. A range with no copy is
  never fenced: nothing could take it away.
- **A cluster with a replica needs three nodes**, and is refused otherwise. A majority of two
  is two, so a cluster of two can never use its copy - which is strictly worse than the same
  two machines without one.
- The agreement is Raft over **one small value**: which copy serves each range, and which are
  behind. Not the facts. Its log gets one entry per election and one per machine that dies, and
  the protocol itself has no I/O and no clock - so five nodes, a partition and a returning
  leader are a test that runs in one process against a clock the test controls.
- **`GET /verify`** asks every copy of every range for a digest of everything it holds and says
  whether they agree. Built out of logical answers - a count per table, every row of every keyed
  field, a total per integer field - rather than bytes on disk, because two healthy copies have
  different files and the same facts. A copy that did not answer makes `agree` false:
  unreachable is not agreement.
- Seven `/internal/` routes carry the fan-out, with a binary encoding that reuses the leaf
  page's own container layout rather than inventing a second one. They need the same roles their
  public counterparts do; a peer is a client with a token, not a trusted origin.
- **Keep-alive**, opt-in on both sides. A client that sends `Connection: keep-alive` gets a
  persistent connection and the fan-out between nodes reuses one per peer; a client that says
  nothing gets one request per connection exactly as before, because a persistent connection
  holds a worker and the default has to be the one that cannot starve a fixed pool. The server
  closes rather than let more than half the pool sit waiting.
- Not built, and each for a stated reason: rebalancing, cross-node atomicity, cluster-wide
  snapshots, quorum reads and writes, membership changes at run time, and repair in the
  background. See [docs/clustering.md](docs/clustering.md).

### Operating it

- **A build mismatch between nodes is refused, not mis-read.** Every request between nodes
  carries the wire version, checked before any body is decoded. Two builds that disagree about
  the encoding would read each other's messages as something else - a length where a tag was -
  and produce an answer that is quietly wrong rather than a failure.
- **Two nodes reading cluster files that disagree are refused too.** Every request carries a
  fingerprint of the file - names, addresses, ranges, who copies whom, who leads - so the
  failure that used to be listed as *undetectable* is now a `409` naming both. Two nodes that
  never speak are still undetectable, and that is the only gap left.
- **`/ready` reports the build and the wire version**, which are different questions: two nodes
  with different builds and the same wire version run side by side, and two with different wire
  versions do not.
- **`checklist.md` is in the repository.** It was linked from the README and from here and was
  ignored rather than written. It says what production would still need and, for each gap,
  whether it is **missing** or **refused** - because those are different, and only one of them
  is a backlog.
- The image ships `big` as well as `bigd`. An image with no way to take a backup is an image
  whose backups happen somewhere else.

- **`/metrics` reports the cluster**: how many copies are behind (the one to alert on -
  redundancy this cluster has lost and will not get back until a repair runs), whether this
  node is currently allowed to serve its range, the agreement's term, and peer requests split
  into unreachable and refused, because an operator's next move differs.
- **A silent peer can no longer hold a worker.** A request with no deadline had no socket
  timeout at all, so a peer that accepted a connection and then stopped - a wedged process, a
  dropped route - kept a coordinator's worker until somebody restarted it. Requests with no
  deadline now get an inactivity timeout, which is a different thing: it bounds silence rather
  than the exchange, so a large batch that takes a minute of commit is not cut off.
- **Containers**, in [deploy/](deploy/): one compose file for a single node and one for three,
  the same binary and the same code path either way. `bigd` still refuses to bind anywhere but
  loopback without a token file, which is why both mount one - and the entrypoint stages
  credentials into a private directory inside the container, because a bind mount does not
  always carry the host's file mode and weakening the check would be the wrong fix.

### Testing and release engineering

- The agreement's **shipped clocks are tested**, three ways: the relationships that make them
  safe are compile-time assertions next to the constants, the numbers themselves run on the
  simulator's virtual clock, and one failover runs on them over real sockets.
- A node that **restarts and reads its agreement state back** is tested end to end, which is
  the half the encoding tests could not cover: every other test uses a store that keeps
  nothing, so nothing exercised the wiring.
- The one case the design cannot detect - the copy serving a range coming back with a replaced
  disk - is pinned by a test rather than left to be discovered. It refuses queries rather than
  answering them, nothing is marked behind, and `GET /verify` is what finds it.

- `rustfmt.toml` pinning the house style, and a `cargo fmt --check` gate in CI.
- Four fuzz targets in the workspace `fuzz/` crate - page parsing, PQL parsing, container
  algebra against a `BTreeSet` oracle, and a b-tree program against a `BTreeMap` - built on
  every push and run weekly.
- Concurrency tests: the read fan-out against hand-computed answers, and the reclaim horizon -
  a live reader keeps every page it can reach, which is the invariant the no-WAL design rests
  on and which had no test.
- A readme per crate, `deny(missing_docs)` on the two published ones, and a `cargo doc` gate
  with `-D warnings`.
- `Api::db()` moved behind an `unstable` feature; every type in `big-api`'s signatures is now
  nameable from `big-api`.

### Performance

- **`Ingest::with_flush_fraction`**, off by default. Commits only the fullest fraction of the
  buffered shards and lets the rest accumulate, so a fragment is committed having gathered
  records from several buffer-fulls rather than one. At `0.25`: 1.4x fewer bytes at 64 and 256
  shards, 1.5x at 1,024, and free on a dense workload. It buys bytes with commits - about
  `1/fraction` times as many fsync pairs - so it is a knob rather than a default: at 64 shards
  it costs 1.2x in wall clock, at 4,096 it saves 1.4x.
- A per-container delta with leaf merging, which improved **dense** write amplification 1.7x at
  batch 100. It does nothing for sparse ids, whose containers are never dense enough to have a
  page of their own - the cost there is a fixed ~4-6 page path rewrite per fragment a commit
  touches, which is a scheduling problem and not a layout one.

### Fixed

- **One aborted connection could stop the server.** `accept` returning `ECONNABORTED` - a client
  that goes away between the handshake and the accept, which is exactly what a coordinator does
  to a peer it has given up waiting for - was treated as the listener failing, and the whole
  server stopped answering. Transient accept errors are now skipped.
- **`/schema` did not report a decimal's scale.** A decimal without its scale is an integer
  wearing a different name: `price > 5` means `> 500` on a field with two of them, and a client
  reading the schema could not know that. `scale` and a time quantum's `granularity` are now
  part of the field description.
- **The query parser could crash the daemon.** `big-plan`'s recursive descent had no depth
  limit, so a nested query - `Count(Union(Union(...` at ten thousand levels, seventy kilobytes,
  well inside the eight megabyte body cap - overflowed the stack and *aborted the process*. A
  stack overflow is not a panic: it cannot be caught, cannot be turned into a `Result`, and on
  `bigd` took every other in-flight request down with it. Any client able to POST a query could
  do it. Nesting is now capped at `big_plan::parse::MAX_DEPTH` (128) and refused with a
  `query_too_deep` / `400`. Found by the fuzz target written for it.

### Known gaps

The largest: no rebalancing - changing a range still means stopping a node and copying a file -
and the benchmark has never been run on hardware that is not a shared droplet — which
*understates* the handicap `big` carries by fsyncing twice unconditionally.
