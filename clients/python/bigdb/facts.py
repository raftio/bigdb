# Copyright 2026 Bany
#
# Licensed under the Apache License, Version 2.0 (the "License");
# you may not use this file except in compliance with the License.
# You may obtain a copy of the License at
#
#     http://www.apache.org/licenses/LICENSE-2.0
#
# Unless required by applicable law or agreed to in writing, software
# distributed under the License is distributed on an "AS IS" BASIS,
# WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
# See the License for the specific language governing permissions and
# limitations under the License.

"""`field record value`, one per line - the body `POST /table/{t}/import` reads.

# Why this route is the one to reach for

The caller chooses the record id, so every fact is a `set` at an address rather than an
allocation. Sending a chunk twice writes the same bits twice, which is writing them once. That
is what makes `/import` idempotent, what makes it safe to retry after an ambiguous failure, and
what makes `bigctl import --resume` possible. An allocating `INSERT` over `/sql` has none of it.

# The line format, exactly

`crates/big-http/src/routes/query.rs::three` splits on the **first two spaces only**, so the
value keeps any spaces it contains and nothing needs escaping. The consequences a client has to
enforce, because the server can only report them as a malformed line:

- a *field name* containing a space would be read as a field and a record id;
- a record id that is not a `u64` is a refusal;
- a newline anywhere in a value ends the line early and turns one fact into two malformed ones.

Lines are trimmed as ASCII, deliberately: a non-breaking space in a value is data rather than
framing, which is the reading a keyed value wants - two keys that differ by one are two keys.
"""

from __future__ import annotations

from collections.abc import Iterable, Iterator
from dataclasses import dataclass
from typing import Any, Final

from .config import DEFAULT_MAX_BYTES
from .errors import RequestTooLarge, ValueRefused

__all__ = ["Fact", "chunk_facts", "render_fact", "render_facts"]

_U64_MAX: Final[int] = (1 << 64) - 1


@dataclass(frozen=True, slots=True)
class Fact:
    """One bit, at an address the caller chose.

    `value` is written with `str()` and is not quoted or escaped - the format has no escape,
    because it does not need one. What a value may say depends on the field's kind, which lives
    in the schema on the server: a number for `int`, `true`/`false` for `bool`,
    `key@unix_seconds` for `timequantum`, and text for `set` and `mutex`.
    """

    field: str
    record: int
    value: Any


def render_fact(fact: Fact) -> str:
    """One line, with the three refusals the format needs and the server cannot name well."""
    if not fact.field:
        raise ValueRefused("a fact with no field name")
    if " " in fact.field or "\n" in fact.field:
        # The server would read `amount paid 3 5` as field `amount`, record `paid` and refuse
        # it as a malformed record id - a sentence that names the wrong problem.
        raise ValueRefused(
            f"{fact.field!r} cannot be a field name here: the import format splits on spaces, "
            f"so a name holding one would be read as a field and a record id"
        )
    if not isinstance(fact.record, int) or isinstance(fact.record, bool):
        raise ValueRefused(f"a record id is a whole number, not a {type(fact.record).__name__}")
    if not 0 <= fact.record <= _U64_MAX:
        raise ValueRefused(f"{fact.record} is not a record id: they run from 0 to {_U64_MAX}")

    value = _text(fact.value)
    if "\n" in value or "\r" in value:
        raise ValueRefused(
            f"a value holding a newline would end its line early and turn one fact into two; "
            f"field {fact.field!r}, record {fact.record}"
        )
    return f"{fact.field} {fact.record} {value}"


def render_facts(facts: Iterable[Fact]) -> bytes:
    """A whole body, newline-terminated.

    A trailing newline does not produce a final empty line - `parse_into` matches `str::lines`
    on that - so the count a refusal names is the count of facts given here.
    """
    lines = [render_fact(fact) for fact in facts]
    return ("\n".join(lines) + "\n").encode("utf-8") if lines else b""


def chunk_facts(
    facts: Iterable[Fact],
    *,
    max_bytes: int = DEFAULT_MAX_BYTES,
) -> Iterator[tuple[int, bytes]]:
    """`(offset, body)` pairs, each under `max_bytes`, never splitting a line.

    `offset` is the index of the first fact in the chunk, so a caller can checkpoint it and
    resume - the same thing `bigctl import --resume` records, and it is only meaningful because
    the route is idempotent.

    A single fact too large for the cap is refused by index rather than silently sent to a 413,
    which mirrors `big-message`'s `MessageTooLarge { at, len, cap }`: a producer that has sent a
    million of them needs to know which one, not that one exists.
    """
    if max_bytes <= 0:
        raise ValueError("max_bytes must be positive")

    buffer: list[str] = []
    size = 0
    first = 0
    for index, fact in enumerate(facts):
        line = render_fact(fact)
        cost = len(line.encode("utf-8")) + 1  # the newline it will be joined with
        if cost > max_bytes:
            raise RequestTooLarge(cost, max_bytes, at=index)
        if size + cost > max_bytes and buffer:
            yield first, ("\n".join(buffer) + "\n").encode("utf-8")
            buffer, size, first = [], 0, index
        buffer.append(line)
        size += cost
    if buffer:
        yield first, ("\n".join(buffer) + "\n").encode("utf-8")


def render_records(records: Iterable[int]) -> bytes:
    """One record id per line, the body `POST /table/{t}/delete` reads.

    The server parses the whole batch before removing any of it, so a malformed line does not
    leave half a batch deleted - and unlike an import there is no undo for the half.
    """
    lines: list[str] = []
    for record in records:
        if not isinstance(record, int) or isinstance(record, bool):
            raise ValueRefused(f"a record id is a whole number, not a {type(record).__name__}")
        if not 0 <= record <= _U64_MAX:
            raise ValueRefused(f"{record} is not a record id: they run from 0 to {_U64_MAX}")
        lines.append(str(record))
    return ("\n".join(lines) + "\n").encode("utf-8") if lines else b""


def _text(value: Any) -> str:
    """A value as the server will read it back.

    `bool` before the general case for the same reason `sql.literal` does it: Python's `True`
    would otherwise arrive as `True`, and `big_db` reads a bool field's value as `true`/`false`.
    """
    if isinstance(value, bool):
        return "true" if value else "false"
    if isinstance(value, float) and (value != value or value in (float("inf"), float("-inf"))):
        raise ValueRefused(f"{value} has no spelling this engine reads")
    return str(value)
