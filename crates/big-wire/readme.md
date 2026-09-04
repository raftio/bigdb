# big-wire

One request off a socket, one response back, and the log line between.

The half of `big-http` that has nothing to do with a database: the HTTP/1.1 parser and its
ceilings, the response encoder, the structured logger, and the two JSON primitives everything
else is built from.

## Why it is a crate

`big-proxy` speaks HTTP to clients and to nodes and touches no data at all. While this code
lived beside the router, linking it meant linking `big-engine`, `big-sql`, `big-exec` and
argon2 — so "the proxy never verifies a password" was a sentence in a readme rather than a
property anybody could check.

It links **no engine**, and CI asserts that the same way it asserts the `contrib` crates have no
dependencies at all. `cargo tree -p big-proxy -e normal` is now the proof.

Its one dependency is `big-tls`, for the base64 decoder an `Authorization: Basic` header needs —
the same decoder PEM parsing already required, and two copies of a base64 decoder is one too
many. With the `tls` feature off, `big-tls` has no dependencies either.

## What did not move

`Response::from_error` classifies an engine error, so it needs the engine's error tree. It is
`big_http::status::response_for` instead — the one function that had to change name for this
split, and it is named in `big-http`'s own docs.

`big-http` re-exports everything here, so nothing that already depended on it had to change.
