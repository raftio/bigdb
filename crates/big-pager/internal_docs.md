# `big-pager` — internal component map

How this crate is put together, and which piece is allowed to know about which. The *why* of
each design decision lives in [`readme.md`](readme.md); this file is the wiring diagram you want
open next to it when you are changing something.

Everything below is drawn from `src/`. Nothing here is aspirational.

---

## 1. Where the crate sits

`big-pager` owns the part of the format that describes the file itself — meta page, root
records, snapshot registry, freelist, page allocation, and the commit sequence. It depends on
exactly one internal crate.

```mermaid
flowchart TD
    subgraph above["Callers"]
        BTREE["big-btree"]
        ENGINE["big-engine"]
        DB["big-db"]
        EXEC["big-exec"]
        EMBED["big-embed"]
    end

    PAGER["<b>big-pager</b><br/>Store, transactions, freelist,<br/>the file itself"]

    PAGE["big-page<br/>Page, MetaPage, ChainPage,<br/>PAGE_SIZE, checksums"]

    subgraph os["Platform, cfg unix"]
        LIBC["libc<br/>flock, F_FULLFSYNC"]
        MMAP2["memmap2"]
    end

    BTREE --> PAGER
    ENGINE --> PAGER
    DB --> PAGER
    EXEC --> PAGER
    EMBED --> PAGER
    PAGER --> PAGE
    PAGER --> LIBC
    PAGER --> MMAP2

    classDef internal fill:#b2f2bb,stroke:#1e1e1e,color:#1e1e1e
    classDef external fill:#a5d8ff,stroke:#1e1e1e,color:#1e1e1e
    class PAGER,PAGE,BTREE,ENGINE,DB,EXEC,EMBED internal
    class LIBC,MMAP2 external
```

**The direction is one-way and load-bearing.** Nothing above this crate sees a file, a mapping,
or an `unsafe` block — `lib.rs` carries `#![deny(unsafe_code)]`, and `mmap.rs` is the single
module that lifts it.

---

## 2. Module map

```mermaid
flowchart TB
    LIB["<b>lib.rs — Store&lt;P&gt;</b><br/>load · init · open_or_init<br/>commit_txn · write_and_flip<br/>truncate_tail · metrics"]

    TXN["<b>txn.rs</b><br/>ReadTxn · WriteTxn<br/>TxnPage · DirtyChains"]

    subgraph chainstate["Chain state"]
        direction LR
        ROOTS["roots.rs<br/>RootRecords"]
        SNAP["snapshot.rs<br/>SnapshotRegistry"]
        FREE["freelist.rs<br/>Freelist · FreeRun"]
        CHAIN["chainio.rs<br/>load_chain<br/>chain_pgnos"]
    end

    PAGERT["<b>pager.rs</b><br/>trait Pager · trait PagerMut"]

    subgraph backends["Backends"]
        direction LR
        MMAP["mmap.rs<br/>MmapPager<br/><i>the only unsafe</i>"]
        MEM["mem.rs<br/>MemPager"]
        COUNT["counting.rs<br/>CountingPager&lt;P&gt;<br/><i>feature-gated</i>"]
    end

    subgraph plumbing["Plumbing"]
        direction LR
        DUR["durability.rs<br/>Durability"]
        ERR["error.rs<br/>StoreError"]
        MET["metrics.rs<br/>Metrics<br/>CommitBreakdown"]
        IO["io.rs<br/>IoCounters<br/>IoStats"]
    end

    LIB --> TXN
    LIB --> chainstate
    TXN --> chainstate
    LIB --> PAGERT
    TXN --> PAGERT
    CHAIN --> PAGERT
    PAGERT --> backends
    MMAP --> IO
    LIB --> plumbing

    classDef internal fill:#b2f2bb,stroke:#1e1e1e,color:#1e1e1e
    class LIB,TXN,ROOTS,SNAP,FREE,CHAIN,PAGERT,MMAP,MEM,COUNT,DUR,ERR,MET,IO internal
```

| Module | Owns | Talks to |
|---|---|---|
| `lib.rs` | `Store<P>`, the commit sequence, page allocation, recovery | everything |
| `pager.rs` | `Pager` / `PagerMut` — the only storage vocabulary | nothing (traits only) |
| `mmap.rs` | `MmapPager`: mmap read path, `pwrite` write path, `flock`, verified-page memo | `io.rs`, `libc`, `memmap2` |
| `mem.rs` | `MemPager`: in-RAM backend for unit tests | — |
| `counting.rs` | `CountingPager<P>`: call-tallying decorator, test-only | any `P` |
| `txn.rs` | `ReadTxn`, `WriteTxn`, `TxnPage`, `DirtyChains` | `Store`, the three state types |
| `roots.rs` | `RootRecords`: `FragmentKey → Pgno`, a `BTreeMap` | — |
| `snapshot.rs` | `SnapshotRegistry`, `Snapshot` | — |
| `freelist.rs` | `Freelist`, `FreeRun`: run-length free space | — |
| `chainio.rs` | Reading a chain off disk, cycle-guarded | `Pager` |
| `durability.rs` | `Durability`: `Full` / `Barrier` / `None` | — |
| `metrics.rs` | `Metrics`, `CommitBreakdown` | `io.rs`, `durability.rs` |
| `io.rs` | `IoCounters` (striped), `IoStats` | — |
| `error.rs` | `StoreError`, `Result<T>` | `big_page::PageError` |

---

## 3. The storage boundary

Two traits, split so a read-only replica can implement half of it.

```mermaid
classDiagram
    class Pager {
        <<trait>>
        +type Ref, derefs to Page
        +read(pgno) Result of Ref
        +page_count() u64
        +capacity() Option of u64
        +verify_bitmap(pgno, page, expected) bool
        +io_stats() Option of IoStats
    }
    class PagerMut {
        <<trait>>
        +write(pgno, page) Result
        +grow(page_count) Result
        +truncate(page_count) Result
        +sync() Result
        +sync_data() Result
    }
    class MmapPager {
        Ref is a bare reference into the mapping
        file, map, file_pages AtomicU64
        verified, 64 sharded memos
        io, IoCounters
    }
    class MemPager {
        Ref is MemRef, a read guard
        pages, RwLock over Vec of Page
    }
    class CountingPager {
        Ref is the inner pager's
        inner P, plus five atomics
    }
    class WriteTxn {
        Ref is TxnPage
        dirty pages first, then the file
    }

    PagerMut --|> Pager : supertrait
    Pager <|.. MmapPager
    Pager <|.. MemPager
    Pager <|.. CountingPager
    Pager <|.. WriteTxn
    PagerMut <|.. MmapPager
    PagerMut <|.. MemPager
    PagerMut <|.. CountingPager
```

Three properties of this shape are relied on elsewhere:

- **`Ref<'a>` is a GAT**, so a backend may return a guard rather than a bare reference. `MemPager`
  returns a `RwLockReadGuard` wrapper; a future buffer pool would return a pinned-page guard.
- **The write side takes `&self`.** Writer exclusivity is an invariant of `Store`, not of the
  backend — with `&mut self` a reader and a writer could not coexist.
- **`WriteTxn` is itself a `Pager`**, which is what lets `big-btree` descend through pages the
  transaction has not committed yet using the same code it uses against a committed tree.

---

## 4. `Store<P>` — what it owns and what guards what

```mermaid
flowchart TB
    subgraph store["Store&lt;P: Pager&gt;"]
        direction TB
        P["pager: P"]
        WL["write_lock: Mutex&lt;()&gt;<br/><i>one writer at a time</i>"]
        ST["state: RwLock&lt;StoreState&gt;"]
        RD["readers: Mutex&lt;BTreeMap&lt;TxnId, u32&gt;&gt;<br/><i>refcount per txn_id</i>"]
        DU["durability: AtomicU8"]
        LC["last_commit: Mutex&lt;CommitBreakdown&gt;"]
    end

    subgraph inner["StoreState"]
        M["meta: MetaPage"]
        R["roots: Arc&lt;RootRecords&gt;"]
        S["snapshots: Arc&lt;SnapshotRegistry&gt;"]
        C["catalog: Arc&lt;Vec&lt;Vec&lt;u8&gt;&gt;&gt;"]
        F["freelist: Freelist"]
    end

    ST --> inner

    classDef internal fill:#b2f2bb,stroke:#1e1e1e,color:#1e1e1e
    classDef ext fill:#a5d8ff,stroke:#1e1e1e,color:#1e1e1e
    class P,WL,ST,RD,DU,LC internal
    class M,R,S,C,F ext
```

### 4.1 Lock hierarchy

Taken outermost-first, everywhere in the crate. There is no path that takes them in any other
order, which is what keeps the thing deadlock-free.

```
write_lock  ─┬─→  state (read or write)  ─┬─→  readers
             │                            └─→  last_commit
             └─→  readers
```

| Lock | Held by | For how long |
|---|---|---|
| `write_lock` | `WriteTxn` (its `MutexGuard` is a field), `set_durability`, `truncate_tail` | the whole transaction |
| `state` | every reader of `meta` / `roots` / `catalog`; `commit_txn`'s final publish | one statement |
| `readers` | `register_reader`, `release_reader`, `oldest_reader` | one statement |
| `last_commit` | written once per commit, read by `metrics()` | one statement |

`durability` is an `AtomicU8` rather than a field of `StoreState` so that reporting metrics never
has to wait on a lock. Poisoning is deliberately ignored on `write_lock` (`Err(p) => p.into_inner()`):
a panicked writer left nothing on disk, so there is no corrupt state to protect.

### 4.2 Why `Arc` on three of the five fields

`roots`, `snapshots` and `catalog` are handed out to readers by `Arc::clone` under the state
lock. A `ReadTxn` therefore keeps the exact snapshot it opened with, even as commits replace
what `StoreState` points at. `catalog` is captured *with* the roots for a specific reason: read
in two steps, a reader can land between a commit's two publishes and pair fresh roots with a
stale schema — which shows up as a just-written fragment being invisible rather than as an error.

---

## 5. Transactions

```mermaid
flowchart LR
    RT["<b>ReadTxn&lt;'db, P&gt;</b><br/>no lock, waits for nobody<br/><br/>txn_id<br/>Arc&lt;RootRecords&gt;<br/>Arc&lt;catalog&gt;"]

    STORE["<b>Store&lt;P&gt;</b>"]

    WT["<b>WriteTxn&lt;'db, P&gt;</b><br/>holds write_lock<br/><br/>base: MetaPage · txn_id = base + 1<br/>horizon · tail_floor · next_pgno<br/>dirty: BTreeMap&lt;Pgno, Page&gt;<br/>scratch: Vec&lt;Pgno&gt;<br/>freelist · roots · snapshots · catalog<br/>dirty_chains: DirtyChains"]

    STORE -->|"begin_read / begin_read_at"| RT
    RT -->|"Drop → release_reader"| STORE
    STORE -->|begin_write| WT
    WT -->|"commit → commit_txn"| STORE
    WT -->|"Drop → nothing at all"| STORE

    classDef internal fill:#b2f2bb,stroke:#1e1e1e,color:#1e1e1e
    class STORE,RT,WT internal
```

| | `ReadTxn` | `WriteTxn` |
|---|---|---|
| Lock | none | `write_lock` for its whole life |
| Registers | yes, `readers[txn_id] += 1` | no |
| Sees | its `Arc` snapshot of roots + catalog | its own dirty pages, then the file |
| Rollback | n/a | doing nothing — no page reached the disk |
| Concurrency | many | exactly one |

**`DirtyChains` is the cost control.** A commit rewrites a chain *whole*, so a chain nobody
touched would cost its entire length in pages for nothing. `dirty_chains` records which of
`roots` / `catalog` / `snapshots` actually changed; the freelist is absent from the struct
because every commit changes it by definition — it records the pages that commit is recycling.

---

## 6. The commit sequence

```mermaid
sequenceDiagram
    participant W as WriteTxn
    participant S as Store::commit_txn
    participant F as Freelist
    participant C as chainio / build_chain
    participant P as PagerMut

    W->>S: commit() — moves dirty, freelist, roots, snapshots, catalog
    S->>S: horizon = min(oldest_reader, oldest_snapshot, base.txn_id)

    rect rgb(178, 242, 187)
    note over S,C: 1–2 · changed chains only
    S->>C: chain_pgnos(base.roots) → recycle into freelist
    S->>F: alloc_pages(roots)
    S->>C: chain_pgnos(base.snapshots) → recycle
    S->>F: alloc_pages(snapshots)
    S->>C: chain_pgnos(base.catalog) → recycle
    S->>F: alloc_pages(catalog)
    end

    rect rgb(165, 216, 255)
    note over S,F: 3 · freelist last: allocating for it mutates it
    S->>C: chain_pgnos(base.freelist) → recycle
    S->>F: alloc_freelist_pages — runs to a fixed point
    end

    S->>C: 4 · build_chain for each Fresh chain, tally by class
    note over S,C: nothing has touched the disk yet

    rect rgb(255, 236, 153)
    note over S,P: 5 · write_and_flip: the only part a crash can interrupt
    S->>P: grow(next_pgno)
    S->>P: write(page) × N
    S->>P: flush — sync or sync_data or nothing
    S->>P: write(meta slot txn_id % 2)
    S->>P: flush
    end

    S->>S: publish under state.write(): meta, roots, snapshots, catalog, freelist
    S->>S: last_commit = tally
    S-->>W: Ok(txn_id)
```

**Atomicity lives entirely in the meta write.** Until the meta page lands, every byte written
above it is unreachable garbage and a crash costs nothing. After it lands, all of it is live.
There is no state in between — which is the whole reason there is no WAL and nothing to replay.

The two flushes move together at every `Durability` level. A level that skipped the first but
kept the second would let the meta page reach the disk while the pages it names had not, and
that is not lost data, it is a file that does not open.

---

## 7. Page lifecycle

The one state machine every other piece of this crate is arranged around.

```mermaid
stateDiagram-v2
    [*] --> Tail: alloc_tail, file grows
    [*] --> Reused: Freelist.alloc(horizon)
    [*] --> Scratch: scratch.pop()

    Tail --> Dirty: written into the txn
    Reused --> Dirty
    Scratch --> Dirty

    Dirty --> Scratch: free() and pgno >= tail_floor
    Dirty --> Live: commit writes it, meta names it

    Live --> Pending: cow or free, push(pgno, txn_id)
    Pending --> Reusable: horizon advances past freed_at
    Reusable --> Reused: a later txn allocates it
    Reusable --> [*]: trim_tail returns it to the filesystem

    note right of Pending
        A reader or snapshot can still
        see the txn that replaced it
    end note
    note right of Scratch
        Allocated and freed in one txn.
        Never named by a committed meta,
        so no reader can hold it
    end note
```

`horizon = min(oldest_reader_txn_id, oldest_snapshot_txn_id, base.txn_id)` — recomputed at
`begin_write` and again inside `commit_txn`. A run with `freed_at <= horizon` is reusable; every
other run stays exactly where it is, however dead it looks.

---

## 8. The four chains

A *chain* is a flat singly-linked list of pages holding fixed-stride entries. The meta page
carries the head page number of each.

```mermaid
flowchart LR
    META["MetaPage<br/>slot = txn_id % 2"]

    META -->|root_records| R1["chain page"] --> R2["chain page"] --> RN["…"]
    META -->|catalog| C1["chain page"] --> CN["…"]
    META -->|snapshots| S1["chain page"] --> SN["…"]
    META -->|freelist| F1["chain page"] --> FN["…"]

    classDef internal fill:#b2f2bb,stroke:#1e1e1e,color:#1e1e1e
    classDef ext fill:#a5d8ff,stroke:#1e1e1e,color:#1e1e1e
    class META ext
    class R1,R2,RN,C1,CN,S1,SN,F1,FN internal
```

| Chain | Stride | In-memory type | Rewritten |
|---|---|---|---|
| Root records | `ROOT_RECORD_BYTES` (24 B) | `RootRecords` (`BTreeMap`) | whenever any fragment root moves |
| Catalog | `CATALOG_ENTRY_BYTES` (128 B) | `Arc<Vec<Vec<u8>>>`, opaque here | only when its bytes actually changed |
| Snapshots | `SNAPSHOT_ENTRY_BYTES` (64 B) | `SnapshotRegistry` | when a snapshot is taken or dropped |
| Freelist | `FREE_ENTRY_BYTES` (16 B) | `Freelist` | **every commit, without exception** |

Two readers of a chain, and they are not interchangeable:

- **`load_chain`** — used at open and by `begin_read_at`. Verifies the checksum of every page,
  parses entries, follows `next`. Cycle-guarded by a hop count bounded by `page_count`.
- **`chain_pgnos`** — used by `recycle_chain` and `truncate_tail`. Returns only the page numbers,
  so the recycled pages can be handed back to the freelist. Same cycle guard, **no checksum
  verification** — see the review note; this is the asymmetry to be aware of when touching it.

---

## 9. Observability

```mermaid
flowchart LR
    subgraph backend["Backend counts its own I/O"]
        IOC["IoCounters<br/>reads: 64 stripes<br/>writes · grows · truncates<br/>syncs · sync_nanos"]
    end

    subgraph gauges["Store gauges the file"]
        FL["Freelist<br/>pending / reusable"]
        RDRS["readers map"]
        SNAPS["SnapshotRegistry"]
        TALLY["CommitBreakdown<br/>data · roots · catalog<br/>freelist · snapshots"]
    end

    IOC -->|io_stats| M["Metrics"]
    FL --> M
    RDRS --> M
    SNAPS --> M
    TALLY --> M

    classDef internal fill:#b2f2bb,stroke:#1e1e1e,color:#1e1e1e
    class IOC,FL,RDRS,SNAPS,TALLY,M internal
```

The split matters. Everything except `io` is a **gauge over the file** — how many pages there
are, how many are pinned, which reader is holding them. None of it says how hard the disk is
being worked to keep that shape, and the two come apart in exactly the case that matters: a
database whose page count is flat while its write rate is enormous is one rewriting the same
pages over and over.

`IoCounters` is asked of the *backend*, never measured above it, because a call into `read` is
not an I/O and how much of one it is differs per backend by more than a constant. `MmapPager::read`
returns a pointer into the mapping; whether that touches the disk is a page fault this process is
never told about. `io_stats()` returning `None` — the honest answer for a pager with no disk under
it — tells an exporter to omit the series rather than publish zeroes that read as an idle database.

Reads are striped 64 ways for the same reason the verified-page memo is: every reader thread takes
one per page, and a bit-sliced scan takes one per page *per plane*, so a single counter would be a
cache line every core wants to own on a 640 ns path.

---

## 10. Test-only components

Both are feature-gated so they cannot reach a release build.

| Feature | Component | Used by |
|---|---|---|
| `crash-injection` | `crash_point(name)` — aborts when `BIG_CRASH_AT` matches. Failpoints: `after_data_sync`, `after_meta_write` | `tests/crash.rs` |
| `counting-pager` | `CountingPager<P>` — tallies calls into the trait | `tests/amplification.rs`, `tests/io_metrics.rs`, and `big-db` |

`CountingPager` lives in this crate rather than beside the comparison benchmarks because the
engine's own regression tests are its main users; moving it out would mean those tests depending
on a crate that depends on them.

---

## 11. Error surface

`StoreError` is flat, and every variant answers a different operator question.

```mermaid
flowchart TB
    E["StoreError"]
    E --> W1["Wrapped<br/>Io · Page(PageError)"]
    E --> W2["Bounds<br/>OutOfBounds · MapSizeExhausted · UnallocatedPage"]
    E --> W3["Open-time<br/>Locked · NoValidMeta · NotADatabase"]
    E --> W4["Integrity<br/>ChainCycle"]
    E --> W5["Operational<br/>SnapshotNotFound · ReadersActive · Unsupported"]

    classDef internal fill:#b2f2bb,stroke:#1e1e1e,color:#1e1e1e
    class E,W1,W2,W3,W4,W5 internal
```

`code()` gives every variant a stable string for logs and HTTP status mapping, and `Page`
delegates rather than flattening: a checksum mismatch reaching an operator through the store is
still a checksum mismatch, and wrapping it in a `storage` code would hide the one detail that
decides what to do about it.

**`NoValidMeta` and `NotADatabase` are deliberately different variants.** An empty path is a
database nobody has created yet; a file with bytes in it is somebody else's, and initialising it
would destroy it. Conflating them once meant `big verify` pointed at the wrong path returned zero
and left a fresh empty database where that file had been.
