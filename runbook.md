# Runbook

What to do to a live `big` database, and what not to do to one. Everything here is exercised
by tests — [backup.rs](crates/big-db/tests/backup.rs) for the file operations,
[delete.rs](crates/big-db/tests/delete.rs) and [server.rs](crates/big-http/tests/server.rs) for
undoing an import. Nothing here is a procedure that has only been reasoned about.

Build the tool once:

```sh
cargo build --release -p big-bin --bin big
```

---

## The one rule

**Never copy a live database file with `cp`, `rsync`, `tar`, or a filesystem-level snapshot
you have not thought about.**

A commit writes its pages, fsyncs, then flips one meta page and fsyncs again. A byte-level
copy that starts before the flip and finishes after it captures half of each — pages from the
new transaction under a meta page that still describes the old one, or the reverse. The result
usually opens. It is not the same database.

There are two supported ways, and which one you want depends on whether the database is being
served.

**Served:** `POST /admin/backup?name=<file>` on the daemon itself. The walk holds a read
transaction for its whole run, so writers keep committing and nothing stops. This is the route
to schedule.

**Not served:** `big backup`. Every subcommand of `big` takes the file's exclusive lock,
so it cannot open a file `big serve` is serving at all - which is why the online path had to be a
route rather than a second process.

Backing up a live database from *outside* the daemon is still not supported, and never will be.

---

## Take a backup

```sh
# While serving. Needs `big serve --backup-dir /backup` and `OPERATE` on `*.*`.
curl -X POST -u "$USER:$PASSWORD" \
  "http://127.0.0.1:7654/admin/backup?name=data-$(date +%F).big"
# {"backup":"data-2026-08-30.big","txn_id":41,"pages":1873,"bytes":10403840}

# Or, with nothing serving the file:
big backup /var/lib/big/data.big /backup/data-$(date +%F).big
```

- `name` is a file **inside** `--backup-dir` and cannot name anything outside it: no
  separators, no leading dot. Without the flag the route answers `501 backup_not_configured`.
- **One at a time.** A second concurrent call is `409 backup_in_progress`. Two walks would each
  pin the reclaim horizon for their whole run while writers kept committing, and the file would
  grow by everything both of them saw.
- **In a cluster this is one node's file.** Back up every node, and understand what you have:
  each copy sits at that node's own transaction, so the set is not a cluster-wide snapshot. See
  [clustering](docs/clustering.md) for why that is not fixable here.
- **The offline form requires the daemon to be stopped.** `big` takes the exclusive lock, and `big serve` holds it for as
  long as it is serving. The claim that a backup is safe alongside a writer is about the walk,
  not about two processes: the copy holds a read transaction, which pins the reclaim horizon so
  a concurrent writer cannot reuse a page the walk still needs. That is what makes an *online*
  backup possible - it is not available from a second process.
- The destination **must not exist**. A backup never overwrites.
- The result is a complete, compact, ordinary database file. It is smaller than the source
  whenever the source has churn on the freelist.
- Cost: it reads every live page and writes every live page. Budget disk and time for the
  size of the *live data*, not the size of the file.

While it runs, `free_pages_reusable` on the source stops falling and
`pages_pending_reclaim_reader` climbs. That is the horizon doing its job, not a leak. Both
recover once the backup finishes.

## Restore

```sh
big restore /backup/data-2026-08-27.big /var/lib/big/data.big
```

Restoring is copying the file into place and opening it. There is no conversion, no replay,
and no separate restore format — a backup *is* a database. `restore` is a spelling of `backup`
that exists so the procedure has a name in a checklist.

Verify before cutting over:

```sh
big verify /var/lib/big/data.big
```

Opening is the check. A bad checksum, an unreadable meta page, or a file version this build
does not know is an error on the way in, so anything that opens and prints its tables is
structurally sound.

## Load a file

`POST /import` is bounded at 8 MiB, so `curl --data-binary @facts.txt` works until the file is
real and then answers `413 request_too_large`. `bigctl import` is the loader: it cuts the file into
chunks of whole lines, sends them in order, and writes down the byte offset the server
acknowledged.

```sh
bigctl import tx facts.txt --resume tx.ck
```

- **An interrupted load resumes rather than restarts.** Run the same command again; it starts at
  the offset in `tx.ck` and says so. The checkpoint is removed when the load finishes, so a
  re-run after that really re-runs.
- **A re-run is safe.** A fact is a bit set at a record id written into the line, so a chunk
  sent twice writes what sending it once wrote. That is also why a dropped connection is
  retried, three times by default, with a backoff.
- **A refusal stops the load and is never retried.** `malformed_line` names the line, and the
  next message names the byte the chunk started at. Fix the file from there; note that
  `--resume` will then refuse the old checkpoint, because the file's size no longer matches the
  one it was taken from.
- **`--chunk-bytes` is the throughput knob**, because the server commits once per request.
  The default is 7 MiB against a ceiling of 8; raising it is worth measuring and lowering it is
  worth doing only if something in front of `big serve` has a smaller idea of large.
- **`missed` means a copy did not take the write.** `bigctl` prints it once per copy and carries
  on, because re-sending reaches the same copies. `bigctl repair` is what closes it.
- One request is in flight at a time, on purpose: there is one writer, and a second request
  would queue behind the first while making the checkpoint meaningless.

A CSV goes through `awk` first — `awk -F, '{print "country", NR, $3}' data.csv | bigctl import tx -`
— at the cost of `--resume`, which needs a file it can seek.

## Undo a wrong import

Deleting is a normal write, so it needs no downtime and no restore:

```sh
# by record id, one per line
printf '101\n102\n' | curl --data-binary @- localhost:7654/table/tx/delete

# a whole field, or a whole table, with its pages
curl -X DELETE localhost:7654/table/tx/field/mistyped
curl -X DELETE localhost:7654/table/scratch
```

- `/delete` reports how many of the ids existed. Deleting one that was never written is a
  success with a count of zero, so a retried delete is safe.
- The whole body is parsed before anything is removed: a malformed line refuses the batch
  rather than removing half of it.
- Cost is not uniform. Clearing a record from a keyed field costs one pass over that shard's
  distinct values, so **deleting in one batch is worth far more than one call per record**.
- Dropping frees the trees immediately, but the pages go to the freelist, not to the
  filesystem. Run `big compact` if the file size is what you were trying to reduce.

Restoring a backup is still the answer when the mistake is older than your retention of what
exactly was imported — a delete needs to know which ids to remove.

## Reclaim disk

```sh
big compact /var/lib/big/data.big
```

**If the database is being served, do this instead**, because it moves the expensive half of
the work to a moment when nothing is stopped:

```sh
curl -X POST -u "$USER:$PASSWORD" \
  "http://127.0.0.1:7654/admin/backup?name=compacted.big"   # online; the copy is compact
systemctl stop big
mv /backup/compacted.big /var/lib/big/data.big
systemctl start big
```

A backup **is** a compact copy - the pages are allocated in walk order into a store with an
empty freelist, so the holes copy-on-write left behind do not exist in the result. What cannot
happen online is the *swap*: `read` hands back a page borrowed straight out of the mapping, and
replacing the file under a live mapping is exactly the dangling reference the pager's four mmap
constraints exist to prevent. So the copy runs while serving and only the rename needs the stop.

- **`compact` itself is offline.** It takes the exclusive lock, so stop `big serve` first. Running it against a served
  database fails with a lock error rather than doing anything behind the daemon's back.
- Needs free space for a second copy of the *live data* alongside the original, briefly.
- Writes `data.compacting`, then renames it over the original and fsyncs the directory. A
  crash before the rename leaves the original untouched and a stray `.compacting` file to
  delete; the rename itself is atomic, so there is no state where the file is half-replaced.
- Prints how many pages it reclaimed.

Compaction is the **only** operation that gives space back to the filesystem. Free pages
inside the file are reused by the next allocation, so a file does not grow without bound — but
it does not shrink on its own either. A table that was large once keeps its high-water mark
until someone runs this.

## Check the file for rot

```sh
big scrub /var/lib/big/data.big
```

`verify` proves the file *opens*, which checks the meta page and the three fixed chains. It
says nothing about the trees, because opening does not read them - and nothing on the query
path checks a branch or leaf checksum either, since a crc over 8 KiB would be the whole cost of
a point read. **So a rotted b-tree page is not an error when a query lands on it. It is a
different number.** `scrub` is the walk that finds that.

- Offline, like every `big` subcommand, and it costs a full read of the live data. Run it
  against a backup, or against a node taken out of rotation.
- It stops at the first mismatch. One bad page and four hundred call for the same action -
  restore from backup - and continuing would mean walking a structure whose page numbers can no
  longer be trusted.
- Dense bitmap pages are covered too, against the checksum the leaf cell above them carries.
- A backup **also** verifies as it copies, so a `POST /admin/backup` that succeeds is itself
  evidence the source was intact at that transaction.

## Drop a time quantum field's old days

```sh
big drop-days /var/lib/big/data.big v visit 1767225600
```

- The day that instant falls in is **kept**. "Keep from here on" that quietly took one more day
  would be off by one in the direction nobody checks.
- **The records are not deleted.** What goes is the per-day index over them, so a windowed
  query stops finding them and a `count(*)` does not change. To remove records, use `/delete`.
- Offline and per file on purpose. A cluster where one node has dropped a day and another has
  not answers a query over that day with part of the truth and no symptom.
- The pages go to the freelist, not to the filesystem. `big compact` if the size is the
  point.

---

## When to worry

The pager collects these and `GET /metrics` exports every one of them, so the table below is a
set of alert rules rather than a debugging exercise. `big verify` prints the same numbers for
a file no daemon is serving.

| Signal | Means | Do |
|---|---|---|
| `page_count` grows while data does not | churn is outrunning reuse | `big compact` during a window |
| `pages_pending_reclaim_reader` stays high | a reader is stuck open | find the long query; reclamation resumes when it ends |
| `pages_pending_reclaim_retention` stays high | a snapshot is pinning old state | drop the snapshot; nothing else releases those pages |
| `free_pages_reusable` large and stable | space is reusable but not returned | normal; compact only if the file size matters |
| open says "neither meta page is readable" | the file is damaged | restore from backup — there is no repair path, by design |
| open names a format version | written by a different build of big | not a damaged file; see below |

## The file will not open

1. `big verify` on the file. The error names which check failed.
2. A version mismatch — the message names both the file's version and the one this build
   writes. **No migration tool exists yet**, because only one format version has ever been
   released; the policy for when that changes is dump/reload, never in place, and it is written
   down in [architecture.md](architecture.md). This is reported distinctly from damage on
   purpose: both used to come out as "neither meta page is readable", and they call for
   opposite actions.
3. `Locked` — another process holds it. `big serve` is probably still running.
4. "neither meta page is readable", or a checksum failure — restore the most recent backup. There is deliberately
   no repair tool: with no WAL there is no half-applied state that a repair could reason
   about, so a file that fails these checks is damaged by something outside the engine
   (hardware, a truncating copy, a byte-level copy of a live file).

## What is not covered yet

Honest gaps, so nobody builds a procedure on top of something that does not exist:

- **No metrics endpoint, no logging.** A failed import returns an HTTP status and nothing
  else.
- **No authentication.** Anyone who can reach the port owns the data. Bind to loopback and put
  a reverse proxy in front, or do not expose it.
- **No query timeouts, no connection cap.** A large query runs to completion; a client that
  disconnects does not stop it.

---

## Running the server

```sh
big serve /var/lib/big/data.big 127.0.0.1:7654 --users /etc/big/users
```

Everything `big serve` takes:

| | Default | What it bounds |
|---|---|---|
| `addr` | `127.0.0.1:7654` | Where it listens |
| `--users <file>` | none | Usernames, roles and password hashes. Must be mode `600` |
| `--tls-cert <file>` | none | PEM certificate chain this server presents |
| `--tls-key <file>` | none | PEM key for it. Must be mode `600` |
| `--peer-cert <file>`, `--peer-key <file>` | none | What this node presents to its peers |
| `--insecure-no-auth` | off | Permits a non-loopback bind with no users |
| `--insecure-no-tls` | off | Permits a non-loopback bind in the clear |
| `--workers <n>` | `cores × 4`, min 8 | Requests handled at once |
| `--queue <n>` | `64` | Connections allowed to wait. Past this: `503` |
| `--read-timeout <s>` | `30` | How long a client may take to send a request |
| `--query-timeout <s>` | none | Wall-clock budget for one query. `0` means none |
| `--durability <level>` | `full` | What a commit promises. See below |
| `BIG_LOG` | `info` | `off`, `error`, `warn`, `info`, `debug` |

**`big serve` refuses to bind anywhere but loopback without `--users`, and refuses again
without a certificate.** Neither is a warning that can be scrolled past — each exits `2`. Anyone
who can reach an unauthenticated port can read and delete everything in the database; anyone on
the path of an unencrypted one can read the password, which is a thing the person also uses
somewhere else. The safe shapes are: bind to loopback and put a proxy in front, or pass both a
users file and a certificate.

`--insecure-no-auth` and `--insecure-no-tls` are two flags because they answer two decisions —
an operator terminating TLS at a proxy needs the second and must not be made to buy the first
with it. Both are named so that they show up in a `ps` listing and in a review.

### The command line

Every route below is reachable with `curl`, and every example in this document uses it — that is
deliberate, because `curl` is on the box and proves the surface needs nothing else. `bigctl` is the
same routes spelled for a shell, and is worth having when a person is typing rather than a script:

```sh
export BIG_ADDR=https://big.internal:7654   # or --addr; a bare host:port is plaintext
export BIG_CREDENTIALS=/etc/big/client.cred # or --credentials-file; mode 600, `user:password`
# --user <name> asks for the password on the terminal instead. There is no --password flag:
# an argument is visible in `ps` and in shell history.

bigctl schema
bigctl sql "SELECT country, count(*) FROM tx GROUP BY country"
bigctl query tx 'Count(Row(country="GB"))'
bigctl records tx --limit 1000           # the cursor is printed to stderr, not into the data
bigctl import tx facts.txt               # or `-` for stdin; chunked and resumable
bigctl verify
bigctl shell                             # sql> by default, .lang pql to switch
```

**Exit codes are the interface for a script**: `0` answered, `1` the server refused and the code
is on stderr, `2` the command line was wrong, `3` nothing was listening. Output is aligned
columns to a terminal and TSV to a pipe, so `bigctl records tx | wc -l` does what it looks like;
`--format json` hands over the server's body untouched.

`bigctl` has no line editing. `rlwrap bigctl shell` gives it history and arrow keys.

There is **no offline mode**: `bigctl` talks to a daemon, always. The offline half
is `big` — backup, restore, compact, verify — and it takes the exclusive lock, so it is the tool
for a database that is *not* being served.

### Field kinds

`POST /table/{t}/field/{f}?kind=<kind>&bit_depth=<n>`:

| kind | holds | notes |
|---|---|---|
| `int` | `0 .. 2^depth - 1` | |
| `signed` | `-2^(depth-1) .. 2^(depth-1) - 1` | `depth` counts the sign bit |
| `decimal` | as `int`, with `scale=<n>` | **unsigned**; a negative decimal is refused |
| `set`, `mutex` | string keys | |
| `bool` | true/false | |
| `timequantum` | string keys, plus a view per granularity | |

A `signed` field always uses every plane it declared — the top one is the sign bit and every
non-negative value sets it. An `int` field of the same declared depth only pays for the planes
its data actually reaches, so do not declare `signed` for data that is never negative.

A value outside the declared range is refused (`422`, `value_out_of_range`), never wrapped. A
*comparison* outside the range is answered normally: `balance > 10000000` against a narrow field
matches nothing, which is the right answer and does not require the client to read the schema
first.

### Listing records

```sh
# A page at a time. `next` is the id to send back as `after`; `null` means the end.
curl -s 'localhost:7654/table/tx/records?limit=1000'
# {"records":[1,2,...,1000],"next":1000}
curl -s 'localhost:7654/table/tx/records?limit=1000&after=1000'
```

`GET /table/{t}/records` reads one shard at a time and stops at the first that fills the page,
so a listing costs a page rather than the table. `POST /query` with `All()` answers the same
question by building the whole exists row first — use the listing route for a scan and `/query`
for a predicate.

There is no cursor to hold open and nothing to close: the last id of a page is the whole cursor,
so a client that stops half way costs the server nothing, and a page can be served by a process
that never saw the page before it.

**The query response changed shape.** Every query returning records now answers
`{"records":[...],"next":...}`; it used to be `{"records":[...]}` with no `next`. A client that
never asks for a page still gets every record — there is deliberately **no default limit** on
`/query`, because silently truncating a client that does not know cursors exist is a worse break
than a field it can ignore. `GET /table/{t}/records` does default, to 1000: nothing was relying
on an unpaged answer from a route that did not exist before.

Paging a query that does not return records — `Count(All())&limit=10` — is refused with `422`
and `not_pageable` rather than answered unpaged.

### Durability

`full` is the default and should stay the default. The other two exist for one caller: a bulk
load whose source you still have, where losing the last second costs a re-run rather than data.
Note that neither is reachable over HTTP - the level is set on the command line or through the
embedding API, so `bigctl` cannot relax it and a load that wants it relaxed is a daemon started
that way, or restarted after.

| Level | Survives the process dying | Survives the machine dying | Survives power loss |
|---|---|---|---|
| `full` | yes | yes | yes |
| `barrier` | yes | yes | **only if the drive honours a cache flush** |
| `none` | yes | **no** | no |

Two things that are *not* traded away at any level:

- **The file always opens.** A commit issues both of its flushes or neither, never just the
  second. Relaxing can cost you recent commits; it cannot leave a meta page naming pages that
  were never written. What you find after a crash at `none` is an older consistent database,
  not a broken one.
- **Nothing about visibility changes.** A commit that returned is a commit every reader sees,
  at every level. The knob is about the disk, not about the transaction.

`barrier` and `full` differ **only on macOS**, where `full` additionally issues `F_FULLFSYNC` to
push past the drive's own write cache. On Linux both are `fdatasync` and the two levels are the
same call. If you are on Linux, the useful choice is `full` or `none`.

The scrape says which one is live:

```
big_durability{level="full"} 1
big_durability{level="barrier"} 0
big_durability{level="none"} 0
```

**Alert on `big_durability{level="full"} == 0` outside a load window.** The failure this catches
is not a crash — it is an ingest that relaxed durability and never put it back. Nothing else
shows it: the file looks exactly as healthy as it did before, right up until the machine reboots.

Changing the level at runtime (through the embedding API, not over HTTP) is bracketed for you:
tightening flushes everything written under the looser setting *before* it takes effect, so
`relax → load → tighten` leaves the load durable rather than leaving a hole at the end of it.

### Users

One `username role hash` per line; `#` starts a comment. The file is written by `big passwd` and
by nothing else — there is deliberately no route that writes it, because one would let a
privileged credential rewrite the credential file over the network.

**The role is a name, not a rank.** What it may do is a set of grants in the catalog, made with
`GRANT`. This file only says which role somebody holds; a name the catalog does not have is no
privileges at all, which is the safe direction and the reason the server still starts.

```
# /etc/big/users  — chmod 600
ops        admin      $argon2id$v=19$m=19456,t=2,p=1$…
loader     loader     $argon2id$v=19$m=19456,t=2,p=1$…
dashboard  analyst    $argon2id$v=19$m=19456,t=2,p=1$…
recovery   superuser  $argon2id$v=19$m=19456,t=2,p=1$…   # holds everything; see below
```

```sh
big passwd /etc/big/users set ops --role admin   # asks for the password twice, echoes neither
big passwd /etc/big/users role ops analyst       # change a role, leave the password alone
big passwd /etc/big/users delete ops
big passwd /etc/big/users list                   # names and roles, never a hash
```

The password is never a flag and never an environment variable: an argument is visible in `ps`
and in shell history. When standard input is not a terminal one line is read from it, which is
the only non-tty path and is there so a provisioning script can pipe one in.

**⚠️ Changing a role here needs a restart.** The users file is read once, at startup. Privileges
themselves are live — a `GRANT` or a `REVOKE` applies to the next statement, cluster-wide — so it
is only the line saying *which* role somebody holds that waits.

### Roles and grants

Roles live in the catalog, so they replicate through the same fan-out a `CREATE TABLE` takes and
a backup carries them with the schema they are about.

```sql
CREATE ROLE analyst;
GRANT SELECT ON sales.* TO analyst;          -- every table in one database
GRANT SELECT, INSERT ON sales.orders TO loader;  -- one table
GRANT ALL ON *.* TO admin;                   -- everything, including making roles
REVOKE INSERT ON sales.orders FROM loader;
SHOW ROLES;
SHOW GRANTS FOR analyst;
DROP ROLE analyst;
```

Seven privileges, granted on `*.*`, `db.*` or `db.table`:

| | what it covers | grantable on |
|---|---|---|
| `SELECT` | reading rows, and `DESCRIBE`/`SHOW CREATE` of the object | all three |
| `INSERT` | writing rows | all three |
| `DELETE` | `POST /table/{t}/delete` | all three |
| `CREATE` | making a database, table, view or column | `*.*`, `db.*` |
| `DROP` | removing one | all three |
| `ALTER` | adding or dropping a column | all three |
| `ROLES` | `CREATE ROLE`, `DROP ROLE`, `GRANT`, `REVOKE` | `*.*` only |

`OPERATE` is an eighth, held on `*.*` only, and guards `/metrics`, `/verify`, `/repair` and
`/admin/backup` — `GRANT OPERATE ON *.* TO ops` grants exactly those four, and nothing about SQL.
No statement ever demands it, so denying it produces no query-side refusal, only a `403` on the
routes it guards.

Grants are **additive and never subtract**: what a role holds on a table is the union of its
grants at `*.*`, at that database, and at that table. `REVOKE` removes a grant rather than adding
a denial, which is what keeps the answer independent of the order the levels are consulted in.

**`superuser` holds everything and is never stored.** It exists before anything is written, cannot
be created, dropped, or granted to, and is the only way out of an empty catalog: making the first
role needs a privilege, and only this role has one already. Keep exactly one credential holding
it, and treat it the way you treat a root password.

**On the day this feature lands, every existing users file names roles no catalog has**, so every
credential holds nothing. The recovery is:

```sh
big passwd /etc/big/users role recovery superuser   # before restarting
# restart, then:
bigctl --user recovery sql "CREATE ROLE admin"
bigctl --user recovery sql "GRANT ALL ON *.* TO admin"
```

There are ceilings: **64 roles** and **256 grants** across all of them. The catalog is re-encoded
on every commit that dirties it — including ones that only wrote data, because fragment metadata
shares the chain — so an unbounded set of grants would make every import pay for them. Grants are
dense enough that this is a great deal of policy: a role with the run of a database is one entry,
not one per table.

| Route | Who |
|---|---|
| `GET /health`, `GET /ready` | **nobody, ever** |
| `POST /sql` | anybody who authenticates; the *statement* then says what it needs |
| `GET /schema` | anybody who authenticates — it answers with names |
| `GET /metrics`, `GET /verify`, `POST /repair`, `POST /admin/backup` | `OPERATE` on `*.*` |
| `POST /table/{t}/query`, `GET /table/{t}/records` | `SELECT` on that table |
| `POST /table/{t}/import` | `INSERT` on that table |
| `POST /table/{t}/delete` | `DELETE` on that table |
| `POST /table/{t}` | `CREATE` on the database holding it |
| `DELETE /table/{t}` | `DROP` on that table |
| `POST`/`DELETE` on a field | `ALTER` on that table |
| `POST`/`DELETE` on a database | `CREATE`/`DROP` on `*.*` |
| `/internal/*` | **another node**, proven by its client certificate — not a user, however privileged |

Give the metrics scraper a role holding `OPERATE` and nothing else. A missing or unknown
credential is `401`; a credential that does not hold what was needed is `403`, and the body names
the user, the privilege and the object — hiding that stops a legitimate operator from
understanding the refusal and stops nobody else.

**A `403` can shadow a `404`.** The privilege is checked before anything is planned, so a caller
who may not read a database and mistypes a table in it is told they may not read it rather than
that it is not there. That is deliberate: answering `404` would make the surface a way to ask
which tables exist. Where the caller *does* hold the privilege, a mistyped name still answers
`404` as it always did.

Credentials are hashed with argon2id, which the file this replaced did not do. That is not a
change of mind about the old reasoning — a token in a plaintext file really was the smaller of
two secrets next to the database file beside it — it is a change in what is being stored. A
password is a thing a person also uses somewhere else, so the file is no longer the smaller
secret. The mode check is still enforced on top: `big serve` refuses to start against a users
file that is group- or world-readable.

**A verification costs about 19 MiB and tens of milliseconds, per verification in flight.** That
is the point of a password hash and it is an operational number. Three things keep it bounded:

- a verified credential is cached for **60 seconds** from when it was verified, so a busy client
  pays once a minute rather than once a request. The window is absolute, never extended by use,
  so a `big passwd delete` takes effect within a minute without a restart. What is cached is
  **which line of the users file the password matched**, not what that role may do — so a `GRANT`
  or a `REVOKE` is never stale, and the window is about passwords only.
- failures are **never** cached, so every wrong password pays — which is also the rate limit.
- at most `--workers / 4` verifications run at once. Past that a request waits up to a second and
  is then refused with `503 busy_authenticating`, which is a retry, not a `401`.

Watch `big_http_password_verifications_total`. In a steady state it should be near flat: if it
tracks your request rate, the cache is not being hit and every request is paying for a hash.
`big_http_password_throttled_total` should be zero; if it is not, either `--workers` is too small
or something is guessing passwords at you.

### TLS

**There is TLS now, and this section used to say there never would be.** The old answer —
terminate in front — was the right one while the credential was a bearer token that belonged to
this database and to nothing else. A password is not that, so it is no longer the only answer.
Both deployments are supported.

```sh
big serve /var/lib/big/data.big 0.0.0.0:7654 \
    --users /etc/big/users \
    --tls-cert /etc/big/server.pem --tls-key /etc/big/server.key   # chmod 600
```

A non-loopback bind is refused twice over: once without `--users`, once without a certificate.
Each refusal has its own override, because they are two decisions — an operator with a proxy in
front wants exactly `--insecure-no-tls` and wants to keep their credentials:

```sh
big serve … 0.0.0.0:7654 --users /etc/big/users --insecure-no-tls
```

which is the deployment this section used to describe, and it still works:

```nginx
server {
    listen 443 ssl;
    server_name big.internal;
    ssl_certificate     /etc/ssl/big.crt;
    ssl_certificate_key /etc/ssl/big.key;

    location / {
        proxy_pass http://127.0.0.1:7654;
        proxy_read_timeout 120s;          # longer than --query-timeout
        client_max_body_size 8m;          # matches MAX_BODY
    }
}
```

Three things worth knowing before you turn it on:

- **TLS 1.3 only.** Clients older than roughly 2017 cannot connect. For a database port that is
  the right side of the trade; a protocol version that is not compiled in cannot be downgraded to.
- **A self-signed certificate is not its own CA.** `curl` will accept one as a `--cacert`; a
  rustls client will not, and says `CaUsedAsEndEntity`. Either issue a real CA and sign with it —
  `deploy/cluster/certs.sh` is ten lines of `openssl` that does exactly this — or use
  `bigctl --insecure-skip-verify`, which says what it is on every run.
- **A build can be made without TLS at all** (`--no-default-features`), and one that was says so
  on its first line: `big: no tls in this build`. Passing `--tls-cert` to it is an error that
  names the build rather than the flag.

A client that speaks plaintext to a TLS port gets an HTTP `400` explaining that, rather than a
TLS protocol error — because `curl http://…` against the new port is the first mistake everybody
makes and rustls's own answer for it is unreadable.

### More than one node

`big serve --cluster /etc/big/cluster.toml --node a data.big 10.0.0.1:7654`

```toml
# /etc/big/cluster.toml — the same file on every node
schema_leader = "a"
peer_ca_file  = "/etc/big/peer-ca.pem"    # who signs a node certificate; the same on every node

[[node]]
name   = "a"
addr   = "10.0.0.1:7654"
shards = "0..64"

[[node]]
name   = "b"
addr   = "10.0.0.2:7654"
shards = "64.."           # the last range must be open, or the space above it has no owner

# A copy of everything `a` holds. No range of its own: it takes `a`'s.
[[node]]
name    = "a-spare"
addr    = "10.0.0.3:7654"
replica = "a"
```

Every node runs the same binary and the same file. Any of them will answer any request: the one
that receives it plans the query and fans it out. `--node` is optional if the address on the
command line is the one in the file.

**What to know before running one.**

- **A record id chooses its node.** Shard `record_id >> 20`, so records `0 .. 67108863` live on
  `a` above and everything from `67108864` on `b`. A client that wants a batch to be atomic
  sends one whose records fall inside a single node's range, because there is no transaction
  across nodes and a half-applied batch answers `500 partially_applied` naming what landed.
- **Backups are per node.** `Db::copy_to` on each node, on its own schedule. A range with no
  `replica` has one copy, and losing that node loses its shards.
- **Changing a range means stopping a node**, copying its file, and editing the config
  everywhere. There is no online shard movement.
- **A range whose serving node is down fails every query over it**, with `503
  owner_unreachable` and the shard range in the message. With a copy this lasts about a second,
  until the agreement moves the range; with no copy it lasts until the node is back. This is
  deliberate — see [clustering](docs/clustering.md#the-choice-and-what-it-costs): an answer here
  is an aggregate, and a stale count is a wrong number with no symptom.
- **`/ready` is about one node.** It reports that node's name and shard range and says nothing
  about its peers, so a probe never takes a healthy node out of rotation for somebody else's
  outage.
- **Users are people; nodes are certificates.** `--users` is who may talk to *this* node.
  Nodes prove themselves to each other with a client certificate signed by `peer_ca_file` —
  there is no shared secret, and no entry in anybody's users file. This closed a real gap: the
  cluster used to hand every node the same `admin` token, so one leaked string was every node
  and nothing could tell which node was speaking.
- **The two directions are exclusive.** A person cannot reach `/internal/*` however privileged
  they are, and a node certificate grants no role on the public routes. Under tokens an `admin`
  credential could post to the internal routes; it cannot now.

```sh
# Which node am I talking to, and what does it hold?
curl -s localhost:7654/ready
# {"status":"ready","tables":3,"txn_id":41,"pages":900,"node":"a","shards":"0..64"}
```

### Replicas, and failing over

`replica = "a"` gives `a`'s range a second copy. **Every write goes to both; every read goes to
the one currently serving the range.** When that one stops answering, the nodes agree among
themselves on another and the range keeps working - in about a second, not after somebody edits
a file.

**Three nodes minimum, once anything has a copy.** `big serve` refuses to start otherwise: failing
over is a decision a majority has to agree on and a majority of two is two, so a cluster of two
can never use its copy. A third node counts whether it is a third copy or another range's
primary.

Know the trade before you write that line. **A write that cannot reach a copy stands** and says
so:

```json
{"imported":2,"missed":["a-spare (0..1) (`a-spare` holds shards 0..1 and could not be reached …)"]}
```

That copy is then marked **behind**, and a copy that is behind will not be promoted - so
nothing ever reads from a copy that missed a write, and no write is refused because a machine
nobody reads from has died. The cost is that redundancy is gone until you repair it: if the
node serving the range dies while its only copy is behind, the range stops rather than answering
from data that is short a batch.

```sh
# Who is answering for what, and is anything behind?
curl -s localhost:7654/ready
# {"status":"ready","tables":3,"node":"a","shards":"0..64","serving":true,
#  "term":2,"leader":"b","behind":["a-spare"]}
```

`serving:false` means this node has lost touch with the agreement and has stopped answering for
its range rather than risk a second node answering too. It is still *ready* - the engine is
fine - so a load balancer should keep probing it and stop sending it work; it will start again
when it can hear the others.

```sh
# Do the copies still hold the same facts? A scan: run it deliberately.
curl -s localhost:7654/verify
# {"agree":true,"ranges":[{"shards":"0..64","primary":"a","agree":true,"copies":[
#   {"node":"a","digest":389034629230753869,"why":null},
#   {"node":"a-spare","digest":389034629230753869,"why":null}]}]}

# Catch up every copy that is behind, and clear the mark. Also a scan.
curl -s -XPOST localhost:7654/repair
# {"repaired":[{"node":"a-spare","fragments":6,"outcome":"caught up"}]}
```

`"agree":false` with a `null` digest means a copy could not be asked, which is not the same as a
copy that disagrees - and neither is agreement. `fragments:0` with `"caught up"` means the copy
had missed nothing after all and only the mark was stale, which is the common case after a brief
blip.

**Run `/repair` after anything that marked a copy behind.** Nothing does it for you: a repair is
a scan and a copy, and a system that starts one by itself starts it at the worst possible
moment. A cron entry is the usual answer.

**What is still an operator's job.** Changing a *range* - which shards exist and how they are
split - is still stopping a node, copying a file, and editing the config everywhere. Only which
copy serves a range moves on its own, and it moves without any data moving, because every node
in the group already holds it.

### Containers

Two compose files, in [deploy/](deploy/): [`single/`](deploy/single/) for one node and
[`cluster/`](deploy/cluster/) for three. Same binary, same code path - `big serve` without
`--cluster` builds itself a cluster of one - so what differs is a config file and how many
containers there are.

```sh
cd deploy/single
mkdir -p secrets && printf 'change-me-please\n' | big passwd secrets/users set ops --role admin
docker compose up -d
```

```sh
cd deploy/cluster
./users.sh && ./certs.sh          # writes secrets/users, and a CA plus one cert per node
docker compose up -d
curl -u ops:$PASSWORD localhost:7654/verify
```

**A container's loopback is its own**, so the daemon binds `0.0.0.0` inside it - and `big serve`
refuses to bind anywhere but loopback without a users file. That is why every service mounts one.
A bind mount carries the host's ownership and mode, which the image cannot predict, so the
entrypoint reads the credentials as root, writes private copies owned by the daemon's user, and
**drops privileges before the daemon starts**. The database never runs as root. The port is
published to `127.0.0.1` on the host; what sits in front of it is your decision, and the default
should not be the internet.

**The daemon and the files have to be on the same host.** `docker context` pointing at a remote
machine means bind mounts resolve *there*: the compose file asks for `./secrets` and the remote
daemon, finding nothing at that path, creates an empty directory and mounts that. The failure
looks like a missing users file, which is exactly what it is.

**One volume per node.** One process holds one file - the pager takes an exclusive lock - so
two nodes pointed at one volume is two nodes fighting over a database only one of them can
open.

**Killing a container loses nothing.** `big serve` has no signal handler and does not need one: a
commit writes its pages, fsyncs, flips the meta page and fsyncs again. A process that dies
leaves a file either before that flip or after it, with no state in between and nothing to
replay - which is why `stop_grace_period` is two seconds rather than the default ten.

**Every request between nodes carries two stamps**: what build it speaks to other nodes, and a
fingerprint of the cluster file it read. A mismatch in either is a `409` naming both sides,
before anything is decoded - so an upgrade that moves the wire version is a coordinated stop
rather than a rolling one, and a cluster file edited on one machine and not another is caught
the moment the two speak. `GET /ready` reports both numbers.

**Docker installed as a snap cannot see outside its confinement.** On a host where `docker` is
a snap, a compose project under `/opt` fails with `open /var/lib/snapd/void/...: no such file
or directory` - the daemon cannot reach the path. Put the project under `$HOME`.

**In a cluster each node is started with `--node`**, because the address it binds
(`0.0.0.0:7654`) is not the address the others reach it on (`a:7654`). Without it the daemon
cannot tell which entry in the file it is, and says so rather than guessing.

### Multi-tenancy

**One tenant per process.** Table names are a flat global namespace with no notion of an owner,
and any credential that can read one table can read all of them — roles are about *verbs*, not
about *rows*. An operator who needs isolation runs one `big serve` per tenant against one file per
tenant, which the exclusive file lock already pushes toward.

This is a decision, not an oversight, and it is not a dead end: a tenant id would go into the
catalog as a new record kind, which is additive and needs no format version bump.

---

## Watch these

`GET /metrics`, Prometheus text. A `read` user when `--users` is configured.

| Metric | It means |
|---|---|
| `big_free_pages_reusable` | Free pages the next allocation may take |
| `big_pages_pending_reclaim_reader` | Free but pinned by a live reader |
| `big_pages_pending_reclaim_retention` | Free but pinned by a snapshot |
| `big_page_count` | Pages in the file |
| `big_live_readers` | Open read transactions |
| `big_txn_id` | Committed transaction id; its *rate* is the write rate |
| `big_storage_reads_total{backend=…}` | Pages the backend handed to the engine |
| `big_storage_writes_total{backend=…}` | Pages written; over the `big_txn_id` rate it is write amplification |
| `big_storage_write_bytes_total{backend=…}` | The same in bytes, for a byte-rate panel |
| `big_storage_grows_total{backend=…}` | Calls that extended the file |
| `big_storage_syncs_total{backend=…}` | Flushes issued; two per commit unless durability is off |
| `big_storage_sync_seconds_total{backend=…}` | Time inside those flushes; over the count it is the mean flush |
| `big_http_connections_rejected_total` | Connections shed because the pool was full |
| `big_http_responses_total{class=…}` | Responses by `2xx`/`4xx`/`5xx` |
| `big_http_request_duration_seconds` | Latency histogram |
| `big_http_queries_timed_out_total` | Queries stopped at their deadline |

`big_oldest_reader_txn_id` is **absent** when no reader is open. It is not reported as zero,
because zero is a real transaction id.

The `big_storage_*` counters are **absent entirely** on a backend that keeps no count, for the
same reason: a block of zeroes reads as an idle database rather than as an unmeasured one. They
are what the *storage layer* did, as opposed to the gauges above, which are the shape the file
ended up in - and the two come apart in the case that matters most. A database whose
`big_page_count` is flat while `big_storage_writes_total` climbs is one rewriting the same pages
over and over; nothing in the gauges shows that at all.

`big_storage_reads_total` counts pages the engine **asked for**, not disk reads. On the mapped
backend a read is a pointer into the mapping, and whether it reaches the platter is a page fault
the kernel handles without telling the process - so there is no `read_bytes` series, and the
number to watch is this one against the write rate. For actual disk reads, the kernel's own
counters (`/proc/<pid>/io`, `major faults`) are the source; this process cannot see them.

`backend` is one fixed label - a process opens one backend for its lifetime - so it costs no
cardinality and tells a dashboard which implementation the numbers came from.

### What to alert on

```promql
# The file is growing and the freelist is not being reused. Something holds the horizon.
rate(big_page_count[15m]) > 0 and big_free_pages_reusable == 0

# A reader has been pinning pages for a long time. This is the usual cause of the above.
big_pages_pending_reclaim_reader > 10000

# The pool is the limit, not the engine. Raise --workers, or find what is slow.
rate(big_http_connections_rejected_total[5m]) > 0

# The engine is failing, not the callers. Every one of these has a line in the log.
rate(big_http_responses_total{class="5xx"}[5m]) > 0

# Writes have stopped. A silent writer is worse than a loud failure.
rate(big_txn_id[15m]) == 0

# Durability was relaxed and not put back. Nothing else makes this visible.
big_durability{level="full"} == 0

# Commits are flushing but the disk is not keeping up. The mean flush, in seconds.
rate(big_storage_sync_seconds_total[5m]) / rate(big_storage_syncs_total[5m]) > 0.1

# Pages written per commit. Copy-on-write pays a floor of a few; a jump means a commit is
# rewriting far more of the tree than it touched, and the file will grow to match.
rate(big_storage_writes_total[15m]) / rate(big_txn_id[15m]) > 100

# Configured for full durability but not flushing. The two disagreeing is a bug, not a setting.
big_durability{level="none"} == 0 and rate(big_storage_syncs_total[15m]) == 0
  and rate(big_txn_id[15m]) > 0
```

Do **not** alert on `4xx`. It is clients being wrong, which is normal traffic.

### The log

One JSON object per line, on stderr. Every request produces exactly one:

```json
{"ts":"2026-08-27T14:19:51.373Z","level":"info","event":"request","id":"1634c7f3-12",
 "method":"POST","path":"/table/tx/query","status":200,"duration_us":51,
 "bytes_in":22,"bytes_out":13,"peer":"10.0.0.4:41022"}
```

The `id` is also returned as `X-Request-Id`, which is how a user's report joins to a log line.

**A `5xx` body never carries the real message.** The client gets a code and a generic sentence;
the full text — which can contain a filesystem path — is in the log line's `detail` field,
against the same `id`. When someone reports a `500`, ask for the `X-Request-Id`.

---

## The file will not open

Every one of these is the exact text `big serve` or `big verify` prints. The remedy differs; read
which one it is before doing anything.

| It says | It means | Do |
|---|---|---|
| `another process holds this file` | A `big serve` or a `big` subcommand already has it | `fuser`/`lsof` the file. One process per file is a soundness requirement, not a policy |
| `neither meta page is readable; this file is damaged` | Both meta pages failed | **Restore from a backup.** There is no WAL, so there is nothing to replay and no repair to attempt |
| `checksum mismatch: page stores 0x…` | A page's bytes are not what was written | Restore. The storage under it lied; check the disk before restoring onto the same one |
| `file format version N, but this build reads … version M` | A file from another build | **Not damage.** Dump and reload with a tool built with both codecs. Never migrate in place |
| `not a big page: magic is 0x…` | Not a big file, or truncated at the front | Check the path. A zero-length file is the usual cause |
| `the file needs N pages but the mapping reserves M` | Outgrew its reserved `mapsize` | Reopen with a larger mapsize |
| `page N is past the end of a M-page file` | Truncated | Restore |

`big verify <file>` is the way to ask. Opening *is* the check — a file that opens and prints
its tables is structurally sound.

## The file keeps growing

Free pages in the interior are reused, so a file does not grow without bound — but it never
shrinks either. Growth with `big_free_pages_reusable` at zero means something is holding the
reclaim horizon down. In order of likelihood:

1. **A long-running reader.** `big_pages_pending_reclaim_reader` is climbing and
   `big_oldest_reader_txn_id` is far behind `big_txn_id`. Find the query. Set
   `--query-timeout` so it cannot happen unattended.
2. **A backup in progress.** Same signature, and it is correct — the copy holds a read
   transaction on purpose. It recovers when the backup finishes.
3. **A retained snapshot.** `big_pages_pending_reclaim_retention` is the one climbing.
4. **Genuine growth.** Everything is fine and the data got bigger.

Only `big compact` returns space to the filesystem, and it is offline — nothing else may have
the file open.

## Error codes

Every error body carries a stable `code` beside its human `error` sentence:

```json
{"error":"table `tx` has no field named `nope`","code":"unknown_field"}
```

Match on `code`, never on `error`. The sentence is reworded whenever it reads better; the code
does not change without a version bump.

| Class | Codes |
|---|---|
| **400** the request is malformed | `parse_error`, `bad_parameter`, `malformed_line`, `bad_request`, `bad_arity`, `bad_argument`, `operator_not_allowed`, `too_precise`, `wrong_field_kind`, `name_too_long`, `row_key_too_long`, `value_too_wide`, `unknown_call` |
| **401 / 403** credentials | `unauthenticated`, `forbidden` |
| **404** the URI names nothing | `unknown_table`, `unknown_field` *(only on `/table/{t}/field/{f}`)*, `no_such_route` |
| **409** something is already there | `name_taken`, `field_redefined`, `backup_destination_exists`, `backup_destination_not_empty` |
| **413** the body is too big | `request_too_large` |
| **422** well formed, cannot be done | `unknown_field` *(named in a body)*, `query_too_large` |
| **499** the client went away | `query_cancelled` |
| **500** the engine, or the file | `io`, `file_damaged`, `page_damaged`, `page_checksum_mismatch`, `page_out_of_bounds`, `page_size_mismatch`, `not_a_big_file`, `tree_damaged`, `payload_too_large`, `unallocated_page`, `snapshot_not_found`, `mutex_conflict`, `panic` |
| **503** try again | `file_locked`, `readers_active`, `mapsize_exhausted`, `server_busy` |
| **504** it ran too long | `query_timeout` |
| **501** this build cannot | `unsupported`, `unsupported_format_version` |

`unknown_field` is `404` when the field is in the URI and `422` when it is in a body. The
distinction is deliberate: a `404` tells a client the endpoint is gone, and for
`POST /table/{t}/query` the endpoint is fine — the query named something that is not there.

**`query_timeout` is a `504`, not a `503`.** `503` means busy, and retrying will work. Retrying
a query that blew its deadline will blow it again; what has to change is the query or the
limit.
