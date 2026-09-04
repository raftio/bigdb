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

"""Bytes to typed answers, mirroring `crates/big-http/src/json.rs`.

Pure: every function here takes a `RawResponse` and returns a result or raises. No sockets, no
clock, no retries - which is what lets one set of tests cover both transports.

A non-2xx never reaches a decoder. `check` turns it into the exception for its status and code
first, so a decoder only ever sees a body the server meant as an answer.
"""

from __future__ import annotations

import json
from collections.abc import Mapping
from typing import Any

from .errors import ProtocolError, classify
from .results import (
    Count,
    Extreme,
    FieldInfo,
    Group,
    Groups,
    PqlAnswer,
    ProjectionRow,
    ProjectionRows,
    Ready,
    RecordPage,
    Schema,
    SqlResult,
    Sum,
    TableInfo,
    TextResult,
    TupleGroup,
    Tuples,
    WriteResult,
)
from .transport.base import RawResponse

__all__ = [
    "check",
    "created_database",
    "created_field",
    "created_table",
    "dropped",
    "health",
    "pql",
    "ready",
    "records",
    "schema",
    "sql",
    "wrote",
]

_JSON = "application/json"


def check(raw: RawResponse) -> RawResponse:
    """The response itself, or the exception the server's refusal means."""
    if raw.ok:
        return raw
    raise classify(
        raw.status,
        raw.body,
        request_id=raw.request_id,
        retry_after=raw.retry_after,
        challenge=raw.header("www-authenticate"),
    )


def body(raw: RawResponse) -> Mapping[str, Any]:
    """A 2xx body as an object, or a `ProtocolError` naming what arrived instead.

    A 2xx that is not JSON is this client's problem to report clearly: it means a proxy answered
    or the build is not the one it thinks it is talking to, and either is worth a sentence.
    """
    check(raw)
    try:
        decoded = json.loads(raw.body)
    except (ValueError, UnicodeDecodeError) as e:
        preview = raw.body[:120].decode("utf-8", "replace")
        raise ProtocolError(
            f"a {raw.status} answer that is not JSON (content-type "
            f"{raw.content_type or 'absent'}): {preview!r}"
        ) from e
    if not isinstance(decoded, dict):
        raise ProtocolError(
            f"a {raw.status} answer that is not an object: {type(decoded).__name__}"
        )
    return decoded


# --------------------------------------------------------------------------------------------
# Probes.
# --------------------------------------------------------------------------------------------


def health(raw: RawResponse) -> bool:
    """`GET /health`, which is a constant and never authenticated."""
    return raw.ok


def ready(raw: RawResponse) -> Ready:
    """`GET /ready`. Every field optional, because a single node sends fewer than a cluster."""
    decoded = body(raw)
    behind = decoded.get("behind")
    return Ready(
        status=_str(decoded.get("status")),
        tables=_int(decoded.get("tables")),
        txn_id=_int(decoded.get("txn_id")),
        pages=_int(decoded.get("pages")),
        node=_str(decoded.get("node")),
        shards=_str(decoded.get("shards")),
        version=_str(decoded.get("version")),
        wire=_int(decoded.get("wire")),
        serving=decoded.get("serving") if isinstance(decoded.get("serving"), bool) else None,
        term=decoded.get("term") if isinstance(decoded.get("term"), int) else None,
        leader=decoded.get("leader") if isinstance(decoded.get("leader"), str) else None,
        behind=tuple(str(b) for b in behind) if isinstance(behind, list) else (),
        raw=decoded,
    )


# --------------------------------------------------------------------------------------------
# Schema.
# --------------------------------------------------------------------------------------------


def schema(raw: RawResponse) -> Schema:
    """`GET /schema`."""
    decoded = body(raw)
    tables = decoded.get("tables")
    if not isinstance(tables, list):
        raise ProtocolError("a schema with no `tables` list in it")
    return Schema(tables=tuple(_table(t) for t in tables), raw=decoded)


def _table(item: Any) -> TableInfo:
    if not isinstance(item, dict):
        raise ProtocolError(f"a table that is not an object: {type(item).__name__}")
    fields = item.get("fields")
    return TableInfo(
        name=_str(item.get("name")),
        engine=_str(item.get("engine")),
        fields=tuple(_field(f) for f in fields) if isinstance(fields, list) else (),
        raw=item,
    )


def _field(item: Any) -> FieldInfo:
    if not isinstance(item, dict):
        raise ProtocolError(f"a field that is not an object: {type(item).__name__}")
    granularity = item.get("granularity")
    scale = item.get("scale")
    return FieldInfo(
        name=_str(item.get("name")),
        # Verbatim. `signedint` here is `signed` on the way in, and normalising the two would
        # invent a third vocabulary neither side speaks.
        kind=_str(item.get("kind")),
        bit_depth=_int(item.get("bit_depth")),
        scale=scale if isinstance(scale, int) and not isinstance(scale, bool) else None,
        granularity=tuple(str(g) for g in granularity) if isinstance(granularity, list) else (),
        raw=item,
    )


# --------------------------------------------------------------------------------------------
# `/sql`.
# --------------------------------------------------------------------------------------------


def sql(raw: RawResponse) -> SqlResult | TextResult:
    """`POST /sql`, in whichever of the two shapes the statement's `FORMAT` asked for.

    **Decided by the response's content type, never by reading the statement.** A client that
    scanned SQL for a `FORMAT` clause would be a second parser to keep in step with `big_sql`,
    which is the thing every client in this repository refuses to be.
    """
    check(raw)
    if raw.content_type and raw.content_type != _JSON:
        return TextResult(
            content_type=raw.content_type,
            text=raw.body.decode("utf-8", "replace"),
        )
    decoded = body(raw)
    columns = decoded.get("columns")
    rows = decoded.get("rows")
    if not isinstance(columns, list) or not isinstance(rows, list):
        raise ProtocolError("a result set with no `columns` and `rows` in it")
    return SqlResult(
        columns=tuple(str(c) for c in columns),
        rows=tuple(tuple(row) if isinstance(row, list) else (row,) for row in rows),
        raw=decoded,
    )


# --------------------------------------------------------------------------------------------
# `/table/{t}/query` and `/table/{t}/records`.
# --------------------------------------------------------------------------------------------


def records(raw: RawResponse) -> RecordPage:
    """`GET /table/{t}/records`, which only ever answers in the record shape."""
    decoded = body(raw)
    return _record_page(decoded)


def pql(raw: RawResponse) -> PqlAnswer:
    """`POST /table/{t}/query`.

    Seven shapes and no discriminant on the wire: they are told apart by which key is present.
    Checked in a fixed order, and a shape none of them matches raises rather than handing back
    an untyped dict - a build that has not heard of an answer should say so.
    """
    return _answer(body(raw))


def _answer(decoded: Mapping[str, Any]) -> PqlAnswer:
    if "records" in decoded:
        return _record_page(decoded)
    if "count" in decoded:
        return Count(value=_int(decoded["count"]), raw=decoded)
    if "sum" in decoded:
        return Sum(value=_number(decoded["sum"]), raw=decoded)
    if "value" in decoded:
        value = decoded["value"]
        return Extreme(value=None if value is None else _number(value), raw=decoded)
    if "tuples" in decoded:
        return Tuples(tuples=tuple(_tuple(t) for t in _list(decoded["tuples"])), raw=decoded)
    if "rows" in decoded:
        return ProjectionRows(
            rows=tuple(_projection(r) for r in _list(decoded["rows"])), raw=decoded
        )
    if "groups" in decoded:
        return Groups(groups=tuple(_group(g) for g in _list(decoded["groups"])), raw=decoded)
    raise ProtocolError(
        f"this answer is in no shape this client knows: keys {sorted(decoded)!r}. "
        f"The raw body is available on the exception's cause if the server is newer than "
        f"this client."
    )


def _record_page(decoded: Mapping[str, Any]) -> RecordPage:
    ids = decoded.get("records")
    if not isinstance(ids, list):
        raise ProtocolError("a record page with no `records` list in it")
    nxt = decoded.get("next")
    return RecordPage(
        records=tuple(_int(r) for r in ids),
        next=nxt if isinstance(nxt, int) and not isinstance(nxt, bool) else None,
        raw=decoded,
    )


def _nested(value: Any) -> PqlAnswer:
    """The inner answer of a tuple or a group.

    `json::group` and the tuple writer both call `value()` recursively, so this is another
    complete answer - and a missing or non-object one is a shape this build does not know,
    which `_answer` will say so about rather than silently reading as empty.
    """
    return _answer(value if isinstance(value, dict) else {})


def _tuple(item: Any) -> TupleGroup:
    if not isinstance(item, dict):
        raise ProtocolError(f"a tuple that is not an object: {type(item).__name__}")
    keys = item.get("keys")
    return TupleGroup(
        keys=tuple(k if k is None else str(k) for k in _list(keys)),
        value=_nested(item.get("value")),
        raw=item,
    )


def _projection(item: Any) -> ProjectionRow:
    if not isinstance(item, dict):
        raise ProtocolError(f"a row that is not an object: {type(item).__name__}")
    values = item.get("values")
    return ProjectionRow(
        record=_int(item.get("record")),
        values=tuple(values) if isinstance(values, list) else (),
        raw=item,
    )


def _group(item: Any) -> Group:
    if not isinstance(item, dict):
        raise ProtocolError(f"a group that is not an object: {type(item).__name__}")
    key = item.get("key")
    return Group(
        key=None if key is None else str(key),
        row=_int(item.get("row")),
        value=_nested(item.get("value")),
        raw=item,
    )


# --------------------------------------------------------------------------------------------
# Writes and DDL.
# --------------------------------------------------------------------------------------------


def wrote(name: str) -> Any:
    """A decoder for `{"imported": n}` or `{"deleted": n}`, with its optional `missed`."""

    def decode(raw: RawResponse) -> WriteResult:
        decoded = body(raw)
        if name not in decoded:
            raise ProtocolError(f"a write answer with no `{name}` count in it")
        missed = decoded.get("missed")
        return WriteResult(
            written=_int(decoded[name]),
            # Node descriptions, not record ids: an unreachable replica does not fail a write,
            # it is named here and `POST /repair` catches it up.
            missed=tuple(str(m) for m in missed) if isinstance(missed, list) else (),
            raw=decoded,
        )

    return decode


def created_table(raw: RawResponse) -> int:
    """`{"table": id}`."""
    return _int(body(raw).get("table"))


def created_field(raw: RawResponse) -> int:
    """`{"field": id}`."""
    return _int(body(raw).get("field"))


def created_database(raw: RawResponse) -> bool:
    """`{"database": name, "created": bool}` - `False` when it was already there."""
    return bool(body(raw).get("created"))


def dropped(raw: RawResponse) -> str:
    """`{"dropped": name}`."""
    return _str(body(raw).get("dropped"))


# --------------------------------------------------------------------------------------------
# Small readers that say what was wrong rather than raising a TypeError three frames later.
# --------------------------------------------------------------------------------------------


def _int(value: Any) -> int:
    if isinstance(value, bool) or not isinstance(value, int):
        raise ProtocolError(f"expected a whole number here, got {value!r}")
    return value


def _number(value: Any) -> int | float:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise ProtocolError(f"expected a number here, got {value!r}")
    return value


def _str(value: Any) -> str:
    if not isinstance(value, str):
        raise ProtocolError(f"expected a string here, got {value!r}")
    return value


def _list(value: Any) -> list[Any]:
    if not isinstance(value, list):
        raise ProtocolError(f"expected a list here, got {value!r}")
    return value
