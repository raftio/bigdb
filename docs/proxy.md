# The front door

**Built.** `bigproxy --cluster cluster.toml` puts one address in front of a cluster: it asks
every node whether it is serving, sends each request to the least busy node that says yes, and
refuses to carry anything that is not on a list of routes written down in advance.

It is not required. Every node is a coordinator, so a client pointed at any one of them gets a
whole answer. What it changes is what happens when that one stops.

## The problem it solves

`deploy/cluster/docker-compose.yml` used to publish node `a` and nothing else, with a comment
saying why:

> Any node answers any request: the one that receives it plans the query and fans it out. Only
> `a` is published, because publishing three ports would suggest a client has to choose, and it
> does not.

That reasoning is right and its conclusion was incomplete. A client does not have to choose
*which node can answer* — but `clients/go` holds one connection to one address, with no host
list, no failover and no view of the topology. So `a` was a single point of failure for two
machines that were both fine.

Publishing all three would not have fixed it: it moves the choice to the client, and the clients
have nowhere to put it. This is the other answer.

## What it deliberately does not do

**It does not route by key.** Which node serves a range is a value the nodes agree on, and it
moves without a byte of data moving with it. A proxy that computed the same answer would be a
second copy of `big-cluster/src/ownership.rs` — one that is wrong for as long as it takes a moved
range to reach it, and that the cluster has no way to correct, because the proxy is not in the
agreement.

The question it answers is narrower and has a published answer:

| question | who answers it |
|---|---|
| which node owns this key | the nodes, among themselves |
| which node is alive | `GET /ready`, unauthenticated |

**It holds nothing.** No file, no lock, no credential. `Authorization: Basic` is forwarded byte
for byte and never parsed, so the daemon stays the only process that has seen a password.

That claim is checked rather than asserted. `big-wire` was split out of `big-http` so the proxy
could speak HTTP without linking an engine, and CI runs:

```
cargo tree -p big-proxy -e normal | grep -E 'argon2|big-engine|big-db|big-sql|big-embed'
```

The whole shipped tree is `big-proxy → big-wire → big-tls` and the standard library.

## Health

One thread asks every node `GET /ready` on an interval, on a connection of its own so a
saturated request pool cannot starve the probe and a probe cannot occupy a request slot.

Two properties of that route shape everything:

**It is always `200`.** `ready()` builds its answer with `Response::ok` unconditionally, so a
node refusing every request still answers `200`. The status line is not the signal; the body is.
A poller watching only the status would keep a dead node in rotation.

**`serving` is absent on a node with no agreement.** The daemon emits that field only when
`controller()` is `Some`, so a solo node — and every node of an unreplicated cluster — never
sends it. **Absent means serving.** Reading it as `false` would empty the rotation of a perfectly
healthy deployment.

| observation | verdict |
|---|---|
| connect, TLS or timeout fails | down |
| status is not 200 | down |
| body is not a readiness answer | down |
| `"serving":false` | down, **immediately** |
| `"serving":true` | up |
| no `serving` field | up |

Hysteresis is asymmetric: three consecutive failures to leave, two consecutive successes to come
back. Leaving is cheap and reversible; coming back too early is a second outage. A node just
admitted is protected from failure-count ejection for one floor, so a node flapping at the poll
frequency settles instead of oscillating — but `serving:false` bypasses that, because it is the
node stating a fact rather than the network being unreliable.

A transport failure on **real traffic** also counts, without waiting for the next probe: a node
that dies between two polls should not eat every request in between. A `4xx` or `5xx` does not —
that is an answer, and a node that refused is not a node that is gone.

**When nothing is in rotation it fails closed**: `503 no_healthy_upstream`, `Retry-After: 1`, and
no request sent to a node known to be down. A node answering `serving:false` will answer `503`
anyway, so sending it work trades a clear refusal for a slower one; a node that is merely
*behind* would answer a count that is quietly wrong and cannot be un-sent.

## Selection

Least in flight, round robin on ties, never sticky.

Round robin spreads *counts*, which is the wrong quantity: a query whose ranges are local is a
direct call and one that fans out to two peers is three round trips, so counting requests keeps
handing work to a node already occupied by a slow scan. Least in flight spreads occupancy, and
it costs nothing — the in-flight ceiling already needed that count.

Not sticky, because there is no per-connection state upstream to be sticky to, and pinning a
long-lived client to a node that is about to stop serving is exactly what failover was for.

## The allowlist

An allowlist, not a denylist. A denylist is a list somebody can forget to extend; the
twenty-one `/internal/*` peer routes are excluded by not appearing.

Matching is positional against the **decoded** path segments, and the upstream path is rebuilt
from the segments that matched — never from the raw request line. That is what makes `..`, `%2f`
and collapsed slashes structurally unable to produce a request the table did not intend.

| tier | routes | default |
|---|---|---|
| data | `/schema`, `/table/{t}/records`, `/table/{t}/query`, `/sql`, `/table/{t}/import`, `/table/{t}/delete` | on |
| ddl | `/table/{t}`, `/table/{t}/field/{f}`, `/database/{d}` | on, `--no-ddl` drops it |
| ops | `/verify`, `/repair`, `/admin/*`, `/cluster/topology` | off, `--allow-ops` adds it |

`ops` is off because every route in it asks about **one node**, and a proxy chooses which node
without telling you. `POST /admin/backup` is the sharpest case: it copies the file of whichever
node received it, so through a proxy it means "back up a node, unspecified".

A route that is not in the table, and one above the tier in force, both answer `404
no_such_route` — the same sentence a typo gets. Not `403`: a refusal that distinguished the two
would answer, for free, the one question somebody probing for the peer surface wants answered.

`/health`, `/ready` and `/metrics` are answered by the proxy and never forwarded. Forwarding
`/health` returns one node's `{"status":"ok"}` chosen at random, which answers neither "is this
endpoint usable" nor "is the cluster up".

## Headers

The forward set is an allowlist too. The daemon reads four header names, so anything else is
surface with no destination.

| direction | carried |
|---|---|
| to the node | `Authorization`, `Content-Type` |
| back to the client | `WWW-Authenticate`, `Retry-After` |

`WWW-Authenticate` is not optional: a `401` without it is a refusal a client cannot act on.

Stripped unconditionally, on every route: `x-big-wire` and `x-big-cluster`. Those are the stamp
a node checks before it will decode a peer message, and forwarding a client-supplied one would
be this process vouching for a claim it cannot check.

`X-Forwarded-For` is **rebuilt** from the peer address rather than extended — a client that can
append to it is a client choosing what the logs say. `--trust-forwarded-for` opts into appending
for a deployment with a real load balancer in front, and says so at startup.

## Retries

Two clauses, and a rule that is not one.

**The request was never sent.** Connect refused, handshake failed, nothing reached the socket —
so the second attempt *is* the first. Safe for every route, imports included. The boundary is
the `write_all` call: once it has been entered, even if it returns an error, the request is
spent.

**The route is repeatable and the failure carried no answer.** `/schema`, `/verify`, listings and
queries. The second answer is the first answer.

`POST /sql` is **not** repeatable, and it is the entry most likely to be argued with. It looks
like a read and is not: `CREATE TABLE` goes through it too, and this proxy does not parse SQL to
find out which it got. Retrying a `CREATE TABLE` that actually succeeded answers "table already
exists" for a statement that worked — a wrong error rather than an honest failure.

**A `5xx` is never retried.** It is an answer. `ClusterError::is_unreachable` puts it best: *a
copy that could not be reached is worth asking another copy about; a copy that refused has given
an answer, and asking somebody else the same refused question turns a clear failure into a
confusing one.* The one exception is a `503` carrying `Retry-After` on a repeatable route —
retried once, on a different node — because that is the daemon saying "this node is busy".

## TLS, and the trap

To clients: `--tls-cert` and `--tls-key`, built with `peer_ca: None`, so this listener never asks
for and never accepts a client certificate. It is a front door for people.

To nodes: `--upstream-ca`, and **no client certificate at all**.

> `--upstream-ca` may point at the cluster's own `peer-ca.pem`. That is not a hole. A CA
> *certificate* is public — `certs.sh` writes it mode 644 and says so — and **trusting a CA is
> not being trusted by it.**

What would grant peer access is a certificate *signed* by that CA and naming a node in the
roster. There is no flag that loads one. Not "off by default": absent. Adding
`--upstream-cert` and `--upstream-key` later would be adding `/internal/*` access, which is why
the absence is written down here rather than left to be noticed.

Two independent things keep the peer surface closed, and neither depends on the other being
configured correctly:

1. The route table does not name `/internal/*`.
2. Presenting no certificate lands as `Identity::None` at a node, which every `Guard::Node` route
   refuses with `403 not_a_peer`.

The second works because a node's client verifier is built with `.allow_unauthenticated()` — a
listener that *required* a certificate would refuse every `bigctl` and every `curl`, which is
every client this database has.

**SNI is the node's name, not its address.** `certs.sh` issues `subjectAltName = DNS:<node>`, so
the handshake uses the name from `cluster.toml`. That is why `--upstream` takes `name=addr` and
not a bare address; getting it wrong produces a handshake failure that reads exactly like a
network problem.

## Ceilings

Each one sits a notch inside the daemon's, so the proxy retires a connection before the node
decides to close it — the discipline `clients/go/config.go` already follows.

| | proxy | daemon |
|---|---|---|
| requests per upstream connection | 900 | 1000 |
| upstream idle | 3s | 5s |
| body | 8 MiB | 8 MiB |
| in flight per node | 32 | — |

Set `--query-timeout` at or above the daemon's. Below it, the proxy gives up while a node is
still working on an answer nobody will read — and that is an outcome nobody knows.

## What it exposes about itself

`GET /health` — constant, asks no node, byte-identical to the daemon's.

`GET /ready` — `200` while at least one node is in rotation, `503` when none is. Read from what
the poller already knows, so a readiness probe cannot get slower as the cluster grows.

Its body is a **superset** of the daemon's shape rather than a different one: `status` still
reads `ready`, so a client decoding the daemon's answer draws the right conclusion, and the
per-node detail arrives in fields it ignores. What is absent is as deliberate — `node`, `shards`
and `tables` have no single value at this layer, and reporting some node's would be a lie rather
than an approximation.

`GET /metrics` — Prometheus text about the proxy, unauthenticated (unlike the daemon's, which
needs `OPERATE`; none of these counters says anything about anyone's data). The nodes' own
`/metrics` are not forwarded: a scraper reaching a different node on each scrape draws a graph
whose every point came from somewhere else.

Two numbers are worth an alert:

- `big_proxy_upstreams_in_rotation` reaching `0` — an outage.
- `big_proxy_route_denied_total` climbing — somebody knocking on doors that are not there.

`big_proxy_upstream_serving` is three-valued: `1` serving, `0` not, `-1` no agreement to report.
That last is what an unreplicated deployment looks like, and it is not the same as `0`.

## Deployment

`deploy/cluster/docker-compose.yml` publishes the proxy and nothing else:

```
cd deploy/cluster
./users.sh ; ./certs.sh
docker compose up -d
curl -u ops:$PASSWORD localhost:7654/ready
docker compose stop a
curl -u ops:$PASSWORD localhost:7654/ready   # in_rotation drops, queries keep working
```

That last pair is the only demonstration that matters: stop the machine a client was reaching,
and the client keeps working.

## What is deliberately absent

- **No shard-aware routing.** See above.
- **No topology refresh.** The node list could be re-read from `GET /cluster/topology`, but that
  route needs `OPERATE`, so the proxy would have to hold a credential — and it holds none. The
  node list is operational configuration, not something a front door infers.
- **No request buffering beyond one body.** 8 MiB × workers is bounded and small.
- **No response caching.** A cache in front of a database is a second source of truth.
- **No client certificate to the nodes.** Covered above, at length, because it is the one change
  somebody would make for convenience and must not.
