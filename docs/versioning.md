# Versioning

## What is guaranteed

Two things carry a compatibility promise. Everything else in this repository is an
implementation detail that happens to be visible.

### 1. The published crates

**`big-api`** and **`big-http`** are the API. They follow [SemVer](https://semver.org): after
`1.0.0`, a breaking change to either takes a major version.

Every type that appears in a `big-api` signature is re-exported from `big-api`, and every type in
a `big-http` signature from `big-http`. That is not a convenience — a type a caller cannot name
is a type they cannot hold — and it is what makes the guarantee mean anything.

**Both query surfaces carry the same promise, and it is `big-http`'s.** A statement that
`POST /sql` accepts today is accepted by the next release, and so is a query `POST
/table/{t}/query` accepts - they are one guarantee because they are one planner. A *refusal*
carries the weaker half of it: the code a refusal answers with will not change meaning, but a
construct that is refused today may be answered later, which is a widening no client can be
broken by. `big-sql` itself is an internal crate and its types are free to move; what is
promised is the statements and the codes, not the AST behind them.

**`bigc`'s command surface follows `big-http`.** A subcommand that works today works next
release, and the exit codes do not change meaning: `0` answered, `1` refused, `2` usage, `3`
nothing listening - a script branches on those. `--format tsv` is stable because scripts parse
it. **Table rendering is not promised**: it is for a person looking at a terminal, and a column
width is not an interface. The `big-cli` *library* is internal like the rest.

### 2. The on-disk format

A file written by one release opens in the next. Where the format has to change, the change
arrives with a migration, and the migration is not a special code path: backup, full compaction
and format migration are the same walk over every page reachable from a consistent set of roots
(`big-db::copy`), so a migration is exercised by every backup test in the suite.

## What is not guaranteed

**The fourteen internal crates.** `big-btree`, `big-cli`, `big-cluster`, `big-column`,
`big-container`, `big-db`, `big-exec`, `big-field`, `big-fragment`, `big-keys`,
`big-page`, `big-pager`, `big-plan`, `big-sql`.

They are on crates.io because a published crate cannot depend on an unpublished one, and
`big-api` and `big-http` depend on four of them between them — **that is the only reason.** Their shape is free to change
in any release. Depending on one directly means pinning an exact version and reading the diff
before every upgrade.

**`big-api`'s `unstable` feature.** It exposes `Api::db()`, which hands out the raw `Db` from a
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
there is one number to reason about and no possibility of a partial release where `big-api`
`0.2` sits on a `big-db` `0.1` that no longer means what it did.

Publish order follows the dependency graph, leaves first:

```
big-container  big-page
big-pager  big-btree  big-fragment  big-keys  big-field  big-plan  big-column
big-db  big-exec
big-api
big-cluster
big-http
big-cli
```

`big-bench` is `publish = false`: it pulls in rival engines to measure against and is no part
of the product.

### Checklist

1. `CHANGELOG.md` — move `[Unreleased]` to the new version with a date.
2. `workspace.package.version` in the root `Cargo.toml`, and the `version` on every path
   dependency that names it.
3. CI green, including `fmt`, `clippy`, `docs`, `msrv` and the crash tests.
4. `cargo publish` in the order above.
5. Tag `v<version>`.

`cargo-semver-checks` runs in CI against `big-api` and `big-http`. It has nothing to compare
against until the first release, and is wired in now so that it does from the second.
