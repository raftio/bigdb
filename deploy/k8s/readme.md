# k8s

Three nodes and a front door, and **`kubectl scale` is how the cluster grows**. The same binary
and the same flags as [`../cluster/`](../cluster/); what is different is that the shape is meant
to change here, so two flags that are off in the compose demo are on, and a pod that appears
gets itself into the cluster instead of waiting for somebody to write a file about it.

## Running it

There is no published image, so build both and put them where the cluster can see them:

```sh
# From the repository root.
docker build -t big:latest .
docker build -t bigproxy:latest -f deploy/proxy/Dockerfile .

kind load docker-image big:latest bigproxy:latest    # or: minikube image load, or push and
                                                     # edit `image:` in the two manifests
```

Then the credentials and the manifests:

```sh
cd deploy/k8s
./secrets.sh          # a CA of its own, a certificate per pod name, and three Secrets.
                      # Prints three passwords - keep them.
kubectl apply -k .

kubectl -n big rollout status statefulset/big
kubectl -n big port-forward svc/big 7654:7654 &
curl localhost:7654/ready
```

Two roles have to be made before the cluster is finished, and one of them is what lets it grow:

```sh
ctl() { kubectl -n big exec -it big-0 -- \
          bigctl --addr https://big-0.nodes:7654 --ca-file /run/big/peer-ca.pem --user ops "$@"; }

ctl sql "CREATE ROLE joiner"
ctl sql "GRANT OPERATE ON *.* TO joiner"      # a new pod registers itself with this
ctl sql "CREATE ROLE dashboard"
ctl sql "GRANT OPERATE ON *.* TO dashboard"   # /metrics, /verify
```

`bigctl` is in the image, the address is the node's *name* because that is what its certificate
says, and `--ca-file` is the CA the entrypoint staged inside the container. `--user` asks for the
password on the terminal, which is why that `exec` has `-it`; a script wants
`BIG_CREDENTIALS=<file>` instead, one `user:password` line at mode 600, because a password given
as an argument is visible in `ps`.

## Scaling out

```sh
kubectl scale statefulset/big -n big --replicas=5
ctl cluster topology
```

That is the whole of it, and what happens is worth following, because **only two of the four
steps are Kubernetes's**:

1. **A certificate.** Already issued. `secrets.sh` mints one per *name* up to a ceiling, and a
   name is cheap: a certificate grants nothing until the roster accepts it, and the roster
   follows the agreement. So there is no `certs.sh` run and nothing to restart.
2. **A cluster file that names the cluster.** Written by the `join` initContainer, per pod, at
   startup. A node's listener accepts a peer whose certificate names somebody on its roster, and
   the roster it *starts* with comes from its own file - a node whose file named only itself
   would refuse every peer that dialled it, have none of its own to dial, and sit there healthy
   and alone. Once it is in touch the roster follows the agreement.
3. **A membership entry.** The same initContainer calls `cluster add-node`, before the daemon
   starts. That registers the pod as a **learner**: it replicates the log, holds no range and
   does not vote, so it neither answers for data it has not got nor raises the bar for an
   election it could not help decide.
4. **A range.** Nobody asks for this. The balancer on the agreement's leader admits the learner
   once it has caught up, and then cuts the tail of the shard space over to a node that holds
   nothing - a split that moves no bytes. One decision per pass, each one committed. That is
   `--balance`, and it is the flag `deploy/cluster` leaves off because two nodes and two ranges
   are the thing that file is explaining.

**The order in step 3 is not incidental.** A node that starts while the cluster has never heard
of it is a voter in a cluster of its own imagining, and it will stand for election once per
timeout - raising the term on every node that hears it - until somebody tells it otherwise.
Registered first, it is reached as a learner, and a learner does not stand.

If the `joiner` role does not exist yet, or the `big-joiner` Secret is missing, the pod still
starts and its init container prints the one command to run:

```sh
kubectl -n big logs big-4 -c join
# join: no credential mounted, so big-4 will not register itself. Run:
# join:   bigctl cluster add-node big-4 big-4.nodes:7654
```

**Past the ceiling.** `secrets.sh` issues eight names and the proxy is told about eight
addresses. `MAX=16 ./secrets.sh` issues the rest, and `upstreams.toml` in
[`10-config.yaml`](10-config.yaml) is the matching list - those two are the same list, and they
are the only two places a ceiling exists.

## Scaling in is not the mirror image

`kubectl scale --replicas=3` deletes `big-4`, and if `big-4` holds a range that range is
unanswerable until it comes back: nothing here is replicated, so the proxy picks a node that is
up and cannot pick data that is not there. **Take the work off it first:**

```sh
ctl cluster drain big-4      # still votes, still coordinates; the balancer moves its ranges away
ctl cluster topology         # wait until its `primary` column is empty
ctl cluster remove big-4     # refused while it still holds a range - that refusal is the point
kubectl scale statefulset/big -n big --replicas=4
```

Draining is not removal, and removal is not deletion. Each is a separate decision because each
has a different failure: a node removed while it holds a range takes the only copy of it, and a
map that goes on naming a node nobody talks to is worse than one that says it is draining.

The PVC is kept when a pod is scaled away (`whenScaled: Retain`), so scaling back up gives
`big-4` its file back rather than an empty one. That is what you want after an accidental scale
down and what you do not want after a real one - `kubectl delete pvc data-big-4` is the second
half of a scale-in that was meant.

## The decisions

### One StatefulSet, and `replicas` as the knob

A node here is not interchangeable with the next one - it has a range, a certificate and a name -
which is the argument for one workload per node, and it is the wrong argument. What it produces
is a deployment where growing means writing YAML, and the cluster is already able to admit a
learner and give it work without being asked. So: one StatefulSet, pod names as node names, and
the parts that genuinely are per-node are handled where they belong - the certificate by a name
issued in advance, the cluster file by an initContainer, the range by the balancer.

What ordinals cost is that the node names are Kubernetes's (`big-0`, not `a`), and that one
Secret is mounted by every pod. Both are written down where they bite: the second is on the
`secrets` volume in [`20-nodes.yaml`](20-nodes.yaml).

### Three lists, three rules

The mistake this deployment makes easy is treating them as one list. They are in one file,
[`10-config.yaml`](10-config.yaml), with the difference at the top:

| | May name a node that does not exist | Why |
|---|---|---|
| `seed.toml` | **No** | A node with no log counts a majority out of it. Three entries with two pods up is a cluster that cannot elect anybody. |
| `upstreams.toml` | Yes | The proxy polls, finds it unreachable, leaves it out of rotation - and picks it up within a health interval when it appears. |
| the certificates | Yes | A certificate is checked against the roster, and the roster follows the agreement. An unused key grants nothing. |

`seed.toml` names the founding three and is **not edited when the cluster grows**. Membership
after that is a committed decision, and `bigctl cluster topology` is where it is. A ConfigMap
edit does not reshape a running cluster and a rollout does not undo one.

### Three nodes, not two

Two is the size at which nothing works: a majority of two is two, so a two-node cluster can
neither fail a range over nor hand the schema-key namespace on - both are decisions a majority
has to commit at the moment one node is the one that went away. `deploy/cluster` runs two because
compose runs on one machine and the second node is there to show the shape.

That is also why `--elect-schema-leader` is on here and off there: with two nodes it is inert
rather than merely cautious.

### The peer Service publishes not-ready addresses

A Service's endpoints are the ready ones by default. A node that has not passed its probe would
have no address; a peer that cannot reach it cannot help it settle the election that would make
it ready; both wait for each other on a cold start. `publishNotReadyAddresses: true` is the flag
that is not that deadlock, and it is also what a joining pod is dialled at before it is anything.

### A probe can say "answering". It cannot say "serving"

`GET /ready` on a node is **always `200`**. Whether it is actually serving its range is
`"serving": true|false` in the body, because a node that has lost touch with the agreement is
still perfectly healthy - it is one promotion away, and restarting it would help nothing. An
`httpGet` probe reads the status line and never the body.

**That is what the proxy is for.** It polls `/ready` every couple of seconds, reads the field and
takes a node that says `false` out of rotation - see `crates/big-proxy/src/health.rs`, whose
module documentation is written around exactly this. So the Service a client is given is the
proxy's, and the proxy's own `/ready` *is* a real readiness signal: `503` when there is nowhere
to send a request, which takes that pod out of its Service.

It also means `podManagementPolicy: OrderedReady` is safe here. If readiness meant "serving", an
ordered start would wait for a quorum that needs the next pod.

### Why nothing runs as root, and the Secret is mode 0440

`big serve` refuses a users file or a private key that anyone but its owner can read. A Secret
volume cannot satisfy that directly:

- its files are owned `root:<fsGroup>`, and the daemon is uid 1000;
- `defaultMode: 0400` is therefore unreadable by the daemon - the entrypoint would silently stage
  nothing, and the failure presents as `cannot read /run/big/users` rather than as a permission
  error on the mount;
- `defaultMode: 0440` is readable, and is exactly what the check refuses.

So the staging in [`../entrypoint.sh`](../entrypoint.sh) - written for bind mounts, whose
ownership the image cannot predict - is what makes a Secret work at all: it copies each file into
`/run/big` at mode 600, owned by whoever is running. `fsGroup: 1000` makes the source readable;
the private copy is what passes the check. The `join` initContainer does the same thing with its
credentials file, for the same reason.

There is nothing left for root to do, so these pods start unprivileged: `runAsNonRoot`, no added
capabilities, `RuntimeDefault` seccomp, a read-only root filesystem with an `emptyDir` over
`/run/big`. The entrypoint notices it is not root and skips the privilege drop - the same path
`user:` in a compose file takes. The namespace enforces the `restricted` Pod Security profile, so
a manifest that regresses on any of that is rejected rather than quietly accepted.

### ClusterIP, and no Ingress in this directory

The compose file publishes to `127.0.0.1` because the hop from a client to the proxy is in the
clear. A ClusterIP is the same promise with the boundary drawn around the cluster instead of the
machine, and `kubectl port-forward` is the loopback hop.

Adding an Ingress or a `type: LoadBalancer` here would publish a plaintext database port, so
neither is in this directory. The fix is not an annotation: give the proxy `--tls-cert` and
`--tls-key` from a Secret of its own and drop `--insecure-no-tls`, or terminate TLS in front of
it - [`../../runbook.md`](../../runbook.md) has a configuration that works.

**`/verify`, `/repair` and `/admin/*` are not reachable through the proxy**, and that is the
allowlist rather than an omission: each asks about *one node*, and a proxy chooses which node
without telling you. `POST /admin/backup` is the sharpest case - through a front door it would
mean "back up a node, unspecified". `--allow-ops` turns them on if you want that anyway.

## Rollouts, drains, and the range that has no copy

No range here has a copy, so **stopping a node makes its range unanswerable until it comes
back**. Every operation below inherits that one fact.

- **A rollout** deletes one pod at a time, so each node's range is down for as long as its pod
  takes to come back. `kubectl rollout status` is the length of it.
- **`kubectl drain`** on a machine evicts the pod, which is the same thing. There is no
  PodDisruptionBudget on the nodes on purpose: `maxUnavailable: 0` would not protect the data, it
  would hang the drain forever and call that protection. What makes a node evictable is a copy of
  its range.
- **The proxy has one**, `minAvailable: 1`, because a second proxy genuinely is as good as the
  first - it holds no file, no lock and no identity.
- **A killed pod loses nothing.** A commit writes its pages, fsyncs, flips the meta page and
  fsyncs again, so a process that dies leaves a file that is either before that flip or after it.
  `terminationGracePeriodSeconds: 2` is generous.
- **The PVCs outlive the StatefulSet.** `kubectl delete -k .` leaves them behind and a re-apply
  picks them back up: deleting a manifest must not be a way to delete a database.

## Backups

Per node, over the node's own port, and never with `big backup` - that subcommand wants the
exclusive lock the daemon is holding:

```sh
kubectl -n big exec big-0 -- curl -sk -u ops:$PASSWORD -X POST \
  "https://127.0.0.1:7654/admin/backup?name=big-0-$(date +%F).db"
kubectl -n big cp big-0:/data/big-0-$(date +%F).db ./
```

Each node holds its own range, so a copy of one node is a copy of one range, and the copies are
not one snapshot. Nothing here runs this for you: that is a CronJob and a bucket, and neither is
in this repository.

## What is not here

**An operator.** `bigctl cluster topology --format json` is the whole surface a placement
controller reads - which ranges exist, who holds each, what is moving - and `rebalance` is one
step against facts gathered afresh. What the initContainer does is the smallest thing that made
`kubectl scale` mean something: write a file, make one call. Everything past that - watching the
API, reacting to a node failure, deciding when to grow - is a controller's job and is not
pretended at here in eighty lines of shell.

**A HorizontalPodAutoscaler.** Scaling out is cheap and scaling in is a sequence with a refusal
in the middle of it; a controller that scaled on CPU would eventually delete a pod holding the
only copy of a range.

**Replication.** The balancer moves ranges; it does not make copies of them. A range with a copy
is what makes a rollout invisible and a node evictable, and setting one up is
[docs/clustering.md](../../docs/clustering.md).

**Monitoring.** `/metrics` is Prometheus text on both, and the two are not the same route. The
proxy's is unauthenticated - it counts requests and latencies across a hop and says nothing about
anyone's data. A node's needs `OPERATE ON *.*`, is per node, and is not reachable through the
proxy, so a scrape config points at the `nodes` Service and carries the `dashboard` credential.
