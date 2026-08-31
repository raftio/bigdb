# Contributing to big

big is a storage engine, and the engine is the whole scope for now. Query language, API
surface, and distribution are deliberately undecided — patches that assume one of those
answers will be asked to wait rather than merged.

## Before writing code

Open an issue first for anything beyond a bug fix. The crates here have a fixed dependency
direction and a handful of invariants that are cheaper to discuss than to review:

- Dependencies flow one way. `big-page` never learns about `big-btree`, and nothing above
  `big-pager` touches `unsafe`.
- Anything that changes a byte layout, `SHARD_WIDTH_EXPONENT`, or `CATALOG_ENTRY_BYTES` is a
  format break. Say so in the issue.
- A row must never straddle a container boundary. Several prefix-scan claims rest on it, and
  `crates/big-fragment/src/coords.rs` asserts it at compile time.

## Building and testing

Stable toolchain, no nightly needed.

```sh
cargo test --workspace
cargo clippy --workspace --all-targets
```

Both must be clean. Clippy is currently at zero warnings and should stay there.

Fuzzing is opt-in and needs nightly, which is why it lives outside the workspace:

```sh
cargo +nightly fuzz run parse_page
```

Crash injection runs as part of the normal suite. It re-executes the test binary as a child
process and kills it at a failpoint compiled in behind a feature flag, so a change to the
commit sequence in `big-pager` needs a matching failpoint.

## What a change needs

- **A test that fails before it and passes after.** Where the crate already has property tests
  against a reference implementation — a `BTreeMap`, a plain set, `croaring` — extend those
  rather than adding an example-based test alongside them.
- **No new `unsafe`.** Every crate root carries `deny(unsafe_code)`, opted out of in exactly
  two modules, each with its safety argument written in prose. A third one needs the argument
  written out and agreed in the issue first.
- **Comments that say why, not what.** The existing comments explain the reasoning behind a
  decision — why depth is per fragment, why clearing happens before setting. Match that.
- **No silent truncation or swallowed errors** at a boundary. Refuse the input instead; see
  `MAX_KEY_LEN` in `big-keys` for the pattern.

## Commits and pull requests

Conventional commits: `feat:`, `fix:`, `refactor:`, `docs:`, `test:`, `chore:`, `perf:`, `ci:`.

Keep one logical change per commit. A pull request description should say what invariant the
change rests on and what would break if it were wrong.

## Contributor License Agreement

**Every contributor must sign a CLA before their first pull request is merged.** Signing is
handled by [CLA assistant](https://github.com/cla-assistant/cla-assistant) as a status check
on the pull request; there is nothing to email.

The agreement is the unmodified [Apache Individual Contributor License Agreement
(ICLA)](https://www.apache.org/licenses/icla.pdf), with a corporate CLA available for
contributions made on an employer's behalf.

Why it is required, stated plainly rather than buried:

- The engine is Apache-2.0 and is intended to stay that way. Nothing published under
  Apache-2.0 can be taken back — a fork of any released commit remains Apache-2.0 forever.
- Layers that do not exist yet — a server, distribution, anything hosted — may ship under a
  different license. The CLA is what keeps that option open, because without it a single
  outside contribution permanently fixes the license of the code it touches.
- The CLA grants a copyright and patent license to the project. It is **not** a copyright
  assignment: you keep ownership of your work and remain free to use it however you like.

A DCO sign-off is not a substitute. `Signed-off-by` certifies that you had the right to submit
the code; it grants no license, so it cannot serve the purpose above.

If you are not willing to sign, that is a legitimate position — open an issue describing the
bug or the design instead. A well-written issue is worth as much as a patch here.
