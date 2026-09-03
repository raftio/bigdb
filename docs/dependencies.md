# Dependencies

This repository used to be able to say the engine had four third-party dependencies. It cannot
any more, and that is worth writing down rather than letting a reader discover it from a lock
file.

## What changed, and why

Authentication moved from a bearer token to a username and a password over TLS. Two things
follow, and neither has a version that costs nothing:

- **A password has to be hashed.** A password hash is the one piece of cryptography where writing
  it yourself is unambiguously wrong, and the whole point of it is to be slow in a way that a
  hand-rolled one will not be.
- **A password has to travel encrypted.** A bearer token belonged to this database and to nothing
  else; terminating TLS at a reverse proxy protected it well enough that carrying a TLS stack was
  the larger cost. A password is a thing a person also uses somewhere else, so sending one in the
  clear risks something that was never ours to risk.

The old answer is still available and is still supported: `--no-default-features` builds the tree
below without any of the TLS half, and `--insecure-no-tls` tells `big serve` that a proxy is
doing the job. CI asserts that this configuration still builds and still passes its tests.

## The engine, without the `tls` feature

Four, as before:

| | Where | For |
|---|---|---|
| `bytemuck` | `big-btree`, `big-cluster`, `big-container`, `big-page` | casting between plain-old-data layouts without `unsafe` |
| `crc32fast` | `big-page` | the page checksum |
| `libc` | `big-pager`, `big-bin` | `pwrite`, file locking, and turning terminal echo off |
| `memmap2` | `big-pager` | the read path |

## What the `tls` feature and the password hash add

Twenty-one crates, and this is all of them. `big-tls` is the only crate that names a TLS crate;
`big-http` is the only one that names a hash.

| | Reached through | For |
|---|---|---|
| `rustls` | `big-tls` | the TLS implementation |
| `ring` | `rustls` | the cryptography under it. Chosen over `aws-lc-rs`, which needs `cmake`, which the build image does not have |
| `rustls-webpki` | `big-tls`, `rustls` | certificate chain verification, and the one question rustls does not answer: which node a client certificate names |
| `rustls-pki-types` | `rustls` | the certificate and key types the two above share |
| `untrusted` | `ring`, `rustls-webpki` | bounds-checked parsing of attacker-supplied bytes |
| `subtle`, `zeroize` | `rustls` | constant-time comparison, and wiping key material |
| `getrandom`, `rand_core` | `ring`, `password-hash` | the operating system's random generator |
| `log`, `once_cell`, `cfg-if` | `rustls`, `ring` | plumbing |
| `argon2` | `big-http` | the password hash: argon2id at OWASP's second profile |
| `password-hash` | `argon2` | the PHC string format, which is what a users file stores |
| `blake2` | `argon2`, `big-http` | argon2's own compression function, and the credential cache's key |
| `base64ct` | `password-hash` | constant-time base64 for the PHC string |
| `digest`, `block-buffer`, `crypto-common`, `generic-array`, `typenum` | `blake2`, `argon2` | the RustCrypto trait plumbing |

Twenty-five crates in total for `big-bin`, against four. That is the number, and it is the
headline of the change rather than a footnote to it.

## What is *not* in here

- **No async runtime, no web framework, no serde.** Those decisions did not change, and nothing
  above brought one in.
- **No platform certificate store.** A trust root is a file an operator names. A database peer
  signed by a public CA is not a peer, it is anybody, so the empty trust store fails closed.
- **Nothing in the contrib clients.** `contrib/big-message` and `contrib/big-message-redis` still
  have empty `[dependencies]` sections, which is the machine-checkable form of "this is what
  somebody outside this repository could write against our wire". They gained `Authorization:
  Basic` - which needs a twenty-line base64 encoder and nothing else - and they did not gain TLS.
  Point them at a loopback daemon, or at a proxy on the same host.

Both claims are checked by the `deps` job in CI rather than by this file, because a claim in a
document is a claim that decays.

## Licences

`cargo deny check licenses` passes with no change to `deny.toml`: every crate above is already
covered by the allow list it had. `ring` in particular resolves to ISC, which was on the list for
other reasons before any of this. Run `cargo deny --all-features check` - the default feature set
does not include the TLS half, so a check without `--all-features` looks at a tree that is not
the one being shipped.
