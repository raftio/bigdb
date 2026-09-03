# Clustering

**Built.** `big serve --cluster cluster.toml` runs a node that owns a range of shards, plans queries
for the whole cluster, fans them out and merges what comes back. A range may be **replicated**,
and a replicated range **fails over on its own**: the nodes agree among themselves on which copy
serves it, and when that copy stops answering they agree on another one.

Where this sits in CAP, and why, is [The choice, and what it costs](#the-choice-and-what-it-costs).
The short version: **CP**, and the availability that usually costs you is bought back by failing
over in about a second rather than by answering from data nobody can vouch for.

What is *not* built is stated as plainly as ever: no rebalancing, no cross-node atomicity, no
cluster-wide snapshot, no gossip. Those are decisions, not a backlog —
[What this does not give you](#what-this-does-not-give-you) says why for each.

Everything below is a decision, not a survey of options. Where an option was rejected, the
reason is stated, because the reason is the part that stays true when the option comes back.

## The choice, and what it costs

Every distributed system answers CAP, and most answer it by accident. This one answers **CP**,
and the reasoning is about what `big` is rather than about what is fashionable.

**An answer here is an aggregate.** `Count`, `Sum`, `TopN` — not a row somebody will look at,
but a number somebody will act on. An available-but-stale answer to a query over rows is a
profile showing an old email address: visible, bounded, per-object, and recoverable. An
available-but-stale *count* is `4,182,003` when the truth is `4,182,009`, and **nothing
downstream can tell the difference**. There is no per-record marker for a caller to check,
because the records were never named; they were counted.

So the two failures are not the same size:

| | What it costs | Can it be undone |
|---|---|---|
| Refusing | an outage, bounded by how long a failover takes | yes - retry |
| Answering from a copy that is behind | a wrong number, in a report | **no** - the answer has already left |

**And the usual objection to CP does not apply once a range fails over.** "CP means
unavailable" is an argument about a system where losing a node means losing a range until a
person edits a file. Here the loss is bounded by an election and a lease, which is a second or
two, not the length of the outage.

**Writing is the exception, and it is an exception for a reason.** A write that reaches only
some copies leaves them different, and that difference *can* be undone: the copy serving the
range holds the truth, so a repair is a copy rather than a negotiation. A read that returned the
wrong number cannot be undone at all. So a write is allowed to stand when a spare cannot be
reached - see [The write path](#the-write-path) - and a read is not allowed to guess.

**Two things could not be available even if the rest were.** A row key must mean one thing
everywhere, so a *new* key met while the schema leader is unreachable is refused, and no amount
of availability elsewhere changes that - the alternative is two ids for one string, which is a
wrong answer with no symptom. And dense row ids are what the storage layer charges for: hashing
keys to ids would need no coordination and would cost the 15x that
[README.md](../README.md) measures.

In PACELC terms: **PC/EL**. Else - when nothing is partitioned - there is no cross-node
coordination on the read path at all, and a fanned-out read is as fast as the slowest owner
rather than as slow as a quorum.

## What the engine already decided

Most of a distributed design is a choice about where data lives and how partial answers
combine. This engine answered both years before anyone wrote this page, and answered them in
the storage layer, so the cluster layer inherits them rather than choosing:

**A record id names its shard, and the client picks the record id.** `shard_of` is a shift —
`record_id >> SHARD_WIDTH_EXPONENT` ([coords.rs:41](../crates/big-engine/src/coords.rs#L41)) —
and every write path takes the record id from the caller: `Fact::Int { record, .. }` and its
siblings in [big-embed](../crates/big-embed/src/lib.rs), one id per line in `POST /import`. Nothing
in the engine ever allocates a record id. So placement needs no coordination at all: by the time
a fact arrives, the client has already chosen which node should hold it, without knowing that it
did.

**Partial answers already combine.** [`Matches`](../crates/big-db/src/matches.rs) is a
`BTreeMap<ShardId, RowSet>` whose `and`, `or` and `andnot` are shard-wise merges that never
scan. Two nodes' answers combine with exactly the operator two fragments' answers combine with.
There was no distributed set algebra to design; there was one to serialise, and it is
[wire.rs](../crates/big-cluster/src/wire.rs).

**The fan-out existed, one level down.** `DbRead::per_fragment`
([db.rs:919](../crates/big-db/src/db.rs#L919)) maps a closure over every candidate fragment and
folds the results. The network fan-out is that same shape one layer higher, over `Api` instead
of over a fragment. It was not a rewrite of the query path.

**The reduce is already order-independent where it has to be.** `Count`, `Sum`, `Min` and `Max`
fold over a `Matches` that shards contributed to in any order. `TopN` explicitly ranks only
"after every shard has contributed"
([big-exec/src/lib.rs:161](../crates/big-exec/src/lib.rs#L161)) and breaks ties on the interned
key rather than the row id, so that two databases holding the same data rank it identically.
That comment was written about shards inside one file. It is true without amendment about shards
across nodes, and the coordinator reaches for the same two functions —
`big_exec::sort_by_key` and `big_exec::rank_top_n` — rather than reimplementing the order.

**One process holds one file.** The mmap pager takes `flock(LOCK_EX | LOCK_NB)` before mapping
([mmap.rs:77](../crates/big-pager/src/mmap.rs#L77)), so one node is one process is one file.
Shards do not move between nodes without a file copy, which is the constraint that makes
rebalancing a scheduled operation rather than a background one.

**The catalog can grow without a format bump.** Record kinds are additive and unknown kinds are
skipped on load ([record.rs:28](../crates/big-page/src/record.rs#L28)). Nothing about clustering
needed that escape hatch in the end: a node's file is an ordinary `big` file, readable by a
build that has never heard of a cluster, because ownership lives in a config file and row keys
were already a catalog record.

What was left to decide, after all of that, was one thing: **which node owns which shards, and
how the row-key namespace stays the same on all of them.** The rest of this page is those two
answers and their consequences.

## Which shards, and which copy of them

A `cluster.toml`, read at startup, names every peer and the half-open `ShardId` range it holds.
**The ranges are static and who serves one is not**: the file says which node starts as the
primary of each range, and after that the nodes decide it among themselves.

```toml
schema_leader   = "b"
# Optional. The first line of this file is the bearer token this node presents to its peers.
# The CA that signs a node certificate. Public, unlike the keys it signs.
peer_ca_file = "/etc/big/peer-ca.pem"

[[node]]
name   = "a"
addr   = "10.0.0.1:7654"
shards = "0..64"

[[node]]
name   = "b"
addr   = "10.0.0.2:7654"
shards = "64.."

# A copy of everything `a` holds. It has no range of its own: saying it twice would be a way
# of saying it differently. Three nodes is the minimum once any of them has a copy - see
# below - and `b` counts even though it holds a different range.
[[node]]
name    = "a-spare"
addr    = "10.0.0.3:7654"
replica = "a"
```

A node has either `shards` or `replica`, never both and never neither. Ranges must be
**disjoint and total**. A gap or an overlap is a startup failure naming the
shards involved, not something resolved by a rule. There is no protocol here that could change
ownership safely — no membership, no consensus, no leases — so a config that disagrees with
itself is a fact the operator has to fix, and pretending otherwise would mean two nodes each
believing they own shard 64 and each answering half a query.

Note `"64.."` on the last node, and note that `"64..128"` there would be **refused**. Total
means the whole space: a record id above shard 128 is one a client can choose, and a range map
that stops at 128 is a range map that would let that id be written by a client and read back by
nobody. The open end is the only spelling that reaches `u64::MAX`.

Which *range* a record belongs to is a pure function of the config - a shift, then a range
lookup, infallible because the ranges were checked to cover the space before the type existed.
Which *node* serves that range is a value the nodes agree on, and it starts as the one the file
names. Nothing moves between ranges, ever; only the answer to "which copy" moves, and it moves
without a byte of data moving with it, because every node in a range's group already holds it.

## Three roles, one binary

`big serve` grew `--cluster <file>` and `--node <name>`, and nothing else. Every node runs the same
binary and may play all three roles at once:

- **Coordinator** — whichever node received the request. Plans the query, fans it out, merges.
  Any node can be one; there is no dedicated coordinator tier, because a coordinator holds no
  state between requests.
- **Shard owner** — holds the file containing its own range. This is what a node was before.
- **Schema leader** — exactly one node, named in the config. Owns the catalog decisions: whether
  a schema change is legal, and what every row key means. See
  [Row keys](#row-keys-and-the-schema-leader).

`GET /ready` reports both the build and the wire version, which are not the same question: two
nodes with different builds and the same wire version run side by side, and two with different
wire versions do not. An upgrade that moves the wire version is a coordinated stop rather than
a rolling one, and `/ready` is how you find out which kind you are about to do.

`--node` says which entry in the file this daemon is. Without it, the daemon matches the address
it was told to bind; if that is not in the file either, it refuses to start and says to pass
`--node`. Two ways of answering "which of these am I", and both failing is a refusal rather than
a guess.

A single-node cluster is one node holding all three roles over `0..`, which is what `big serve`
without `--cluster` builds for itself. That is deliberate, and it is enforced rather than
intended: `Server` holds a `Cluster`, never an `Api`, so the un-clustered configuration is the
general one with a peer count of zero. There is no second path for it to be the one nobody
tests.

## The read path

`POST /table/{t}/query` at the coordinator:

1. **Plan once**, against the coordinator's schema. Planning is pure — `big-plan` has no
   dependencies and links no pager — so it costs nothing to do before the network is touched,
   and a query that will not type-check is refused without any node hearing about it.
2. **Fan out** the plan to every primary, over `POST /internal/query`. Every primary and no
   replica: a replica holds the same records, so asking it as well would double every count.
   Not the query text: the plan. Re-parsing per node would let two nodes disagree about what was asked, which is the
   class of bug that is impossible to see in a result.
3. **Each owner executes locally** through `big_exec::execute` against its own read transaction,
   over only the shards it owns, and answers with a serialised `Value`.
4. **Merge.** `Value::Rows` merges with `Matches::or` — owners contribute disjoint shard sets,
   so the merge cannot double-count. `Count` and `Sum` add. `Min` and `Max` take the extreme of
   the extremes.

**This node is never asked over a socket.** The local share of every fan-out is a direct call.
A coordinator that reached its own listener would spend a worker to reach a worker, and would
deadlock outright once the pool was full — the request holding the last worker would be waiting
for a worker to answer it.

**The merge is directed by the plan, not by the answers.** `Value::Extreme` does not say whether
it came from a `Min` or a `Max`, and folding two of them the wrong way is a wrong answer that
looks like a right one. The coordinator has the plan because it made it.

`Distinct`, `TopN` and `GroupBy` are the ones that need care, and the care they need was already
written down. Each is a per-row aggregate over groups, so the coordinator sums every node's
group counts for a row **before** ranking or truncating — a row that is merely second on every
node would otherwise lose to one that leads a single node. For `TopN` this means a node must
return more than `n` groups, so the plan is rewritten on its way out: each owner is asked for
*all* of its groups and the coordinator cuts the merged list. A tighter bound is possible and is
an optimisation, not a design. This is the one place where what the owners are asked is not
literally what the client asked, and it is the one place it has to be:
`top_n_ranks_after_every_node_has_contributed` in
[cluster.rs](../crates/big-http/tests/cluster.rs) fails without it.

Answers are merged in node order rather than arrival order. The fold is order-independent, so
this buys nothing but determinism — and determinism is the difference between a result a client
can compare against a previous one and a result it cannot.

## The write path

`POST /table/{t}/import` at the coordinator:

1. Parse the batch and **resolve every key against the schema leader first**, before any fact is
   sent anywhere. A key that cannot be interned fails the whole batch here, where nothing has
   been written yet.
2. Split the facts by `shard_of(record)` and send **every copy** of each range the facts for its
   shards, along with the row ids for the keys **that share actually uses**. The primary first,
   so that when only one copy has the batch it is the one reads go to.
3. Answer when every copy has committed.

**A batch spanning two ranges is two commits and there is no transaction over both.** If one
range commits and another fails, the batch is half applied, and the response says which shards
landed (`partially_applied`, `500`). This is stated as plainly as the absence of a WAL is stated
in [architecture.md](../architecture.md), and for the same reason: an atomicity guarantee that
exists in the documentation and not in the code is worse than none, because callers build on it.
A caller who needs all-or-nothing sends a batch whose records fall in one shard range, which is
a property they can compute themselves from `SHARD_WIDTH`.

**A copy that cannot be reached does not fail the write.** This is the one place availability
wins over consistency here, and it wins for a stated reason: refusing would mean one machine
nobody reads from can stop a range being written to - a hole failing over does not plug, because
failing over replaces a dead *primary* and this is a dead *spare*. So the write stands, the
answer names the copy that did not take it:

```json
{"imported":2,"missed":["a-spare (0..1) (`a-spare` holds shards 0..1 and could not be reached …)"]}
```

and the agreement records that copy as **behind**. What makes that safe is the other half:
**a copy that is behind is one the agreement will not promote.** Nothing ever reads from a copy
that missed a write, because reads go to the copy serving the range and a copy that is behind
cannot become that copy. If every other copy of a range is behind, the range does not move and
says so - which is the honest answer, and the reason `POST /repair` exists.

Owners are written **in order, one at a time**, and a refusal stops the batch reaching the
owners after it. Concurrency would make a half-applied batch more likely rather than less: the
one thing here that cannot be taken back is a commit, so a failure before the first one is worth
keeping clean. A batch refused before anything committed is reported as the plain refusal it is,
and can simply be sent again.

Note what is *not* a problem here. Two owners never contend, because a record id belongs to
exactly one of them and a fragment holds facts about its own records only. The single-writer
constraint each node has is per node, and the cluster's write throughput is the sum.

**One node owning everything writes exactly as it always did.** The interning and the facts go
into one transaction, because there is nobody to agree with; splitting them would pay for an
agreement with two commits and four fsyncs on a database that has no peers.

## Schema changes

DDL goes to the **leader first**, and only then to everybody else. The leader is where a name
that is already taken, a field kind that contradicts an existing one, or a table that is not
there to drop is refused — in one place, before any other node has heard of it. What the other
nodes then apply is a change that has already been ruled legal.

Table and field ids are **not** carried between nodes. They are each node's own numbering, and
nothing on the wire depends on two nodes agreeing about them: names do the resolving, all the
way down. The id a client is told is the leader's, which is a choice about which of two equally
true numbers to print. Row ids are the only ids that have to mean the same thing everywhere,
which is why they are the only ones with a protocol.

A node that cannot be reached during the second phase leaves the schema half applied, reported
the same way a half-applied batch is and naming the nodes that are missing it. Unlike a batch,
every node is attempted even after the first failure: there is nothing to lose by trying the
rest, and a change that reached three nodes out of four is finished by hand from a list of the
one that is left.

## Row keys and the schema leader

This is the only genuinely hard part, and the code already said so:
[big-keys](../crates/big-keys/src/lib.rs) opens by noting that a row key "has to mean the same
thing in every shard, which makes assigning one the single point in the write path that needs
agreement", and that whatever distributed design comes later gets decided there.

**All DDL and all interning route to the schema leader.** A node that meets an unknown key
during an import asks the leader, which either returns the existing row id or assigns the next
one under its own write transaction. Row ids are immutable and never reused
([`KeyStore::intern`](../crates/big-keys/src/lib.rs#L92)), so a mapping a node already holds can
never go stale — a miss is a round trip, not a wrong answer. The coordinator therefore looks in
its own key store first and asks the leader only about what it does not already know.

**An owner is told what a key means; it does not choose.** `KeyStore::assign` records a mapping
rather than inventing one, and **refuses one that contradicts what this node already holds**
rather than overwriting it. That refusal is the check that makes routing every key through one
node worth doing, applied at the node that would otherwise be the one to disagree. The
assignments land in the same transaction as the facts that use them, so a refused batch leaves
neither behind.

**Hashing the key to a row id is refused.** It needs no coordination at all, which makes it the
obvious answer until you price it. `KeyStore` hands out *dense* ids — a counter per
`(table, field)` — and density is not an accident of the implementation, it is what the storage
layer charges for. Rows are the bitmap's first axis; sparse row ids spread the same facts over
far more containers, and the same 100k records over 64 shards already costing 15x is the measured
version of that effect ([README.md](../README.md)). It would also change `TopN`'s tie-break from
"first written" to "hash order", which is a visible difference nobody asked for.

**Per-node row id ranges are refused.** They also need no coordination, and they are worse than
hashing: two nodes independently interning `"GB"` give it two different row ids, so a `GroupBy`
returns two groups that are the same group, and nothing anywhere in the system can notice. A
design whose failure mode is a silently wrong answer is not a cheaper design.

**When the leader is unreachable**: reads work normally, writes of keys already in the local
store work normally, and **a write introducing a new key is refused** with a stable error code
saying so (`schema_leader_unreachable`, `503`). Not queued, not assigned locally and reconciled
later. This is the same shape as every other refusal in the engine — see the
`UnknownContainerType` reasoning in [architecture.md](../architecture.md) — and the shape matters
more here than anywhere, because the alternative is two row ids for one string, which is exactly
the failure per-node ranges were rejected for.

## The routes a node uses to reach another

Thirteen, all `POST`, all under `/internal/`, all with a binary body and a binary answer. They are
not part of the public surface and a client has no reason to speak to them.

```text
POST /internal/query     a plan and what is left of its deadline    -> a Value      read
POST /internal/records   a page of ids from this node's shards      -> record ids   read
POST /internal/digest    everything you hold, as one number         -> a number     read
POST /internal/import    facts, with their row ids already fixed    -> a count      write
POST /internal/delete    record ids this node owns                  -> a count      write
POST /internal/intern    what do these keys mean (the leader only)  -> row ids      write
POST /internal/ddl       one schema change, already ruled legal     -> an id        admin
POST /internal/raft      one message of the agreement               -> nothing      admin
POST /internal/schema    the schema, as you hold it                 -> the schema   read
POST /internal/fragments what do you hold for this table            -> a list       read
POST /internal/fragment  send me this one                           -> one fragment read
POST /internal/fragment/put  take this one, whole                   -> nothing      admin
POST /internal/keys      every row key of this table                -> the keys     read
POST /internal/keys/put  take these row keys                        -> nothing      admin
POST /internal/repaired  this copy has caught up                    -> nothing      admin
```

The last six are the repair, and they need `admin` on the way in for a reason worth stating:
replacing a fragment is not writing a fact, it is replacing what a node holds, and saying that a
copy has caught up is what lets that copy start answering reads. Both are the size of power a
schema change is, not the size of an insert.

The public surface grew by two to match: `GET /verify`, which turns those digests into an
answer, and `POST /repair`, which acts on it. Everything else a client can reach kept its shape.

**Every request between nodes carries two stamps**, and both are checked before a body is
decoded. The **wire version** is what this build speaks, bumped whenever a message changes
shape: two builds that disagree about the encoding read each other's messages as something else
- a length where a tag was - and the result is not a refusal, it is an answer that is quietly
wrong. The **cluster fingerprint** is a hash of the file this node read: names, addresses,
ranges, who copies whom, who leads. A node given a file somebody edited on one machine and not
another used to be undetectable until a query started answering half of itself; now the two
cannot speak without noticing.

Neither is a version negotiation. There is one version and one file, and a mismatch is a `409`
saying what this node expected - because whoever reads it is looking at two machines and needs
to know which one to correct.

**A peer is a different kind of caller, not a very privileged client.** It used to be a client
with a token, and each `/internal/*` route needed the role its public counterpart needed. That
only ever meant something while a peer presented the same *kind* of credential a person did.

What a peer presents now is a client certificate, checked during the handshake and before a byte
of HTTP is read, signed by the CA in `peer_ca_file` and naming a node in this file. So the whole
of the requirement is *be a node*, and it cuts both ways: a person cannot reach `/internal/*`
however privileged they are, and a node certificate grants no role on the public routes. The
`--peer-cert` and `--peer-key` a node presents are `big serve` flags rather than entries here,
because one shared file cannot name node `a`'s private key without also naming node `b`'s.

A cluster with no peer CA anywhere is allowed and runs unauthenticated between its nodes, and
`big serve` says so on the way up - refusing it here would refuse it only for clusters. A cluster
that names a peer CA and gives a node no certificate is refused outright, because that node could
not reach any peer and the failure would show up only under load.

**Every decoder there reads bytes it did not write.** A length is checked against what is left
before anything is allocated on the strength of it, recursion is bounded by the same
`MAX_DEPTH` the query parser uses, trailing bytes are refused rather than ignored, and a
container that decodes into something malformed — values out of order — is refused rather than
handed to the set algebra. The property is tested with arbitrary bytes: nothing a peer can send
makes a decoder panic.

**Containers are not re-encoded.** A leaf cell already stores an array as little-endian `u16`s,
a run as pairs of them and a bitmap as its raw words, and that is exactly what goes on the wire
under the same type tag. A second encoding for the same three shapes would be a second place
that has to agree about what a container is, and the second one is the one that drifts.

**Connections are reused.** A request asks for `Connection: keep-alive` and a connection that
comes back alive goes into a small per-peer pool, so a fan-out does not pay a handshake per
leg. The server may refuse — it holds a worker from a fixed pool while a connection waits, so
past half the pool it closes rather than starve — and a refusal costs nothing but the connect
that would have happened anyway. **Keep-alive is opt-in, which is not what HTTP/1.1 says**: a
client that says nothing gets one request per connection, exactly as before, because the
default has to be the one that cannot starve a fixed pool.

A pooled socket can be closed by the peer between the two decisions. It is checked for before
the write and retried after, and **only for a request that is safe to send twice**: a query, a
listing and an intern are, an import, a delete and a schema change are not. A `delete` sent
twice would report how many records the *second* one removed, which is a number the caller
would be told and would be wrong.

## The failure detector is the heartbeat, and nothing else

There is no gossip here and no membership protocol, and the reason is that the agreement
already answers the question. A protocol that has to say "I am here" every four hundred
milliseconds in order to keep leading knows exactly which nodes have stopped saying it, so the
failure detector costs nothing and has no state of its own: it is
[`Raft::last_heard`](../crates/big-cluster/src/raft.rs), a timestamp per node, updated by
messages that were going to be sent anyway.

What a separate membership layer would add is a *second* opinion about who is up, converged by
a second protocol, which the routing decision would then have to be reconciled against. One
opinion, held by the thing that already has to have one, is fewer moving parts and one fewer
thing that can disagree with itself.

The same argument disposes of schema gossip. The schema has a single owner, so there is nothing
to converge; other nodes hold a copy of an append-only mapping, and a copy of immutable facts
needs invalidation, propagation and conflict resolution exactly never.

And it is why `/ready` does not check the peers. A node that is ready is one that can serve its
own range; a readiness probe that failed because a *different* machine was down would take a
healthy node out of rotation for somebody else's outage. It reports which node this is, which
shards it holds, whether it is currently allowed to serve them, and who leads the agreement -
and nothing about whether anybody else is well.

## Failure modes

| What fails | What happens |
|---|---|
| A shard owner is unreachable | **The whole query fails.** `503`, `owner_unreachable`, and the shard range that could not be reached. No partial answer. |
| The schema leader is unreachable | Reads unaffected. Writes of known keys unaffected. Writes introducing a new key, and all DDL, refused with `503` `schema_leader_unreachable`. |
| An owner refuses | Its status and stable code travel out unchanged, prefixed by which node said it. The coordinator does not reinterpret a `404`. |
| An owner answers with bytes this build cannot read | `502`, `peer_unreadable`. In practice this means two nodes are running different builds. |
| An import spans two owners and one fails | `500`, `partially_applied`, naming the shard ranges that landed and the ones that did not. |
| A replica of a range is unreachable | Reads unaffected - they go to the copy serving the range. The write **stands** and names the copy it missed, and the agreement marks that copy behind. |
| The copy serving a range stops answering | The agreement gives the range to a copy that is not behind, in about a second. Queries in the window fail with `503`; queries after it do not. |
| Every other copy of a range is behind | The range does not move, and stays unavailable. Promoting a copy that is behind would answer from data somebody else has and this one does not. |
| A node loses touch with the agreement | It stops answering for its range (`503`, `not_serving`) before anything could be promoted in its place. A range with no copy is never fenced: nothing could take it. |
| Two copies of a range disagree | Nothing notices until somebody asks. `GET /verify` is the asking, and `POST /repair` is the answer. |
| The copy **serving** a range comes back with a replaced disk | It refuses queries rather than answering them - the schema went with the data, so it says it has no such table, naming itself. Nothing is marked behind: it was never unreachable long enough, and no other copy was a better answer. `GET /verify` is the only thing that finds it, and `POST /repair` says which copy it cannot take from. |
| A cluster of two has a replica in it | Startup fails. A majority of two is two, so such a cluster can never use its copy - which is strictly worse than the same two machines without one. |
| A schema change reaches the leader and not everyone | The same, for the schema. Finished by hand from the list of nodes that are missing it. |
| `cluster.toml` has a gap or an overlap | Startup fails, naming the shards. Not resolved by a rule. |
| Two nodes read cluster files that disagree | Refused the moment they speak: every request between nodes carries a fingerprint of the file, and a mismatch is a `409` naming both. Two nodes that *never* meet are still undetectable, and that is now the only gap ownership-by-configuration leaves. |
| Two nodes are running different builds | Refused before any body is decoded: every request between nodes carries the wire version. Two builds that disagree about the encoding would read each other's messages as something else, which is an answer that is quietly wrong rather than a failure. |
| A node's disk is lost | Its shards are lost. See below. |

The first row is the one worth defending. A count that is missing a node's contribution looks
exactly like a correct count, and there is no downstream check that would catch it — which is
precisely why this engine refuses a file it half-understands rather than skipping the parts it
does not ([architecture.md](../architecture.md)). A partial answer with a `partial: true` flag
beside it is the same trade with a warning nobody reads. If partial reads are ever wanted, they
arrive as an explicit per-request opt-in, never as the default.

## What this does not give you

The most important section on this page.

- **No repair in the background.** `POST /repair` is a thing an operator runs, or a thing cron
  runs; nothing catches a copy up on its own. A repair is a scan and a copy, and a system that
  starts one by itself is a system that starts one at the worst possible moment. Backup is still
  per node and [`Db::copy_to`](../crates/big-db/src/db.rs#L69) is still the whole story: an
  online walk under a read transaction, producing an ordinary database file.
- **No membership changes at run time.** Adding or removing a node is editing the file and
  restarting, on every node. See [What consensus is still not used
  for](#what-consensus-is-still-not-used-for).
- **No quorum reads or writes.** A read goes to one copy and a write goes to all of them. What
  a quorum would buy - a write surviving the loss of a minority - is bought instead by letting
  the write stand and marking the copy behind, which costs one entry in a log that is already
  there.
- **No rebalancing.** Changing a range means stopping a node, copying a file, and editing the
  config. There is no online shard movement, and building one means answering what a query does
  while a shard is in flight — a question worth asking only once replication has answered the
  easier ones.
- **No cross-node atomicity**, as above — and not for want of a protocol. Two-phase commit
  needs a transaction held open across a network round trip, and this engine will not do that
  for two reasons it already had: `big-embed` exists so that a read or write transaction never
  outlives one call, and a node has a single writer, so a held-open write would block every
  other write to that node until a coordinator that may have died said otherwise. Building 2PC
  here means giving up the single-writer lock or the facade's rule, and both of those are load
  bearing.
- **No cluster-wide snapshot.** Each owner serves the fan-out from its own read transaction,
  taken when its part of the request arrived. A distributed query can therefore straddle two
  different commits on two different nodes, and see a record on one that a concurrent writer had
  not yet added on the other. Single-node reads keep exactly the guarantee they have today; a
  fanned-out read does not have it. This is a property of the design and not a defect to be
  fixed incrementally — fixing it means a cluster-wide transaction id, which means the meta page
  flip stops being the only atomic point, which is a different engine.
- **No asynchrony.** A coordinator holds one worker for the whole life of a fan-out, so two
  nodes that saturate while fanning out to each other are each waiting on the other's pool. The
  bounded queue sheds with `503` and the socket deadlines bound it, so this is congestion rather
  than deadlock - but the remedy is more workers, and there is no work-stealing or continuation
  anywhere that would make it not a remedy.
- ~~**No cross-node authorisation model.**~~ **Closed.** A peer used to present a bearer token
  like any other client, so a token that could write to a coordinator could write to any node
  directly, and there was no notion of "this request came from a peer". There is one now: a node
  proves itself during the TLS handshake with a client certificate signed by `peer_ca_file`, and
  the name in that certificate has to be a node in the cluster file. That gives the two things
  the token could not - the `/internal/*` routes are reachable *only* by a node, and a person's
  credentials never are however privileged they are; and one leaked key is one node rather than
  the whole cluster, because each node holds its own.

## Replication, and failing over

A range may be held by more than one node. `replica = "a"` says this node holds exactly what
`a` holds; one of them **serves** the range, and that is the one reads go to.

**Write to all, read from one.** Every write reaches every copy that is reachable, and a copy
it could not reach is named in the answer and marked behind. Reads go to the copy serving the
range and to no other, because a copy that is not serving is a copy that may have missed
something and nothing can tell an incomplete count from a complete one.

**A cluster with a replica needs at least three nodes**, and is refused otherwise. Failing over
is a decision a majority has to agree on and a majority of two is two, so a cluster of two can
never use its copy - and the survivor of a failure would stop serving rather than risk being
the second node to answer for one range. That is strictly worse than the same two machines with
no copy at all. A third node counts whether it is a third copy or another range's primary.

### What the nodes agree on

One value: **which copy serves each range, and which copies are behind.** Not the facts, not the
row keys, not the schema - those have owners already and needed no protocol. The log that holds
this value gets one entry per election and one per machine that dies, so it is short, cold, and
never on the path of a query.

That framing is what makes hand-writing [the agreement](../crates/big-cluster/src/raft.rs)
defensible. Consensus over the *write path* would have to be fast, would have to snapshot and
compact, and would be the largest and least tested thing in the tree. Consensus over one small
value has none of those problems and answers the question.

It is Raft, with its rules in the shape the paper states them, and it has **no I/O and no
clock**: every decision is a function of the state, the message and a timestamp handed in, and
every effect comes back as a value. That is not a style preference. The bugs in a consensus
protocol are all in the rules - a vote granted to a stale log, a commit counted across terms -
and a rule that can only be observed by starting five processes and waiting is a rule nobody
tests. [The tests](../crates/big-cluster/tests/raft.rs) run five nodes, a partition and a
returning leader in one process against a clock the test controls.

Three things are worth naming because they are the ones that go wrong:

- **A vote reaches the disk before it reaches the network.** A vote that is sent and then
  forgotten in a restart is two votes in one term, which is two leaders. A node that cannot
  persist stops participating, which looks to its peers exactly like a node that is down - the
  one failure every other node already handles.
- **A leader commits by counting replicas of an entry from its own term**, and appends a no-op
  on election so it has one. Counting an older entry's replicas is the classic way to commit
  something a later leader is still entitled to overwrite.
- **The election timeout is derived from the node and the term**, not from a random number
  generator. It still breaks split votes, and a test of an election says the same thing every
  time it runs.

### Why two nodes never serve one range

A failure detector cannot tell a dead node from an unreachable one, so the node that might have
been replaced has to be the one that stops. **A node serves a replicated range only while it has
heard from the agreement recently**, and a promotion is proposed only after silence long enough
that the old copy must already have stopped. The assumption is that clocks run at roughly the
same rate, which is the assumption every lease makes, and it is written here rather than left to
be discovered.

A range with **no** copy is never fenced. Nothing could take it away, so a node that stops
hearing from the agreement has lost nothing, and stopping would be an outage invented rather
than avoided.

**The copy serving a range is the truth, even when its disk was replaced.** That is the axiom
this whole page rests on and it has a cost worth naming: a node that comes back empty fast
enough that nothing was promoted goes on serving a range it no longer holds. In practice it
refuses rather than answers - the schema went with the data, so the first thing a client sees
is that node saying it has no such table - and `GET /verify` is what finds it either way. There
is no automatic answer here, because there is nothing in the cluster in a position to
contradict the copy that serves a range.

**Failing back is not a thing.** A node that comes back does not take its range again.
Ownership moves when it has to and stays where it lands, because a node that flaps would
otherwise move the range on every flap, and each move is a moment where a query fails.

### Do the copies still agree?

`GET /verify` asks every copy of every range for a digest of everything it holds:

```json
{"agree":false,"ranges":[{"shards":"0..","primary":"a","agree":false,"copies":[
  {"node":"a","digest":6921730895140298979,"why":null},
  {"node":"a-spare","digest":null,"why":"`a-spare` holds shards 0.. and could not be reached (…)"}]}]}
```

The digest is built out of *logical* answers — a count per table, every row of every keyed
field with how many records are in it, a total per integer field, both rows of every boolean —
never out of bytes on disk. Two healthy copies have different files: pages land in different
places, the freelist differs, compaction may have run on one. A digest over the file would
report a difference on every pair of healthy nodes, which is the same as reporting nothing.

Three things it is not. It is **not a proof**: two databases with the same digest are
overwhelmingly likely to hold the same facts and nothing here rules out the pair that does not.
It is **not cheap**: every keyed field is grouped and every integer field summed, over every
record the node holds, so it is a thing an operator runs deliberately and not a probe. And
**unreachable is not agreement** — a copy that did not answer makes `agree` false, because a
report that rolled those two together is a report people learn to ignore.

### Catching a copy up

`POST /repair` brings every copy the agreement has marked behind back into line and clears the
mark. Without it one blip costs a cluster its redundancy for good, because a copy marked behind
is a copy that will never be promoted.

**A copy, not a merge — and that is a consequence of choosing consistency.** Every write reaches
the copy serving the range before it reaches any other, so that copy is the truth and a repair
is not reconciling two opinions, it is replacing one. A merge would be wrong in the direction
that matters: a copy that missed a *deletion* holds bits the truth does not, and a union would
put them back. (Under an available-first design the merge would be unavoidable, and the
deletions would need tombstones to survive it. Choosing CP is what makes this a copy.)

**What moves is what differs.** Fragments are compared by cardinality first, and under the rule
above that comparison is a *proof* rather than a hint: a copy that is behind holds a subset of
what the serving copy holds, and a subset with the same count is the same set. A blip that cost
one batch costs one fragment to repair, not a database.

Three things travel, in this order, and the order is the point:

1. **The schema**, or nothing else can land - a fragment belongs to a field, and a copy that was
   away while the field was created has never heard of it.
2. **The row keys**, or the bits mean nothing - a row is a number until something says which
   string it stands for, and a copy with the bits and not the mapping answers `Count` correctly
   and `GroupBy` with nulls.
3. **The fragments that differ**, each in its own transaction. A repair that is interrupted
   leaves the copy closer to the truth than it was and still marked behind, which is exactly
   the state it should be left in.

## What consensus is still not used for

The agreement decides one value. It is deliberately not extended to:

- **The write path.** Facts are not replicated by consensus; they are sent to every copy and
  reconciled by a repair. A quorum write would put an election's worth of machinery in front of
  every batch to buy a guarantee that write-to-all plus a mark already gives.
- **Cluster-wide read snapshots.** See below: that is a different engine, not a bigger protocol.
- **Membership.** The set of nodes is the config file, and changing it is a restart. Online
  membership change is the part of Raft most often got wrong, it is needed only for a cluster
  that grows without a maintenance window, and a range cannot move between groups anyway
  without the file copy that rebalancing would need.
