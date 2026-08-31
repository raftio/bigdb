# A batch loader

`architecture.md` drew one box with a dashed outline: **ingest**, captioned *"no batch client —
callers POST to /import themselves"*. This document plans `bigi`, the fourth binary, and like
`docs/cli-plan.md` it spends most of its length on what that binary is **not** — because a
loader is where a client is most tempted to start knowing things the server knows.

## The gap, precisely

`POST /table/{t}/import` takes one fact per line and commits the batch. The body is bounded:

```rust
pub const MAX_BODY: usize = 8 << 20;   // crates/big-http/src/lib.rs
```

So a caller with more than 8 MiB of facts — which is any real load — gets `413
request_too_large` and has to split the file themselves. `bigc import` cannot do it for them:
[Rule 1](cli-plan.md#rule-1--the-client-adds-no-vocabulary) says no subcommand sends two
requests, and splitting a file is a loop over requests by definition.

That is the whole gap. Not "ingest is slow" and not "there is no CSV support" — **a file larger
than 8 MiB has no client at all.**

## The shape

```
   bigi  file ──chunk──► ≤7 MiB of whole lines ──POST /import──► bigd
          │                                            │
          │                                       {"imported":n}
          ▼                                            │
     checkpoint ◄────────── byte offset, after the ack ┘
```

One request in flight, in file order, and a byte offset written down after each acknowledgement.
Everything else in this document follows from those three.

## Seven decisions

**1. A fourth binary, not a subcommand of `bigc`.** Rule 1 is not a style preference — it is
tested, by `every_subcommand_reaches_a_route_that_exists` and by the hand-maintained
`PUBLIC_ROUTES` list next to it. A chunking loop inside `bigc` would either break that test or
force it to be weakened, and a rule weakened once is a rule that stops catching the thing it was
written for. `bigi` is a different binary with a different rule, stated in Decision 2.

**2. `bigi`'s rule: one *file*, one route, many requests.** Where `bigc` promises one request,
`bigi` promises that every request it sends is the same route with a different slice of the same
file. It still adds no vocabulary: it does not know what a field is, does not parse a value, does
not consult the schema. A line is bytes on their way to the only thing that understands them,
and a refusal comes back as the server's own code and sentence — the property `bigc` has, kept.

**3. The only dependency is `big-cli`.** Not `big-api`, not `big-http`. `big-cli` has an empty
`[dependencies]` section, so depending on it leaves the dependency graph proving the same thing
it proved before: **this binary cannot link the engine.** In exchange `bigi` reuses
`big_cli::http::Client`, `big_cli::json::Failure`, `big_cli::read_token` and
`big_cli::exit` rather than growing a second HTTP writer, a second JSON reader and a second
token-file check that can disagree with the first three.

**4. Retry is allowed here, and the earlier refusal is not being ignored.** `cli-plan.md`
refuses retries with: *"an import that half-landed must not be sent twice by a client that cannot
know."* That reasoning is about a client that cannot know. This one can, and the reason is in
the engine rather than in the client's care:

```rust
Fact::Int  { .. } => w.set_int(..)      // crates/big-api/src/lib.rs
Fact::Key  { .. } => w.set_key(..)
Fact::Bool { .. } => w.set_bool(..)
```

Every fact is a **set**, never an increment, and **the caller chooses the record id** — it is in
the line. Sending the same chunk twice therefore writes the same bits twice, which is writing
them once. Idempotency is a property of the line format, not a discipline the loader keeps, and
it is what makes both retry and resume correct rather than approximately correct.

The retry is still narrow: **transport failures only.** `Unreachable` is retried with a backoff;
a refusal — `malformed_line`, `unknown_field`, `404` — is never retried, because a chunk the
server understood and rejected will be rejected identically the second time, and retrying it
only delays the message the operator needs to read.

**5. One request in flight.** Not a connection pool, not a pipeline depth.

`big-db` has one writer. `Cluster::import` walks ranges *sequentially* and stops at the first
that refuses, so that "the report says exactly which ones landed". A second request in flight
would queue behind the first at the write lock — buying almost nothing — while destroying the
one property that makes a checkpoint meaningful: that everything before offset *N* in the file
has been applied. Concurrency here trades a correctness property for a throughput gain the
engine's own write path will not deliver.

The throughput knob that *does* work is `--chunk-bytes`, because the server commits once per
request. Doubling the chunk halves the number of fsyncs. See "The open question" below for
where the rest of the throughput went.

**6. The checkpoint is a byte offset on the input, and it is written after the ack.** The file
is applied in order and every chunk is idempotent, so "resume at the offset the server last
acknowledged" is exactly right, not an approximation. If the process dies between the
acknowledgement and the checkpoint write, resume re-sends one chunk — which is Decision 4's
whole point.

The checkpoint records the target route, the input path and the input's size, and refuses to
resume when any of them has changed. It does **not** hash the file: hashing 100 GB to decide
whether to skip reading 100 GB defeats the purpose, and the check that is affordable is stated
rather than dressed up as one that is not.

`--resume` requires a seekable input, so `bigi import tx - --resume ck` is refused rather than
quietly ignored. A pipe has no offset to record.

**7. `bigi` reads a file; `bigc` does not.** Worth stating because `bigc`'s usage text has said
`import <table> <file>|-` since it shipped, and `args::source` treats anything that is not `-` as
a **literal body**: `bigc import tx facts.txt` posts the nine bytes `facts.txt` and the server
answers `malformed_line`. No test caught it because every test uses `-`. The usage text is
corrected to say what the code does, and `<file>` starts meaning a file in the binary whose job
is files.

## What is refused

| Refused | Why |
|---|---|
| CSV, TSV, JSON input | A mapping from columns to fields is a second place the schema lives, and the schema lives on the server. Deferred rather than dismissed — see "Next" |
| Generating record ids | The line format makes the caller choose the id. A loader that invented one would make a second run create duplicates, destroying exactly the idempotency Decisions 4 and 6 stand on |
| Parallel requests | Decision 5 |
| Validating a line before sending it | Decision 2, and `bigc`'s Rule 2. The field kinds live in `big-db`, which this binary cannot link |
| Retrying a refusal | Decision 4. A rejected chunk is rejected identically the second time |
| A progress bar with colour or cursor tricks | One `\r` line to stderr when stderr is a terminal, and plain lines when it is not. `--progress`/`--no-progress` force either |
| TLS | The same answer as everywhere else: a reverse proxy |

## Phases

| | |
|---|---|
| **0** | This document; the dashed box in `architecture.md`; a row in `checklist.md`; Decision 4 written into `cli-plan.md` where the opposite is written today |
| **1** | `chunk.rs` — whole lines, two ceilings, an offset. Pure, and tested before anything opens a socket |
| **2** | `args.rs`, `checkpoint.rs`, `lib.rs::run` |
| **3** | Tests against a real `bigd` in-process: a file past `MAX_BODY`, a load interrupted and resumed, a load run twice |
| **4** | `Makefile`, `Dockerfile`, `README.md`, `runbook.md` |

## Next

**CSV, and why it is a phase of its own rather than a flag.** Nobody has a file of
`field record value` lines; they have a CSV. Today the answer is `awk` into `bigi import tx -`,
which works and costs nothing in throughput — but a pipe is not seekable, so it costs
**resume**, and resume is the feature that matters at 100 GB. Staging the converted file instead
costs 2× the disk.

So the argument for `bigi --csv --map record=id,country=cc` is not convenience, it is that
reading the original file directly is the only way to get one pass *and* a checkpoint. That is a
real reason, and it is also a schema-shaped thing living in a client, which is what this
repository refuses everywhere else. It gets decided on its own, with the chunker already tested
underneath it.

## The open question

**`/import` commits once per request, and the 240× is sitting unused.**

`docs/performance-plan.md` measured `Db::ingest(capacity)` at 240× and `Db::bulk_load` at 19×,
and says what they do: *"It does not make a commit cheaper; it makes commits rarer."* Both exist
on `Db` — `crates/big-db/src/db.rs:212` and `:216` — and **nothing in `big-api`, `big-http` or
`big-cluster` calls either.** The HTTP path is `Api::import`, which is a loop over facts and
then `w.commit()`, once per request.

So a loader that chunks perfectly still pays one commit per 7 MiB, and no amount of care on the
client side changes that. The ceiling on ingest over HTTP is on the server.

What it would take is an `Api` that can hold a batch open across requests — and that is the
reason this is a question rather than a phase. A request that returns before its facts are
durable breaks the promise every other route makes, so it would need a shape that says so: an
explicit load session, or a `?durability=` on the route, or a bounded window with an fsync at
its end. Each is a change to the compatibility surface, not a performance patch.

It may well end as **refused**, with that as the reason. What it must not do is stay unnoticed,
because "the client chunks well" reads like the problem is solved.
