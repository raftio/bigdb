# Performance: point reads and sparse write amplification

Written against the VPS run of 2026-08-28 ([`bench/results/REPORT.md`](../bench/results/REPORT.md)),
then **rewritten against measurement**, because the first version of this document diagnosed both
problems wrongly and a profiler said so within the hour.

| | before | after | status |
|---|---|---|---|
| **P1** single-thread point read | 12.8µs (59k/s) | **1.23µs**, 10.4× | fixed, tests green |
| **P2** sparse/64 bytes written | 447.2 MiB for 9.7 MiB stored | unchanged | **diagnosed, not fixed** |

Both first diagnoses were plausible, cheap to write, and wrong. They are kept below rather than
deleted: what is worth carrying forward is not the conclusions but why guessing produced two of
them and measuring produced neither.

---

## P1 — the single-threaded point read

### What it was not

> *First diagnosis, wrong:* "`Bsi::get` asks for 22 rows; the addressing puts them at a stride of
> 16 container keys, so they span 337 keys and `find_many` re-descends from the root for each one.
> A 21-bit read costs up to 22 full descents."

The arithmetic was right and the conclusion did not follow. The benchmark reads 20,000 dense
records, so every row of the fragment lives in **one** container: 21 cells, one leaf page, depth
one. `find_many` was already answering all 21 keys from a single descent. Descents were never the
cost.

The tell was available without reading any code: the cost barely moved between a 20,000-record
corpus and a 200,000-record one (12,994ns against 13,927ns). A tree-descent problem scales with
the tree.

### What it was

Cost per plane, holding the corpus fixed and varying only the field's `bit_depth`:

| bit_depth | planes | ns | ns/plane |
|---|---|---|---|
| 1 | 2 | 821 | — |
| 8 | 9 | 5,341 | 635 |
| 16 | 17 | 10,243 | 509 |
| 20 | 21 | 12,843 | 650 |
| 24 | 25 | 13,116 | 68 |
| 32 | 33 | 13,075 | −5 |

Linear in the number of planes that actually hold data, flat once the planes run past the corpus's
20 bits and no longer have containers. Fixed overhead — name resolution, catalog lookup — was
77ns, and `db.read()` 452ns. Neither mattered.

So the cost was ~640ns per plane. And:

```
bitmap_page_checksum over one 8 KiB page      606 ns
```

`LeafCell::verify_base` ran `crc32fast::hash` over the **entire 8 KiB page** on every probe of
every plane. A dense container lives on a page of its own, a bit-sliced read touches one such page
per bit, and each touch re-checksummed 8,192 bytes to extract one. 21 × 606ns = 12.7µs, against a
12.8µs measured read. **The checksum was not part of the read; it was the read.**

### The fix

The check is the one parent-to-child integrity link in the tree and deleting it was not on the
table. Instead it is now **remembered**, which is sound for a reason the engine already
guarantees: under copy-on-write a page's bytes never change once written, because modifying a page
allocates a *new* page number. A verification can therefore only go stale when a number is
recycled and written again — and that is one place, `write`.

- `Pager::verify_bitmap(pgno, page, expected)` — new trait method. The default recomputes the CRC
  every time, so any backend that ignores it stays exactly as correct and exactly as slow as
  before.
- `MmapPager` overrides it with a memo of page number → verified checksum, striped 64 ways because
  point reads hit it once per plane from every reader at once. `write` forgets the entry before
  touching the bytes; `truncate` drops entries for pages that no longer exist.
- `CountingPager` forwards rather than inheriting the default — it wraps the real pager in the
  benchmarks, and silently recomputing there would have hidden the memo from the only thing that
  measures it.
- `WriteTxn` refuses the memo for a page it is still building: a number this transaction allocated
  may carry a memo from whatever occupied it before.
- `big-page` grew `contains_checked` / `resolve_checked` / `bitmap_checked` — the same work with
  the verification lifted out for the caller to do — and `LeafCell::checksum_error` so a caller
  whose memo said no can still report *why*.

Every page is still verified before its first use. Only the repetition is gone.

**The scan path got it too.** `FragmentRead::for_each`, which is what `count_ge` walks, was paying
the same CRC per container. That was not the target and is fixed by the same change.

### Guards

- `crates/big-pager/tests/verified_memo.rs` — new. A wrong checksum never passes; a memo never
  outlives the bytes it described (page recycled with different contents must not verify against
  the old checksum); nothing survives reopening the file; and the memoising implementation agrees
  with the recomputing default on every input, so the change is cost and not behaviour.
- Full suite: **384 passed, 0 failed**, including the existing
  `crates/big-page/tests/leaf.rs` corruption tests, which still go through the verifying
  `LeafCell::bitmap` and still reject a rotten page.
- Byte columns unmoved, as a read-path change requires: `crates/big-db/tests/amplification.rs`
  passes untouched.

---

## P2 — sparse write amplification

**Not fixed.** What follows is the diagnosis, the failed attempt, and why the remaining options
are larger than a patch.

### What it is not

> *First diagnosis, wrong:* "Three chains are rewritten whole per commit — freelist, roots,
> catalog — and they scale with how many fragments the database *has*. At sparse/512 batch 1, 17
> of 24 pages per commit are catalog. Coarsen the zone map so ordinary writes stop dirtying it."

This came from the prose under the report's page-class table. **That table's own `dominant` column
says "b-tree paths" in six rows of nine.** Totals over a 10,000-record ingest, page counts,
deterministic:

| layout | batch | catalog | roots | data | free | data share |
|---|---|---|---|---|---|---|
| sparse/64 | 1 | 2,071 | 10,000 | 50,093 | 10,000 | 69% |
| sparse/64 | 100 | 279 | 100 | 31,524 | 100 | **98.5%** |
| sparse/64 | 1000 | 30 | 10 | 2,714 | 10 | **98.2%** |
| sparse/512 | 1 | 46,743 | 38,980 | 64,735 | 10,000 | 40% |
| sparse/512 | 100 | 1,665 | 391 | 63,221 | 100 | **96.7%** |
| sparse/512 | 1000 | 170 | 40 | 26,336 | 15 | **99.2%** |

The fixed chains dominate in exactly one shape: one record per commit, at a high shard count. The
figures the report headlines — 26,299 bytes/record at batch 100, 2,272 at batch 1000 — come from
rows where the three chains together are **one to three per cent** of the bytes.

The zone-map coarsening was implemented anyway, to see. Measured: **identical page counts, to the
page**, and 0.2% on bytes/record. It was reverted — it trades away `count_ge` pruning precision,
which is the column this engine exists to win, and bought nothing.

### What it is

`data`: one root-to-leaf copy-on-write path per fragment a commit touches, paid whether that
fragment received one record or a thousand. A shard is its own fragment and a fragment is its own
b-tree, so a commit spanning 64 shards rewrites 64 independent paths.

The arithmetic closes: 64 shards × ~5 pages per path × 10 commits ≈ 2,714 pages, which is the
sparse/64 batch-1000 row exactly.

### Options, and why none is a patch

1. **Touch fewer fragments per commit.** Already built, already measured: `Db::ingest(capacity)`
   recovers 240× and `Db::bulk_load` 19×. This is the answer that exists today, and for a caller
   who can use it the problem is largely solved. It does not make a commit cheaper; it makes
   commits rarer.

2. **Make a fragment's path rewrite cheaper.** The path is root → branch → leaf plus the pages
   dense containers own. There is not much fat: copy-on-write has no unit smaller than a page.

3. **Stop one commit meaning N trees.** One b-tree per field with the shard as a key prefix, so a
   commit rewrites one path rather than 64. This is the fix that would actually move the number,
   and it changes the addressing in `crates/big-engine/src/coords.rs` — which that file
   documents as wire format, "part of the wire format: every peer exchanging data must agree". A
   version break, with the process in [`docs/versioning.md`](versioning.md). Not a performance
   patch.

4. **Make the fixed chains incremental** — the obvious reading of the old diagnosis. Worth one to
   three per cent at the batch sizes that matter, and it needs a page-format change of its own,
   because the chains are singly linked: rewriting page *k* under copy-on-write gives it a new
   number, so every page before it has to be rewritten to point at it. Poor value, and it is
   listed only so the next person does not rediscover it as an idea.

**Recommendation: nothing here should be built without deciding on (3) first.** (1) is shipped,
(2) is thin, and (4) is a format change that buys almost nothing. (3) is a format change that buys
the whole finding, and that is a design decision rather than an optimisation.

---

## What generalises

Two wrong diagnoses in one document, both written from reading code and prose, both refuted by an
hour with a profiler. Neither needed a subtle experiment — P1 fell to timing a corpus at two sizes
and a field at nine bit depths, P2 to printing the numbers the report already collected.

The report's own Finding 6 records an earlier instance of the same failure: prose asserting
"batching does not help" beside data showing a 17,800× drop. The fix applied there — deriving the
verdict from the measurements so it cannot drift — is the one that generalises. A conclusion a
program computes from its own numbers stays true. A conclusion typed next to them does not.

Neither profiler is committed — both were a few dozen lines against the public API. P1 needed one
loop timing `get_int` over a corpus at two sizes and a field at nine bit depths, beside a timing
of `bitmap_page_checksum` on its own. P2 needed one loop summing `Metrics::last_commit` per class
across an ingest, which is deterministic and takes seconds. Anyone re-opening either question
should write them again rather than trust this page.
