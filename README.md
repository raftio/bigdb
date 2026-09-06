A distributed bitmap-native analytical database.

A b-tree of roaring containers over 8 KB pages, pure copy-on-write, with the read path borrowing
straight out of the mapped file. Every fact is one bit at `(row, record)`, so a filter is an
intersection and a count is a population count rather than a scan.

It ships as two binaries: `big`, the daemon and the offline file tools, and `bigctl`, the client
that talks to a running daemon over HTTP.

## Use cases

Questions whose answer is a number over a great many records, asked again and again with a
different filter each time.

- **The numbers behind a dashboard.** A BI panel is the same handful of aggregates re-asked with
  whatever the reader clicked: `count`, `sum`, `avg`, a `GROUP BY`, a top ten. `EXPLAIN` says
  what a statement will do before it does any of it, `--query-timeout` puts a wall-clock ceiling
  on it, and `--format table|tsv|json` decides how the answer comes back.
- **Segments and audiences.** `country = 'GB' AND plan = 'pro' AND NOT churned` is three bitmaps
  intersected and one population count. A condition added is another intersection, not another
  pass over the records, so what a filter costs follows the containers it touches rather than
  the number of records the table holds.
- **Event and product analytics.** A time quantum field writes a per-day index beside the fact,
  so `WHERE visit = 'home' AND visit BETWEEN 1767225600 AND 1769904000` reads the days it needs
  and no others. Retention is `big drop-days`, which drops those days from the index and leaves
  the records where they are.
- **A counting engine beside a system of record.** The database that owns the rows keeps owning
  them; this one answers *how many*. A record id is the address a bit is written at rather than
  a key this side invented, so it can be the id the source already has, and the same import run
  twice writes what it wrote once.
- **Realtime, from a program that is running.** [`contrib/big-message`](contrib/big-message/)
  takes events as they are produced — no file to point at, and no ids of its own to invent — and
  a commit publishes its roots and its catalog together, so a fact is answerable by the next
  query rather than by the next batch. There is no index to rebuild and nothing to replay after
  a crash. [`examples/producer/`](examples/producer/) runs it in one command.
- **A serving layer downstream of a stream or a warehouse.** Loads are chunked, resumable and
  safe to re-run, which is what makes a replay from upstream converge rather than double-count.
  Past one machine, each node owns a range of shards and any node plans the whole query, so a
  table wider than a host still answers with one number.

**What it is not for.** Reading rows back one at a time: a record is bits spread across fields,
not a row sitting somewhere. Atomicity across nodes: there is none, by decision —
[docs/clustering.md](docs/clustering.md) says why, for that and for every other thing a cluster
here does not give you. There is no Postgres or MySQL wire protocol either, so a BI tool reaches
it over HTTP or through the `database/sql` driver in [`clients/go`](clients/go/), not through an
ODBC driver it already ships. `UPDATE`, `DELETE FROM` and joins beyond a single equi-join are
refused by name, before anything runs, with a sentence saying what exists instead.

## Features

| | |
|---|---|
| **Storage** | A b-tree of roaring containers over 8 KB pages, pure copy-on-write. **No WAL, no checkpoint, nothing to replay after a crash** — a commit writes pages, fsyncs, flips the meta page, and fsyncs again. |
| **Reads** | One writer and many readers, each on the transaction it opened. The read path borrows straight out of the mapped file and takes no lock. |
| **Fields** | `set`, `mutex`, `bool`, `int` at a chosen bit depth, signed `int`, `decimal`, `float32`, `float64`, `date`, `datetime`, `timequantum`. Everything but a set is bit-sliced, so a `sum` or a `min` is a walk over planes rather than over records. |
| **SQL** | `SELECT` with `WHERE`, `GROUP BY`, `HAVING`, `ORDER BY`, `LIMIT`, `UNION ALL`, one equi-join on a keyed column, plus views, `INSERT`, DDL, `SHOW`, `GRANT`/`REVOKE` and `EXPLAIN`. `count`, `count(distinct)`, `sum`, `min`, `max`, `avg`, `median`/`quantile`, and the usual scalar functions. |
| **PQL** | `Count`, `Sum`, `Min`, `Max`, `Distinct`, `TopN`, `GroupBy`. The SQL surface lowers into exactly these, through the same planner, so neither can express what the other cannot. |
| **Cluster** | Ranges of shards over many nodes, every node a coordinator. A replicated range fails over on its own in about a second; `--balance` lets the cluster move ranges as nodes join, fill and drain. CP, deliberately. |
| **Front door** | `bigproxy` puts one address in front of a cluster, sends each request to the least busy node that reports ready, and carries nothing outside a list of routes written down in advance. See [docs/proxy.md](docs/proxy.md). |
| **Write path** | One writer at a time, and `--write-coalesce` lets concurrent writers share a transaction and therefore one pair of fsyncs. `--write-async` offers `?ack=queued`, answered when the facts are held rather than when they are committed — a different promise, and [runbook.md](runbook.md) says exactly what it costs. |
| **Durability** | `--durability full`, `barrier` or `none`. A checksum on every page it can reach and `big scrub` to walk them, `big backup` and `POST /admin/backup` while serving, `big compact` offline and `--reclaim` online to give space back to the filesystem. |
| **Access control** | argon2id passwords over TLS, roles that are sets of grants rather than ranks, and certificates rather than shared secrets between nodes. See [docs/access-control.md](docs/access-control.md). |
| **Live queries** | `GET /watch?sql=` holds a connection open and pushes the answer again whenever it changes — server-sent events over a chunked body. A live query rather than a change feed: the answer here is a number, and the engine keeps no log of which rows moved. Off unless `--watch-max` says otherwise, because each subscriber holds a worker. |
| **Operations** | `/health`, `/ready`, Prometheus `/metrics`, `GET /verify` and `POST /repair` for replication, per-query and per-connection timeouts, a ceiling on row keys. |
| **Clients** | Go, including a `database/sql` driver, and Python in progress — [`clients/`](clients/readme.md). [`crates/big-embed`](crates/big-embed/) is the same engine as a library, with no server in the way. |

## Requirements

Rust 1.88 or newer, stable toolchain. No nightly (except for fuzzing).

## Install

```sh
cargo build --release -p big-bin --bin big --bin bigctl
# binaries land in target/release/
```

Or with the Makefile, which symlinks both into `~/.cargo/bin`:

```sh
make install
```

## Quick start

Start a daemon in one terminal:

```sh
make start                # background, waits for /health
# or: big serve .local/data.big 127.0.0.1:7654
```

Create a table, load some facts and ask a question:

```sh
bigctl create table tx
bigctl create field tx country --kind set
bigctl create field tx amount --kind int --bit-depth 20

printf 'country 1 GB\ncountry 2 US\ncountry 3 GB\namount 1 100\namount 2 250\namount 3 75\n' \
  | bigctl import tx -

bigctl sql "SELECT country, count(*), sum(amount) FROM tx GROUP BY country"
```

`make demo` runs exactly the above. `make stop` stops the daemon, `make clean-data` deletes it.

## The two binaries

### `big` — serve, and offline file tools

```sh
big serve <file> [addr]        # serve over HTTP; addr defaults to 127.0.0.1:7654
big backup <file> <dest>       # consistent copy, safe while a writer runs
big restore <src> <dest>       # copy a backup into place
big compact <file>             # rewrite the file as a compact copy of itself
big verify <file>              # open it and report what it holds
big scrub <file>               # recompute every checksum it can reach
big drop-days <file> <table> <field> <unix-seconds>
```

Every subcommand but `serve` takes the file's exclusive lock, so the daemon must be stopped
first. `big serve --help` lists the flags — tokens, cluster, durability, timeouts, limits.

**Never copy a live database with `cp`, `rsync` or `tar`.** A commit can land between the bytes
the copy has read and the ones it has not. Use `big backup`, or `POST /admin/backup` while
serving. See [runbook.md](runbook.md).

### `bigctl` — ask a running daemon

Every command is exactly one route. There is no offline mode and no `--file`.

```sh
bigctl sql "SELECT ..."              # one SELECT, a write, or a schema change
bigctl query <table> <call>          # one PQL call
bigctl records <table>               # every record id, in order
bigctl shell                         # a loop over sql and query
bigctl schema                        # every table and field
bigctl create table|field ...        # DDL
bigctl drop table|field ...
bigctl import <table> <file>|-       # one fact per line: `field record value`
bigctl delete <table> <file>|-       # one record id per line
bigctl verify | repair               # replication
bigctl health | ready | metrics
```

Options: `--addr` (or `$BIG_ADDR`), `--credentials-file` (or `$BIG_CREDENTIALS`), `--format table|tsv|json`,
`--timeout`. A token is read from a file and never taken as a flag — an argument is visible in
`ps` and in shell history.

Exit codes: `0` answered, `1` the server refused, `2` usage, `3` nothing was listening.

Large loads are chunked, resumable and safe to re-run: every fact is a bit set at the record id
written in the line, so sending a chunk twice writes what sending it once wrote.

```sh
bigctl import tx facts.txt --resume facts.txt.ck
```

## HTTP surface

| | |
|---|---|
| `GET /health`, `GET /ready` | probes, never authenticated |
| `GET /metrics` | Prometheus text; needs `OPERATE` on `*.*` |
| `GET /table/{t}/records?after=&limit=` | records in order, a page at a time |
| `GET /verify` | do the copies of every range still hold the same facts |
| `POST /repair` | catch up every copy that is behind |
| `POST /admin/backup?name=<f>` | online compact copy; needs `--backup-dir` and `OPERATE` on `*.*` |
| `/internal/*` | another node of the cluster, proven by its client certificate |

Auth is a username and a password over TLS. Users live one `username role hash` per line in a
mode-600 file passed to `--users`, written by `big passwd` and by nothing else; the hashes are
argon2id. **A role is a name, not a rank**: what it may do is a set of grants in the catalog,
made with `GRANT SELECT ON sales.* TO analyst` and friends. `superuser` holds everything without
being stored, which is how a fresh database gets its first role. See
[docs/access-control.md](docs/access-control.md).

The daemon refuses a non-loopback bind twice: once with no `--users`, once with no `--tls-cert`.
Each refusal has its own override (`--insecure-no-auth`, `--insecure-no-tls`) because they are
two decisions - terminating TLS at a proxy in front is a supported deployment and must not cost
you your credentials.

Nodes do not use passwords with each other. Each holds a certificate signed by the cluster's
`peer_ca_file`, and the name in it is its name in the cluster file, so a leaked key is one node
rather than the whole cluster.

TLS is a cargo feature (`tls`, on by default for the binaries). Built without it, `big-http` has
the dependency tree it always had, and CI asserts that.

## Layout

```
crates/            the engine, bottom-up: page → pager → btree → engine → db → plan/sql/exec
crates/big-bin/    the `big` and `bigctl` binaries
crates/big-http/   the server
crates/big-embed/  the library API
contrib/           written against the HTTP surface, as an outside user would
deploy/            single-node, compose cluster and Kubernetes deployments
examples/          runnable docker compose stacks
bench/             benchmarks; excluded from the workspace test run
fuzz/              cargo-fuzz targets, nightly only
```

Dependencies flow one way. `big-page` never learns about `big-btree`, and nothing above
`big-pager` touches `unsafe`.

## Development

```sh
make check      # lint, then test, then docs — the gates CI runs
make test       # cargo test, workspace less big-bench
make lint       # cargo fmt --check, then clippy -D warnings
make docs       # cargo doc with warnings denied
make cov        # line coverage per crate (needs cargo-llvm-cov)
make e2e        # both binaries, run as processes
```

`make help` lists everything. See [CONTRIBUTING.md](CONTRIBUTING.md) before opening a patch.

## Docs

- [runbook.md](runbook.md) — backup, restore, undoing an import, what not to do to a live file
- [docs/clustering.md](docs/clustering.md) — what the cluster does and does not give you
- [docs/versioning.md](docs/versioning.md) — what is promised across versions
- [docs/sql-testing.md](docs/sql-testing.md) — the data-driven SQL corpora
- [docs/performance-plan.md](docs/performance-plan.md)
- [deploy/readme.md](deploy/readme.md), [examples/readme.md](examples/readme.md)

## License

Apache-2.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE).
