# big-page

The byte layout of a page, and the parsing of one back out of bytes.

Every page is `PAGE_SIZE` bytes, carries a checksum, and is parsed through a function that
returns `Result`. **Nothing in this crate may panic on malformed input.** It sits directly
behind `mmap`, which means the bytes it parses are whatever is in the file — including whatever
a corrupt disk, a truncated write or a hostile file happens to contain. That is why this is the
crate with a fuzz target on it.

## The page kinds

- `meta` — the two meta pages, of which the one with the higher committed transaction id and a
  valid checksum is the truth. This is the whole recovery story; there is no WAL to replay.
- `leaf` — cells, each a container key and either an inline container or a pointer to a bitmap
- `branch` — separator keys and child page numbers
- `chain` — overflow: anything that does not fit in one page, held as a linked run of them
- `record` — the root records and the catalog, which are chains with a known shape

`layout` holds the offsets and the checksum; `key` holds the container key ordering that both
leaf and branch depend on.

## Checksums

Every page carries one, with a single deliberate exception: the bitmap pages a leaf cell points
at. A bitmap page is 8 KiB of dense bits with no free space to put a checksum in without either
shrinking the bitmap or growing the page, and both cost more on every read than the exception
does. `architecture.md` says so in full.

## Not here

Anything about *files*. Reading a page from a mapping, writing one back, allocating page
numbers and deciding when a write is durable are all `big-pager`. This crate is pure bytes in,
structure out, and it does not link anything that opens a file.

## Tests

Unit tests per page kind, plus a fuzz target in the workspace `fuzz/` crate — `cargo +nightly
fuzz run parse_page` — which asserts the invariant this crate exists to hold: every entry point
that touches bytes off disk returns `Result` and never panics. Its seeds are real pages rather
than noise, so the fuzzer starts from something that parses.
