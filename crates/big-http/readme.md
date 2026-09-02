# big-http

The smallest HTTP surface that makes the engine reachable from outside the process.

Ships `big serve`, and a `Server` you can embed. No framework, no async runtime, no protocol beyond
HTTP/1.1 with a `Content-Length` body — deliberately the least server that could work, so that
the decision to have one at all stays cheap to revisit.

```console
$ big serve data.big 127.0.0.1:7654 --tokens tokens.txt
$ curl -H 'Authorization: Bearer …' -d 'Count(Row(country="GB"))' \
      localhost:7654/table/tx/query
```

## Routes

| | | Role |
|---|---|---|
| `POST` | `/table/{t}/query` | read |
| `GET` | `/table/{t}/records?after=&limit=` | read |
| `GET` | `/schema` | read |
| `POST` | `/table/{t}/import` | write |
| `POST` | `/table/{t}/delete` | write |
| `POST` | `/table/{t}`, `/table/{t}/field/{f}` | admin |
| `DELETE` | `/table/{t}`, `/table/{t}/field/{f}` | admin |
| `GET` | `/health`, `/ready` | none, ever |
| `GET` | `/metrics` | read |

`/health` and `/ready` are never authenticated: a probe that needs a credential is a probe that
reports the credential's health instead of the server's.

`GET /verify` (read) asks every copy of every replicated range whether it still holds the same
facts, and `POST /repair` (admin) catches up the ones that are behind. Both are scans, and
operators' tools rather than probes.

Thirteen more, under `/internal/`, exist for one node to reach another: `query`, `records`,
`digest`, `import`, `delete`, `intern`, `ddl`, `raft`, `schema`, `fragments`, `fragment`,
`fragment/put`, `keys`, `keys/put`, `repaired`. They take a binary body, answer with one, and need the same roles
their public counterparts do — a peer is a client with a token, not a trusted origin. A client
has no reason to speak to them; see [clustering](../../docs/clustering.md).

## Five decisions worth reading before you deploy it

**Every route goes through the coordinator, even with one node.** `Server` holds a
`big_cluster::Cluster`, never an `Api`, so `big serve` without `--cluster` is a cluster of one over
every shard. A second path for the un-clustered case would be the path nobody tests.

**Keep-alive is opt-in.** A persistent connection holds a worker from the pool below, so a
client that says nothing gets one request per connection, exactly as before. A client that
sends `Connection: keep-alive` gets one - the fan-out between nodes does - and even then the
server closes rather than let more than half the pool sit waiting on connections that are not
asking for anything.

**Concurrency is a fixed pool, not a thread per connection.** A thread per connection with no
ceiling makes the connection count an unbounded multiplier on both memory and threads. Worse, an
accepted socket with no timeout lets a client that sends one byte and then nothing hold a thread
for as long as it likes. The pool bounds how many requests run at once, the queue bounds how many
wait, and anything past that is refused with `503` immediately. **Shedding load is the honest
answer; queueing it only moves the failure somewhere harder to see.**

**Transport security is not here and is not going to be.** Termination belongs to a reverse proxy
— see `runbook.md`. A TLS stack would be a larger dependency than the entire engine, and a
hand-written one is out of the question. What *is* enforced is the half that keeps that from
being an excuse: `big serve` **refuses to bind anywhere but loopback without a token file**, and
overriding that needs `--insecure-no-auth`, a flag that says what it is.

**Tokens are a file, not a database.** One `token role` per line, roles `read` / `write` /
`admin`, and the file must be mode `600` or `big serve` refuses to start.

## Metrics

`/metrics` renders Prometheus text with the `version=0.0.4` content-type suffix, which is part of
the contract: a scraper reads it to decide how to parse, and omitting it makes some of them
guess.

## Stability

This crate and `big-api` are the published surface and carry a semver guarantee. See
`../../docs/versioning.md`.
