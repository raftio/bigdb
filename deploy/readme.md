# deploy

Three deployments, because they are three different things and pretending otherwise is how a
single-node database ends up with a cluster's configuration and none of its guarantees.

| | What it is | When |
|---|---|---|
| [`single/`](single/) | One node, every shard, no peers | Anything that fits on one machine |
| [`cluster/`](cluster/) | Two ranges, one node each | More data than one machine holds |
| [`k8s/`](k8s/) | Three nodes that grow with `kubectl scale` | A cluster whose shape changes |

**`cluster/` and `k8s/` are not the same deployment in two syntaxes.** The compose one is a fixed
shape being explained: two nodes, two ranges, and every flag that would redraw them turned off.
The Kubernetes one is a shape that changes: `--balance` is on, so a pod that appears is admitted
and given the tail of the shard space by the cluster's own leader, and a pod registers itself as
it starts. What that costs is a third node - a majority of two is two, so a two-node cluster can
commit neither a failover nor a schema-leader handover.

**They are the same binary and the same code path.** `big serve` without `--cluster` builds itself a
cluster of one and runs every request through the same coordinator a multi-machine deployment
does; a second path for the un-clustered case would be the path nobody tests. What the two
directories differ in is a config file and how many containers there are.

## One node

```sh
cd single
mkdir -p secrets && chmod 700 secrets
printf 'change-me-please\n' | big passwd secrets/users set ops --role superuser
docker compose up -d
curl -u ops:change-me-please localhost:7654/ready
```

`superuser` and not `admin`: see [Who may do what](#who-may-do-what) below. A users file that
names a role the catalog does not have is a credential that authenticates and may do nothing,
which is what `--role admin` writes on a fresh database.

## Two nodes

```sh
cd cluster
./users.sh                       # writes secrets/users and prints the passwords it generated
./certs.sh                       # writes secrets/peer-ca.pem and one certificate per node
docker compose up -d

# **HTTPS, not HTTP.** Each node serves its port with the certificate `certs.sh` issued it -
# that is the same listener its peers dial, so there is no way for one to be TLS and the other
# not. The certificate names the node (`DNS:a`), so from the host either skip the check or
# resolve the name to loopback.
curl -k https://localhost:7654/ready
# {"status":"ready",...,"node":"a","shards":"0..64","serving":true,"term":1,"leader":"a","behind":[]}
curl -k -u ops:$PASSWORD https://localhost:7654/verify
# {"agree":true,"ranges":[{"shards":"0..64","primary":"a","agree":true,"copies":[...]}, ...]}

# Or without `-k`, checking the certificate against the CA that signed it:
curl --cacert secrets/peer-ca.pem --resolve a:7654:127.0.0.1 -u ops:$PASSWORD https://a:7654/ready

# The second credential `users.sh` wrote names a role that does not exist yet. Make it:
ctl() { docker compose exec -e BIG_ADDR=https://a:7654 a bigctl --ca-file /run/big/peer-ca.pem --user ops "$@"; }
ctl sql "CREATE ROLE dashboard"
ctl sql "GRANT OPERATE ON *.* TO dashboard"
```

`bigctl` is in the image, which is why the helper above reaches for it there rather than on your
machine. The address is the node's *name* because that is what its certificate says, and
`--ca-file` is the CA the entrypoint staged inside the container. `--user` asks for the password
on the terminal, which is what you want at one; a script wants `BIG_CREDENTIALS=<file>` or
`--credentials-file <file>`, one `user:password` line at mode 600, because a password given as an
argument is visible in `ps`. `serving: true` with a
`leader` is the line that says the two of them found each other; `serving: false` with
`leader: null` and a term that keeps climbing is the shape of a peer handshake that is failing.

Only the proxy is published. Any node answers any request - the one that receives it plans the
query and fans it out - so publishing a port per node would suggest a client has to choose, and
it does not.

Neither range has a copy, so neither fails over: stop `a` and shards `0..64` are unanswerable
until it is back. That is the smallest shape that runs, not the one to run in production - "What
a joining node needs first" below adds the third node that makes a copy usable.

## Five things worth knowing before you run either

**`big serve` refuses to bind anywhere but loopback without a users file**, and a container's
loopback is its own, so both compose files mount one. That refusal is the reason these examples
have credentials in them at all; it is not decoration.

**It refuses a non-loopback bind in the clear as well**, which is a second decision with a second
flag, and the one most likely to be met as a container that starts and exits two lines later - a
container binds `0.0.0.0`, so no address here satisfies the check by being local enough. **The two
deployments answer it differently, and the difference is not cosmetic.** `single/` passes
`--insecure-no-tls`, which is earned by the published port being `127.0.0.1` on the host: the
plaintext hop never leaves the machine. `cluster/` cannot take that answer, because the port a
person connects to is the same port the *peers* connect to - so every node gets `--tls-cert` and
`--tls-key`, and the waiver would only break the cluster instead.

**A users file that anyone but its owner can read is refused too**, and a bind mount carries
whatever ownership and mode the *host* gave it - root-owned `600` on one machine, `0755` on
Docker Desktop, some other uid elsewhere. Nothing the image can do makes those agree with a
strict check, so the entrypoint reads the file as root and writes a private copy owned by the
daemon's user, then **drops privileges before the daemon starts**. The database never runs as
root; the container is root for the length of one `install`.

What is mounted is the *directory*, never the files: a single-file bind mount breaks the first
time anything replaces the file rather than writing through it, which is what every editor and
most secret managers do.

If your runtime forbids starting as root, set `user:` in the compose file - the entrypoint
notices, skips the drop, and stages whatever it can read. Then the host files have to be
readable by that user, which is the trade you have chosen.

**One volume per node, never one shared.** One process holds one file - the pager takes an
exclusive lock - so two nodes pointed at one volume is two nodes fighting over a database only
one of them can open.

**A container that is killed outright loses nothing.** `big serve` has no signal handler and does not
need one: a commit writes its pages, fsyncs, flips the meta page and fsyncs again, so a process
that dies leaves a file that is either before that flip or after it. There is no state in
between, nothing to replay, and `stop_grace_period` is short on purpose.

## Who may do what

**Two halves, in two places.** Who you are is a line in the users file on the server's disk,
written by `big passwd` and read once at startup. What you may do is a set of grants in the
catalog, written by `GRANT` and read on every statement. A users file cannot hand out a
privilege, and no route can write a users file - one that could would let an `admin` credential
rewrite the credential file over the network.

**The `read`/`write`/`admin` ladder is gone.** A role is a name now, made with `CREATE ROLE` and
given privileges with `GRANT`, and a name the catalog does not have is *no privileges at all* -
the safe direction, and a silent one. A credential written `--role admin` on a fresh database
authenticates, reaches `GET /schema`, and is refused everything else.

**`superuser` is the way in.** Reserved by name, never stored, holds everything, cannot be
created, dropped or granted to. It exists because a grant has to be made by somebody who already
holds the privilege to make one, so an empty catalog has nobody who could write the first one.
Give it to `ops` and to nothing else.

```sh
bigctl --user ops sql "CREATE ROLE analyst"
bigctl --user ops sql "GRANT SELECT ON sales.* TO analyst"
big passwd secrets/users set alice --role analyst    # then restart this node
```

The last line is the trap: the users file is read **once, at startup**, so changing which role
somebody holds waits for a restart. Grants themselves are live - a `GRANT` or a `REVOKE` applies
to the next statement, cluster-wide - and a role created *after* a users file already names it
needs no restart either, which is why `users.sh` writes the name first and creates the role after.

**The operational routes are one server-wide privilege**, not a role that also happened to read
tables: `/metrics`, `/verify`, `/repair`, `/admin/backup` and everything under `/admin/cluster`
need `OPERATE ON *.*`. `/health` and `/ready` are never authenticated - they have to answer while
the database is unhealthy, which is exactly when a credential check might not.

docs/access-control.md is the whole of it, including the privilege table and the recovery drill.

## Changing the shape while it runs

**`cluster.toml` says what the cluster was when it started.** Every committed decision after that
replaces it: a range that split, a range that moved, a node that joined or left. The file is a
seed, so two nodes of one cluster legitimately hold different ones - and `cluster_id` is what
they recognise each other by, which is why the file has one and why every node's copy must carry
the same string.

`bigctl` is in the image, so these run from inside, with the same `ctl` helper the quickstart
defines:

```sh
ctl cluster topology
# node     addr          state  primary   copy    behind
# a        a:7654        voter  0..64
# b        b:7654        voter  64..900
# d        d:7654        voter  900..
# bigctl: leader `a`, schema leader `a`, epoch 1

# Scale out. What a fourth node needs is below - certificate, file, service - and then:
ctl cluster add-node d d:7654   # joins as a learner
ctl cluster admit d             # makes it a full member

# Give it something to hold: cut the tail above everything written, which moves no bytes...
ctl cluster split 900 to d
# ...or hand over a populated range, which does.
ctl cluster move 2 to d

# Scale in. Draining is not removal: the node still votes and still coordinates, it is just
# given no new ranges and has its own taken away. Removing one that still holds a range is
# refused.
ctl cluster drain b
ctl cluster remove b
```

One row per node, because a node is what both halves of that answer are about: `primary` is what
it answers for, `copy` is what it holds against a failover, and a node with both columns empty is
a learner that has joined and been given nothing yet. `--format json` hands over the whole
document unchanged, which is what a script wants.

**A node joins as a learner** - it replicates the log, holds no range and does not vote - so
nothing reads from it while it catches up and it does not raise the bar for an election it could
not help decide. `admit` is the step that makes it count.

### What a joining node needs first

Three things, and the second is the one that is not obvious.

**A certificate from the CA the cluster already trusts.** Re-running `./certs.sh` would mint a new
CA and lock out the nodes that are talking, so it takes a name instead and signs one more
against the CA that is there:

```sh
./certs.sh d          # writes secrets/d.pem and secrets/d.key, touching nothing else
```

**Either an address, or a `cluster.toml` that names the whole cluster.** The short way is
`--join`:

```sh
bigctl cluster join d d:7654      # run against a node already in the cluster
# added `d` at d:7654, as a learner. Now run this on d:7654:
#
#   big serve <file> d:7654 --join a:7654 --cluster-id big-demo --node d
```

`cluster join` adds the node here and prints the command to run over there, because the two
steps happen on two machines and only work in that order. **The order is not a formality**: a
node dials in presenting a certificate, and a certificate naming somebody the cluster has never
heard of is refused at the handshake - so the node has to be added before it starts, not after.

Two things the joining node still has to be told, because neither can be asked for. `--cluster-id`
is what stamps every peer request, so a node cannot dial its way to an id it does not have; and
`--peer-ca` is what makes a peer's answer worth reading. Both are flags an orchestrator can
template, where a file had to be copied to the machine and kept current.

**A cluster that runs no agreement cannot admit anybody**, and says so rather than letting the
node start and fail later. Admission is a decision, and a cluster whose ranges have no copies
commits none - so growing this way needs the third node that replication needs anyway.

The long way still works and is what a fixed deployment should keep using: copy the cluster's
file, append the new node as a `replica` of an existing primary - the shape that parses without
overlapping anybody's range, and a seed the agreement overwrites within the second - and start
it with `--cluster` and `--node d`.

```toml
cluster_id    = "big-demo"     # the same string, or the cluster does not recognise it
schema_leader = "a"
peer_ca_file  = "/run/big/peer-ca.pem"

# ... a and b exactly as they appear in cluster.toml ...

[[node]]
name    = "d"
addr    = "d:7654"
replica = "b"
```

**Its own volume and a service in the compose file**, with the same command as the others and
`--node d`. One volume per node, never a shared one.

Then `admit` - or let the node be admitted on its own once it has caught up - and give it
something to hold with `split` or `move`. `cluster topology` is how you check it landed, and
`verify` is how you check the copies agree afterwards.

**A proxy in front does not see any of this until it is told to look.** `bigproxy` reads its
upstreams once at startup, so a node admitted afterwards is one no request can reach; `--discover`
makes it follow the cluster's membership instead. Off by default, because a front door that grew
an upstream nobody wrote down is one whose shape an operator cannot predict. See
`crates/big-proxy/readme.md`.

**Nothing balances itself unless you ask.** The balancer is off by default, and
`bigctl cluster rebalance` is one step against facts gathered afresh: a cluster needing three
moves takes three calls, because each move is a moment where a query can fail. That is the call
an autoscaler or a Kubernetes controller puts on a timer, and it is the same verb an operator
runs by hand.

Two of these deserve their own warning. `cluster move` holds the request for as long as the range
takes to copy and refuses writes *to that range*, retryably, for the last pass only - reads never
stop. `cluster schema-leader` is the one change that corrupts rather than fails if it is got
wrong, which is why it copies every row key and every promised record id before it commits.

docs/clustering.md has the shape of all of it.

## Backups

**A running node is backed up over its own port, not with `big backup`.** The subcommand wants the
exclusive lock the daemon is holding, so `docker compose exec big big backup ...` answers `another
process holds this file` and writes nothing - it is the tool for a file nothing has open. What
works against a live node is the route, which both compose files configure with `--backup-dir
/data`:

```sh
# A consistent copy, taken while the daemon is serving. `cp` is NOT safe - a commit can land
# between the bytes it has already read and the ones it has not.
curl -u ops:$PASSWORD -X POST "localhost:7654/admin/backup?name=big-$(date +%F).db"
# {"backup":"big-2026-09-04.db","txn_id":41,"pages":8,"bytes":65536}

# And off the machine, because a backup on the same disk is not a backup.
docker compose cp big:/data/big-$(date +%F).db ./
```

The name is a file inside the backup directory and cannot name one outside it, and a name that
already exists is refused rather than overwritten - which is what keeps `name=big.db` from being
a way to lose the database. One at a time per node.

Without `--backup-dir` the route answers `501 backup_not_configured` and says so; the directory
is a command-line decision rather than a request one because a request that chose its own path
could write anywhere the process can.

In a cluster this is **per node**: each holds its own range, and a copy of one node is a copy of
one range - and the copies are not one snapshot. Only the proxy is published, so the nodes are
reached from inside, over their own TLS:

```sh
docker compose exec b curl -sk -u ops:$PASSWORD -X POST \
  "https://localhost:7654/admin/backup?name=b-$(date +%F).db"
docker compose cp b:/data/b-$(date +%F).db ./
```

Nothing runs this for you. That is a cron entry and a place to put the output, and neither is
in this repository.

## TLS: between the nodes here, and in front of the published port

**The cluster speaks TLS to itself.** `certs.sh` issues a CA and one certificate per node, and
that is how the nodes authenticate to each other - there is no shared secret between them at
all. A leaked key is one node rather than the whole cluster, which is what the peer token it
replaced could not offer.

**One listener, so the client port and the peer port are the same port.** That is the fact the
cluster's configuration follows from: give a node `--peer-cert` alone and it dials its peers over
TLS while answering them in the clear, which is a handshake failure on every peer, an election
that never settles, and `serving: false` on a node whose own logs look fine. So `cluster/` gives
every node `--tls-cert` and `--tls-key` - the same certificate `certs.sh` issued it, which is why
that script asks for `serverAuth` and `clientAuth` both - and its published port is HTTPS.

**`single/` has no peers, so it has the other answer.** It publishes to `127.0.0.1` and passes
`--insecure-no-tls`, because what is in front of the port is the operator's decision and the
default should not be "the internet".

**That flag is a promise about the port, and publishing it wider breaks the promise.** Two ways
to keep it: terminate TLS at a reverse proxy - `runbook.md` has a configuration that works, and
the flag is how you tell `big serve` you have done so - or give the published node `--tls-cert`
and `--tls-key` of its own and drop the flag entirely. A client then reaches it as
`bigctl --addr https://host:7654`, because TLS is chosen by the scheme rather than guessed.
