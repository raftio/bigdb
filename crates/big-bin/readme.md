# big-bin

Everything `big` ships as an executable, which is two binaries.

```
big     serve <file> [addr]     hold the file open and answer over HTTP
        backup | restore | compact | verify | drop-days | scrub
                                take the exclusive lock and work on the file

bigctl  sql | query | records | schema | create | drop
        verify | repair | health | ready | metrics | shell
                                one subcommand, one route, over HTTP
        import | delete         one file, one route, as many requests as it takes
```

The split is which process owns the file. `big` owns it; `bigctl` owns nothing and asks a
running daemon. That is the only distinction an operator has to hold, and it is why
`big compact` fails immediately against a served database instead of doing something clever
behind the daemon's back.

```sh
big serve data.big 127.0.0.1:7654 --tokens tokens.txt

bigctl create table tx
bigctl create field tx country --kind set
bigctl import tx facts.txt --resume tx.ck
bigctl sql "SELECT country, count(*) FROM tx GROUP BY country"
```

## What the client may not do

`bigctl` adds no vocabulary. It does not parse a statement, validate one, or answer anything
offline: a statement travels as bytes and an error comes back as the server's own code and the
server's own sentence, printed without rewording. `sql_no_joins` on a terminal *is* the string
the server chose.

That used to be enforced by the dependency graph — the client was a package with an empty
`[dependencies]` section and *could* not link `big-sql`. Cargo gives every `[[bin]]` of a
package the same dependencies, so shipping both binaries from one crate gave that up. **The rule
survives the guarantee that used to hold it up.** Do not import the engine into `src/client/` or
`src/ingest/`; a second query surface drifts from the first, and the second one always loses.

## Layout

| path | what |
| --- | --- |
| `src/serve.rs` | the daemon: bind, cluster, tokens, deadlines |
| `src/offline.rs` | backup, restore, compact, verify, drop-days, scrub |
| `src/client/` | argv, HTTP, JSON, rendering, the shell |
| `src/ingest/` | chunking, checkpoints, progress — the load loop |
| `src/bin/` | two `main`s, each only streams, environment and an exit code |

The logic is in the library so the tests drive exactly what a user drives. `tests/cli.rs` and
`tests/ingest.rs` both run a real server in-process on a loopback port rather than a mock,
because the risk worth testing is agreement with what the server actually writes.

Was four binaries in four packages: `bigd`, `big`, `bigc` and `bigi`. See `docs/versioning.md`
for the migration, and `architecture.md` for what the consolidation cost.
