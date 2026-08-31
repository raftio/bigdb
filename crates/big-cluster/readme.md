# big-cluster

Static shard ownership, the fan-out over it, and the wire it travels on.

Above `big-api` and below `big-http`, which is the whole reason it is its own crate: the merge
has to live somewhere that has never heard of a socket handler and somewhere `big-db` has never
heard of, and neither of those places existed.

The design, including everything it deliberately does not give, is
[docs/clustering.md](../../docs/clustering.md). This page is what the crate contains.

## A cluster of one is not a special case

```rust
Cluster::solo(api)                    // every shard, no peers, its own schema leader
Cluster::new(api, config, token)      // one node of a configured cluster
```

`big-http`'s `Server` holds a `Cluster`, never an `Api`, so a `bigd` started without `--cluster`
runs every request through the same coordinator that a four-node deployment does. A second path
for the un-clustered case would be the path nobody tests.

## CP, and what that buys

This crate answers CAP with **CP**, because an answer here is an aggregate: an available-but-
stale `Count` is a wrong number with no symptom, while a refusal is an outage that can be
retried. The usual price of CP - unavailability - is paid back by **failing over in about a
second** rather than by answering from data nobody can vouch for.

Writing is the one exception. A write that could not reach a copy **stands**, names that copy,
and the agreement marks it behind; a copy marked behind is one that will not be promoted, so
nothing ever reads from a copy that missed something. Refusing instead would mean one machine
nobody reads from can stop a range being written to, which is a hole failing over does not
plug.

The reasoning in full, including what a partition costs each way, is in
[docs/clustering.md](../../docs/clustering.md#the-choice-and-what-it-costs).

## The seven modules

**`config`** — `cluster.toml`, and every way it is allowed to be refused. Ranges must be
disjoint and total; a gap or an overlap is a startup failure naming the shards, because there is
no protocol here that could resolve one safely. `owner(shard)` returns a node rather than an
option, and can, because totality was checked before the type existed.

**`wire`** — what one node sends another. Containers are not re-encoded: a leaf cell already
stores an array as little-endian `u16`s, a run as pairs of them and a bitmap as its raw words,
and that is what goes on the wire under the same type tag. Plans travel, never query text. Every
decoder here reads bytes it did not write, so lengths are checked against what is left before
anything is allocated, recursion is bounded by the query parser's own `MAX_DEPTH`, and trailing
bytes are refused rather than ignored.

**`client`** — one `POST`, a `Content-Length` body, a status and a body back. No redirects, no
chunked encoding, no TLS. Connections are not reused because `big-http` answers one request per
connection; what is pooled is permission to have a request in flight at all, so a saturated
coordinator does not open one connection per worker per peer and have most of them shed.

**`raft`** — the agreement, over the one value that has to be agreed: which copy serves each
range, and which copies are behind. Not the facts - those have owners already. **No I/O and no
clock**: every decision is a function of the state, the message and a timestamp handed in, and
every effect comes back as a value, because the bugs in a consensus protocol are all in the
rules and a rule that needs five processes to observe is a rule nobody tests.

**`controller`** — the thread that drives it, the failure detector that comes free with its
heartbeats, and the lease that stops a node serving a range it may already have lost.

**`digest`** — every fact a node holds, as one number, built out of logical answers rather than
bytes on disk: two healthy copies have different files and the same facts. It is what `verify`
compares. Not a proof, not cheap, and run deliberately.

**`merge`** — putting the owners' answers back together, **directed by the plan**. `Value::Extreme`
does not say whether it came from a `Min` or a `Max`, and folding two of them the wrong way is a
wrong answer that looks like a right one. Groups are summed before they are ranked or cut, and
the ordering is `big_exec::sort_by_key` and `big_exec::rank_top_n` reached for rather than
reimplemented — two definitions of one order would mean one query answered two ways depending on
how many nodes were asked.

## The two things a node does not decide for itself

**Row ids.** All interning routes to the schema leader. A node is *told* what a key means and
refuses a mapping that contradicts one it already holds, rather than overwriting it. Two row ids
for one string is a wrong answer nothing downstream can notice, which is why the alternatives
that need no coordination were rejected rather than merely not chosen.

**Whether a schema change is legal.** DDL reaches the leader first; the other nodes apply a
change that has already been ruled legal. Table and field ids stay each node's own numbering,
because nothing on the wire depends on two nodes agreeing about them.

## What it does not do

No replication, no rebalancing, no cross-node atomicity, no cluster-wide snapshot, no
membership. Each of those is a decision with a reason, and the reasons are in
[docs/clustering.md](../../docs/clustering.md#what-this-does-not-give-you).
