# big-btree

A copy-on-write b-tree whose leaves hold roaring containers.

Keyed by container key — a `(row, chunk)` pair — and valued by the container that key names.
Every write copies the pages it touches rather than editing them in place, so a reader holding
an older root sees a consistent tree for as long as it holds it, with no locking between the
two.

## Generic over the pager, on purpose

Reads take `impl Pager`, so the entire tree is exercised against an in-memory pager with no
file, no mapping and no `unsafe` anywhere in the test binary. The `MmapPager` is one
implementation of a trait, not a dependency of the algorithm.

## The walk is one function, with two callers

`walk::visit_tree` enumerates every page reachable from a root. `copy_tree` and `free_tree` are
both written on top of it rather than each doing their own traversal, which is what makes them
impossible to disagree: **a page class the copy forgets is a page class the free forgets too,
so it fails a test instead of silently losing a container during a backup.** Backup, full
compaction and format migration are all the same operation — walk from a consistent set of
roots, write into a fresh file with new page numbers — and that operation is here.

## Layout

- `read` — descent and iteration: `find`, `find_many`, `scan`, `count`, `collect`
- `write` — insert, remove, split, merge, all copy-on-write
- `walk` — `visit_tree`, and the `copy_tree` / `free_tree` pair built on it
- `item` — what a leaf yields: a key and the container under it

## Tests

A `BTreeMap` model test under proptest, and a second one under the fuzzer — `cargo +nightly
fuzz run btree_program`. The fuzzer is not a copy of the proptest: it is coverage-guided rather
than randomly sampled, it checks after *every* step rather than at the end, and it walks the
tree with `visit_tree` to assert no page is reachable twice. A contents-only oracle passes on a
tree with a duplicated page, which is a double free waiting to happen.

## A sharp edge worth knowing

`remove` always keeps a root page. A tree emptied by deletions is an empty tree, not an absent
one, so the layer above has to notice and drop the root record itself — otherwise a fragment
emptied by deletes leaks a page and a live root record. `big-db` is where that is handled.
