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

"""What an answer is, once it has been read.

Every class here is frozen and carries `raw`, the decoded body it came from. The `raw` is not
decoration: it is what lets a field a newer server adds be reachable without a release of this
client, which matters for a surface that is still growing.

Kinds and engine names are the server's spellings, kept verbatim and never translated. There
are two vocabularies - `?kind=signed` on the way in, `"signedint"` on the way back
(`routes/mod.rs::parse_kind` against `json.rs`'s `Debug`-lowercased render) - and a client that
normalised them into one would be inventing a third that neither side speaks.
"""

from __future__ import annotations

from collections.abc import Mapping
from dataclasses import dataclass, field
from typing import Any

__all__ = [
    "Count",
    "Extreme",
    "FieldInfo",
    "Group",
    "Groups",
    "PqlAnswer",
    "ProjectionRow",
    "ProjectionRows",
    "Ready",
    "RecordPage",
    "Schema",
    "SqlResult",
    "Sum",
    "TableInfo",
    "TextResult",
    "TupleGroup",
    "Tuples",
    "WriteResult",
]

_EMPTY: Mapping[str, Any] = {}


# --------------------------------------------------------------------------------------------
# Schema.
# --------------------------------------------------------------------------------------------


@dataclass(frozen=True, slots=True)
class FieldInfo:
    """One field, as `/schema` describes it."""

    name: str
    #: The server's spelling: one of `set mutex bool int decimal timequantum signedint float32
    #: float64 date datetime`. Note this is *not* the vocabulary `create_field` takes.
    kind: str
    bit_depth: int
    #: Present only for a decimal. A decimal without its scale is an integer wearing a different
    #: name - `price > 5` means `> 500` on a field with two of them.
    scale: int | None = None
    #: Present only for a time quantum, as single characters: `Y`, `M`, `D`, `H`.
    granularity: tuple[str, ...] = ()
    raw: Mapping[str, Any] = field(default_factory=lambda: _EMPTY, repr=False)


@dataclass(frozen=True, slots=True)
class TableInfo:
    """One table and its fields."""

    name: str
    #: `bitmap`, `bitmap+columnar` or `columnar`, verbatim.
    engine: str
    fields: tuple[FieldInfo, ...] = ()
    raw: Mapping[str, Any] = field(default_factory=lambda: _EMPTY, repr=False)

    def field_named(self, name: str) -> FieldInfo | None:
        """The field by that name, or `None`."""
        return next((f for f in self.fields if f.name == name), None)


@dataclass(frozen=True, slots=True)
class Schema:
    """Every table this node knows about."""

    tables: tuple[TableInfo, ...] = ()
    raw: Mapping[str, Any] = field(default_factory=lambda: _EMPTY, repr=False)

    def table(self, name: str) -> TableInfo | None:
        """The first table with that name.

        **First, not the only one.** `TableInfo` carries a `database` on the server
        (`crates/big-embed/src/schema.rs`) that `json::schema` does not render, so two
        same-named tables in two databases arrive here indistinguishable. Across a single
        database this is a lookup; across several it is a guess, and that is the server's to
        fix rather than this client's to paper over.
        """
        return next((t for t in self.tables if t.name == name), None)


# --------------------------------------------------------------------------------------------
# `/sql`.
# --------------------------------------------------------------------------------------------


@dataclass(frozen=True, slots=True)
class SqlResult:
    """Columns and rows, the shape a SQL client is written to read."""

    columns: tuple[str, ...] = ()
    rows: tuple[tuple[Any, ...], ...] = ()
    raw: Mapping[str, Any] = field(default_factory=lambda: _EMPTY, repr=False)

    def dicts(self) -> list[dict[str, Any]]:
        """The rows as dictionaries, for the reading that does not care about order."""
        # strict=False: a row the server sent shorter than its own header is its
        # business to explain, and losing the other rows over it helps nobody.
        return [dict(zip(self.columns, row, strict=False)) for row in self.rows]

    def scalar(self) -> Any:
        """The one cell, for a statement that answers with one.

        Raises `ValueError` rather than returning `None` on an empty result, because `None` is
        also a perfectly good cell value and the two would be indistinguishable.
        """
        if len(self.rows) != 1 or len(self.rows[0]) != 1:
            raise ValueError(
                f"this result is {len(self.rows)} rows by {len(self.columns)} columns, not one cell"
            )
        return self.rows[0][0]


@dataclass(frozen=True, slots=True)
class TextResult:
    """A result set the statement asked for in `FORMAT TSV|TSVWithNames|CSV|CSVWithNames`.

    Handed back as text rather than parsed. Reading it back into rows would mean a CSV reader
    in this client disagreeing with the writer in `crates/big-sql/src/shape.rs` about quoting -
    and a caller who asked for CSV asked for it because something downstream reads CSV.
    """

    content_type: str
    text: str


# --------------------------------------------------------------------------------------------
# `/table/{t}/query` - one class per shape `json::value_paged` can write.
# --------------------------------------------------------------------------------------------


@dataclass(frozen=True, slots=True)
class Count:
    """`{"count": n}`."""

    value: int
    raw: Mapping[str, Any] = field(default_factory=lambda: _EMPTY, repr=False)


@dataclass(frozen=True, slots=True)
class Sum:
    """`{"sum": n}`.

    Signed, unsigned and real sums all arrive in this one shape on purpose: JSON numbers are
    signed, so a client reading it needs to know nothing about how the field was declared.
    """

    value: int | float
    raw: Mapping[str, Any] = field(default_factory=lambda: _EMPTY, repr=False)


@dataclass(frozen=True, slots=True)
class Extreme:
    """`{"value": n | null}` - a min or a max.

    `None` is nothing matched, which is a different answer from a total of nothing. That is why
    the server writes `null` here rather than `0`.
    """

    value: int | float | None
    raw: Mapping[str, Any] = field(default_factory=lambda: _EMPTY, repr=False)


@dataclass(frozen=True, slots=True)
class RecordPage:
    """`{"records": [...], "next": n | null}`.

    `next` is set only when the page came back exactly as long as the limit asked for, so it
    means "there may be more after this id" rather than "there is more".
    """

    records: tuple[int, ...] = ()
    next: int | None = None
    raw: Mapping[str, Any] = field(default_factory=lambda: _EMPTY, repr=False)


@dataclass(frozen=True, slots=True)
class TupleGroup:
    """One entry of `{"tuples": [...]}`: a list of keys, and the value for that combination."""

    keys: tuple[str | None, ...]
    value: PqlAnswer
    raw: Mapping[str, Any] = field(default_factory=lambda: _EMPTY, repr=False)


@dataclass(frozen=True, slots=True)
class Tuples:
    """`{"tuples": [...]}` - a pair (or wider) grouping asked for in the query language."""

    tuples: tuple[TupleGroup, ...] = ()
    raw: Mapping[str, Any] = field(default_factory=lambda: _EMPTY, repr=False)


@dataclass(frozen=True, slots=True)
class ProjectionRow:
    """One entry of `{"rows": [...]}`: a record and its projected cells."""

    record: int
    values: tuple[Any, ...] = ()
    raw: Mapping[str, Any] = field(default_factory=lambda: _EMPTY, repr=False)


@dataclass(frozen=True, slots=True)
class ProjectionRows:
    """`{"rows": [...]}` - a projection asked for in PQL rather than in SQL.

    One object per record rather than an array of cells, because this route's answers name what
    they hold; the result-set shape belongs to `/sql`, where the client asked for a table.
    """

    rows: tuple[ProjectionRow, ...] = ()
    raw: Mapping[str, Any] = field(default_factory=lambda: _EMPTY, repr=False)


@dataclass(frozen=True, slots=True)
class Group:
    """One entry of `{"groups": [...]}`.

    `key` is the bucket's own name and `row` the number it is addressed by. A keyed group
    carries the string beside the row id because a row id is meaningless without the dictionary
    that issued it; a calendar bucket has no dictionary and writes itself out as the date it
    stands for.
    """

    key: str | None
    row: int
    value: PqlAnswer
    raw: Mapping[str, Any] = field(default_factory=lambda: _EMPTY, repr=False)


@dataclass(frozen=True, slots=True)
class Groups:
    """`{"groups": [...]}`."""

    groups: tuple[Group, ...] = ()
    raw: Mapping[str, Any] = field(default_factory=lambda: _EMPTY, repr=False)


#: What `Client.query` can answer with. A union with no discriminant on the wire - the shapes
#: are told apart only by which key is present - so `decode.pql` checks in a fixed order and
#: refuses a shape it does not recognise rather than handing back a dict pretending to be typed.
PqlAnswer = Count | Sum | Extreme | RecordPage | Tuples | ProjectionRows | Groups


# --------------------------------------------------------------------------------------------
# Writes and probes.
# --------------------------------------------------------------------------------------------


@dataclass(frozen=True, slots=True)
class WriteResult:
    """What an import or a delete managed."""

    written: int
    #: The copies that did not take it, described the way the server describes them -
    #: `"a-spare (0..1) (connection refused)"`. **Strings, not record ids**: an unreachable
    #: replica does not fail a write, it is named here and marked behind, and `POST /repair`
    #: catches it up. A non-empty `missed` means the write is on fewer copies than it should be.
    missed: tuple[str, ...] = ()
    raw: Mapping[str, Any] = field(default_factory=lambda: _EMPTY, repr=False)

    @property
    def complete(self) -> bool:
        """Whether every copy took it."""
        return not self.missed


@dataclass(frozen=True, slots=True)
class Ready:
    """`GET /ready`, which is never authenticated.

    The cluster fields - `serving`, `term`, `leader`, `behind` - are present only on a node that
    is in one. `wire` is data here and never a gate: this client does not send `x-big-wire`,
    which `routes/mod.rs::mismatched` checks on `/internal/*` alone.
    """

    status: str = ""
    tables: int = 0
    txn_id: int = 0
    pages: int = 0
    node: str = ""
    shards: str = ""
    version: str = ""
    wire: int = 0
    serving: bool | None = None
    term: int | None = None
    leader: str | None = None
    behind: tuple[str, ...] = ()
    raw: Mapping[str, Any] = field(default_factory=lambda: _EMPTY, repr=False)
