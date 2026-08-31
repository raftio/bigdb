# big-keys

Row key translation: the string value of a set, mutex or time quantum field, to the row it
lives in.

## Why this is the only translation layer

Record ids are used raw — a caller's `RecordId` is the bit position, with no dictionary in
between. Row keys are the exception, and they are the exception because a row has to occupy the
same position in *every* shard: two shards that disagreed about which row `"london"` is would
produce a union that is silently wrong rather than an error.

That makes interning a row key **the single point in the write path that requires agreement**,
which is why it is a crate rather than a module. Whatever distributed design comes later gets
decided here, and the rest of the write path is untouched by it.

## The store

```rust
store.intern(table, field, "london")?  // -> RowId, assigning one if new
store.id(table, field, "london")       // -> Option<RowId>, never assigning
store.name(table, field, row)          // -> Option<&str>, the way back
```

`intern` and `id` are separate on purpose: a read must never mint a row id, because doing so
would make a query that matched nothing indistinguishable from a query that grew the schema.

`MAX_KEY_LEN` is 104 bytes. A longer key is refused at `intern` rather than truncated — two distinct keys that truncate to
the same prefix would merge two rows' worth of data with no error anywhere.

## Dropping

`remove_scope` and `remove_table` exist because dropping a field or a table has to drop its keys
with it. Without them a dropped-and-recreated field would inherit the old field's key
assignments, and the old data would reappear under the new field's name.
