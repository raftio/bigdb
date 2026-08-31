# big-field

Field types: the convention that decides which row a value goes into.

Nothing here stores anything. A field type is a *mapping* — given a value and a record, which
bits in which rows of a fragment does it turn on — and the fragment underneath does not know
which convention produced the bits it holds. That separation is what lets five field kinds share
one b-tree implementation.

## The five kinds

**`bsi` — bit-sliced integers.** A `u64` stored as one row per bit: value `5` at depth 3 sets
the record's bit in plane 0 and plane 2. A range query is then a boolean circuit over the
planes rather than a scan of values, which is the trade this engine is built around — `count`
over millions of records touches `bit_depth` rows, but a point read has to reconstruct the value
from one plane per bit where a b-tree would do a single descent. Row 0 is `EXISTS_ROW`, so
"which records have this field at all" is one more row rather than a separate structure.

**`signed`** (in `big-db`) is a `bsi` with the sign handled above it. It is a distinct field
kind rather than a flag because writing a signed value into an unsigned field stores a different
number silently instead of failing.

**`set`** — the simple case: one row per interned key, and a write only ever turns bits on.
Nothing to serialise against, so a set write can wait with the rest of the batch.

**`mutex`** — a set where each record holds at most one value. Enforcing that means finding the
value being replaced, which means a **read**, which is why a mutex write cannot be buffered with
the others: it keeps a shadow view of what each record currently holds and has to consult it at
the moment of the write.

**`quantum`** — a keyed value that also happened at a time. The fact goes into the standard view
exactly as `set` would, and additionally into one view per declared `Granularity`. Those extra
views are the entire point: a query for a range of days reads the day views it asks about
instead of every record ever written. `DEFAULT_GRANULARITY` is `[Day]`.

## The calendar is hand-rolled, and small

`civil_from_days` is Howard Hinnant's algorithm. Days-to-date is the only calendar arithmetic
this engine needs, and it is thirty lines — a date/time crate would be a dependency an order of
magnitude larger than the thing it replaces, in a crate whose entire job is naming views.
