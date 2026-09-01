# Testing the SQL surface

Four hundred statements in files, five properties over predicates nobody wrote, and three gates
that fail when the corpus stops covering the code. This page says what each of those is for and
where the boundaries between them are.

## The shape

| | where | what it holds | how many |
|---|---|---|---|
| Claims | `crates/big-sql/tests/translate` | the arguments: two surfaces, one plan; a refusal says what exists instead | ~55 functions |
| Translation corpus | `crates/big-sql/tests/testdata` | every clause, type name and refusal, against the tree it translates into | ~405 cases |
| Logic corpus | `crates/big-cluster/tests/logic` | statements against real records, and the rows they answer with | ~150 cases |
| HTTP configuration | `crates/big-http/tests/logic.rs` | the same corpus over a socket | the same cases |
| Properties | `crates/big-cluster/tests/metamorphic.rs` | generated predicates against ground truth and against each other | 5 × 256 |
| Gates | `crates/big-sql/tests/gates.rs` | that the corpus keeps covering the code | 3 |
| Fuzz | `fuzz/fuzz_targets/parse_sql.rs` | no input is a crash | unbounded |

## Why files as well as functions

A hand-written test states a claim, and the good ones do: *these two surfaces resolve to the same
plan*, *this refusal says what exists instead*. That is worth four lines of Rust and a paragraph
of why, and those tests stay.

Breadth is a different job. Four hundred statements each checked against the tree it translates
into is not four hundred claims — it is one claim held at four hundred points, and writing each
point as a function buries the interesting tests among the routine ones while making the routine
ones expensive enough that nobody adds the four hundred and first.

So breadth moved into files, where a case costs two lines and the expected output is generated.
The claims stay in `tests/translate`, where the prose can say why.

## The file format

```
plan
SELECT count(*) FROM t WHERE amount >= 500
----
Count t
└── amount >= 500
```

A directive, the statement under it, and — after `----` — what the directive prints. The expected
block ends at a blank line. Words after the directive name are its arguments, which is where a
second *input* goes, so that the block after `----` is always output and therefore always safe to
regenerate.

Comments start with `#` and survive a rewrite, as do blank lines and case order.

### Directives

`crates/big-sql/tests/testdata` — no schema, no file, no pager:

| | prints |
|---|---|
| `plan` | one tree per call the statement makes, and per search |
| `shape` | the answer: its columns, `HAVING`, `ORDER BY` and cut |
| `same <pql>` | the plan both surfaces resolve to, having checked they agree |
| `error` | the stable code the refusal carries |
| `ddl` / `insert` / `show` | the schema change, the write, the catalog question |
| `render` | a `CREATE TABLE` written back out of the columns it declared |

`crates/big-cluster/tests/logic` — a real database, one per file:

| | does |
|---|---|
| `statement` | runs it, expecting success. Prints nothing, or the refusal |
| `exec` | the same, printing the one-cell answer a change comes back with |
| `query [rowsort]` | runs it and prints the rows |
| `same <pql>` | the same, having checked the query language answers identically |
| `error` | expects a refusal and prints its stable code |

## Regenerating

```
BIG_REWRITE=1 cargo test -p big-sql     --test testdata
BIG_REWRITE=1 cargo test -p big-cluster --test logic
BIG_TEST_FILTER=joins cargo test -p big-sql --test testdata
```

A rewrite **fails the run** and names the files it changed. That is deliberate twice over:
`libtest` captures the output of a test that passes, so the notice would be invisible exactly
when it matters; and a green `make test` under `BIG_REWRITE=1` is a green run that proves
nothing. The plain re-run, with nothing left to regenerate, is the one that says it passed.

**A rewrite is a diff to read, not a fix.** It is the cheap way to add a hundred cases and the
cheap way to accept a hundred regressions, and the only thing between the two is that the change
shows up in `git diff` at the level of the answers.

## Configurations

One corpus, more than one way to run it — the shape CockroachDB's logic tests have. The files
stay a description of what the engine answers; each configuration claims some other path answers
the same.

`crates/big-http/tests/logic.rs` runs `big-cluster`'s corpus over a real socket. It does not check
the files' expected blocks: those are aligned text and the route answers JSON, TSV or CSV, so
re-deriving one from the other would mean writing a JSON reader in a test and then checking that
reader. Instead every statement goes to the server *and* to a local cluster in the same order —
so both hold the same data at every step — and what is asserted is that the bytes coming back are
the bytes the engine's own renderer produces, that a refusal carries the same stable code, and
that its status does not invite a retry.

`big_testfile::check` is that mode: expected blocks ignored, files never rewritten.

## The three gates

A corpus decays in a way a test suite does not: nothing about it fails when a feature is added
and no case is written for it. `crates/big-sql/tests/gates.rs` turns that from a matter of
discipline into a matter of the build going red.

1. **Every refusal is reached by a statement.** Each of the 40 `Refused` variants carries a
   sentence saying what exists instead, and that sentence is the whole reason the list is
   enumerated rather than left as free text. A refusal no statement reaches is a sentence written
   once and never read since. One exception is listed with its reason — see below.
2. **Every declared table survives being written back out.** `parse::column_type` decides what a
   type name means and `render::create_table` decides how a field is written back; they are two
   directions of one table, and an inverse kept honest by nothing drifts. Every `CREATE TABLE` in
   the corpus goes out through the renderer and back through the parser.
3. **No statement the corpus can be cut into makes the parser panic.** Every prefix and every
   one-byte deletion of every case, which is around 60,000 inputs, in the ordinary test suite.
   `fuzz/fuzz_targets/parse_sql.rs` runs the same contract with no bound on its inputs.

### The excused refusal

`sql_insert_too_large` needs more than ten thousand tuples in one statement, which is a file
nobody would read. It is checked in `tests/translate/writes.rs`, where the rows can be generated.

The list was two. `sql_no_time_window` was not hard to write — **nothing constructed it.** It was
the refusal a window earned before v3 gave the planner a field class that could tell a set from a
time quantum, and the window it refused is now answered; the variant, its code and its sentence
had outlived it by a release. This gate found that on its first run, which is the argument for
having it.

## The properties

`crates/big-cluster/tests/metamorphic.rs` generates `WHERE` clauses — nested, negated, mixed
across every field class — and checks each against two things that are not the engine.

**The ground truth.** The two hundred records are built by the test, so the predicate is evaluated
over them in Rust and the number compared directly. This is the strongest oracle available: it
does not say the engine is consistent with itself, it says the engine is right.

**The partition.** `count(p) + count(NOT p) = count(*)` is the relation CockroachDB's TLP looks
for, and there it is approximate — SQL's three-valued logic puts a row where `p` is null in
neither half. There is no null here, so the equation is exact, which makes it a sharper
instrument than it is there. Inclusion–exclusion over `OR`, the split of one predicate by
another, and the sum of a grouping's counts are the same idea at three more angles.

`proptest` shrinks before reporting, so a failure arrives as the smallest `WHERE` clause that
still disagrees — usually short enough to paste into `tests/logic` as a case and keep.

## Adding a case

1. Write the directive and the statement. Leave the block off.
2. `BIG_REWRITE=1 cargo test -p <crate> --test <corpus>`.
3. Read the block it generated. If it is not what the statement means, the bug is in the engine
   and the block is the report.
4. Re-run without `BIG_REWRITE`.

If the case is making an argument rather than covering a surface, write it in
`crates/big-sql/tests/translate` instead, with the paragraph that says why.
