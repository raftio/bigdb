# Comparative benchmarks

Not part of the engine. This crate exists to pull in rival engines and measure `big` against
them, which is why it sits outside `crates/` and never goes to crates.io.

Two comparisons, not one.

**The storage comparison** puts **`big-db`** against **`redb`**, **`lmdb`**, **`fjall`** and
**`sqlite`**, and asks what a commit costs and what a file weighs.

**The analytical comparison** puts it against **`duckdb`**, **`datafusion`** and
**`clickhouse`** — the engines sold into the same market rather than the ones built out of
the same parts — and asks what the engine *answers*: intersections, group-bys, top-n, distinct
counts and sums over a predicate. `big` describes itself as a bitmap-native analytical database,
and until this table existed the only analytical question in the benchmark was `count_ge`.

They share a workload generator and a rule, and nothing else. Each has its own binary.

**A third, which is not a comparison.** `profile_load` asks where the time in a load goes, and
it is the one to run before optimising anything: a load's wall clock is the caller's fact
building, the buffering, the commit and the fsyncs added together, and those four respond to
entirely different fixes. It moves one of them per line, so the gaps between the lines are the
answer rather than the rows.

Everything here is driven by one script, so a run on a fresh box is one line:

```sh
./scripts/bench env                  # the machine, and whether the disk has room
./scripts/bench load 10000000        # where the time in a load goes
./scripts/bench ab   10000000        # the same, working tree against HEAD
./scripts/bench storage
BENCH_PEERS=olap-peers ./scripts/bench olap 1000000
```

Each command writes its report to `bench/results/` with the machine recorded above it, because
a benchmark number without the machine under it is not a result.

**Both are single-node, and neither is a measurement of distribution.** Every engine here — `big`
included — is asked from one process against one file. There is no shard fan-out, no replication,
no merge and no network in any number this crate produces, and `big serve --cluster` is never started.
That keeps the comparison honest, because every peer is single-node too, and it also means the
clustered path is something this crate deliberately does *not* speak to: a fanned-out query pays
a plan encode, a round trip per owner and a merge, and none of those costs is on any table here.
That path is covered by tests instead — `crates/big-cluster/tests/` and
`crates/big-http/tests/cluster.rs`.

They are not four versions of the same question. `redb` and `lmdb` are the architectural peers —
copy-on-write B-trees over a memory-mapped file with a single writer — so a difference against
them is an engineering difference rather than a difference of genre. `fjall` is the LSM, and is
there precisely to show where the genre matters rather than the implementation. `sqlite` is the
odd one out and earns its place by it: the only entrant with a query planner, and the only one
whose range query is answered by an index the engine chose to use rather than one this harness
built by hand.

RocksDB has no adapter. It is the second LSM, and `fjall` already answers what an LSM answers
here; the C++ build it needs was not worth carrying for a second data point of the same genre.

FeatureBase has no adapter either, and that is the more painful of the two omissions: it is the
only engine that would have been the same idea as `big` rather than a rival to it — a roaring
b-tree over fixed-size pages with a bit-sliced index on top, the format it ships as RBF. The
project stopped in 2023, publishes no static binary, and its last image runs only under Docker,
while the isolation every number here depends on is *Docker stopped*. An adapter that could never
be run against a live server would be unverified code implying otherwise, so there is none.

## Running

```sh
cargo run   -p big-bench --release --bin report   # space, write amplification, correctness
cargo bench -p big-bench                          # latency and throughput

# As root, so the cold-read column is a cold read rather than a reopen.
sudo -E cargo run -p big-bench --release --bin report

# The analytical table. Peers are behind features because of what they cost to build, not
# because they are optional to the argument: DuckDB bundles a C++ source tree and DataFusion
# brings the whole Arrow stack, and neither belongs in every `cargo test` CI runs.
cargo run -p big-bench --release --features olap-peers --bin olap
cargo run -p big-bench --release --features olap-peers --bin olap -- 1000000

# What the query front end costs, with no storage under it. Needs no peers and no isolation:
# it never touches a file, so it is the one measurement here a loaded host cannot spoil.
cargo run -p big-bench --release --bin frontend
```

The analytical table carries a **`big-sql`** column: the same engine over the same file, asked
in SQL instead of PQL. It is there to retire an argument rather than to rank anything - `big`
was being handed its own query language while every rival parsed SQL, and it was fair to wonder
how much of its margin was simply a parser it did not run. The two columns turn out to differ by
less than this host's run-to-run noise, which is a non-answer, so `--bin frontend` answers it
properly: **all six questions plan in 9.8µs through PQL and 24.2µs through SQL, against roughly
21,000µs of query time - the entire front end is 0.12% of a run, and the gap between the two
languages is 0.069% of it.** There was never room in the numbers for a parser to matter.

ClickHouse is started by hand and the harness never starts it, nor stops it: if nothing is
listening it is named in a *Not run* list with the address it was looked for at, rather than the
run failing.

```sh
# A static binary rather than a container. The report's isolation method is to stop Docker before
# measuring, so hosting a benchmark peer in Docker would contradict the run it is part of. The
# binary runs with Docker still down.
clickhouse server &                              # :8123, or set BIG_BENCH_CLICKHOUSE
```

The report takes a few minutes, most of it `big` ingesting. `cargo bench` runs the read sweep to
two million records and takes considerably longer.

The charts in the top-level `README.md` are rendered from the same numbers by a standalone
script into `docs/img/`:

```sh
python3 bench/charts/generate.py               # stdlib only, no dependencies
```

It reads its numbers from a table at the top of the script rather than from a run, so a new run
means editing `results/REPORT.md`, then that table, then re-rendering. Each chart is one file
on a transparent ground rather than a light/dark pair: an `<img>` cannot see the theme of the
page hosting it — `prefers-color-scheme` reports the OS setting — so a `<picture>` swap shows
the dark variant on a light page whenever the two disagree. Every ink is instead a mid-tone
that clears 3.6:1 on both GitHub surfaces.

## The rules this harness enforces

**Same durability.** Every engine defaults to something different, and letting each pick its own
would hand whichever is most honest a large handicap for being honest. The trait takes durability
as a parameter, every engine is measured at both settings, and the report prints which was used.
`full` means the commit survives power loss; `relaxed` means it survives the process and not the
machine. `big` used to implement only `full` and the harness said so rather than hiding it; it
now has a knob, and the gap between its two lines is what that knob is worth.

**Same question, verified.** Every engine's answer is checked against a ground truth computed
without any engine. A benchmark that measures a wrong answer measures nothing.

**Same index.** `count_ge` asks about values, which no primary key-value table can answer
without reading all of it. `redb` therefore maintains a secondary `(value, id)` index and pays
for it on every ingest, exactly as a real user would. Calling the resulting full scan "redb's
range query" would be rigging the result.

**A curve, never a number.** Write cost is reported against batch size, because a single batch
size can hide or manufacture almost any conclusion.

**One table per process boundary.** ClickHouse answers over a socket and every timing it reports
includes a round trip that no in-process engine pays. Ranking it in one column against `big` and
DuckDB would measure the socket as much as the engine, so it gets its own table, the round trip
is measured on its own and printed as a row, and `big` appears in both tables as a scale rather
than as a competitor. The same boundary is why the three rules above are enforced only inside the
storage comparison: a server the harness did not start has no durability setting the harness can
match, and pretending otherwise would be worse than splitting the table.

**No answer without a checker, in either table.** The analytical questions are harder to get
right than `count_ge`, so the ground truth for all six of them — the intersection, the sum, the
group counts, the top-n, the distinct count — is computed with plain iterators in
`src/wide.rs` and every engine is checked against it before a single timing is kept. The
workload is built so those answers are unique: `TopN` orders by count, and every engine here
breaks a tie differently, so the category frequencies are made strictly distinct by
construction. A benchmark that cannot say which answer is right cannot say an engine got it
wrong.

## What it cannot do honestly

- **Write amplification for anything but `big`.** `big` reports exact bytes through
  `CountingPager`; the others would need OS-level tracing, so they report `n/a` rather than an
  estimate.
- **A quiet machine.** Treat the timed half as a comparison run back to back, never as a fact.
  Single-shot timings are medians of three, which discards one descheduled run and nothing more.
- **A size for a server, in general.** ClickHouse can state the bytes its own table occupies and
  does. An engine that could not would get `n/a` rather than the size of a data directory holding
  system tables and logs belonging to a server the harness did not start.
- **A cold read, unless it is run as root.** The harness closes and reopens the engine before the
  cold measurement, which always works and drops the engine's own mapping or block cache. It then
  tries to drop the OS page cache, which needs root. The report prints `cold` when it managed
  both and `reopened` when it managed only the first, rather than putting a warm number under a
  cold heading.

## The operating envelope

What the measurements above actually support, stated so a user does not have to infer it.

**`big` is at its best when:**

- **Ids are dense or append-shaped.** This is the axis that matters most and the one its rivals
  do not have. The same ten thousand records cost `19` bytes each in one commit when the ids are
  consecutive and `110` when they land across sixty-four shards — and at batch size 1 the gap is
  `338,615` against `67,309`. Records **per fragment per commit** is the number to reach for
  first, and the capacity sweep in the report is the table for choosing it. It is not the whole
  model: two configurations can agree on it and still differ tenfold, because what a fragment
  rewrite *costs* depends on how dense that fragment is. A fragment holding tens of thousands of
  records has containers dense enough to own a page each, which copy-on-write rewrites in full;
  one holding a few hundred keeps them inline in a leaf. The report derives this from its own
  numbers rather than asserting it, and `where a commit's pages go` is the section that measures
  it directly.
- **Commits are large.** Not batches — commits. A copy-on-write engine rewrites every container
  it touches in full, and a container costs the same whether one bit in it changed or ten
  thousand. `Db::ingest(capacity)` exists so that a caller handing over small batches still
  commits large ones, and `Db::bulk_load` exists for a first load whose size is known.
- **Queries are set-shaped.** Counting, intersecting, grouping and range predicates over large
  result sets are what a bit-sliced index is for: it touches a bit plane once and then counts
  whole words. The `count_ge` sweep in the report is where the crossover against an ordered index
  sits.
- **Space matters.** `big` compacts to a fraction of what the others occupy for the same data,
  and deleting is clearing bits rather than removing keys from two indexes — which is why its
  removal row is the fastest here by a wide margin.

**Reach for something else when:**

- **Writes arrive one at a time and must be durable one at a time.** A commit per record is the
  worst case for this engine by four orders of magnitude, and no amount of tuning changes the
  shape of it.
- **The workload is point lookups.** A BSI read reconstructs a value from one bit plane per bit,
  where a B-tree does one descent. That is a design consequence, not a regression.
- **Ids are sparse and unbuffered.** If records genuinely cannot be accumulated before being
  committed — and `ingest` cannot do it for you because they must be visible immediately — then
  the sparse penalty is paid in full.
- **You need more than one writer, or more than one node.** Neither is in scope; see
  `architecture.md`.
