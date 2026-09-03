# producer

Messages into a table, and the table read back. The whole thing is one command.

```sh
./run.sh
```

It builds two images, starts `big serve` with a users file, creates `tx`, produces 20,000
messages through [`contrib/big-message`](../../contrib/big-message/), and then asks the server
what it got. `./run.sh 200000` for more. Nothing is installed on your machine.

Expected output, abbreviated:

```
==> producing 20000 messages
20000 rows

==> how many landed
count
20000

==> and what they were
country  count
GB       5000
JP       5000
US       5000
VN       5000

==> the record ids, which the producer never named
{"records":[0,1,2,3,4],"next":4}
```

Stop it and delete the data with `docker compose down -v`.

The server is published on `127.0.0.1:7654` while it runs, so your own `bigctl` can talk to it.
`make start` in this repository uses that same port, so if it is taken:

```sh
BIG_PORT=7655 ./run.sh
```

Nothing inside the demo depends on it - the producer reaches the server over the compose network.

## What this is showing

**The client never names a record.** The last thing `run.sh` prints is `{"records":[0,1,2,3,4],…}`,
and nothing the producer sent decided those numbers. Record ids are the engine's own coordinates -
`shard_of(record)` is what picks the node that holds a row - so the SDK has no vocabulary for
them, and could not have one without deciding sharding on the client. Grep the producer's API for
`record` and there is nothing to find.

It takes a different route to see them at all, which is the same point from the other side:
`_record_id` is what a record is *called*, not a column a select list can ask for. `SELECT
_record_id FROM tx` is refused; `GET /table/tx/records` is the way, and there is no `bigctl`
subcommand for it either.

**Which is why running it twice doubles the count.** An `INSERT` that does not carry ids gets new
ones, so a message sent again is a new row rather than the same row rewritten. `big-message` is
at-least-once and says so; the narrow window where that bites in practice is `Error::Unknown` -
the request was written in full and the answer never arrived - and the producer stops there
rather than guessing. `contrib/big-message/readme.md` has the reasoning, and `examples/redis-sink`
will have the dedup that bounds it.

(The other ingest path, `bigctl import`, *does* carry record ids, and is idempotent because of
it. It is also about 4.5x faster. Use it when you have a file; this crate is for when you have a
stream.)

**Two tokens, not one.** `docker-compose.yml` declares `demo-admin admin` and `demo-writer write`.
The server raises the authority it demands per statement - DDL needs `admin`, an `INSERT` needs
`write` - so the producer runs with the smaller of the two. If you want to see the check, put
`demo-writer` in the `ctl()` helper in `run.sh` and watch `CREATE TABLE` get refused.

## Without docker

If you have the toolchain, the same demo is a handful of lines and no containers. `make install`
first, or spell `bigctl` as `target/debug/bigctl`:

```sh
make start                                        # 127.0.0.1:7654; loopback, so no token needed
bigctl sql 'CREATE TABLE tx (amount INT, country TEXT)'
cargo run -p big-message --example produce -- 127.0.0.1:7654 tx 20000
bigctl sql 'SELECT country, count(*) FROM tx GROUP BY country'
curl -s 'http://127.0.0.1:7654/table/tx/records?limit=5'
make stop && make clean-data
```

`make start` binds loopback and therefore needs no credentials at all - `big serve` refuses a
non-loopback address without a users file, and a container's loopback is its own, which is the
entire reason the compose file above has credentials in it.

## Not a deployment

The tokens are written in the compose file, the database is a throwaway volume, and there is no
TLS. That is fine for a thing that runs for ninety seconds and is not fine for anything else.
[`deploy/`](../../deploy/readme.md) is the one to copy: single node and cluster, with the
credential in a file the deployment manages rather than in the yaml.

One difference is deliberate rather than sloppy. `deploy/single` bind-mounts its secrets
directory; this file uses a `config` with the content inline. A bind mount needs the docker
daemon to be able to see the directory, which is true of a local daemon and false of a remote or
rootless one - point `docker context` at a daemon over SSH and a mounted secrets directory
arrives *empty*, with no error to explain it. A demo should run against whatever docker you have.
