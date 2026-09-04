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

"""The route table: what to send, and what the answer means.

One builder per route, all pure - they return an `Op` and never touch a socket. `Client` and
`AsyncClient` both drive these, which is what makes the two surfaces the same surface.

# Reaching a table that is not in the default database

Two spellings, and they mean the same thing on every table route:

    ops.records("sales.orders")                  # qualified in the path
    ops.records("orders", database="sales")      # named by parameter

So this client sends **both**: the parameter, because `routes/mod.rs::refuse` reads it to build
the RBAC object, and the table name exactly as given, so a caller who qualifies it is not
second-guessed. A qualified path wins over a parameter that disagrees, being the more specific
of the two.

That symmetry is recent. `?database=` used to be read by the guard and then dropped by
`/import`, `/delete`, `/records` and `/query`, and `/import` additionally matched the raw path
segment against the bare `TableInfo::name` - so a table outside the default database was
reachable by qualified path on three routes and by nothing at all on `/import`. Both halves are
fixed in `crates/big-http/src/routes/query.rs::scoped`;
`tests/test_integration.py::test_a_table_outside_the_default_database_is_reachable_both_ways`
is the client-side pin.
"""

from __future__ import annotations

from collections.abc import Callable, Iterable
from dataclasses import dataclass
from typing import Any, Generic, TypeVar

from . import decode
from .config import DEFAULT_MAX_BYTES
from .errors import RequestTooLarge
from .escape import escape_segment, query_string
from .facts import Fact, render_facts, render_records
from .results import (
    PqlAnswer,
    Ready,
    RecordPage,
    Schema,
    SqlResult,
    TextResult,
    WriteResult,
)
from .transport.base import RawResponse

__all__ = ["Op"]

#: What a route's decoder answers with. Carried on `Op` so `Client._run` is typed by the
#: route it is given rather than by `Any`.
T = TypeVar("T")


@dataclass(frozen=True, slots=True)
class Op(Generic[T]):
    """One request, and what to do with what comes back."""

    method: str
    target: str
    body: bytes
    decode: Callable[[RawResponse], T]
    #: Whether sending this twice is the same as sending it once. Consulted only after an
    #: `Unknown` - a failure where the request was written in full and the outcome is not known.
    idempotent: bool
    #: What to call this in a `TransportError`, so a caller logging one knows which write is in
    #: doubt without decoding a URL.
    name: str


def _sized(body: bytes, cap: int, name: str) -> bytes:
    """The body, or a refusal that never opens a socket.

    `big_http::MAX_BODY` is checked against `Content-Length` *before* the body is read
    (`crates/big-http/src/request.rs`), so an over-large request is a clean 413 rather than a
    partial write - but it is still a round trip to be told something measurable locally.
    """
    if len(body) > cap:
        raise RequestTooLarge(len(body), cap)
    return body


def _scoped(database: str | None, **extra: Any) -> str:
    """A query string with `?database=` folded in, when there is one."""
    return query_string({"database": database, **extra})


# --------------------------------------------------------------------------------------------
# Probes. Never authenticated, and both are reads.
# --------------------------------------------------------------------------------------------


def health() -> Op[bool]:
    return Op("GET", "/health", b"", decode.health, True, "health")


def ready() -> Op[Ready]:
    return Op("GET", "/ready", b"", decode.ready, True, "ready")


# --------------------------------------------------------------------------------------------
# Data plane.
# --------------------------------------------------------------------------------------------


def sql(
    statement: str, *, database: str | None = None, cap: int = DEFAULT_MAX_BYTES
) -> Op[SqlResult | TextResult]:
    """`POST /sql`.

    **Not idempotent, and this client will not try to decide otherwise.** A `SELECT` is
    perfectly safe to repeat and an allocating `INSERT` is not, but telling them apart means a
    SQL parser here kept in step with `big_sql` - a second parser, which is the thing every
    client in this repository refuses to be. So the whole route is treated as the dangerous
    half. A caller who knows their statement is a read can send it again themselves.
    """
    body = statement.encode("utf-8")
    return Op(
        "POST",
        f"/sql{_scoped(database)}",
        _sized(body, cap, "sql"),
        decode.sql,
        False,
        "sql",
    )


def query(
    table: str,
    pql: str,
    *,
    database: str | None = None,
    after: int | None = None,
    limit: int | None = None,
    cap: int = DEFAULT_MAX_BYTES,
) -> Op[PqlAnswer]:
    """`POST /table/{t}/query`. A read; PQL has no write form.

    `after` and `limit` are passed through without checking whether this call's answer can be
    paged - that would need a PQL parser. A call whose answer is not records comes back as
    `422 not_pageable`, which is a clear enough sentence to hand straight to the caller.
    """
    body = pql.encode("utf-8")
    return Op(
        "POST",
        f"/table/{escape_segment(table)}/query{_scoped(database, after=after, limit=limit)}",
        _sized(body, cap, "query"),
        decode.pql,
        True,
        "query",
    )


def records(
    table: str,
    *,
    database: str | None = None,
    after: int | None = None,
    limit: int | None = None,
) -> Op[RecordPage]:
    """`GET /table/{t}/records`.

    No default limit, matching the server: absent means everything, and a default here would
    silently truncate a caller who does not know a cursor exists.
    """
    return Op(
        "GET",
        f"/table/{escape_segment(table)}/records{_scoped(database, after=after, limit=limit)}",
        b"",
        decode.records,
        True,
        "records",
    )


def import_facts(
    table: str,
    facts: Iterable[Fact] | bytes | str,
    *,
    database: str | None = None,
    cap: int = DEFAULT_MAX_BYTES,
) -> Op[WriteResult]:
    """`POST /table/{t}/import`.

    **Idempotent**, and it is the only write route that is. A fact is a bit set at an address
    the caller chose, so sending a chunk twice writes the same bits twice - which is writing
    them once. That is what makes retrying an ambiguous failure safe here and nowhere else.

    Pre-rendered `bytes` or `str` are accepted so a caller with a file on disk pays nothing to
    send it.
    """
    if isinstance(facts, bytes):
        body = facts
    elif isinstance(facts, str):
        body = facts.encode("utf-8")
    else:
        body = render_facts(facts)
    return Op(
        "POST",
        f"/table/{escape_segment(table)}/import{_scoped(database)}",
        _sized(body, cap, "import"),
        decode.wrote("imported"),
        True,
        "import",
    )


def delete_records(
    table: str,
    records_: Iterable[int],
    *,
    database: str | None = None,
    cap: int = DEFAULT_MAX_BYTES,
) -> Op[WriteResult]:
    """`POST /table/{t}/delete`.

    A `POST` rather than a `DELETE` because the ids arrive in the body: a `DELETE` carrying one
    is legal but widely mishandled by proxies, and a URL long enough to name a batch is not.

    Idempotent - clearing a bit twice clears it once - with one caveat worth knowing: the
    `deleted` count on a repeat is the count of what that attempt cleared, which after a
    successful-but-unacknowledged first attempt is smaller than the batch.
    """
    return Op(
        "POST",
        f"/table/{escape_segment(table)}/delete{_scoped(database)}",
        _sized(render_records(records_), cap, "delete"),
        decode.wrote("deleted"),
        True,
        "delete",
    )


# --------------------------------------------------------------------------------------------
# Schema and DDL.
#
# None of these is marked idempotent, even though `POST /table/{t}` and `POST /database/{d}`
# answer happily to a repeat. In a cluster a DDL that was written in full may have landed as
# `partially_applied`, and sending it again turns one thing to inspect into two. The default is
# the conservative one; a caller who knows better calls it again themselves.
# --------------------------------------------------------------------------------------------


def schema() -> Op[Schema]:
    return Op("GET", "/schema", b"", decode.schema, True, "schema")


def create_table(table: str, *, engine: str | None = None) -> Op[int]:
    """`POST /table/{t}?engine=`.

    `engine` is passed through unvalidated. `crates/big-bin/src/client/mod.rs` states the
    policy for `--engine` and it holds here: an engine name this client has never heard of is a
    matter between the caller and the server.
    """
    return Op(
        "POST",
        f"/table/{escape_segment(table)}{query_string({'engine': engine})}",
        b"",
        decode.created_table,
        False,
        "create_table",
    )


def drop_table(table: str) -> Op[str]:
    return Op(
        "DELETE",
        f"/table/{escape_segment(table)}",
        b"",
        decode.dropped,
        False,
        "drop_table",
    )


def create_field(
    table: str,
    field: str,
    *,
    kind: str,
    bit_depth: int | None = None,
    scale: int | None = None,
) -> Op[int]:
    """`POST /table/{t}/field/{f}?kind=&bit_depth=&scale=`.

    `kind` is the **write** vocabulary, which is not the one `/schema` answers in: a signed
    integer is `signed` here and comes back `signedint`. Passed through unvalidated so a kind
    the engine gains later needs no release of this client; `config.FIELD_KINDS_WRITE` lists
    what `parse_kind` accepts today, for autocompletion.
    """
    return Op(
        "POST",
        f"/table/{escape_segment(table)}/field/{escape_segment(field)}"
        + query_string({"kind": kind, "bit_depth": bit_depth, "scale": scale}),
        b"",
        decode.created_field,
        False,
        "create_field",
    )


def drop_field(table: str, field: str) -> Op[str]:
    return Op(
        "DELETE",
        f"/table/{escape_segment(table)}/field/{escape_segment(field)}",
        b"",
        decode.dropped,
        False,
        "drop_field",
    )


def create_database(database: str) -> Op[bool]:
    """`POST /database/{d}` - `{"created": false}` when it was already there."""
    return Op(
        "POST",
        f"/database/{escape_segment(database)}",
        b"",
        decode.created_database,
        False,
        "create_database",
    )


def drop_database(database: str, *, cascade: bool = False) -> Op[str]:
    """`DELETE /database/{d}?cascade=true`.

    Without `cascade` a database holding tables is a `409`, which is the refusal you want: the
    two-word difference between an empty database and a full one is not one to guess at.
    """
    return Op(
        "DELETE",
        f"/database/{escape_segment(database)}" + query_string({"cascade": cascade or None}),
        b"",
        decode.dropped,
        False,
        "drop_database",
    )
