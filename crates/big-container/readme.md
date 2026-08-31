# big-container

Roaring containers: the set of up to 65,536 integers that everything above is built out of.

A container holds the low 16 bits of a set of values sharing the same high bits. Which of the
three representations it uses is a consequence of its own contents rather than a choice the
caller makes:

| Representation | Holds | Costs |
|---|---|---|
| Array | sorted `u16`s | 2 bytes per value |
| Bitmap | 1024 `u64` words | 8 KiB flat, whatever the cardinality |
| Run | sorted `(start, len)` pairs | 4 bytes per run |

`ARRAY_BREAK_EVEN` is where the first two cross — 4,096 values, the point at which two bytes
each stops being cheaper than a flat bitmap — and it is a constant here rather than a number
written into a policy elsewhere, because the crossover is a property of the layout and moving
the layout has to move the threshold with it.

## What is not decided here

**Where the container lives.** These are values, not pages. `big-page` decides how one is laid
out in bytes on disk and `big-btree` decides which one answers a given key. A container does not
know it is persisted.

**When to change representation.** `optimize` will tell you the cheapest form for a given set,
but nothing here calls it on its own. Rewriting a container is a write, and writes are the
b-tree's to schedule.

## Layout

- `container/` — the representations and the operations that read them
- `builder` — accumulating a container from values without going through a set per insert
- `ops/` — the boolean algebra: and, or, andnot, plus rank and select
- `delta` — the difference between two containers, which is what a copy-on-write write needs
- `interval` — run arithmetic, shared by the run representation and the range operations
- `optimize` — which representation a given set of values is cheapest in

## Tests

Unit tests throughout, and the boolean operations are checked against `BTreeSet<u16>` as an
oracle: any disagreement between a container operation and the obvious slow implementation of
the same question is a failure.

The same oracle runs under the fuzzer — `cargo +nightly fuzz run container_ops` — which is where
it earns its keep. Three representations means six implementations of every binary operation,
chosen by a dispatch that depends on *both* operands, and a hand-written test picks the pairs
someone thought of.
