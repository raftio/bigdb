# big-message-redis

A Redis stream into a bigdb table.

```
big-redis-sink \
  --redis 127.0.0.1:6379 --stream events --group g1 \
  --addr 127.0.0.1:8080 --table tx \
  --map amount:int=amount,country=country \
  --dedup-field msg_id
```

## The order, which is the whole design

```
XREADGROUP  →  send  →  flush  →  XACK
```

Every part of it is load-bearing:

- Acknowledging **before** the write would lose a batch to a crash. Acknowledging after repeats
  one instead, and repeating is the failure this sink can survive.
- The acknowledgement names **exactly** the entries the flush covered, which is why the sink
  flushes explicitly rather than letting the producer's linger decide.
- An `Error::Unknown` — the request was written and the outcome is unknown — acknowledges
  nothing and stops. Those entries stay pending, which is the honest state.

`a_write_the_server_refuses_acknowledges_nothing` pins the middle rule; the ordering is only
checkable from the Redis side, so the tests use a scripted peer that records every command.

## Delivery

**At-least-once.** A crash between bigdb taking a batch and Redis being told repeats that batch,
and because the server allocates the record ids, a repeat is *new records* rather than the same
ones written again. That is inherited from `big-message` and cannot be removed while the ids stay
the server's.

`--dedup-field <column>` closes the restart half of it. The sink writes each entry's stream id
into that column, and on start-up asks the table which of its **pending** entries are already
there — a question bounded by what one process had in flight, not by the size of the stream — and
acknowledges those without rewriting. Measured against a real Redis:

```
big-redis-sink: written 0
big-redis-sink: acknowledged 3
big-redis-sink: already written 3
```

Three entries pending, three already in the table, three rows afterwards rather than six.

Without it the same restart writes them again — `without_a_dedup_column_a_pending_entry_is_written_again`
pins that too, so it is a documented trade rather than a surprise.

What stays open either way is the window inside one flush: a crash after bigdb commits and before
it answers is an `Unknown`, and nothing in the table distinguishes it. That is the residue.

## The mapping is the operator's

A stream entry's fields and a table's columns are two vocabularies chosen by two different
people. This sink does not guess between them: it reads no schema, infers no types, and matches
no names by accident. `--map` is declared or the sink does not start.

```
--map <field>[:<kind>]=<column>
```

Kinds: `text` (default), `int`, `signed`, `float`, `decimal`, `bool`. A kind chooses which
*literal* to write; what a value **means** is still the server's to decide, in
`big_embed::fact::from_literal`. A value that will not read as the kind it was declared is
refused at the entry that carried it, naming that entry — not at the server, where it would
refuse the whole batch and name none of them.

An entry missing a mapped field stops the sink, because a mapping that does not fit is usually
wrong for every entry rather than for one. `--skip-incomplete` acknowledges and passes over it
instead — acknowledges, because an entry left pending would be delivered again for ever and will
be as wrong the next time.

## Consumer groups

The group is created with `MKSTREAM` at `$`, so a sink started before its producer does not fail,
and a group that begins does not replay everything written before it existed.

`--consumer` is this process's name in the group, defaulting to the hostname and pid. **Two sinks
must not share one**: a consumer name owns a pending list, so sharing means each recovering the
other's unfinished work. `--claim-after <seconds>` takes over entries idle that long in *another*
consumer's list, which is how a dead process's work is picked up.

## Protocol

RESP2, written rather than linked: five type markers and a length prefix, and commands go out as
an array of bulk strings. `HELLO` is never sent, so the server answers in RESP2 — what every
version since 2.0 does by default.

Length prefixes rather than quoting is what makes an argument containing `\r\n` still one
argument — `an_argument_holding_crlf_cannot_forge_a_second_command`.

## Dependencies

One: `big-message`, the producer beside this. Everything else is `std`.

```
cargo tree -p big-message-redis --edges normal
big-message-redis v0.1.0
└── big-message v0.1.0
```

The engine is not reachable from here, which is the property `big-message`'s empty
`[dependencies]` has, kept one crate further out.

## Exit codes

`0` the stream went quiet and `--once` was given · `1` stopped · `2` usage
