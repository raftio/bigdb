# A client on the command line

> **Superseded in part.** This document argued for `bigc` as a third binary (Decision 1). The
> binaries were later consolidated: four became two, and the client is now `bigctl`, built from
> `crates/big-bin` alongside `big`. Decision 1's *structural* half no longer holds - one package
> ships both, so the client links the engine and the empty-`[dependencies]` guarantee is gone.
> Its other half still does: `big`'s offline subcommands take the exclusive lock, which is why
> `big serve` and `bigctl` are separate commands rather than one. Decision 2 is unchanged -
> there is still no offline query mode. Read the rest as the reasoning that produced the
> surface, not as a description of the build. See `architecture.md` and `docs/versioning.md`.

`big` backs a file up, checks it and shrinks it. `bigd` serves one. Nothing in this repository
*asks* a running database a question — that is `curl`, in the README and ten times in the
runbook. This document plans the third binary, and spends most of its length on what that
binary is **not**, because the last thing a query surface needs is a second one that drifts.

**The claim.** A client is honest here if it adds no vocabulary. Every subcommand is one public
route, spelled for a shell instead of for `curl`; every refusal comes back from the server with
the code the server chose. The moment the CLI can answer something `bigd` cannot, it has become
a second engine with a worse test suite.

---

## The shape

```
   bigc  argv ──args──► Command ──http──► bigd ──► {"columns":…,"rows":…}
                                                          │
                          render ◄── json (read-only) ◄────┘
                             │
                        table (a tty) │ tsv (a pipe) │ json (verbatim)
```

`big-cli` depends on **nothing** — not `big-embed`, not `big-plan`, not `big-sql`, not `std`'s
async because there isn't one. That is not minimalism for its own sake: a client that cannot
link the engine cannot accidentally grow an offline path, so
[Rule 1](#rule-1--the-client-adds-no-vocabulary) is enforced by the dependency graph rather than
by discipline. `big-http` and `big-embed` appear as **dev**-dependencies only, so the tests drive
a real server in-process the way `crates/big-http/tests/sql.rs` already does.

## Four decisions, taken here

**1. A new binary, not a subcommand of `big`.** `docs/sql-plan.md` refused `big sql` for two
reasons. One was structural — `big-db` would depend on crates that depend on `big-db` — and a
leaf crate dissolves it. The other still stands: `big` is the offline half, every subcommand
takes the exclusive lock, and a query subcommand there would be the one command that fails
whenever the database is actually being served. So `bigc` is its own binary, `big` is untouched,
and the split on the command line is the split that already exists in the system.

**2. There is no offline query mode.** No `--file`. Querying goes through `bigd`, in both
languages, and a `bigc --file data.big` would be a second query path — un-clustered only, which
`architecture.md` rejects by name as "the path nobody tests". A user with a file and no daemon
starts a daemon.

**3. The CLI reads JSON; the server keeps writing only JSON.** The alternative was content
negotiation — `Accept: text/tab-separated-values` and a TSV writer next to `json.rs` — which is
a change to twelve routes and a new thing in the compatibility promise, to save one file. A
reader in the client is ~200 lines fed by exactly one producer, and it can be strict: any shape
`bigd` does not emit is an error, not a guess.

**4. No line editing, and it is documented rather than half-built.** No history, no arrow keys,
no completion: that is `rustyline` or a hand-written termios mode, and neither is worth the
dependency ledger this repository keeps. `bigc shell` reads lines. `rlwrap bigc shell` gets
history, and the usage text says so.

## The surface

One subcommand per public route. The name of the route is the name of the command.

| | |
|---|---|
| `bigc sql "SELECT count(*) FROM tx WHERE country = 'GB'"` | `POST /sql` |
| `bigc query tx 'Count(Row(country="GB"))'` | `POST /table/{t}/query` |
| `bigc records tx [--after id] [--limit n]` | `GET /table/{t}/records` |
| `bigc import tx -` | `POST /table/{t}/import` |
| `bigc delete tx -` | `POST /table/{t}/delete` |
| `bigc schema` | `GET /schema` |
| `bigc create table tx` / `create field tx country --kind set` | `POST /table/{t}`, `.../field/{f}` |
| `bigc drop table tx` / `drop field tx country` | `DELETE /table/{t}`, `.../field/{f}` |
| `bigc verify` / `bigc repair` | `GET /verify`, `POST /repair` |
| `bigc health` / `bigc ready` / `bigc metrics` | the three probes |
| `bigc shell` | a loop over `sql` and `query` |

A statement or a file is read from **stdin** when the argument is `-`, so `bigc` composes with
the shell rather than replacing it.

Common flags, and nothing else: `--addr` (`BIG_ADDR`, default `127.0.0.1:7654`), `--token-file`
(`BIG_TOKEN`), `--format table|tsv|json`, `--timeout <seconds>`.

**A token is never a flag.** `--token <secret>` puts the secret in `ps` output and in shell
history; the file is read with the mode-600 check `big_http::auth` already applies to the
server's side of the same secret, copied rather than depended on.

**Output follows the destination.** A tty gets aligned columns; a pipe gets TSV, because a
column-aligned table is a format nobody can parse and everybody tries to. `--format` overrides
both. `--format json` prints the body verbatim — the escape hatch for anything this client
renders badly.

**Exit codes are part of the surface**: `0` answered, `1` the server refused (the code is on
stderr), `2` usage, `3` nothing was listening. `bigd` already exits `2` on a bad flag.

## The shell

`bigc shell` is a loop, not a language. It reads a statement, sends it, renders the answer.

Two prompts because there are two surfaces and neither is privileged: `sql>` by default,
`pql>` after `.lang pql`. **Guessing the language from the first token is refused** — `Count`
is a legal start to both a PQL call and nothing in SQL, and a client that guesses wrong reports
a parse error from the wrong parser, which is the least helpful error in the system.

Meta-commands are dotted, so nothing collides with either language: `.lang`, `.schema`,
`.format`, `.timing`, `.quit`. That list does not grow without a reason written down.

## What is refused

| Refused | Why |
|---|---|
| `--file`, any offline query | Decision 2: a second query path |
| A `--token` flag | The secret would be in `ps` and in history |
| Line editing, history, completion | Decision 4; `rlwrap` does it |
| A config file, profiles, named connections | Two env vars and a flag; a config file is a fourth place a wrong address can hide |
| Output paging, colour | `less` and a terminal already exist |
| TLS | The same answer as everywhere else: a reverse proxy |
| Retries on a failed write | An import that half-landed must not be sent twice by a client that cannot know; `big-cluster::client` makes the same distinction. **`bigi` does retry, and is not an exception being carved out here:** it retries a *transport* failure only, and it can because a fact is a bit set at a record id the caller wrote in the line — so the same chunk sent twice is the same chunk sent once. The client that cannot know is still this one. See `docs/ingest-plan.md`, Decision 4 |

## Two rules

### Rule 1 — the client adds no vocabulary

Every subcommand is one route. No composite command, no "helpful" client-side loop over pages,
no local filtering. A test sends every subcommand's request to a real server and asserts none
answers `404` or `405`; a second, hand-maintained list asserts the reverse direction — twelve
routes, twelve entries in the table above — and breaks when a thirteenth route ships without a
spelling here.

### Rule 2 — the client parses nothing it sends

Statements go to the server as bytes. `bigc` does not tokenize SQL, does not validate PQL, does
not check a table name. It cannot: `big-sql` and `big-plan` are not linked, by Decision 1. What
comes back is the server's error with the server's code, which is the only way `sql_no_joins`
means the same thing on the command line as it does over HTTP.

## What the build changed

One addition and one clarification, written down rather than folded in quietly.

**`.table` is a sixth meta-command.** The plan named five, and said the list does not grow
without a reason. Here is the reason: a PQL call is asked *of* a table and the route carries the
table in its path, so the `pql>` prompt could not send anything at all without one. `.table tx`
sets it; `.table` alone prints it. The alternative was for the shell to guess a table, which is
the same mistake as guessing a language one paragraph up.

**`.schema` talks to the server, and the others do not.** It is the one meta-command that is a
request rather than a setting, so it is handled where the client is rather than in the pure
function that handles the rest. Worth noting because it is the seam where a sixth *route*-backed
meta-command would go, and there should not be one: `bigc schema` already exists, and a shell
that grew a second spelling of every subcommand would be a second surface again.

## Phases

| | |
|---|---|
| **0** This document, `checklist.md`, `architecture.md` | The decisions before the code |
| **1** `http.rs` and `json.rs` | The two risky files. `GET`/`POST`, `Content-Length`, one deadline for the exchange; a reader that accepts only shapes `bigd` emits |
| **2** `args.rs` and the one-shot subcommands | Hand-written, shaped like `bigd`'s `Options::parse`; `--help` to stdout and exit `0`, a bad flag to stderr and exit `2` |
| **3** `render.rs` | table / tsv / json, `std::io::IsTerminal` (std, MSRV 1.88), no dependency |
| **4** `shell.rs` | The loop, two prompts, five meta-commands |
| **5** Docs | README (the `curl` block **stays** — it is the proof no client is needed), `runbook.md`, `CHANGELOG.md`, `docs/versioning.md` |
| **6** Deliberately not built | Everything in "What is refused" |

All six shipped. `crates/big-cli` has an empty `[dependencies]` section, which is the claim of
this document expressed as something a build can check.

### The tests that carry the claim

`crates/big-cli/tests/cli.rs` starts a real `Server` on `127.0.0.1:0` (`Api::in_memory`,
`serve_n`) and drives the client library against it: every subcommand, the two auth failures,
every exit code, and a refusal — `SELECT … JOIN …` — asserted to reach the user as
`sql_no_joins` with the server's sentence intact, not a client rewording.

## Versioning

`docs/versioning.md` promises two things, and this adds a third of a kind it already has a
shape for. **The command surface follows `big-http`**: a subcommand that works today works next
release, exit codes do not change meaning, and `--format tsv` is stable because scripts parse
it. **Table rendering is not promised** — it is for a human looking at a terminal, and column
widths are not an interface. The `big-cli` *library* is an internal crate like the other twelve.

## Risks

1. **The client grows a feature the server does not have.** Rule 1, plus the parity test. The
   first request will be client-side pagination over `/records`, which is a loop the caller can
   write and the client cannot make cheaper.
2. **The JSON reader disagrees with the writer.** They are two files with one producer between
   them; the tests go through a real server, so a change to `json.rs` that the reader cannot
   take fails in `big-cli` rather than in a user's terminal.
3. **A partial answer looks like a whole one.** A response truncated by a dropped connection
   must not render as a short table. `Content-Length` is read and enforced; short is an error.
4. **`bigc` becomes where features land** because a route is more work than a flag. Rule 2 makes
   that structurally hard: the client cannot understand a query it is not allowed to link.
