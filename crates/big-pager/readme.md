# big-pager

**A shadow-paged storage layer for a bitmap-native database: mmap reads, `pwrite` writes, and
pure copy-on-write transactions with no write-ahead log.**

---

## Abstract

`big-pager` is the layer of `big` that owns the file and the transactions over it. It provides
single-writer/multi-reader ACID transactions, snapshot isolation, and point-in-time reads over
an 8 KiB paged file, and it does so with **no WAL, no checkpoint, and no recovery path**: a
commit writes pages, flushes, flips one of two meta pages, and flushes again. Crash recovery is
picking whichever meta page has the higher `txn_id` and a valid checksum. There is nothing to
replay, so there is nothing to replay incorrectly.

The read path is a borrow straight out of a `MAP_SHARED` mapping — no copy, no buffer pool, no
page descriptor — which is what makes a bitmap engine's characteristic access pattern (one bit
read from each of many pages, per probe) affordable. The write path never touches the mapping;
it is `pwrite` only, because the order in which dirty pages reach the disk *is* the atomicity
argument.

This document states the design, the invariants it rests on, the proofs that the two
non-obvious algorithms terminate, the measured cost model, and the limits — including the ones
that are not fixable within this design.

**Contributions.**

1. A commit protocol whose atomicity reduces to a single 8 KiB write, with a durability knob
   that provably cannot break it (§6.3, §8).
2. A soundness argument for `mmap`-borrowed reads in safe Rust, reduced to four mechanically
   enforced constraints, each with a test (§5).
3. A run-length-encoded freelist that must allocate its own pages, with a termination proof for
   the fixed-point loop this requires (§7.3).
4. A verified-page memo that is sound *because of* copy-on-write, removing a per-probe CRC that
   was measured at 606 ns of a 640 ns read (§5.4).
5. A cost model measured in pages rather than seconds, asserted in CI, and honest about the one
   term that still scales with database size rather than with the change (§10).

---

## 1. Position in the system

`big` stores every fact as one bit at `(row, record)`. A filter is a set intersection and a
count is a population count. Physically that is a b-tree of roaring containers over 8 KiB pages,
and the crates split along the seams of that sentence:

| Crate | Owns |
|---|---|
| `big-container` | Roaring containers: array, bitmap, run, and the set operations on them |
| `big-page` | Byte layout, parsing and checksums of the five page kinds |
| `big-btree` | The b-tree that walks those pages |
| **`big-pager`** | **The file: meta page, root records, catalog, freelist, allocation, commit** |

Those four together are what FeatureBase keeps in one package called `rbf`, for Roaring B-tree
Format. The name is on none of the crates here on purpose: no single one of them earns it, and a
crate named after a b-tree it does not contain sends readers looking in the wrong place.

**Everything above this crate sees pages and transactions. It does not see files, mappings, or
`unsafe`.** `#![deny(unsafe_code)]` holds for the whole crate except `src/mmap.rs`, which is the
only module that opts back in, and §5 is its safety argument.

---

## 2. Why shadow paging rather than a WAL

The classical choice is between shadow paging (Lorie, 1977) and write-ahead logging
(ARIES; Mohan et al., 1992). WAL won the industry for reasons that are real: it turns random
writes into sequential ones, it supports fine-grained locking and partial page updates, and it
lets a group commit amortise the flush.

None of those advantages apply with full force here, and one of the costs applies with unusual
force:

- **Updates are never partial.** A roaring container is rewritten wholesale or not at all. There
  is no in-page delta to log that would be smaller than the page.
- **The b-tree is already copy-on-write.** Rewriting a leaf rewrites its parents to the root.
  Shadow paging is not an extra cost imposed on the design; it is what the design already does.
- **Writes are already batched.** An analytical ingest commits large transactions, so the
  sequential-write advantage of a log is partly recovered by writing many pages per commit
  anyway.
- **A WAL would double the code that can be wrong.** Recovery is the part of a storage engine
  that runs least often and matters most. Shadow paging has no recovery *code*: `Store::load`
  reads two pages and picks one.

The trade accepted in exchange is write amplification — a changed leaf costs its whole
root-to-leaf path — and that cost is measured, not assumed (§10).

---

## 3. The storage interface

Two traits, deliberately split so a read-only replica implements only the half it needs.

```rust
pub trait Pager {
    type Ref<'a>: Deref<Target = Page> where Self: 'a;

    fn read(&self, pgno: Pgno) -> Result<Self::Ref<'_>>;
    fn page_count(&self) -> u64;
    fn capacity(&self) -> Option<u64>;
    fn verify_bitmap(&self, pgno: Pgno, page: &Page, expected: u32) -> bool;
    fn io_stats(&self) -> Option<IoStats>;
}

pub trait PagerMut: Pager {
    fn write(&self, pgno: Pgno, page: &Page) -> Result<()>;
    fn grow(&self, page_count: u64) -> Result<()>;
    fn truncate(&self, page_count: u64) -> Result<()>;
    fn sync(&self) -> Result<()>;
    fn sync_data(&self) -> Result<()>;
}
```

Four things about this shape are load-bearing.

**The GAT is not decoration.** `Ref<'a>` lets a backend hand back a guard instead of a bare
reference. `MmapPager` returns `&'a Page`; `MemPager` returns a lock guard; a buffer-pool backend
would return a pinned-page guard. Fixing `Ref = &'a Page` would close that door permanently, and
the door is the migration path off the single-process constraint of §12.

**The write side takes `&self`, not `&mut self`.** Writer exclusivity is an invariant of `Store`,
not of the backend. With `&mut self`, a reader and a writer could not coexist — and readers
running unimpeded alongside a writer is the entire point of copy-on-write.

**`verify_bitmap` is on the trait, not in the b-tree.** It is the one parent-to-child integrity
link in the tree, and whether it can be memoised depends on facts only the backend knows (§5.4).
The default implementation recomputes the CRC every time: always correct, never wrong, just slow.

**`io_stats` is asked of the backend, not measured above it.** A call into `read` is not an I/O,
and how much of one it is differs per backend by more than a constant: a mapped read copies
nothing and may not touch the disk at all; a file read is a syscall and 8 KiB. `None` — the
honest answer for a pager with no disk under it — tells a metrics exporter to omit the series
rather than publish zeroes that read as an idle database.

### 3.1 Backends

| Backend | `Ref<'a>` | Use |
|---|---|---|
| `MmapPager` | `&'a Page` | Production. The only `unsafe` module in the crate. |
| `MemPager` | `MemRef<'a>` (lock guard) | Unit tests; no file, no fsync, no transaction needed. |
| `CountingPager<P>` | delegates | Test-only decorator tallying calls, for asserting cost (§10). |

`Store<P>` is generic over the traits, so swapping the backend touches nothing inside it. The
test `backend_is_swappable` runs the same routine over both real backends to keep that honest.

---

## 4. The file format

```
page 0   meta slot 0        ┐ a commit writes slot txn_id % 2
page 1   meta slot 1        ┘ recovery picks the higher valid txn_id
page 2.. b-tree pages, chain pages, bitmap pages — allocated, never rewritten in place
```

`PAGE_SIZE` is 8192, chosen in `big-page` so that a dense roaring container is exactly one page.
`META_PAGES` is 2. A meta page carries magic `0x47494230`, a format version, the page size, flags,
`txn_id`, `page_count`, and the head page number of four chains:

| Chain | Entry | Rewritten |
|---|---|---|
| Root records | 24 B: `FragmentKey` (20 B) → root `Pgno` (4 B) | Whenever any fragment root moves |
| Catalog | 128 B, opaque to this crate | Only when its bytes actually changed |
| Snapshots | 64 B: `(id, txn_id, root_records, flags, expires_at, name)` | Only when a snapshot is taken or dropped |
| Freelist | 16 B: `(freed_at, first, len)` | Every commit, without exception |

A *chain* is a flat singly-linked list of pages holding fixed-stride entries. Chains are read by
`chainio::load_chain`, which is **cycle-guarded**: `next` comes off disk, so a corrupt or hostile
file could otherwise induce an infinite walk. The guard is a hop count bounded by `page_count`,
and exceeding it is `ChainCycle`, not a hang.

The four chains are flat lists rather than trees on purpose. Three of them are small by
construction; the fourth, root records, is the one place where flatness has a measurable cost,
and §10 says exactly how much.

### 4.1 Opening a file, and the case that used to be destructive

```rust
match pager.page_count() {
    0                 => Store::init(pager),     // nobody has created this database yet
    n if n >= 2       => Store::load(pager),     // read the meta pages
    n                 => Err(NotADatabase { .. }) // somebody else's file
}
```

**An empty path and a short file are not the same case, and conflating them is destructive.** A
file with bytes in it but fewer than two pages holds zero *pages*, so it used to look exactly
like a new database and got initialised on top of. A larger file was always refused, by the magic
in its meta page; this refuses the range where there was no magic to check. `MmapPager::open`
enforces the same rule in bytes, because the byte length is only known there.

### 4.2 Recovery, in full

```rust
let meta = pick_meta(read_meta(0), read_meta(1))?;   // higher txn_id, valid checksum
```

That is the whole of it. What the code adds is not recovery logic but *diagnosis*: when both
slots fail, the two failures are not flattened. A version mismatch wins over anything else,
because it is the only failure with an action attached — run the migration — and a file written
by a different build of `big` must not be indistinguishable from a corrupted one. The two call
for opposite responses. Everything else collapses to `NoValidMeta`, because "slot 0 had a bad
magic and slot 1 a bad checksum" tells an operator nothing the summary does not.

---

## 5. The read path: borrowing out of a mapping

`MmapPager::read` returns `&'a Page` pointing into a `MAP_SHARED` mapping. No copy is made, no
descriptor is allocated, and no lock is taken. For a bit-sliced point read — which touches one
bit in each of many pages — this is the difference between a viable design and an unviable one.

Borrowing from a mapping in safe Rust is only sound under conditions. There are four, all
enforced in `src/mmap.rs`, each with a test.

### 5.1 The four constraints

1. **`mapsize` is reserved once at open and never remapped.** `DEFAULT_MAPSIZE` is 1 TiB of
   virtual address space, which is close to free on 64-bit. Outgrowing it is a hard
   `MapSizeExhausted`, never a silent remap — remapping is precisely what would turn every live
   borrow into a dangling pointer. The usual objection to mmap ("the file grows, the mapping
   moves") is answered by reserving the address space up front.
   *Test:* `exhausting_mapsize_is_an_error_not_a_remap`.

2. **`flock(LOCK_EX)`, taken before mapping.** This is what stops another process truncating or
   overwriting under the mapping, and it is the reason the pointer cast in `read` is sound at
   all. Direct consequence: **one process per file** (§12).
   *Test:* `second_handle_cannot_open_the_same_file`.

3. **Writes never go through the mapping.** `pwrite` only. Writing through the mapping would
   hand the kernel the decision of *when* each dirty page reaches the disk, and that ordering is
   the entire atomicity argument of §6.3. This constraint is also what makes `big` immune to the
   first and most serious of the objections in Crotty et al., "Are You Sure You Want to Use MMAP
   in Your DBMS?" (CIDR 2022): the transactional-safety problem there is the page cache flushing
   dirty mapped pages at a time of its choosing. Here no mapped page is ever dirty.

4. **`ftruncate` before touching a new page.** Inside a reserved mapping, the region past EOF is
   mapped but unbacked; touching it is a SIGBUS. `read` is therefore bounded by `file_pages`,
   never by `mapsize`, and `truncate` shortens that bound *before* shortening the file so no read
   can slip into the gap.
   *Tests:* `page_inside_mapping_but_past_eof_is_an_error`, and `truncate_tail` refuses to run
   while any reader is alive.

### 5.2 What remains: SIGBUS

A media error under the mapping raises SIGBUS, and there is no way to turn that into a `Result`.
A handler plus `longjmp` recovers no invariant that copy-on-write has not already secured.

It is accepted because **copy-on-write makes every crash point safe**: the process dies, restarts,
and the meta page with the highest `txn_id` and a valid checksum wins. Media failure costs
availability, never correctness. Operationally this means **a media error is a process abort** — a
supervisor must restart the process, and alerting should distinguish a SIGBUS abort from a bug
abort, because they call for different people.

This is the second CIDR-2022 objection, and it is conceded rather than answered. The third
(I/O stalls on page faults) is likewise conceded: a fault is invisible to this process, which is
why `IoStats::reads` counts *pages the engine asked for* and does not pretend to count disk
reads. The fourth (single-threaded eviction and TLB shootdowns) does not arise, because nothing
here dirties a mapped page.

### 5.3 A platform assumption worth stating

`MAP_SHARED` and `pwrite` must see each other's data. **POSIX does not guarantee this.** Linux
and macOS both do, via a unified buffer cache, and `big` runs on those two. 32-bit is
unsupported, and `O_DIRECT` is not available under this design.

On macOS, `fsync(2)` returns once data reaches the driver; the drive may still hold it in a
volatile cache, and a power cut there can reorder the two flushes a commit depends on.
`MmapPager::sync` therefore issues `F_FULLFSYNC` on macOS, falling back to `sync_data` on
filesystems that reject it. On Linux `fdatasync` already carries the guarantee.

### 5.4 The verified-page memo

Each dense bitmap page carries a CRC in its parent leaf cell — the one parent-to-child integrity
link in the tree. A bit-sliced point read asks the question once per bit plane while reading a
single bit from each, so recomputing a CRC over the whole 8 KiB page *was the entire cost of
reading an integer*: 606 ns of a 640 ns plane, measured.

`MmapPager` therefore remembers that a page number verified against a given checksum. **This is
sound rather than a shortcut, and the reason is copy-on-write:** a page's bytes never change once
written, because modifying a page allocates a *new* page number. A remembered answer can only go
stale when a number is recycled and written again — so `write` forgets the memo *before* the
`pwrite`, never after, so that a reader racing the write can never find a memo that outlived the
bytes it was about. `truncate` drops entries past the new end for the same reason.

Two details are deliberately crude. The memo is striped 64 ways, because point reads hit it once
per plane from every reader at once and a single lock would serialise exactly the path it exists
to speed up. And a stripe that reaches 4096 entries is **emptied wholesale** rather than evicted
by policy: forgetting costs one CRC per page and can never produce a wrong answer, which is a
far better trade than carrying an eviction policy into the one module that is allowed `unsafe`.

A write transaction overrides this: a page it is still building is checked for real every time,
because a page number it just allocated may carry a memo from whatever lived there before.
*Tests:* `tests/verified_memo.rs`, four of them, including
`a_memo_never_outlives_the_bytes_it_was_about`.

---

## 6. Transactions

```rust
let store = Store::open_or_init(MmapPager::open_default(path)?)?;

let r = store.begin_read();          // no lock, no waiting
let root = r.root(&key);

let mut w = store.begin_write();     // one writer at a time
let new = w.cow(old_pgno)?;          // a new pgno; the old page stays intact
w.write(new, page)?;
w.set_root(key, new);
w.commit()?;
```

### 6.1 Readers

`begin_read` takes the meta page under a short read lock, clones two `Arc`s, and is done. It
registers the reader's `txn_id` in a refcounted map — refcounted, not flagged, so two readers at
the same `txn_id` do not release each other's hold.

The roots and the catalog are captured **together**, not fetched in two steps. A commit publishes
both under one lock; reading them separately can land between the two and pair fresh roots with a
stale schema, which surfaces as a just-written fragment being invisible rather than as an error.

`ReadTxn::read` returns a reference tied to `&self`, not to the `Store`. A page therefore cannot
be reclaimed while a slice still points into it: **the borrow checker enforces the lifetime rule
that a pin count would otherwise enforce at runtime.**

### 6.2 Writers

One writer at a time, via a mutex whose guard the `WriteTxn` holds. Readers are unaffected.

`cow(pgno)` copies the page's bytes, allocates a new page number, and frees the old one. The old
page stays exactly as it was; every reader still on the old meta page keeps reading it.

Allocation prefers, in order: **this transaction's own discards**, then the freelist, then the
file tail. The first tier matters more than it looks. A page that this transaction both allocated
and freed is unreachable by construction — it was never named by a committed meta page, so no
reader can hold it and the horizon has nothing to say about it. Handing it straight back is what
stops a large transaction growing the file by its own churn; without it, every page a
copy-on-write rewrite abandons mid-transaction is dead weight until the *next* transaction can
reclaim it. The `tail_floor` recorded at `begin_write` is what lets `free` tell its own pages
from inherited ones.

**Rollback is doing nothing at all.** Dropping an uncommitted `WriteTxn` is a complete rollback:
no page reached the disk, and the old meta still points at the old tree.

A `WriteTxn` is itself a `Pager` — its own dirty pages first, then the file — which is what lets
`big-btree` descend through pages this transaction has not committed yet, using the same code it
uses against a committed tree.

### 6.3 The commit protocol

```
1. For each metadata chain that changed: return its old pages to the freelist,
   then allocate new ones.  A chain nobody touched keeps the pages it has.
2. Allocate the freelist's own pages, to a fixed point (below).
3. grow()   — ftruncate to the new page count
4. pwrite   — every dirty page
5. flush
6. pwrite   — the new meta page into slot txn_id % 2
7. flush
```

**Atomicity lives entirely in step 6.** Until the meta page lands, every byte written above it is
unreachable garbage and a crash costs nothing. After it lands, all of it is live. There is no
state in between, which is the whole reason there is no WAL.
*Tests:* `tests/crash.rs` forks a child that aborts at an injected failpoint —
`crash_between_data_fsync_and_meta_write_keeps_the_old_tree`,
`crash_after_meta_write_leaves_a_complete_committed_tree`. The failpoints are compiled out
unless the `crash-injection` feature is on, so a production build cannot be aborted by an
environment variable.

Step 1 exists because a commit rewrites a chain wholesale, so a chain nobody touched would
otherwise cost its entire length in pages for nothing. The catalog is the expensive one — 128
bytes per fragment — and most transactions do not alter a byte of it. `set_catalog` compares
before it dirties: a memcmp against hundreds of kilobytes of rewriting and two flushes' worth of
latency behind it. *Test:* `an_unchanged_chain_is_not_rewritten`.

---

## 7. Free space

### 7.1 The freelist is flat and run-length encoded

Deliberately not a b-tree: under copy-on-write, a b-tree freelist would have to **allocate pages
in order to record freed pages**, and that recursion has no natural floor.

An entry is `(freed_at, first, len)` in 16 bytes. Pages freed by copy-on-write are almost always
contiguous — rewriting a fragment releases its whole page range at once — so `compact()`, which
merges adjacent runs sharing a `freed_at`, collapses hundreds of thousands of page numbers into a
few thousand runs.

### 7.2 The horizon

A run is reclaimable once no live reader and no registered snapshot can still see the transaction
that replaced it:

```
horizon = min(oldest live reader txn_id,
              oldest registered snapshot txn_id,
              current meta txn_id)
```

`alloc(horizon)` will only take from a run whose `freed_at <= horizon`. This is the entire MVCC
mechanism; there is no version chain and no undo log.

*Tests:* `a_live_reader_keeps_every_page_it_could_reach`, `the_reader_still_sees_the_old_bytes`,
`reclaim_resumes_when_the_reader_goes`,
`a_reader_held_across_concurrent_writers_never_sees_a_recycled_page`.

### 7.3 Why the allocation loop runs to a fixed point, and why it terminates

The freelist has to allocate its own pages, and allocating mutates the very thing being
serialised. The obvious way out — *always take freelist pages from the file tail* — cuts the
recursion but is **wrong**: every commit returns more pages to the freelist than it consumes, so
the file grows by a page per commit and never stops. That bug is invisible in the code and
visible only over many commits, which is why it has a test of its own.

The correct rule allocates from the freelist itself and iterates:

```rust
freelist.compact();
while out.len() < pages_needed(FREE_ENTRY_BYTES, freelist.entry_count()) {
    out.push(freelist.alloc(horizon).unwrap_or_else(|| alloc_tail()));
}
```

**Termination.** `Freelist::alloc` always takes from the **front** of a run:

```rust
run.first += 1;
run.len   -= 1;
if run.len == 0 { self.runs.remove(idx); }
```

so it never splits a run, and the entry count is therefore non-increasing. The right-hand side of
the loop condition is a monotone function of the entry count, so it never grows; the left-hand
side grows strictly on every iteration. The loop therefore terminates, and in practice converges
in one or two passes.

> **If `alloc` is ever changed to take from the middle of a run** — to serve contiguous
> allocations, say — this argument collapses and the loop may not terminate. That change requires
> a new termination argument, not just a new test.

*Test:* `repeated_commits_do_not_grow_the_file_without_bound`.

### 7.4 Returning space to the filesystem

`truncate_tail` drops reclaimable runs that sit flush against the end of the file, repeatedly,
because removing one run can expose the one before it. Only trailing pages are handled: moving a
page anywhere else means rewriting whoever points at it, and that knowledge lives in the b-tree,
not here.

Two ordering rules make it safe, and both are subtle:

- **It refuses to run while any reader is alive** (`ReadersActive`). Shrinking the file turns the
  region past the new EOF back into unbacked mapping, and a live borrow into it would be a SIGBUS.
- **The trimmed freelist is written back before the meta flip, and the meta flip before the
  `ftruncate`.** Trimming happens in memory; without writing the chain back, a reopen before the
  next commit would read a freelist listing pages past the end of the file and hand them out. And
  a crash between the write-back and the flip leaves the *old* meta pointing at a chain that has
  forgotten some free pages: they leak until the next compaction finds them. That is the right way
  round — a leak, never a page handed out twice.

The freelist chain is rewritten *in place* here, which is safe in this one method and nowhere
else: no reader can exist, the chain's own pages are live and so cannot be inside a trimmed run,
and the trimmed list is never longer than the old one.

---

## 8. Durability

```rust
store.set_durability(Durability::None)?;   // bulk load
// ... load ...
store.set_durability(Durability::Full)?;   // flushes first, then takes effect
```

| Level | Flush | A committed transaction survives |
|---|---|---|
| `Full` (default) | `F_FULLFSYNC` on macOS, `fdatasync` on Linux | power loss |
| `Barrier` | `fdatasync` | the process and the OS dying, not the drive's volatile cache |
| `None` | nothing | the process dying; not the machine |

**The invariant every level respects: the two flushes move together, or neither happens.** A
level that skipped the first flush but kept the second would let the meta page reach the disk
while the pages it names had not — and that is not lost data, it is a file that does not open.
So what the knob trades is *how far up the stack the last commits are guaranteed to have
travelled*, never atomicity. Even `Durability::None` leaves an older consistent file rather than
a broken one. *Tests:* `no_level_flushes_an_odd_number_of_times`,
`a_relaxed_commit_is_still_readable_and_still_atomic`,
`a_relaxed_commit_still_leaves_a_file_that_opens` (under crash injection).

Two design points:

- **Settable at any time, not fixed at open**, because the caller this exists for is a bulk load
  inside a process that also serves ordinary traffic. Fixing it at open would have made that a
  restart.
- **Tightening flushes first.** After `set_durability(Full)` returns, every commit that has
  already happened is covered by at least the new promise. Without that, the loader that
  carefully tightened up at the end of its batch would still lose the batch. Relaxing does not
  flush; there is nothing to make less durable.

`truncate_tail` flushes unconditionally whatever the setting says: "the file shrinks after the
shorter meta lands" is only true if the meta really is on disk, and the other order leaves a meta
naming pages past EOF — a file that does not open. Nobody asked for a faster truncate.

---

## 9. Snapshots

Two meta pages carry no history, so snapshots are stored explicitly. A registry entry is
`(id, txn_id, root_records, flags, expires_at, name)` at 64 bytes, and `begin_read_at` accepts
only an id that is actually in the registry. Reading an arbitrary past `txn_id` would mean never
being able to reclaim a page, so it is not offered.

`create_snapshot` pins the state as of *before* the current transaction: its root records are the
ones already on disk. A snapshot flagged `SNAP_PINNED` never expires. Rollback is instant, but
only within retention — past that, the pages are gone and there is nothing left to point at.

**One caveat is real and stated rather than hidden.** A snapshot pins root records and nothing
else; `begin_read_at` hands back the *current* catalog. For a schema that only ever grows this is
harmless — the extra entries describe fragments the snapshot's roots simply do not have — but it
stops being harmless the moment dropping a field can remove an entry. **A snapshot must not be
used to read across a field drop.**

---

## 10. Cost model

The engine is measured in **pages**, not seconds. A page count is the same number on every
machine, so it can be asserted in CI; a duration is a fact about a laptop's thermal state.
`CountingPager` tallies calls into the trait, and `tests/amplification.rs` asserts them.

Pages written by a commit that rewrites exactly one fragment root, measured:

| Fragments in the database | Pages written | Bytes |
|---:|---:|---:|
| 0 | 1 | 8 KiB |
| 100 | 4 | 32 KiB |
| 1 000 | 6 | 48 KiB |
| 4 000 | 15 | 120 KiB |

An empty commit costs exactly one page: the meta. At four thousand fragments, the fifteen are one
data page, one freelist page, one meta page, and **twelve pages of root records** — because the
root-record chain is a flat list rewritten in full whenever any root moves, which is every data
commit. Four thousand records at 24 bytes is 94 KiB.

**This is the one term that scales with the size of the database rather than with the size of the
change, and it is a known limitation rather than a mystery.** Removing it means making the root
records a tree instead of a list. The test that would show that work paying off already exists:
`commit_cost_grows_with_the_database_not_the_change` bounds the growth rate rather than pinning an
exact count, so it fails on a regression and passes on an improvement.

The catalog used to contribute to this number and no longer does: skipping an unchanged chain took
a single-fragment commit at 4 000 fragments from 79 pages to 15.

A second measured property, `read_path_allocates_nothing`: reading the same page twice yields the
same address, and no page read allocates. Reads are borrows, not copies.

---

## 11. Observability

Without these, the failure mode of copy-on-write only shows up when the disk fills.

```
oldest_reader_txn_id            which reader is holding the freelist back
pages_pending_reclaim_reader    growth caused by long-running queries
pages_pending_reclaim_retention growth caused by time travel
free_pages_reusable             reclaimable right now
page_count / live_readers / snapshots / fragments / txn_id
durability                      what a commit currently promises
last_commit                     pages by class: data, roots, catalog, freelist, snapshots
io                              reads, writes, write_bytes, grows, truncates, syncs, sync_nanos
```

Three of these exist for reasons worth stating:

**Reader-blocked and retention-blocked pages are reported separately** because merged they would
not tell you which knob to turn. What snapshots block is computed exactly; whatever remains is
attributed to readers.

**`last_commit` is broken down by class** because "this commit wrote 41 pages" is not something
anyone can act on. The b-tree paths and the fixed chains call for opposite fixes, and which
dominates is a question about the workload rather than about the engine. §10 is that breakdown
turned into a test.

**`io` exists because every other metric is a gauge over the file** — how many pages there are,
how many are pinned, who is holding them. None of it says how hard the disk is being worked to
keep that shape, and the two come apart in exactly the case that matters: a database whose page
count is flat while its write rate is enormous is one rewriting the same pages over and over.

Read counts are striped 64 ways; nothing else is. Writes, grows, truncates and flushes all happen
under the write lock, so one atomic each is uncontended by construction. Reads are the opposite:
every reader thread takes one per page, and a bit-sliced scan takes one per page *per plane*, on
a path measured at 640 ns. Nothing on the read path is timed — `Instant::now` twice per read
would be paying a measurable slowdown for the privilege of measuring it — but flushes are, being
rare, two per commit, and milliseconds each. That is also the only place the two flush
*strengths* are visible at all: `Full` and `Barrier` issue the same count and differ in what they
wait for.

---

## 12. Limits and non-goals

| Limit | Why it is not a bug |
|---|---|
| **One process per file** | `flock(LOCK_EX)` is a soundness requirement of the mapping (§5.1), not a convenience. Multi-process access requires a buffer-pool backend; the `Pager` GAT is what keeps that possible. |
| **One writer at a time** | Copy-on-write over a single meta page. Concurrent writers would need either a merge or a lock manager, and neither is free. |
| **64-bit only** | A 1 TiB reservation is close to free on 64-bit and impossible on 32-bit. |
| **No `O_DIRECT`** | The design depends on `MAP_SHARED` and `pwrite` sharing a unified buffer cache. |
| **SIGBUS on media failure** | Unturnable into a `Result`; made survivable by copy-on-write (§5.2). |
| **Write amplification** | The price of having no WAL. Measured, bounded, and asserted (§10). |
| **Root records scale with fragment count** | Real, known, and the fix is a tree (§10). |
| **Snapshots do not pin the catalog** | Stated in §9. Safe for additive schemas only. |
| **Growth beyond `mapsize`** | A hard error, never a silent remap. Reopen with a larger `mapsize`. |

Errors are total and named for what an operator should do about them: `Locked`, `NoValidMeta`,
`NotADatabase { bytes }`, `MapSizeExhausted { need, mapsize }`, `OutOfBounds { pgno, page_count }`,
`ChainCycle { root }`, `SnapshotNotFound`, `UnallocatedPage`, `ReadersActive`, `Unsupported`.
Nothing in `big-page` beneath this crate may panic on malformed input, and a fuzz target asserts it.

---

## 13. Evaluation

Two kinds of measurement, answering different questions.

**Counted** — `tests/amplification.rs`, via `CountingPager`. How much work, on every machine,
forever. A regression in pages-per-commit fails CI rather than showing up as a slower bar.

**Timed** — `benches/write.rs`, `benches/read.rs`, criterion. How fast on this machine today.
Useful against a saved baseline, meaningless as an absolute number quoted anywhere else.

```sh
cargo test  -p big-pager                                        # 71 tests
cargo test  -p big-pager --test amplification -- --nocapture    # the counted half
cargo bench -p big-pager -- --save-baseline main                # then --baseline main
```

| Bench group | Question |
|---|---|
| `b1_empty_commit` | What does a commit cost when the transaction changed nothing? |
| `b2_dirty_pages` | How much of a commit scales with pages actually dirtied? |
| `b3_commit_vs_fragments` | Does a one-fragment commit cost more as the database grows? |
| `b4_cow` | What does holding a large dirty set in memory cost? |
| `b5_alloc` | Freelist reuse against extending the file. |
| `b6_read` | Sequential, scattered, and a mapping not yet faulted in. |
| `b8_readers` | What the reader registry costs across several threads. |

Every group that can run on both pagers does. `MemPager` makes `sync` a no-op, so the gap between
a `mem` line and its `mmap` twin is the price of durability and nothing else — on macOS, two
`F_FULLFSYNC` calls per commit, which dominates everything.

Two things the harness cannot do honestly, so it does not pretend to: **cold reads from the
device** (`mmap_fresh_map` opens a mapping this process never touched, but the file is still in
the OS cache, and purging that needs root) and **a quiet machine** (thermal state and background
indexing move these numbers more than most code changes do).

### 13.1 Test suite

71 tests, all passing. (A 72nd, `crash_child`, is `#[ignore]`d and is not a test: it is the
body a crashing child process runs, launched by the four in `tests/crash.rs`.) The ones worth
knowing about:

| Test | What it protects |
|---|---|
| `corrupt_newer_meta_falls_back_to_older` | The whole of crash recovery (§4.2) |
| `crash_between_data_fsync_and_meta_write_keeps_the_old_tree` | Atomicity is the meta write (§6.3) |
| `repeated_commits_do_not_grow_the_file_without_bound` | The freelist fixed point (§7.3) — not visible by reading the code |
| `commit_cost_grows_with_the_database_not_the_change` | The cost model (§10) |
| `a_memo_never_outlives_the_bytes_it_was_about` | The verified-page memo (§5.4) |
| `a_reader_held_across_concurrent_writers_never_sees_a_recycled_page` | The horizon (§7.2) |
| `no_level_flushes_an_odd_number_of_times` | Durability cannot break atomicity (§8) |
| `second_handle_cannot_open_the_same_file`, `page_inside_mapping_but_past_eof_is_an_error`, `exhausting_mapsize_is_an_error_not_a_remap` | mmap constraints 1, 2, 4 (§5.1) |
| `a_file_too_short_to_be_a_database_is_refused_rather_than_overwritten` | §4.1 |
| `backend_is_swappable` | The `Pager` abstraction is real |

---

## 14. Related work

**Shadow paging.** Lorie, "Physical Integrity in a Large Segmented Database" (TODS, 1977) is the
original; Rodeh, "B-trees, Shadowing, and Clones" (TOS, 2008) is the modern treatment and the
foundation of btrfs. `big-pager` is a straightforward member of this family, distinguished mainly
by having only four fixed metadata chains to shadow rather than an arbitrary tree of them.

**Write-ahead logging.** Mohan et al., "ARIES" (TODS, 1992). §2 states why it was not chosen.

**Memory-mapped storage engines.** LMDB (Chu, "MDB: A Memory-Mapped Database and Backend for
OpenLDAP", 2011) is the closest relative in spirit: copy-on-write b-tree, two meta pages,
single-writer, mmap reads. `big-pager` differs in three ways that matter — writes go through
`pwrite` rather than a writable mapping, snapshots are explicit registry entries rather than
long-lived read transactions, and the freelist is run-length encoded because copy-on-write over
bitmap pages frees long contiguous ranges. BoltDB is LMDB's Go descendant and shares the shape.

**The case against mmap.** Crotty, Leis and Pavlo, "Are You Sure You Want to Use MMAP in Your
DBMS?" (CIDR 2022). §5.1 and §5.2 answer it point by point: two of the four objections do not
arise under constraint 3, and two are conceded explicitly.

**Roaring bitmaps.** Chambi et al., "Better bitmap performance with Roaring bitmaps"
(arXiv:1402.6407); Lemire et al., "Consistently faster and smaller compressed bitmaps with
Roaring" (arXiv:1603.06549) and "Roaring Bitmaps: Implementation of an Optimized Software
Library" (SPE, 2018). These describe what `big-container` stores; this crate is what puts it on
disk.

**Bitmap indexes.** O'Neil, "Model 204 Architecture and Performance" (1987); O'Neil and Quass,
"Improved Query Performance with Variant Indexes" (SIGMOD, 1997) for bit-sliced arithmetic;
Wu, Otoo and Shoshani, "Optimizing bitmap indices with efficient compression" (TODS, 2006) for
WAH and FastBit.

**The nearest system.** FeatureBase (formerly Pilosa) is the only other engine the authors know of
that is bitmap-native rather than bitmap-indexed, and its RBF storage layer is the same shape as
`big-page` + `big-btree` + `big-container` + `big-pager` together: a b-tree of roaring containers
over 8 KiB pages. It is documented in product documentation and in US patents 11,886,411 and
9,489,410 rather than in a peer-reviewed paper. The differences here are the ones this document
is about: no WAL, `pwrite`-only writes under a read-only mapping, an explicit snapshot registry,
and a durability knob that cannot compromise atomicity.

---

## Appendix A. Not in this crate

Container set operations (`big-container`), the b-tree itself (`big-btree`), fragments and
bit-sliced planes (`big-engine`), planning and execution (`big-plan`, `big-exec`), SQL
(`big-sql`). `trait Pager` is enough for `big-btree` to be tested against `MemPager` with no file
involved.

## Appendix B. Glossary

| Term | Meaning |
|---|---|
| **Page** | 8 KiB, the unit of everything. A dense roaring container is exactly one. |
| **Pgno** | A page number, `u32`. |
| **Chain** | A flat singly-linked list of pages holding fixed-stride entries. |
| **Fragment** | One `(table, field, view, shard)`, keyed by a 20-byte `FragmentKey`. |
| **Root record** | `FragmentKey → root pgno`, 24 bytes. |
| **Horizon** | The oldest `txn_id` anything can still read. The freelist may not reclaim past it. |
| **Scratch page** | A page a transaction both allocated and freed; reusable immediately. |
| **Memo** | The remembered result of a dense bitmap page's checksum verification. |
