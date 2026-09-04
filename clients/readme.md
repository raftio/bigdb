# Clients

Libraries for talking to `big serve` over HTTP. Each one is written against the HTTP surface
and nothing else, so none of them can drift from the server by depending on its internals.

| | language | state |
|---|---|---|
| [`go/`](go) | Go 1.23+, stdlib only | data plane, schema, and a `database/sql` driver |
| [`python/`](python) | Python 3.10+, stdlib only | in progress |

The two share their rules rather than their code: the SQL escaping is a port of
[`contrib/big-message/src/sql.rs`](../contrib/big-message/src/sql.rs) in each, test vectors
included, so a change to the dialect breaks all three in the same place.

Not clients, but client-shaped, and worth knowing about before writing another one:

- [`crates/big-bin/src/client/`](../crates/big-bin/src/client) — `bigctl`, one exchange per
  command with `Connection: close`.
- [`contrib/big-message/`](../contrib/big-message) — a producer library, and the reference for
  keep-alive lifetime and the not-sent/unknown split.
