# Versioning

## What is guaranteed

Two things carry a compatibility promise. Everything else in this repository is an
implementation detail that happens to be visible.

### 1. The published crates

**`big-embed`** and **`big-http`** are the API. They follow [SemVer](https://semver.org): after
`1.0.0`, a breaking change to either takes a major version.

Every type that appears in a `big-embed` signature is re-exported from `big-embed`, and every
type in a `big-http` signature from `big-http`. That is not a convenience — a type a caller
cannot name is a type they cannot hold — and it is what makes the guarantee mean anything.

**They are the same database reached two ways.** `big-embed` is big in your own process;
`big-http` is big over a socket. Neither is a layer above the other in the sense a caller has to
care about — pick by where the code runs, not by what it can do.

**Both query surfaces carry the same promise, and it is `big-http`'s.** A statement that
`POST /sql` accepts today is accepted by the next release, and so is a query `POST
/table/{t}/query` accepts - they are one guarantee because they are one planner. A *refusal*
carries the weaker half of it: the code a refusal answers with will not change meaning, but a
construct that is refused today may be answered later, which is a widening no client can be
broken by. `big-sql` itself is an internal crate and its types are free to move; what is
promised is the statements and the codes, not the AST behind them.

**`bigctl`'s command surface follows `big-http`.** A subcommand that works today works next
release, and the exit codes do not change meaning: `0` answered, `1` refused, `2` usage, `3`
nothing listening - a script branches on those. `--format tsv` is stable because scripts parse
it. **Table rendering is not promised**: it is for a person looking at a terminal, and a column
width is not an interface. The `big-bin` *library* is internal like the rest.

**Nor is the wording of a refusal, and that came due when the parsers moved to `clap`.** Every
subcommand that worked before works now and the four exit codes are unchanged, so a script that
branches on a code still branches correctly. What changed is what a mistake *says*:
`bigctl: unknown option --nope` is now `error: unexpected argument '--nope' found`, often with a
did-you-mean. A script matching on those strings was matching on something that was never
promised - match the exit code. Three things also widened, which no caller can be broken by:
`--flag=value` is accepted everywhere, `-V`/`--version` exist, and `bigctl help <command>` works
rather than exiting 2.

The one narrowing: a load's own flags must now follow their subcommand.
`bigctl --dry-run import t f` is refused; `bigctl import t f --dry-run` is not. Those flags mean
nothing to any other command, and accepting them anywhere was what let `bigctl schema --dry-run`
be silently ignored. The *client's* flags - `--addr`, `--format`, `--timeout`, the credential
ones - are still accepted on either side of the subcommand.

**The binaries were renamed before 1.0, and that broke the promise above once.** Four became
two: `bigd` is `big serve`, `bigc` is `bigctl`, and `bigi` is `bigctl import`. The subcommands,
the flags and the four exit codes came through unchanged, so a script that branches on a code
still branches correctly - what it has to change is the name it invokes and, for a load, the
fact that `import` now reads a *file* rather than taking the facts on the command line. That is
the whole migration:

| was | is |
| --- | --- |
| `bigd data.big 127.0.0.1:7654` | `big serve data.big 127.0.0.1:7654` |
| `bigc sql "..."` | `bigctl sql "..."` |
| `bigi import t facts.txt` | `bigctl import t facts.txt` |
| `bigc import t "field 1 GB"` | write the facts to a file, or pipe them with `-` |

A load's summary also moved: the fact count is on stdout, rendered like any other answer and
honouring `--format`, and the loop's own numbers - lines, chunks, bytes, elapsed - are on
stderr with the progress. A pipe now carries the count and nothing else.

**One published crate was renamed before 1.0 as well: `big-api` is `big-embed`.** Nothing had
been published when it moved, so no `Cargo.toml` outside this repository ever named the old one.
The name was the whole defect: `big-api` read as the layer that speaks to a network, which is
`big-http`'s job and was never this crate's - the module doc had to say so in prose because the
name said the opposite. Nothing else moved. The types, the re-exports and the `unstable` feature
came through untouched, so the migration is two lines:

| was | is |
| --- | --- |
| `big-api = "0.1"` | `big-embed = "0.1"` |
| `use big_api::Api;` | `use big_embed::Api;` |

No name will be reused for something different, and there will be no further renames before
1.0 without an entry here.

### 2. The on-disk format

A file written by one release opens in the next. Where the format has to change, the change
arrives with a migration, and the migration is not a special code path: backup, full compaction
and format migration are the same walk over every page reachable from a consistent set of roots
(`big-db::copy`), so a migration is exercised by every backup test in the suite.

## What is not guaranteed

**The thirteen internal crates.** `big-bin`, `big-btree`, `big-civil`, `big-cluster`,
`big-container`, `big-db`, `big-engine`, `big-exec`, `big-keys`, `big-page`, `big-pager`,
`big-plan`, `big-sql`.

They are on crates.io because a published crate cannot depend on an unpublished one, and
`big-embed` and `big-http` depend on most of them between them — **that is the only reason.**
Their shape is free to change in any release. Depending on one directly means pinning an exact
version and reading the diff before every upgrade.

**`contrib/`.** `big-message` and `big-message-redis`. These are written *against* `big-http`'s
promise rather than being part of it: they are clients, and they hold no state a release could
migrate. `big-message`'s `[dependencies]` section is empty and `big-message-redis` holds exactly
that one entry, so "these cannot reach the engine" is checkable rather than asserted. They are
`publish = false` and carry no promise of their own — what they depend on is the HTTP surface
above, and that is the guarantee to read.

**`big-embed`'s `unstable` feature.** It exposes `Api::db()`, which hands out the raw `Db` from a
crate with no guarantee. Turning it on is an explicit opt-out of everything above.

**Anything before `1.0.0`.** While the version is `0.x`, a minor bump may break the API. The
on-disk promise holds from the first release regardless, because the cost of breaking it is paid
by whoever already has data.

## MSRV

`rust-version = "1.88"`, asserted by the `msrv` job in CI on every push. Raising it is a minor
version bump, not a patch.

A declared `rust-version` with no job behind it is a comment rather than a promise, which is
why the job exists rather than the field alone.

## Releasing

Versions move together. Fifteen crates in one workspace at one `workspace.package.version`, so
there is one number to reason about and no possibility of a partial release where `big-embed`
`0.2` sits on a `big-db` `0.1` that no longer means what it did.

Publish order follows the dependency graph, leaves first:

```
big-container  big-civil
big-page
big-pager  big-keys
big-btree  big-plan
big-engine  big-sql
big-db
big-exec
big-embed
big-cluster
big-http
big-bin
```

Five members are `publish = false` and are no part of the product: `big-bench`, which pulls in
rival engines to measure against, `big-e2e` and `big-testfile`, which exist only to test, and
`contrib/big-message` and `contrib/big-message-redis`, which are clients written against the
published HTTP surface.

### Checklist

1. Write the release notes. There is no `CHANGELOG.md` yet: nothing has been released, so
   every entry one could hold would describe the first release rather than a change to it.
   Whoever cuts that release writes it then, out of the git log.
2. `workspace.package.version` in the root `Cargo.toml`, and the `version` on each entry in
   `[workspace.dependencies]` beside it. Those are the only two places a version is written.
3. CI green, including `fmt`, `clippy`, `docs`, `msrv` and the crash tests.
4. `cargo publish` in the order above.
5. Tag `v<version>`.

`cargo-semver-checks` runs in CI against `big-embed` and `big-http`. It has nothing to compare
against until the first release, and is wired in now so that it does from the second.
