# golang-ex

Rows into a table through the [Go client](../../clients/go/), written twice, and the count not
moving. The whole thing is one command.

```sh
./run.sh
```

It builds two images, starts `big serve` with a users file, and runs the Go program twice
against it — 20,000 records each time. `./run.sh 60000` for more, which is also enough to make
the import take more than one request. Nothing is installed on your machine.

Expected output, abbreviated:

```
==> first run
WARN bigdb: sending a password over a plaintext connection; base64 is not encryption addr=big:7654
==> connected to big:7654
    node local, 0 table(s), version 0.1.0
==> table tx (country SET, amount INT)

==> importing 20000 records
    chunk 1: 40000 facts sent
    40000 facts in 1 request(s), 25ms

==> what is in there
    country  count  sum
    GB  5000  2637500
    ...

==> the record ids, which this client chose
    [0 1 2 3 4]
    Count(All()) = 20000

20000 records written. Run this again:
    ...

==> second run, the same command
    ...
20000 records before, 20000 after: the second run changed nothing, which is
the point. /import is idempotent because the caller names the address.
```

The warning on the first line is the client's, it is correct, and it is left on. There is no TLS
here and the address is not loopback from inside the container, so a password does cross a
plaintext socket — `WithoutPlaintextWarning()` would silence it, and an example that reached for
that would be teaching the wrong reflex. `docker-compose.yml` says `--insecure-no-tls` for the
same reason: out loud.

Stop it and delete the data with `docker compose down -v`.

The server is published on `127.0.0.1:7654` while it runs, so your own `bigctl` can talk to it.
`make start` and [`examples/producer`](../producer/) use that same port, so if it is taken:

```sh
BIG_PORT=7655 ./run.sh
```

Nothing inside the demo depends on it — the Go program reaches the server over the compose
network.

## What this is showing

**The client names every record, and that is what makes it idempotent.** The record id is the
loop counter in [`main.go`](main.go), so record 7 is record 7 on every run and a second run sets
bits that are already set. `/import` is the route where that is true: a fact is one bit at
`(field, record, value)`, and writing a bit twice is writing it once. Run it a third time with a
larger count and the report says the other true thing — the extra ids are new rows and the ones
before them were rewritten in place.

**This is the opposite half of [`examples/producer`](../producer/).** That demo writes the same
shape of data through `contrib/big-message`, which sends `INSERT`s that carry no ids and lets
the server allocate them — so running it twice doubles the count. Neither is a bug. If you have
a stream and no natural key, you take at-least-once and dedup downstream; if you have a key you
can turn into an id, `/import` gives you exactly-once for free. The trade is the one thing both
readmes are for.

**The chunk is the resume point.** `ImportStream` splits a batch across requests and calls back
after each one lands, with the number of facts sent so far. Because `/import` is idempotent, a
caller that wrote that number down could restart from it — which is the shape `bigctl import
--resume` has, and the only checkpoint the client offers. The example sets `WithMaxBytes(1 MiB)`
rather than taking the 7 MiB default for two reasons it spells out in a comment: a chunk is one
request and has to finish inside the client's deadline, and the chunk size *is* the resume
granularity.

**`country` is a `set`, and that is a measured choice rather than a modelled one.** A record
here holds exactly one country, which is what `mutex` exists to enforce — but the same 40,000
facts take about 13 seconds through a `mutex` field and 0.014 through a `set` or an `int`, and
the gap widens with the record count rather than staying put:

| facts, 4 distinct values | `mutex` |
|---|---|
| 10,000 | 1.60s |
| 20,000 | 4.18s |
| 40,000 | 10.87s |

Doubling the facts costs 2.6x the time, so the per-write cost grows with what the field already
holds — consistent with clearing a record's previous bit, and not explained by cardinality (at a
fixed 10,000 facts, going from 1 distinct value to 10,000 moves it 1.01s → 2.53s). The demo
writes one country per record either way; the constraint is what it gives up.

**Record ids are not a column.** The last thing each run prints comes from `Records`, not from a
query. `SELECT _record_id FROM tx` is refused — `_record_id` is what a record is *called*, and
`GET /table/{t}/records` is the route that lists them.

**The credential is a `superuser`, and that is a demo's compromise rather than a pattern.**
Privileges live in the catalog now — `CREATE ROLE` makes one and `GRANT` gives it something —
and `big passwd --help` says the consequence for a users file: a role name the catalog does not
have is simply no privileges. So `demo-app write` would not be a smaller version of this
credential, it would be a user who can do nothing at all, and a demo that opened with a
provisioning step would not be one command. [`deploy/`](../../deploy/readme.md) is where a role
per job belongs.

The refusals are worth seeing anyway, and both are one edit away in `docker-compose.yml`:
change the role to anything else for `403 forbidden: user ... does not hold CREATE`, or empty
the credential file for `401 unauthenticated`. `main.go` prints the server's own sentence and
code in both cases rather than a message of its own.

## Without docker

The example is an ordinary Go module with a `replace` onto `clients/go`, so it builds against
the client in this tree rather than a published one:

```sh
make start                              # 127.0.0.1:7654; loopback, so no credential needed
go run . 127.0.0.1:7654 tx 20000
go run . 127.0.0.1:7654 tx 20000        # same count
make stop && make clean-data
```

`make start` binds loopback and therefore needs no credentials at all — `big serve` refuses a
non-loopback address without a users file, and a container's loopback is its own, which is the
entire reason the compose file above has a credential in it.

`$BIG_CREDENTIALS` points at a file holding one `user:password` line; leave it unset against a
loopback daemon with no users file, which is what `make start` gives you. A credential is read
from a file and never taken as an argument, here for the same reason as in `bigctl`: an argument
is visible in `ps` and in shell history.

Exit codes are `bigctl`'s: `0` answered, `1` the server refused, `2` usage, `3` nothing was
listening.

## Not a deployment

The tokens are written in the compose file, the database is a throwaway volume, and there is no
TLS. That is fine for a thing that runs for a minute and is not fine for anything else.
[`deploy/`](../../deploy/readme.md) is the one to copy: single node and cluster, with the
credential in a file the deployment manages rather than in the yaml.
