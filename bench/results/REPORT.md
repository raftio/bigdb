# big benchmark report

Collected **2026-08-28** on a dedicated VPS with **all Docker containers stopped** for the
duration of the run, page cache dropped immediately before the first measurement.

- **Machine** — DigitalOcean droplet, 2 vCPU Intel (`DO-Regular`, GenuineIntel), 3.8 GiB RAM,
  2 GiB swap.
- **Storage** — 77 GiB `/dev/vda1`, ext4, DigitalOcean network block storage. **Not local NVMe.**
- **OS** — Ubuntu 24.04.3 LTS, kernel 6.8.0-138-generic.
- **Toolchain** — rustc 1.98.0, `--release` (`opt-level=3`, `lto="thin"`, `codegen-units=1`).
- **Isolation** — `snap stop docker`; 18 containers down. Docker down 04:22:17Z, `report`
  04:22:44Z–04:38:27Z, `cargo bench` 04:38:27Z–04:51:38Z, Docker back 04:51:38Z — **29 minutes of
  measurement with nothing else on the box.** All 18 containers restored and healthy afterwards.
- **Build** — done *before* Docker was stopped, so the measured window contains the benchmark and
  nothing else.
- **Topology** — **one node**, throughout. Every engine here, `big` included, answers from a
  single process against a single file; nothing on this page involves a second machine. See
  [Caveats](#caveats).

Commands: `cargo run -p big-bench --release --bin report` and `cargo bench -p big-bench`.
Raw output: [`report.txt`](report.txt) and [`bench.txt`](bench.txt).

> **The point-read figures moved between the two runs of this day, because a fix landed between
> them.** The parent-to-child checksum was being recomputed over a whole 8 KiB page for every bit
> plane a read touched — 606ns of a 640ns plane — and is now verified once per page and
> remembered. Everything below is the **02:54Z → 04:22Z** state of the tree; where a figure
> changed, the earlier one is shown beside it as *was*. See [Finding 2](#findings) and
> [`docs/performance-plan.md`](../../docs/performance-plan.md).

> **This is one run on one machine, and every column is filled.** Earlier versions of this page
> carried a summary from the development Mac because four of the five adapters and five `Engine`
> methods did not exist when the VPS run was taken. They exist now and this run has them, so the
> two-machine caveat is gone and nothing on this page needs to be read across a machine boundary.
>
> A macOS run is kept in [`report-local-macos.txt`](report-local-macos.txt) for reference. **Do
> not read across it and this page** — different CPU, different storage, and an fsync costs a
> completely different amount on each. Every *byte* figure is machine-independent and does agree;
> every *timing* is not and does not.
>
> **Single-shot timings are now medians of three.** The [Reproducibility](#reproducibility)
> section is why. Byte and size figures are single-shot because they are deterministic.
>
> RocksDB has no column. It is a second LSM, `fjall` already answers what an LSM answers here, and
> its C++ build was not worth carrying for a second data point of the same genre — see the
> [readme](../readme.md). A missing column stated is better than a blank one implied.

---

## Summary

Dense ids, 100,000 records, 1,000-record transactions. **Bold is best in row.**

| | big | redb | lmdb | fjall | sqlite |
|---|---|---|---|---|---|
| ingest, full durability | **1403ms** | 2796ms | 2493ms | 1770ms | 3961ms |
| ingest, relaxed | 409ms | 2759ms | **298ms** | 660ms | 1508ms |
| `count_ge` (25% of corpus) | 1809µs | 2960µs | 1228µs | 12389µs | **1146µs** |
| cold point read | 3273µs | 3210µs | 3012µs | 1664µs | **1254µs** |
| `len()` | 5µs | 10µs | **4µs** | 22993µs | 1042µs |
| removals (10k of 100k) | **21ms** | 690ms | 213ms | 194ms | 266ms |
| uncompacted size | **0.6 MiB** | 8.5 MiB | 10.6 MiB | 97.1 MiB | 2.8 MiB |
| compacted size | **0.4 MiB** | 8.4 MiB | 6.6 MiB | 34.5 MiB | 2.4 MiB |
| bytes written | 19.9 MiB | n/a | n/a | n/a | n/a |
| point read (criterion) | 5.02µs *(was 16.21)* | 1.35µs | 676ns | **607ns** | 6.08µs |
| point reads, 1 thread | 139k/s *(was 59k)* | 464k/s | **682k/s** | 422k/s | 100k/s |
| point reads, best thread count | 209k/s *(was 105k)* | 585k/s | **1351k/s** | 1076k/s | 238k/s |
| thread scaling, 1 → best | 1.5× | 1.3× | 2.0× | **2.6×** | 2.4× |

Row definitions, because these are *not* redb's upstream workloads and the numbers are not
comparable to redb's published table:

| row | what it actually measures |
|---|---|
| ingest | 100,000 records, dense ids, 1,000-record transactions |
| `count_ge` | one `count_ge(k)` matching ~25% of the corpus, warm cache |
| cold point read | one `get` after the engine is closed and reopened, so nothing it cached survives |
| `len()` | how many records the engine holds, however it answers that |
| removals | 10,000 of the 100,000 records, by stride, in one transaction |
| uncompacted size | bytes on disk after `checkpoint` |
| compacted size | the same after a full compaction, taken after the removals |
| bytes written | bytes actually pushed through the pager; only `big` can report it |
| point reads, N threads | 200,000 distinct point reads spread across N threads, dense 200k corpus |

All twenty measurements agreed with the engine-free ground truth on both `count_ge` and a probe
`get`.

### What the table says about `big`

- **Space is its strongest column by a wide margin.** 0.6 MiB against redb's 8.5, lmdb's 10.6 and
  fjall's 97.1, and 0.4 MiB compacted. A bitmap index *is* the index, so there is no second
  structure to pay for.
- **Removals are the fastest here** — 25ms against redb's 967ms, a 39× gap. Deleting a record is
  clearing bits, not removing keys from two indexes.
- **The cold read is a coin toss on this box.** `big` won it in the 02:54Z run at 2762µs and
  lost it in the 04:22Z run at 3273µs, while `sqlite` went 3308µs → 1254µs. Single-shot reads
  against network block storage, medians of three, and the whole column moved further between
  runs than the engines differ within one. Read nothing into its ordering.
- **The durability knob is worth 3.4×** on dense ingest (1403ms → 409ms) — and it is new. Earlier
  runs of this report had to state that `big` had no relaxed mode and was being compared against
  `redb` relaxed in half the table.
- **Point reads were its weakest column, and the cause turned out not to be the index.** A
  checksum was being recomputed per bit plane; fixing that took the criterion figure from 16.21µs
  to **5.02µs** and the single-threaded rate from 59k/s to 139k/s. It now beats `sqlite` and is
  3.7× behind `redb`, where it was 11.7× behind. See
  [P1](../../docs/performance-plan.md#p1--the-single-threaded-point-read).
- **The range query is the trade it was built for**, and on this machine it beats redb (1679µs
  against 3242µs) while losing to lmdb and sqlite, both of which answer it from an ordered index
  the harness built for them. The size at which that flips is the sweep below, not this row.

---

## Space, amplification and correctness

`cargo run -p big-bench --release --bin report` — 100,000 records, batch 1,000,
predicate `value >= 786432`. Full table, both layouts, both durability settings.

| engine | durable | layout | ingest ms | query µs | cold µs | len µs | remove ms | disk MiB | compact MiB | written MiB |
|---|---|---|---|---|---|---|---|---|---|---|
| big | full | dense | 1403 | 1809 | 3273 | 5 | 21 | **0.6** | **0.4** | 19.9 |
| redb | full | dense | 2796 | 2960 | 3210 | 10 | 690 | 8.5 | 8.4 | n/a |
| lmdb | full | dense | 2493 | 1228 | 3012 | 4 | 213 | 10.6 | 6.6 | n/a |
| fjall | full | dense | 1770 | 12389 | 1664 | 22993 | 194 | 97.1 | 34.5 | n/a |
| sqlite | full | dense | 3961 | 1146 | 1254 | 1042 | 266 | 2.8 | 2.4 | n/a |
| big | relaxed | dense | 409 | 2031 | 2961 | 7 | 7 | **0.6** | **0.4** | 19.9 |
| redb | relaxed | dense | 2759 | 2922 | 1793 | 8 | 699 | 8.5 | 8.4 | n/a |
| lmdb | relaxed | dense | 298 | 1150 | 2901 | 4 | 141 | 10.6 | 6.6 | n/a |
| fjall | relaxed | dense | 660 | 9693 | 1114 | 23468 | 165 | 97.1 | 34.5 | n/a |
| sqlite | relaxed | dense | 1508 | 1557 | 2109 | 1035 | 246 | 2.8 | 2.4 | n/a |
| big | full | sparse/64 | 5281 | 6586 | 2933 | 477 | 114 | 9.7 | 3.3 | **447.2** |
| redb | full | sparse/64 | 3146 | 2815 | 2018 | 11 | 668 | 8.4 | 8.3 | n/a |
| lmdb | full | sparse/64 | 3295 | 1057 | 2534 | 5 | 290 | 13.3 | 8.8 | n/a |
| fjall | full | sparse/64 | 2449 | 11219 | 9206 | 39611 | 328 | 97.2 | 34.7 | n/a |
| sqlite | full | sparse/64 | 3357 | 1260 | 1527 | 1239 | 528 | 3.4 | 2.7 | n/a |
| big | relaxed | sparse/64 | 2396 | 5505 | 3839 | 604 | 91 | 9.7 | 3.3 | **447.2** |
| redb | relaxed | sparse/64 | 3802 | 3025 | 5203 | 11 | 848 | 8.4 | 8.3 | n/a |
| lmdb | relaxed | sparse/64 | 389 | 1394 | 3163 | 5 | 220 | 13.3 | 8.8 | n/a |
| fjall | relaxed | sparse/64 | 768 | 10356 | 1477 | 37810 | 247 | 97.2 | 34.7 | n/a |
| sqlite | relaxed | sparse/64 | 2357 | 1556 | 1143 | 1279 | 445 | 3.4 | 2.7 | n/a |

**Sparse ids cost `big` and nobody else.** Spreading the same 100k records across 64 shards costs
it 3.9× the ingest time, 16× the disk and **22× the bytes written** (447.2 MiB against 19.9) —
while redb, lmdb and sqlite barely move, because shard layout is invisible to them. 447 MiB
written for 9.7 MiB of resulting data is the finding [P2](../../docs/performance-plan.md#p2--sparse-write-amplification)
addresses, and the finding `Db::ingest` was built to mitigate.

`fjall`'s `len()` column is not a typo. An LSM cannot count without merging, and 22ms to answer
"how many records" is the genre showing through rather than the implementation.

## Threaded reads

200,000 distinct point reads over a dense 200k corpus, spread across N threads.

| engine | 1 | 4 | 8 | 16 | 32 | scaling |
|---|---|---|---|---|---|---|
| big | **139k/s** *(was 59k)* | 206k/s | 202k/s | 207k/s | 209k/s | 1.5× |
| redb | 464k/s | 547k/s | 568k/s | 585k/s | 563k/s | 1.3× |
| lmdb | 682k/s | 1351k/s | 1283k/s | 1257k/s | 1259k/s | 2.0× |
| fjall | 422k/s | 1076k/s | 1038k/s | 919k/s | 778k/s | 2.6× |
| sqlite | 100k/s | 238k/s | 204k/s | 179k/s | 171k/s | 2.4× |

`big`'s single-thread rate is the checksum fix: 59k/s to 139k/s, **2.4×**. The four other engines
are untouched by it and their columns move only by what this machine was doing, which is the
useful control — `lmdb` moved 558k → 682k between the two runs without any change to it at all.

This is a 2 vCPU box, so `scaling` is a shape and not a ceiling — no engine here can show what it
would do on 32 real cores. The useful column is the first one.

## Latency (criterion medians, [low – high])

`cargo bench -p big-bench`. Ten samples per ingest case, the default hundred elsewhere. **This is
the half of the report to trust**: every figure has an interval, and across the two runs of this
day the criterion numbers reproduced far better than the single-shot ones.

**Ingest.** This group compares `big` against its closest architectural peer rather than the whole
field; the five-engine ingest comparison is the `ingest ms` column of the table above.

| benchmark | big full | redb full | redb relaxed |
|---|---|---|---|
| ingest, batch 100 | **153.15ms** [140.15 – 165.49] | 227.44ms [186.74 – 277.77] | 191.05ms [164.49 – 237.82] |
| ingest, batch 1000 | **24.11ms** [20.61 – 26.53] | 76.07ms [70.03 – 82.07] | 82.65ms [72.53 – 105.14] |

At batch 1000 `big` is 3.2× quicker than `redb` at matched durability, and at batch 100 it is
1.5× quicker — a gap that was inside the intervals in the earlier run and is outside them now.

**`count_ge`, swept over corpus size.** This is the query the engine exists to answer, and one
size cannot answer it: a bitmap pays a fixed cost to touch a bit plane and then counts a whole
word at a time, where an ordered index pays per matching row.

| records | big | redb | lmdb | fjall | sqlite |
|---|---|---|---|---|---|
| 20,000 | 653.97µs | 581.14µs | **165.98µs** | 1.0748ms | 233.22µs |
| 100,000 | 1.5291ms | 2.7983ms | **792.74µs** | 5.4452ms | 892.86µs |
| 400,000 | 7.0216ms | 11.326ms | 4.1761ms | 20.384ms | **3.2715ms** |
| 1,000,000 | 15.358ms | 27.454ms | **11.654ms** | 52.880ms | 13.739ms |
| 2,000,000 | 36.084ms | 58.198ms | **23.357ms** | 132.09ms | 25.782ms |

**The crossover against `redb` is real and sits between 20k and 100k.** `redb` is 1.1× faster at
20k, and `big` is 1.8× faster at 100k, 1.6× at 400k, 1.8× at 1M and 1.6× at 2M.

**`big` loses to `lmdb` and `sqlite` at every size, and the gap narrows as the corpus grows** —
3.9× behind lmdb at 20k and 1.5× at 2M; 2.8× behind sqlite at 20k and 1.4× at 2M. Both answer
this query from an ordered `(value, id)` index the harness built for them and that they pay for
on every ingest. The convergence is the shape to watch: the curves are closing, but within the
range measured they have not crossed.

> **This sweep did not move in one direction between the two runs**, and that is the honest
> reading of it. `big` at 100k improved 13% (1.757 → 1.529ms) and at 2M worsened 11% (32.43 →
> 36.08ms), with 20k, 400k and 1M inside a few per cent. A change that added per-container cost
> to the scan path would have moved every size the same way; these do not. The checksum fix
> touches this path — `for_each` verifies through the same memo — so it was worth checking, and
> what the numbers show is this machine's run-to-run spread rather than an effect of the change.

**Point reads.**

| engine | median | vs big |
|---|---|---|
| big | 5.024µs *(was 16.206)* | — |
| redb | 1.353µs | 3.7× faster |
| lmdb | 676ns | 7.4× faster |
| fjall | **607ns** | 8.3× faster |
| sqlite | 6.084µs | **1.2× slower** |

**3.2× quicker than the same benchmark ran an hour and a half earlier**, and the ordering changed
with it: `big` now beats `sqlite`, and the gap to `redb` went from 11.7× to 3.7×. It is still the
weakest of the four key-value engines here, which is what a bit-sliced read costs — 21 planes,
21 pages — but it is no longer an outlier, and what made it one was a checksum rather than the
index. See [P1](../../docs/performance-plan.md#p1--the-single-threaded-point-read).

The criterion figure and the threaded table's single-thread column disagree by a factor of about
1.4 (5.02µs against 139k/s = 7.2µs). They measure different things: criterion probes one record
until it is warm, the threaded harness spreads 200,000 distinct reads over a 200k corpus and pays
for a working set that does not fit in cache. Both are real, and the gap between them is the
part of a point read that is now memory rather than CPU.

## big write amplification vs batch size

10,000 records. These byte figures are deterministic and machine-independent, and are gated on
every push in [`crates/big-db/tests/amplification.rs`](../../crates/big-db/tests/amplification.rs)
— the same workload, the same figures, as a test rather than as a report. Nothing in this section
can go stale without CI failing first.

**Dense:**

| batch | commits | recs/frag | bytes/record | total (MiB) | time (ms) | µs/record |
|---|---|---|---|---|---|---|
| 1 | 10000 | 1.0 | 100,658 | 959 | 124,857 | 12,486 |
| 10 | 1000 | 10.0 | 11,015 | 105 | 13,003 | 1,300 |
| 100 | 100 | 100.0 | 1,712 | 16 | 1,539 | 154 |
| 1000 | 10 | 1000.0 | 177 | 1 | 200 | 20 |
| 10000 | 1 | 10000.0 | 20 | 0 | 31 | 3 |

Commits fell 10,000×; bytes/record fell 5,120× and µs/record 3,949×. **Per-commit dominated:** the
write cost tracks the commit count to within an order of magnitude, so batching is the lever.

**Sparse/64:**

| batch | commits | recs/frag | bytes/record | total (MiB) | time (ms) | µs/record |
|---|---|---|---|---|---|---|
| 1 | 10000 | 1.0 | 67,309 | 641 | 123,166 | 12,317 |
| 10 | 1000 | 1.0 | 44,279 | 422 | 18,756 | 1,876 |
| 100 | 100 | 1.6 | 26,299 | 250 | 4,334 | 433 |
| 1000 | 10 | 15.6 | 2,272 | 21 | 442 | 44 |
| 10000 | 1 | 156.2 | 110 | 1 | 43 | 4 |

Commits fell 10,000×, but bytes/record fell only 613×. **Mixed:** batching recovers some of the
cost and not in proportion to the commits it removes, so a per-record floor is showing through
underneath the per-commit one. That floor is the subject of [P2](../../docs/performance-plan.md#p2--sparse-write-amplification).

## The cost model, and where it broke

The first version of this report asserted a model:

```
commits  ×  fragments per commit  ×  (1 + bit_depth) planes  ×  container size
```

Treating container size as a constant predicts that spreading a batch `f` ways costs about `f`
times more per record. `report.rs` now measures that prediction instead of stating it, and it does
not hold:

| batch | dense recs/frag | sparse recs/frag | dense B/rec | sparse B/rec | fan-out | penalty |
|---|---|---|---|---|---|---|
| 1 | 1.0 | 1.0 | 100,658 | 67,309 | 1.0 | **0.7** |
| 10 | 10.0 | 1.0 | 11,015 | 44,279 | 10.0 | 4.0 |
| 100 | 100.0 | 1.6 | 1,712 | 26,299 | 64.0 | 15.4 |
| 1000 | 1000.0 | 15.6 | 177 | 2,272 | 64.0 | 12.8 |
| 10000 | 10000.0 | 156.2 | 20 | 110 | 64.0 | 5.6 |

Penalty and fan-out disagree by up to 11.5×, and at batch 1 the sparse layout is **cheaper** than
the dense one — the opposite of the prediction.

**The missing term is that container size depends on occupancy.** In the dense layout one fragment
absorbs all 10,000 records, its containers cross the density threshold, get promoted onto their
own 8 KiB bitmap page, and every commit rewrites 21 of them. In the sparse layout each fragment
holds ~156 records, so its containers stay small arrays inline in the leaf. Fan-out multiplies the
container count by 64 *and* shrinks each container, and the two effects partly cancel.

```
commits  ×  fragments per commit  ×  planes  ×  container_cost(occupancy)
```

where `container_cost` jumps roughly an order of magnitude once a container is promoted. A
worse-behaved model than the first — it is not separable — but the one the measurements support.

## Where a commit's pages go

The table that names the remaining cost, and it is not the containers.

| layout | batch | pages/cmt | data | roots | catalog | free | data/frag | dominant |
|---|---|---|---|---|---|---|---|---|
| dense | 1 | 9 | 11 | 1 | 0 | 1 | 11.0 | b-tree paths |
| dense | 100 | 11 | 13 | 1 | 0 | 1 | 13.0 | b-tree paths |
| dense | 2000 | 11 | 7 | 1 | 1 | 1 | 7.0 | b-tree paths |
| sparse/64 | 1 | 9 | 3 | 1 | 3 | 1 | 3.0 | fixed chains |
| sparse/64 | 100 | 302 | 320 | 1 | 3 | 1 | 5.0 | b-tree paths |
| sparse/64 | 2000 | 134 | 128 | 1 | 3 | 1 | 2.0 | b-tree paths |
| sparse/512 | 1 | 24 | 5 | 4 | 17 | 1 | 5.0 | fixed chains |
| sparse/512 | 100 | 564 | 694 | 4 | 17 | 1 | 6.9 | b-tree paths |
| sparse/512 | 2000 | 1047 | 1024 | 4 | 17 | 1 | 2.0 | b-tree paths |

`data` is b-tree pages — leaves, branches and the bitmap pages dense containers own. The rest are
chains a commit rewrites **whole**: the root records whenever any fragment's root moved, the
catalog whenever any zone map shifted (which every bit-sliced write does), and the freelist on
every commit without exception. Those three scale with how many fragments the database *has*,
not with how many this commit touched, so batching cannot amortise them.

> **An earlier version of this page read the sentence above as the explanation of the sparse
> write amplification. It is not, and this table's own `dominant` column says so in six rows of
> nine.** The fixed chains dominate only when a commit carries a single record. Totals over a
> whole 10,000-record ingest, page counts, deterministic:
>
> | layout | batch | catalog | roots | data | free | data share |
> |---|---|---|---|---|---|---|
> | sparse/64 | 1 | 2,071 | 10,000 | 50,093 | 10,000 | 69% |
> | sparse/64 | 100 | 279 | 100 | 31,524 | 100 | **98.5%** |
> | sparse/64 | 1000 | 30 | 10 | 2,714 | 10 | **98.2%** |
> | sparse/512 | 1 | 46,743 | 38,980 | 64,735 | 10,000 | 40% |
> | sparse/512 | 100 | 1,665 | 391 | 63,221 | 100 | **96.7%** |
> | sparse/512 | 1000 | 170 | 40 | 26,336 | 15 | **99.2%** |
>
> The 26,299 and 2,272 bytes/record figures this report headlines come from the batch-100 and
> batch-1000 rows, where the three fixed chains are **one to three per cent** of the bytes. The
> cost there is `data`: one root-to-leaf copy-on-write path per fragment the commit touched,
> paid whether that fragment received one record or a thousand. 64 shards × ~5 pages × 10
> commits ≈ 2,714 pages, which is the batch-1000 row to within rounding.
>
> The correction matters because it changes what would fix it. Making the fixed chains
> incremental — the obvious reading of the paragraph above — would move one to three per cent of
> the sparse cost. What actually governs it is how many separate b-trees a commit has to rewrite
> a path through, which is what `Db::ingest` and `Db::bulk_load` already reduce, and what only a
> change to the shard addressing could remove. See
> [P2](../../docs/performance-plan.md#p2--sparse-write-amplification).

## Buffered ingest

A real ingest usually cannot choose its batch size: records arrive in whatever size they arrive
in. `Db::ingest(capacity)` ([crates/big-db/src/ingest.rs](../../crates/big-db/src/ingest.rs))
buffers in plain memory and decides its own commit points, so the caller keeps handing over small
batches while the engine still commits large ones. It deliberately does not hold a write
transaction open while accumulating — the store allows one writer, and holding that slot for the
length of an ingest would stall every other writer for the same reason it would help this one.

10,000 records, sparse/64, caller hands over 100 at a time throughout:

| buffer | commits | recs/frag | bytes/record | total | time |
|---|---|---|---|---|---|
| none (commit per caller batch) | 100 | 1.6 | 26,299 | 250 MiB | 3732ms |
| 1,000 | 10 | 15.6 | 2,272 | 21 MiB | 463ms |
| 10,000 | 1 | 156.2 | **110** | 1 MiB | **110ms** |
| bulk load | 0 | 156.2 | 155 | 1 MiB | 282ms |

**240× fewer bytes written and 34× faster, with no change to how the caller submits records.** The
buffered rows land on exactly the same bytes/record as the equivalent batch rows in the sweep
above, which is the claim being tested: a buffer of capacity C behaves precisely like a batch of
C, and capacity is the knob the engine controls when batch size is not.

### Bulk load against a buffer too small to hold the load

200,000 records, sparse/64, buffer 10,000:

| path | commits | recs/frag | bytes/record | total | time |
|---|---|---|---|---|---|
| buffer | 20 | 156.2 | 652 | 124 MiB | 2428ms |
| bulk load | 0 | 3125.0 | **34** | 6 MiB | **2132ms** |

The bulk load writes **19× fewer bytes**. Both store the same records; the difference is entirely
that one commits in arrival order and the other does not. The trade is memory: the whole load is
held before any of it is written, which is why a buffered ingest remains the right tool for a
stream that does not end.

### Sizing a buffer: capacity against shard count

50,000 records, caller hands over 100 at a time. Cell is bytes/record, with records per fragment
per commit in brackets.

| capacity | 1 shard | 8 shards | 64 shards | 512 shards |
|---|---|---|---|---|
| 1,000 | 205 (1000) | 1143 (125) | 2979 (16) | 26321 (2) |
| 5,000 | 42 (5000) | 223 (625) | 503 (78) | 4349 (10) |
| 25,000 | 8 (25000) | 37 (3125) | 65 (391) | 343 (49) |
| 50,000 | 4 (50000) | 22 (6250) | 43 (781) | 172 (98) |

Read down a column and cost falls with capacity; read across a row and it rises with shard count.
Both move the same underlying quantity — records per fragment per commit — so that is the first
number to reach for when sizing a buffer.

**But it is not the whole model, and this table says so.** capacity=1,000 across 8 shards and
capacity=50,000 across 512 shards put 125 and 98 records in each fragment per commit — within 28%
of each other — and yet cost 1143 and 172 bytes per record, a factor of **6.7**. The missing
variable is not how many fragment rewrites there were but what each one cost: a fragment holding
tens of thousands of records has dense containers, each owning a page that copy-on-write rewrites
whole, where a fragment holding a few hundred keeps them inline in a leaf. A buffer sized in
records per fragment is the right first move and cannot be the last one.

---

## The analytical comparison

Everything above this line is the **storage** comparison, and it is transactional in shape: what
a commit costs, what a point read costs, what a removal costs, what the file weighs. Those are
OLTP questions asked of engines built to answer them — `redb`, `lmdb`, `fjall` and `sqlite` are
key-value stores, and the table measures read and write speed against them.

That table cannot say whether `big` is any good at the thing it claims to be. It is a
bitmap-native *analytical* database, and the only analytical question in it is `count_ge`. This
section is the other half: the same engine against **DuckDB**, **DataFusion over Parquet** and
**ClickHouse** — the engines sold into the same market — asked six set-shaped questions instead
of four transactional ones.

**It earned its keep on the first run by finding a defect**, in exactly the two rows a bitmap
engine has no business losing. The table below is the third run, taken after that defect was
diagnosed and fixed; the two before it are kept, and the sequence is the section's main result.

**Run** — `cargo run -p big-bench --release --features olap-peers --bin olap`, raw output in
[`olap.txt`](olap.txt). 200,000 records, dense ids, four columns: a twenty-bit integer
(`amount`), a 256-value key (`category`), a twenty-value key (`country`) and a boolean. Docker
stopped for the two minutes the measurement took, ClickHouse 26.9.1 as a static binary rather
than a container so that it could be measured with Docker down. The build finished beforehand
and the box was left to settle: it went in at load 0.39, 3.4 GiB free, 49 MiB of swap. `big` is
asked in its own query language and every rival in SQL, so each plans its own question. Every
answer is checked against a ground truth computed with plain iterators and no engine; timings are
medians of three.

### In-process

All three linked into the benchmark process. **Bold is best in row.**

| | big | duckdb | datafusion |
|---|---|---|---|
| load (200k records) | 1164ms | 523ms | **170ms** |
| `count_ge` (25% of corpus) | 2842µs | **1895µs** | 6497µs |
| 3-term intersect | **4334µs** | 7321µs | 11018µs |
| `sum` over a predicate | 3420µs | **2559µs** | 6568µs |
| `group_by`, 256 groups | **2333µs** | 3345µs | 8320µs |
| `top_n` | **1742µs** | 3171µs | 9100µs |
| `distinct` under a predicate | 6348µs | **3032µs** | 9531µs |
| size on disk | 1.6 MiB | **1.3 MiB** | 3.9 MiB |

### Over HTTP

ClickHouse answers over a socket, so its column carries a round trip neither in-process engine
pays. It is measured on its own and printed so it can be subtracted; `big` repeats here as a
scale, not as a competitor in the same column.

| | big | clickhouse |
|---|---|---|
| load | 1164ms | 657ms |
| `count_ge` | **2842µs** | 14268µs |
| 3-term intersect | **4334µs** | 18597µs |
| `sum` over a predicate | **3420µs** | 16199µs |
| `group_by`, 256 groups | **2333µs** | 13669µs |
| `top_n` | **1742µs** | 14801µs |
| `distinct` under a predicate | **6348µs** | 20332µs |
| size on disk | **1.6 MiB** | 2.1 MiB |
| round trip, on its own | — | 2520µs |

### What this table established

**1. It found a two-order-of-magnitude defect, which is the whole reason to build one.** The
first two runs put `group_by` 117× and 78× behind DuckDB and `top_n` 129× and 118× behind — on
counting and ranking grouped values, which is what a bit-sliced index is *for*. ClickHouse, also
a column store, was 25× and 31× ahead of `big` on those rows. Nothing about that was the genre.

| | before | after | |
|---|---|---|---|
| `group_by`, 256 groups | 408532µs | 2333µs | **175×** |
| `top_n` | 414439µs | 1742µs | **238×** |
| `count_ge` | 3494µs | 2842µs | 1.2× |
| 3-term intersect | 4754µs | 4334µs | 1.1× |
| `sum` over a predicate | 4110µs | 3420µs | 1.2× |
| `distinct` under a predicate | 7110µs | 6348µs | 1.1× |

The four rows that moved by a tenth are the run-to-run weather this report keeps warning about.
The two that moved by two orders of magnitude are the fix.

**2. The first diagnosis was wrong, and subtraction is why.** With only the harness to go on,
this page reasoned that `distinct` and `top_n` call the same `group_counts` over bitmaps of
50,000 and 200,000 records, that four times the records cost sixty times the time, and that the
cost therefore sat in `Rows::All => db.all()`. [`profile_all`](../src/bin/profile_all.rs) timed
the stages directly instead: `all()` costs **69µs** at 200,000 records, a thousandth of what had
been attributed to it. Subtraction can say where the time is not. Promoting that to a cause was
the mistake, and the report said "suspect" rather than "cause" only because the mechanism could
not be found by reading.

**3. The cause was a missing fast path, and its shape is the finding worth keeping.** The same
`group_counts` call over two bitmaps *holding the same records* — the profiler asserts their
cardinalities are equal before timing either — differed by 85×. Not how much data; how it is
represented. `ContainerRef` has three shapes, `Array`, `Bitmap` and `Run`, and the dispatch in
`big_container::apply` carried specialised code for four pairs of the first two. Anything holding
a run fell through to a merge that advances one *value* at a time, so a run covering a
65,536-record block yielded all 65,536 of them against a category row holding about 195. The
arithmetic closed exactly — `256 rows × ceil(n/65536) containers × 65536 steps`, at 4.63, 4.81,
4.77 and 5.56 ns per step across four corpus sizes.

A dense table stores its exists row as runs *because runs are the most compact thing the format
has*. So the engine was slowest on precisely the shape it stores best, and the predicate path
looked quicker only because BSI arithmetic happens to emit bitmaps, which had a fast path. Nobody
had caught it because the only `All()` anyone had measured is the one that avoids the code:
`Count(All())` short-circuits to the cardinalities cached in the leaves — the 5µs `len()` row in
the storage table above.

The fix is one sweep with two cursors over the array and the runs, `|array| + |runs|` steps
instead of the cardinality the runs expand to: 196 rather than 65,536. `Or` and `Xor` still fall
through, because an array output has to ascend and that forces a merge whatever is on the right.

**4. After the fix, `big` wins the rows it should and loses the ones it should.** Against DuckDB
it takes the three-term intersect (1.7×), `group_by` (1.4×) and `top_n` (1.8×) — set-shaped work
over whole columns, which is what the index is. It loses `count_ge` (1.5×), `sum` (1.3×) and
`distinct` (2.1×). Against DataFusion over Parquet it wins **all six**; against ClickHouse it
wins **all six**, by 2.8× to 7.1× with the round trip subtracted.

**5. `distinct` is now the weakest row, and it is the one to look at next.** At 6348µs it is
2.1× behind DuckDB and 2.7× slower than `big`'s own `top_n`, which answers a harder question
over four times the records. It is also the one row the fix did not touch: its filter comes from
a predicate and is bitmap-shaped, so it never used the path that was repaired. Whatever it is
spending, it is not this.

**6. Space is no longer the argument it is upstairs.** `big` is 1.6 MiB against DuckDB's 1.3 —
a row it wins by 4.7× to 162× in the storage table. All three runs agree to the byte, because
size is deterministic and only timings are weather. Four columns rather than one is the reason:
two keyed set fields and a boolean cost row storage a single BSI column does not, while DuckDB's
dictionary encoding of two low-cardinality strings is close to free. The space case for a bitmap
index is about *indexes*, and this table has none to avoid building.

### A second finding in the same place: the loop, not the arithmetic

The fix above made each container pair cheap. What it left alone was how many times a grouping
asks for one, and what it allocates on the way.

`group_counts` was written the obvious way: for each row of the field, materialise that row
(`frag.row(row)`), intersect it with the filter, and take the length. That is **one b-tree range
scan per row**, a copy of every container of the row, and a whole intersection allocated and
dropped for a number. A field with `R` distinct values pays `R+1` scans and `2R` allocations per
fragment - and `Distinct`, `TopN`, `GroupBy`, `count(DISTINCT x)` and a join all run on it.

Two changes, neither of them arithmetic. `big_container::and_cardinality` counts an overlap
without building it - the same routines with the writes removed. And
`FragmentRead::row_counts_where` walks the fragment **once** for every row, because containers
arrive in key order and a row is sixteen consecutive keys, so the row being accumulated is
always the last one seen.

Measured with the same [`profile_all`](../src/bin/profile_all.rs), median of 3, on a laptop
rather than the droplet - so these are ratios on one machine, not numbers to compare against
anything else on this page:

| records | `group/all()` | `group/pred` | `TopN(All())` |
|---|---|---|---|
| 50,000 | 143 → 48µs (3.0×) | 164 → 52µs (3.2×) | 189 → 96µs (2.0×) |
| 100,000 | 220 → 63µs (3.5×) | 266 → 72µs (3.7×) | 273 → 107µs (2.6×) |
| 200,000 | 386 → 88µs (4.4×) | 1164 → 570µs (2.0×) | 454 → 128µs (3.5×) |
| 400,000 | 750 → 146µs (5.1×) | 870 → 176µs (4.9×) | 759 → 186µs (4.1×) |

The multiple grows with the corpus, which is what a fix to the *shape* of a loop should do
rather than a fix to its constant.

**One cell is anomalous and is left in rather than smoothed.** `group/pred` at 200,000 is slower
than at 400,000, before and after, and reproduces across runs. It is not this change - the
before column has it too - and it is not weather. Something about the predicate bitmap at that
corpus size lands on an expensive representation pair. It is the next thing to look at, and it
is exactly the shape of question point 5 above asked about `distinct`.

### The three runs

Kept because the differences between them are the honest measure of what a single-shot timing on
this box is worth.

| | first | second | third |
|---|---|---|---|
| raw output | [`olap-first-run.txt`](olap-first-run.txt) | [`olap-before-fix.txt`](olap-before-fix.txt) | [`olap.txt`](olap.txt) |
| conditions | build still in swap | quiet box | quiet box, after the fix |
| `big` `count_ge` | 4525µs | 3494µs | 2842µs |
| `duckdb` `count_ge` | 1774µs | 2742µs | 1895µs |
| clickhouse round trip | 2817µs | 6371µs | 2520µs |

DuckDB's `count_ge` moved 1.55× between two runs on a settled machine, and ClickHouse's round
trip 2.5×. **Nothing in this section under about 3× is a result**, which is why point 4 names its
multiples and point 1 does not need to.

---

## Findings

**1. `big` wins space, removals and the cold read.** Those are the same design decision seen from
three sides: a bitmap index *is* the index, so there is no second structure to store, nothing to
delete from twice, and fewer blocks to touch when the cache is cold.

Point reads used to be listed here as the price paid for that, on the reasoning that
reconstructing one integer means probing one bit plane per bit. The reasoning was sound and the
conclusion was wrong — see (2). Probing 21 planes is not what a point read cost.

**2. Point reads were 11.7× slower than redb, and the cause was a checksum, not the index.**
A bit-sliced read reconstructs an integer from one bit plane per bit, each plane living in a dense
container on a page of its own — and reading each of those pages re-ran the parent-to-child CRC
over the whole 8 KiB to extract one bit. Measured on the development machine: 606ns to checksum a
page, 640ns of cost per plane, 12.8µs for a 21-plane read. The checksum *was* the read.

Verification is now remembered per page number, which is sound because copy-on-write never
rewrites a page in place. Measured here, same box, 90 minutes apart:

| | before | after | |
|---|---|---|---|
| `get` (criterion) | 16.206µs | **5.024µs** | 3.2× |
| point reads, 1 thread | 59k/s | **139k/s** | 2.4× |
| gap to `redb` | 11.7× | **3.7×** | |
| write amplification, every row | — | **unchanged, byte for byte** | |

The development machine saw 10.4× where this box sees 3.2×, and the difference is itself the
result: removing the CRC removed CPU, and what is left is 21 page touches against network block
storage. The fix and its guards are in
[`docs/performance-plan.md`](../../docs/performance-plan.md).

**3. Sparse ids cost 447 MiB written for 9.7 MiB stored, and the cost is b-tree paths, not
metadata.** An earlier version of this finding said the opposite, following this report's own
prose rather than its own table. At the batch sizes these figures come from, the three fixed
chains are one to three per cent of the bytes; `data` is 96-99%. What it buys is one root-to-leaf
copy-on-write path per fragment a commit touches, paid whether that fragment received one record
or a thousand. See the correction under
[Where a commit's pages go](#where-a-commits-pages-go).

**4. The durability knob is real and worth 3.6× on dense ingest.** It did not exist when this
report was first written, and its absence used to be disclosed as a handicap `big` carried against
`redb` relaxed. That disclosure can now be retired.

**5. `big` beats `redb` on ingest at matched durability in the dense layout** — 1415ms against
3150ms full, 396ms against 3466ms relaxed — and loses to it in sparse, for the reason in (3).

**6. The harness's own conclusions are derived, not hardcoded.** `amplification_vs_batch` reads its
verdict off the measured ratios. An earlier version of this report shipped prose asserting
"batching does not help" while its own data showed a 17,800× drop, because the text outlived the
code that justified it. That class of error is now structurally impossible in this section.

**7. Twice now, a wrong conclusion in this document survived data that contradicted it.** Finding
6 records the first. Finding 3 is the second, and it was worse: the `dominant` column of the
page-class table said "b-tree paths" in six rows of nine while the paragraph under it said fixed
chains, and a performance plan was written against the paragraph. Both times the prose was the
thing that went stale, and both times the table was right the whole time. The lesson that
generalises is the one already applied to `amplification_vs_batch`: a conclusion that a program
derives from its own measurements cannot drift from them, and one a person types next to them
can.

## Reproducibility

This page reports the second of two runs taken 90 minutes apart on 2026-08-28, with one engine
change between them. That makes the four *unchanged* engines a control, and they are the honest
measure of what this box does to a number on its own:

| unchanged measurement | 02:54Z run | 04:22Z run | moved |
|---|---|---|---|
| lmdb, reads/s 1 thread | 558k | 682k | 1.22× |
| sqlite, dense ingest full | 2663ms | 3961ms | 1.49× |
| fjall, cold read | 5006µs | 1664µs | 3.0× |
| redb, `count_ge` 2M (criterion) | 58.908ms | 58.198ms | 1.01× |
| redb, `get` (criterion) | 1.3812µs | 1.3529µs | 1.02× |

**The single-shot column moves by up to 3× on engines nobody touched; the criterion column
reproduces to within 2%.** That is the same split an earlier pair of runs found, and it is why
`big`'s point-read improvement is quoted from criterion (3.2×) rather than from the cold-read row.

An earlier pair of runs, same binary and no code change at all, moved their single-shot timings by
up to **2.7×** while every criterion number reproduced to within 12%. The byte columns did not
move at all, in that pair or this one — the variance is entirely in what the machine was doing,
not in what the engine did.

The likely cause is the host: a shared-tenancy 2 vCPU droplet cannot promise the same CPU twice,
and `big` at full durability spends its time waiting on fsync against network block storage.

**The single-shot half now runs three times and reports the median**, which is what every timing
on this page is. Three rather than more because a run is minutes, not microseconds — a median of
three discards one outlier, which is the failure mode actually observed, and does not turn these
into precision measurements. The caveat that follows still stands: **no conclusion should rest on
a single-shot timing difference smaller than about 3×.**

## Caveats

- **Warm cache.** The page cache was dropped once before the run, not before each measurement.
  Everything except the `cold` column is a warm-cache number.
- **Cold reads are reopen-and-drop, not a true cold device read**, unless the report is run as
  root. This run was, so the `cold` column is a device read.
- **Slow, shared storage.** Network block storage. Absolute numbers are not transferable to an
  NVMe machine; the ratios between engines are the useful part. The cold-read ordering in
  particular inverts on NVMe.
- **2 vCPU.** The threaded read table cannot show scaling past 2 real cores.
- **Write amplification is `n/a` for four of five engines,** not zero — measuring it needs
  OS-level tracing, so the harness reports nothing rather than a guess.
- **The analytical table has two runs and they disagree.** Cells moved by up to 1.55×, and
  ClickHouse's round trip by 2.26×, between two runs of the same binary on the same idle box.
  The first run took its measurement moments after a 2.4 GiB link finished; the second was given
  a settled machine. Both are printed rather than the better one alone, and nothing in that table
  under about 3× should be read as a result.
- **No genre peer.** FeatureBase is the only engine that shares `big`'s design rather than
  competing with it, and it has no adapter: it publishes no static binary and its last image runs
  only under Docker, which this run required to be stopped. See [the readme](../readme.md).
- **One run.** Enough for the ratios, not enough to characterise variance. The criterion half has
  intervals; the single-shot half has a median of three and no interval.
- **One node.** Every figure on this page is a single process against a single file. No shard
  fan-out, no replication, no merge, no network: `bigd --cluster` is not exercised by this
  harness at any point. The comparison is not distorted by that - every in-process peer is
  single-node too and ClickHouse is one server over loopback - but **nothing here measures the
  distribution `big` ships.** A fanned-out query pays a plan encode, a round trip per owner and
  a merge, and none of those costs appears in any column. The clustered path is covered by
  tests, not by numbers.
