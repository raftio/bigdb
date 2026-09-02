# deploy

Two deployments, because they are two different things and pretending otherwise is how a
single-node database ends up with a cluster's configuration and none of its guarantees.

| | What it is | When |
|---|---|---|
| [`single/`](single/) | One node, every shard, no peers | Anything that fits on one machine |
| [`cluster/`](cluster/) | Two ranges and a copy of one | More data than one machine holds, or a range that has to survive losing one |

**They are the same binary and the same code path.** `big serve` without `--cluster` builds itself a
cluster of one and runs every request through the same coordinator a three-machine deployment
does; a second path for the un-clustered case would be the path nobody tests. What the two
directories differ in is a config file and how many containers there are.

## One node

```sh
cd single
mkdir -p secrets && printf 'change-me-please admin\n' > secrets/tokens
docker compose up -d
curl -H 'Authorization: Bearer change-me-please' localhost:7654/ready
```

## Three nodes

```sh
cd cluster
./tokens.sh                       # writes secrets/tokens and secrets/peer.token
docker compose up -d
admin=$(cat secrets/peer.token)
curl -H "Authorization: Bearer $admin" localhost:7654/ready
# {"status":"ready",...,"node":"a","shards":"0..64","serving":true,"term":1,"leader":"a","behind":[]}
curl -H "Authorization: Bearer $admin" localhost:7654/verify
```

Only `a` is published. Any node answers any request - the one that receives it plans the query
and fans it out - so publishing three ports would suggest a client has to choose, and it does
not.

## Four things worth knowing before you run either

**`big serve` refuses to bind anywhere but loopback without a token file**, and a container's
loopback is its own, so both compose files mount one. That refusal is the reason these examples
have credentials in them at all; it is not decoration.

**A token file that anyone but its owner can read is refused too**, and a bind mount carries
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

## Backups

`big` serves the file and backs it up - one binary, two subcommands - so a backup does not have
to happen somewhere else:

```sh
# A consistent copy, taken while the daemon is running. `cp` is NOT safe - a commit can land
# between the bytes it has already read and the ones it has not.
docker compose exec big big backup /data/big.db /data/big.db.backup

# And off the machine, because a backup on the same disk is not a backup.
docker compose cp big:/data/big.db.backup ./big-$(date +%F).db
```

In a cluster this is **per node**: each holds its own range, and a copy of one node is a copy
of one range. `docker compose exec b big backup ...` for each service.

Nothing runs this for you. That is a cron entry and a place to put the output, and neither is
in this repository.

## There is no TLS in either of these

Termination belongs to a reverse proxy, and `runbook.md` has a configuration that works. Both
compose files publish to `127.0.0.1` for that reason: what is in front of the port is the
operator's decision, and the default should not be "the internet".
