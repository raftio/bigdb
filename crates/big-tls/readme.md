# big-tls

The socket a request arrives on, plain or TLS, and what it proves about the caller.

Two crates need a socket that is either a plain `TcpStream` or a TLS session over one:
`big-http` accepts connections and `big-cluster` opens them. `big-cluster` sits underneath
`big-http`, so it cannot borrow a type defined there — and a copy in each would mean the same
feature flag, the same PEM loader, the same private-key mode check and the same "is this pooled
connection still usable" rule maintained twice. They are here once instead.

## The feature

```toml
big-tls = { workspace = true }                      # no dependencies at all
big-tls = { workspace = true, features = ["tls"] }  # rustls
```

With `tls` off this crate depends on nothing, `Wire` is a `TcpStream` behind one `match`, and
`TlsConfig` is uninhabited — which is what lets `ServerConfig` carry an `Option<TlsConfig>`
field with no `#[cfg]` at any of the places that construct one. `big-bin` turns the feature on;
`big-http` and `big-cluster` leave it off and forward it.

CI asserts the claim rather than repeating it: `cargo tree -p big-http --no-default-features -e
normal` must not mention rustls.

## What is here

| | |
|---|---|
| `Wire` | one accepted connection, `Read + Write`, plus `socket()` for the things that belong to the file descriptor rather than the session — timeouts, and the watchdog's peek |
| `ClientWire` | the outbound half, with no buffer of its own so the peer pool can see the socket |
| `TlsConfig` | what a listener presents, and the roster of node names a client certificate may name |
| `ClientTls` | what a caller trusts, and what it presents when asked |
| `Identity` | what the connection proved: nothing, or a named node |
| `base64` | strict, no whitespace, padding required — used by PEM here and by `Authorization: Basic` above |
| `pem` | PEM to DER, and the error messages that say which of three files was swapped |
| `mode` | the one rule for what makes a file fit to hold a secret |

## Two things worth knowing before changing it

**The handshake runs on a worker, not on the accepting thread.** A signature is a millisecond
and a round trip is however long the network is. Doing that where `accept()` is called would cap
new connections at a few hundred a second and would stop the server being able to shed load —
and a saturated server that cannot say it is saturated is the failure the worker pool exists to
avoid.

**`socket()` is not a second way to write.** Under TLS there is no second write handle; bytes
have to go through the session that encrypts them. The duplicated descriptor exists because
`O_NONBLOCK` and `SO_RCVTIMEO` live on the open file description that both handles share, which
is how a timeout set here reaches a read that happens inside rustls.

## TLS 1.3 only, and `ring`

`ring` rather than the default `aws-lc-rs`, because the build image has a C compiler and no
`cmake`. TLS 1.2 is not compiled in: both ends of every connection here are ours or a current
curl, and a protocol version that is absent is one that cannot be downgraded to. The cost is
clients older than roughly 2017.
