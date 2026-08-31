# big-ingest

`bigi` — load a file into a running `bigd`.

`POST /table/{t}/import` takes one fact per line and is bounded at 8 MiB, so anything larger has
to arrive as several requests. `bigc` cannot send several: every one of its subcommands is
exactly one route, and that rule is what keeps it from growing a second query surface. This
binary is the loop, and nothing else.

```sh
bigi import tx facts.txt --resume tx.ck
bigi delete tx ids.txt
awk '{print "country", NR, $3}' data.csv | bigi import tx -
```

**The one dependency is `big-cli`**, whose own `[dependencies]` section is empty — so the graph
still proves this cannot link the engine, and the HTTP writer, the JSON reader, the URL escape
and the token-file check are `bigc`'s rather than second copies of them. Nothing here knows what
a field is or parses a value; a refusal comes back as the server's own code and sentence.

**A load is resumable because a fact is a bit set at a record id written in the line** —
`set_key`, never an increment — so sending a chunk twice writes what sending it once wrote.
`--resume` records the byte offset the server last acknowledged, and re-runs and retries are
correct for the same reason. It is also why `bigi` will never invent a record id: a loader that
did could not be run twice.

One request is in flight at a time. `big-db` has one writer, so a second would queue behind the
first while destroying the ordering a checkpoint depends on. The knob that does work is
`--chunk-bytes`: the server commits once per request.

CSV, parallel requests, retrying a refusal and generating ids are each refused with a reason
written down in [`docs/ingest-plan.md`](../../docs/ingest-plan.md), and `bigi --help` is the
surface itself. Exit codes: `0` loaded, `1` refused, `2` usage, `3` nothing listening.
