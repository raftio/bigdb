# big-civil

The civil calendar, and the two spellings a date arrives in. No dependencies.

```rust
use big_civil::{parse_date, format_date, parse_datetime, format_datetime};

assert_eq!(parse_date("2024-01-15"), Some(19_737));
assert_eq!(format_date(19_737), "2024-01-15");

assert_eq!(parse_datetime("2024-01-15 10:30:00"), Some(1_705_314_600));
assert_eq!(parse_datetime("2024-01-15"), Some(1_705_276_800)); // midnight
```

## Why it is a crate

The calendar is needed at both ends of the tree, and the two ends cannot see each other.
`big-engine` names a time quantum's views (`YYYYMMDD`) and has always had the day-to-civil half
of this. `big-plan` turns `'2024-01-15'` into a number to compare against, and has no
dependencies of its own on purpose. Neither can depend on the other without inverting the
layering, so the alternative was a copy of Hinnant's algorithm in each — and `big_sql::render`
says what this tree thinks of an inverse kept in another crate.

## What a date is

A `DATE` is a count of days from 1970-01-01; a `DATETIME` is a count of seconds from the same
instant. Both signed, both proleptic Gregorian.

**No timezone and no leap second.** A written date is the date it says, and
`2024-01-15 10:30:00` is 37800 seconds into that day everywhere — the only reading that survives
being compared against a value written by somebody else's clock.

## What it refuses

Parsing is strict, because a date accepted in two spellings reads back in one of them:

| Refused | Why |
|---|---|
| `2024-1-5` | not zero padded |
| `2023-02-29` | not a leap year |
| `2024-13-01`, `2024-01-32` | no such month or day |
| `2024-01-15 24:00:00` | a second spelling of the next midnight |
| `2024/01/15`, `24-01-15` | not the format |

`T` is accepted in place of the space, so an ISO-8601 timestamp pasted from elsewhere is read
rather than refused over a separator.
