# big-proxy

One address in front of many nodes.

A cluster publishes one node's port, because publishing three would suggest a client has to
choose and it does not — any node plans a query, fans it out and merges what comes back. But the
shipped clients hold one connection to one address with no failover and no topology, so the
published node is a single point of failure for machines that are all perfectly healthy. That is
the gap this closes.

```
bigproxy 127.0.0.1:7650 --upstream a=a:7654 --upstream b=b:7654
```

## What it does not do

**It does not route by key.** Which node owns a shard is a value the nodes agree on, and it moves
without a byte of data moving with it. A proxy that computed the same answer would be a second
copy of `big-cluster`'s `ownership.rs` that is wrong for as long as it takes a moved range to
reach it — and that the cluster has no way to correct, because the proxy is not in the agreement.

The question it answers is narrower and has a published answer: *which node is alive*. `GET
/ready` is unauthenticated and reports `serving`, which the daemon documents as the one field a
load balancer may act on.

**It holds no client's credential.** `Authorization: Basic` is forwarded byte for byte and
never parsed here; the daemon stays the only process that has seen a client's password. No file
and no lock either, with one exception the operator asks for: `--discover-credentials` names a
file holding a credential of this proxy's own, used for one read and nothing else. See
**Membership** below.

**It does not decide what a node holds.** With `--discover` it reads *who is in the cluster*;
the `ranges` in the same answer are deliberately ignored, for the reason routing by key is.

## Membership

The upstreams above are the cluster as it was when this process started. A node admitted after
that is one no request can reach, so a front door in front of a cluster that grows is a front
door that has to be restarted to see the growth.

```
bigproxy 127.0.0.1:7650 --upstream a=a:7654 --discover --discover-credentials /etc/big/proxy.cred
```

`--discover` reads `GET /cluster/topology` from a node already in rotation, on the health
interval, and adopts what it finds. That route demands `Operate`, so a cluster with a users file
needs a credential here - one `user:password` line, mode 600, checked the way `bigctl` checks
its own. A cluster with no users file needs nothing.

**Off by default.** A deployment that grew an upstream nobody wrote down is one whose shape an
operator cannot predict from what they wrote, and that is worth more than the restart it saves.
It is the same stance the daemon takes with `--balance` and `--reclaim`.

**A discovered node starts out of rotation** and earns its way in with `--health-pass` probes,
where a node named on the command line starts in. The asymmetry is the point: an optimistic
start is right at startup because the alternative is answering `503` while every node is fine,
but a node that appears mid-flight invents no outage by waiting - and the moment a cluster
announces a node is the moment it is least likely to have caught up. **A node already known
keeps its health record**, so reading the membership every two seconds cannot become a way of
quietly resetting the health check.

**The answer comes from one node, chosen without telling anybody.** That is the honest weakness
of reading membership here, and it is why only a node in rotation is asked and why nothing in
this loop decides that a node is *ready* - it decides only that a node exists. Which of them get
requests stays the health check's call.

`big_proxy_upstreams_discovered_total` and `big_proxy_upstreams_removed_total` count the moves;
`/ready` lists the set as it stands.

## The route table

`src/allowlist.rs` names every route a client may reach, and it is an allowlist rather than a
denylist because a denylist is a list somebody can forget to extend. The twenty-one
`/internal/*` peer routes are excluded by not appearing in it.

| tier | routes | default |
|---|---|---|
| data | `/schema`, `/table/{t}/records`, `/table/{t}/query`, `/sql`, `/table/{t}/import`, `/table/{t}/delete` | on |
| ddl | `/table/{t}`, `/table/{t}/field/{f}`, `/database/{d}` | on, `--no-ddl` to drop |
| ops | `/verify`, `/repair`, `/admin/*`, `/cluster/topology` | off, `--allow-ops` to add |

`ops` is off because every route in it asks about **one node**, and a proxy chooses which node
without telling you. `POST /admin/backup` is the sharpest case: it copies the file of whichever
node received it, so through a proxy it means "back up a node, unspecified".

A route that is not in the table, and one that is above the tier in force, both answer `404
no_such_route` — the same sentence a typo gets. Not `403`: a refusal that distinguished the two
would answer, for free, the one question somebody probing for the peer surface wants answered.

`/health`, `/ready` and `/metrics` are answered here and never forwarded. Forwarding `/health`
would return one node's `{"status":"ok"}` chosen at random, which answers neither "is this
endpoint usable" nor "is the cluster up".

## Why it cannot reach `/internal/*`

Three independent things, and no flag turns any of them off.

1. The route table does not name those paths, so no request is ever built for one.
2. `src/headers.rs` strips `x-big-wire` and `x-big-cluster` from every client request, on every
   route. Those are the stamp a node checks before it will decode a peer message, and forwarding
   a client-supplied one would be this process vouching for a claim it cannot check.
3. **This proxy presents no client certificate.** At a node's listener that lands as
   `Identity::None`, which every `Guard::Node` route refuses with `403 not_a_peer`.

`--upstream-ca` may point at the cluster's own `peer-ca.pem`, and that is not a hole: the CA
*certificate* is public — `certs.sh` writes it mode 644 and says so. Trusting a CA is not being
trusted by it. What would grant peer access is a certificate **signed** by that CA and naming a
node in the roster, and **there is no flag that loads one**. Not "off by default": absent.
Adding `--upstream-cert` and `--upstream-key` later would be adding `/internal/*` access, and
that is why their absence is written down here rather than left to be noticed.

## Ceilings

Every one sits a notch inside the daemon's, so this proxy retires a connection before the node
decides to close it:

| | proxy | daemon |
|---|---|---|
| requests per upstream connection | 900 | 1000 |
| upstream idle | 3s | 5s |
| body | 8 MiB | 8 MiB |
| in flight per node | 32 | — |

## Status

Phase 1. The route table, the header rules, the pooled upstream client and the listener are in
place and forward to one node. Health polling, several nodes, selection and retries are Phase 2;
`/metrics` and TLS are Phases 3 and 4. `src/ops.rs` reports every node as `unprobed` until the
poller exists, which is the honest answer of a proxy that has not been told otherwise.
