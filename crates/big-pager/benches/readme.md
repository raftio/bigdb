# Benchmarks

Two kinds of measurement live here, and they answer different questions.

**Timed** (`write.rs`, `read.rs`, criterion). How fast on this machine today. Useful against a
saved baseline, meaningless as an absolute number quoted anywhere else.

**Counted** ([`tests/amplification.rs`](../tests/amplification.rs)). How much work, on every
machine, forever. `CountingPager` tallies pages read, written and synced, so the result is a
number that can be asserted rather than eyeballed. That file is a test, not a benchmark,
because a regression in pages-per-commit should fail CI rather than show up as a slower bar.

## Running

```sh
cargo bench -p big-pager                       # everything, several minutes
cargo bench -p big-pager -- b3                 # one group
cargo test  -p big-pager --test amplification -- --nocapture   # the counted half
```

Comparing against a baseline is the only way the timed half is worth anything:

```sh
cargo bench -p big-pager -- --save-baseline main
# ... make a change ...
cargo bench -p big-pager -- --baseline main
```

## What each group is for

| Group | Question |
|---|---|
| `b1_empty_commit` | What does a commit cost when the transaction changed nothing? |
| `b2_dirty_pages` | How much of a commit scales with pages actually dirtied? |
| `b3_commit_vs_fragments` | Does a one-fragment commit cost more as the database grows? |
| `b4_cow` | What does holding a large dirty set in memory cost? |
| `b5_alloc` | Freelist reuse against extending the file. |
| `b6_read` | Sequential, scattered, and a mapping not yet faulted in. |
| `b8_readers` | What the reader registry costs when several threads open transactions. |

## Reading the numbers

Every group that can run on both pagers does. `MemPager` makes `sync` a no-op, so the gap
between a `mem` line and its `mmap` twin is the price of durability and nothing else — on
macOS that is two `F_FULLFSYNC` calls per commit, which dominates everything.

Two things the harness cannot do honestly, so it does not pretend to:

- **Cold reads from the device.** `mmap_fresh_map` opens a mapping this process never touched,
  but the file is still in the OS cache. Purging that needs root.
- **A quiet machine.** Thermal state and background indexing move these numbers more than most
  code changes do. Trust the counted half; treat the timed half as a comparison, never a fact.
