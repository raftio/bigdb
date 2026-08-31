# big-pager

The file and the transactions over it: an mmap read path, a `pwrite` write path, and pure
copy-on-write transactions. No WAL, no checkpoint, nothing to replay after a crash.

What this crate owns is the part of the format that describes the file itself — meta page,
root records, freelist, page allocation, and the commit sequence. The page layouts live in
`big-page`, the b-tree that walks them in `big-btree`, and the containers it stores in
`big-container`.

Those four together are what FeatureBase keeps in one package called `rbf`, for Roaring B-tree
Format. The name is on none of the crates here on purpose: no single one of them earns it, and
a crate named after a b-tree it does not contain sends readers looking in the wrong place.

Everything above this crate sees pages and transactions. It does not see files, mappings,
or `unsafe`.

## The interface

Two traits, deliberately split so a read-only replica can implement only the half it needs.

```rust
pub trait Pager {
    type Ref<'a>: Deref<Target = Page> where Self: 'a;
    fn read(&self, pgno: Pgno) -> Result<Self::Ref<'_>>;
    fn page_count(&self) -> u64;
    fn capacity(&self) -> Option<u64>;
}

pub trait PagerMut: Pager {
    fn write(&self, pgno: Pgno, page: &Page) -> Result<()>;
    fn grow(&self, page_count: u64) -> Result<()>;
    fn sync(&self) -> Result<()>;
}
```

Two things about the shape are load-bearing:

**The GAT is not decoration.** `Ref<'a>` lets a backend hand back a guard instead of a bare
reference. `MmapPager` returns `&'a Page`; `MemPager` returns a lock guard. A buffer-pool
backend would return a pinned-page guard. Fixing `Ref = &'a Page` would close that door.

**The write side takes `&self`, not `&mut self`.** Writer exclusivity is an invariant of
`Store`, not of the backend. With `&mut self` a reader and a writer could not coexist, and
readers are meant to keep running while a writer is active.

`Store<P>` is generic over these traits, so swapping the backend touches nothing in it. The
test `backend_is_swappable` runs the same routine over both backends to keep that honest.

## Backends

| Backend | `Ref<'a>` | Use |
|---|---|---|
| `MmapPager` | `&'a Page` | production; the only `unsafe` module here |
| `MemPager` | `MemRef<'a>` (lock guard) | unit tests; no file, no transaction needed |

## The four mmap constraints

These are conditions for soundness, not tuning options. All four are enforced in
`src/mmap.rs`, and each has a test.

1. **`mapsize` is reserved once at open and never remapped.** Outgrowing it is a hard
   `MapSizeExhausted`, never a silent remap — remapping is precisely what would turn every
   live borrow into a dangling one. The common objection to mmap ("the file grows, the
   mapping moves") is answered by reserving virtual address space up front, which is close
   to free on 64-bit.
2. **`flock(LOCK_EX)`, taken before mapping.** This is what stops another process
   truncating or overwriting under the mapping, and it is the reason the pointer cast in
   `read` is sound at all. Direct consequence: **one process per file.** Multi-process
   access requires switching back to a buffer pool.
3. **Writes never go through the mapping.** `pwrite` only. Writing through the mapping
   would give up control over the order dirty pages reach the disk, and that order is the
   entire atomicity argument.
4. **`ftruncate` before touching a new page.** Inside a reserved mapping, the region past
   EOF is mapped but unbacked; touching it is a SIGBUS. `read` is therefore bounded by
   `file_pages`, never by `mapsize`.

**SIGBUS on media failure is accepted.** There is no way to turn it into a `Result`, and a
handler plus `longjmp` recovers no invariant. It is acceptable because copy-on-write makes
every crash point safe: the process dies, restarts, and the meta page with the highest
txn_id and a valid checksum wins. Media failure costs availability, not correctness.
Operationally this means **media error equals process abort** — a supervisor must restart,
and alerting should tell a SIGBUS abort apart from a bug abort.

**Platform assumption, worth stating because it is not a theorem:** `MAP_SHARED` and
`pwrite` must see each other's data. POSIX does not guarantee this; Linux and macOS both do,
via a unified buffer cache. 32-bit is unsupported, and `O_DIRECT` is no longer available.

## Transactions

```rust
let store = Store::open_or_init(MmapPager::open_default(path)?)?;

let r = store.begin_read();            // no lock, no waiting
let root = r.root(&key);

let mut w = store.begin_write();       // one writer at a time
let new = w.cow(old_pgno)?;            // new pgno; the old page stays intact
w.write(new, page)?;
w.set_root(key, new);
w.commit()?;
```

Dropping a `WriteTxn` without committing is a complete rollback, because nothing reached the
disk and the old meta still points at the old tree.

`ReadTxn::read` returns a reference tied to `&self`, not to the `Store`. A page therefore
cannot be reclaimed while a slice still points into it — the borrow checker enforces the
lifetime rule rather than a runtime flag.

### Commit sequence

1. Hand every page of the three rewritten chains back to the freelist.
2. Allocate pages for root records and the snapshot registry.
3. Allocate pages for the new freelist (see below).
4. `ftruncate` to the new page count.
5. `pwrite` every dirty page, then `fsync`.
6. `pwrite` the new meta page into slot `txn_id % 2`, then `fsync`.

Atomicity lives entirely in step 6. Crash anywhere before it and the previous meta still
describes a complete, consistent tree.

## Freelist

Flat and run-length encoded. Not a b-tree: under copy-on-write, a b-tree freelist would have
to allocate pages in order to record freed pages.

Pages freed by copy-on-write are almost always contiguous — rewriting a fragment releases its
whole page range at once — so RLE collapses hundreds of thousands of page numbers into a few
thousand runs.

A run is reclaimable once no live reader and no registered snapshot can still see the
transaction that replaced it.

### Why the allocation loop runs to a fixed point

The freelist has to allocate its own pages, and allocating mutates the very thing being
serialised. The obvious rule — *always take freelist pages from the file tail* — cuts the
recursion but is **wrong**: every commit returns more pages to the freelist than it consumes,
so the file grows by a page per commit and never stops.

The correct rule allocates from the freelist itself and iterates:

```rust
while free_pgnos.len() < pages_needed(FREE_ENTRY_BYTES, freelist.entry_count()) {
    free_pgnos.push(freelist.alloc(horizon).unwrap_or_else(|| alloc_tail()));
}
```

This terminates, and the reason is a real constraint rather than an implementation detail:
`Freelist::alloc` always takes from the **front** of a run and so never splits one. The entry
count is therefore non-increasing, the right-hand side never grows, and the left-hand side
grows strictly every iteration. In practice it converges in one or two passes.

**If `alloc` is ever changed to take from the middle of a run** — to serve contiguous
allocations, say — this argument collapses and the loop may not terminate.

## Snapshots

Two meta pages carry no history, so snapshots are stored explicitly. A registry entry is
`(id, txn_id, root_records, flags, expires_at, name)`; `begin_read_at` only accepts an id that
is actually in the registry. Reading an arbitrary past `txn_id` would mean never reclaiming a
page, so it is not offered.

A pinned snapshot never expires. Rollback is instant, but only within retention: past that
the pages are gone and there is nothing left to point at.

## Metrics

Growth caused by long-running readers and growth caused by time travel are reported
separately, because merged they would not tell you which knob to turn.

```
oldest_reader_txn_id
pages_pending_reclaim_reader
pages_pending_reclaim_retention
free_pages_reusable
page_count / live_readers / snapshots / fragments / txn_id
```

Snapshot-blocked pages are computed exactly; whatever remains is attributed to readers.

## Not here

Container set operations (`big-container`), the b-tree (`big-btree`), fragments
(`big-engine`). `trait Pager` is enough for `big-btree` to be tested against `MemPager`
without a file.

## Tests

`cargo test -p big-pager` — 23 tests. The ones worth knowing about:

- `corrupt_newer_meta_falls_back_to_older` — the whole of crash recovery.
- `repeated_commits_do_not_grow_the_file_without_bound` — catches the freelist rule above;
  it is not visible by reading the code.
- `a_live_reader_blocks_reclaim_and_is_reported_as_such` and
  `a_snapshot_blocks_reclaim_under_the_retention_reason` — the two metrics do not bleed.
- `second_handle_cannot_open_the_same_file`, `page_inside_mapping_but_past_eof_is_an_error`,
  `exhausting_mapsize_is_an_error_not_a_remap` — mmap constraints 1, 2 and 4.
