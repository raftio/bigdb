# bigdb

A distributed bitmap-native analytical database.

A b-tree of roaring containers over 8 KB pages, pure copy-on-write, with the read path borrowing
straight out of the mapped file. Every fact is one bit at `(row, record)`, so a filter is an
intersection and a count is a population count rather than a scan.

It ships as two binaries: `big`, the daemon and the offline file tools, and `bigctl`, the client
that talks to a running daemon over HTTP.

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
deploy/            single-node and cluster deployments
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
