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

"""PEP 249, so `pandas.read_sql` and the tools that expect a driver work.

A thin layer over `Client`. It adds three things and nothing else: `?` binding, a `description`
inferred from the answer, and `executemany` that sends one statement instead of a thousand.

# What this database does not have, and what PEP 249 says to do about it

`commit()` is a no-op and `rollback()` raises `NotSupportedError`. That is what the
specification prescribes for a database without transactions, and this route has none:
`crates/big-http/src/routes/query.rs::sql` says it outright - the route answers one statement
and remembers nothing, which is also why there is no `USE` and why `database` is a connection
argument here rather than a statement.

`lastrowid` is always `None`. A record id is the address a bit is written at and
`shard_of(record)` is which node owns it - the engine's own coordinate rather than a key the
data chose. `contrib/big-message`'s readme makes the same point at more length: the id is not in
this API because it is not the caller's to know.

`threadsafety = 1`: the module can be shared, connections cannot. `Client` owns one keep-alive
socket with a request counter and no lock, so two threads on one connection would interleave
requests on one stream. Adding a lock would serialise the calls without saying so.
"""

from __future__ import annotations

import datetime
from collections.abc import Iterable, Iterator, Sequence
from typing import Any, Final

from .client import Client
from .config import DEFAULT_ADDR, DEFAULT_CONFIG, Config
from .errors import (
    DatabaseError,
    DataError,
    Error,
    IntegrityError,
    InterfaceError,
    InternalError,
    NotSupportedError,
    OperationalError,
    ProgrammingError,
    Warning,
)
from .results import SqlResult, TextResult
from .sql import bind, literal, placeholders

__all__ = [
    "BINARY",
    "DATETIME",
    "NUMBER",
    "ROWID",
    "STRING",
    "Binary",
    "Connection",
    "Cursor",
    "DataError",
    "DatabaseError",
    "Date",
    "DateFromTicks",
    "Error",
    "IntegrityError",
    "InterfaceError",
    "InternalError",
    "NotSupportedError",
    "OperationalError",
    "ProgrammingError",
    "Time",
    "TimeFromTicks",
    "Timestamp",
    "TimestampFromTicks",
    "Warning",
    "apilevel",
    "connect",
    "paramstyle",
    "threadsafety",
]

apilevel: Final[str] = "2.0"

#: Threads may share the module, but not connections. See the module docstring.
threadsafety: Final[int] = 1

#: `?`. The dialect has no server-side parameters, so every one is substituted textually - and
#: `?` is the only marker the lexer can never produce in a valid statement outside a string or a
#: comment. `%` *is* a token (`Tok::Arith`), so `format`/`pyformat` would collide with modulo and
#: with `LIKE 'a%b'`. See `sql.bind` for the full argument.
paramstyle: Final[str] = "qmark"


# --------------------------------------------------------------------------------------------
# Type objects and constructors. PEP 249 requires all of them.
# --------------------------------------------------------------------------------------------


class _TypeObject:
    """Compares equal to every type code it covers."""

    def __init__(self, name: str, *codes: str) -> None:
        self.name = name
        self.codes = frozenset(codes)

    def __eq__(self, other: object) -> bool:
        if isinstance(other, _TypeObject):
            return self.name == other.name
        return other in self.codes

    def __hash__(self) -> int:
        return hash(self.name)

    def __repr__(self) -> str:
        return self.name


STRING: Final = _TypeObject("STRING", "set", "mutex", "text")
BINARY: Final = _TypeObject("BINARY", "binary")
NUMBER: Final = _TypeObject("NUMBER", "int", "signedint", "decimal", "float32", "float64")
DATETIME: Final = _TypeObject("DATETIME", "date", "datetime", "timequantum")
ROWID: Final = _TypeObject("ROWID", "record")

Date = datetime.date
Time = datetime.time
Timestamp = datetime.datetime


def DateFromTicks(ticks: float) -> datetime.date:
    return datetime.date.fromtimestamp(ticks)


def TimeFromTicks(ticks: float) -> datetime.time:
    return datetime.datetime.fromtimestamp(ticks).time()


def TimestampFromTicks(ticks: float) -> datetime.datetime:
    return datetime.datetime.fromtimestamp(ticks)


def Binary(value: bytes) -> bytes:
    """Required by the specification. This dialect has no binary literal, so a value that
    reaches a statement through here is refused by `sql.literal` rather than mangled."""
    return bytes(value)


# --------------------------------------------------------------------------------------------


def connect(
    dsn: str = DEFAULT_ADDR,
    *,
    user: str | None = None,
    password: str | None = None,
    database: str | None = None,
    ca_file: str | None = None,
    insecure_skip_verify: bool = False,
    config: Config = DEFAULT_CONFIG,
) -> Connection:
    """One connection. `dsn` is an address - `host:port` or `https://host:port`."""
    return Connection(
        Client(
            dsn,
            user=user,
            password=password,
            database=database,
            ca_file=ca_file,
            insecure_skip_verify=insecure_skip_verify,
            config=config,
        )
    )


class Connection:
    """PEP 249's connection, wrapping a `Client`."""

    # The optional extension: the exception classes reachable from the connection, so code
    # holding one does not have to import the module to catch from it.
    Warning = Warning
    Error = Error
    InterfaceError = InterfaceError
    DatabaseError = DatabaseError
    DataError = DataError
    OperationalError = OperationalError
    IntegrityError = IntegrityError
    InternalError = InternalError
    ProgrammingError = ProgrammingError
    NotSupportedError = NotSupportedError

    def __init__(self, client: Client) -> None:
        self.client = client
        self._closed = False

    def close(self) -> None:
        self._closed = True
        self.client.close()

    def commit(self) -> None:
        """A no-op. Every statement is its own commit; there is no session to close."""
        self._check()

    def rollback(self) -> None:
        """Always refused.

        PEP 249 says a database with no transaction support should raise here rather than
        pretend. Silently doing nothing would let code believe it had undone a write.
        """
        raise NotSupportedError(
            "this route answers one statement and remembers nothing, so there is nothing to "
            "roll back"
        )

    def cursor(self) -> Cursor:
        self._check()
        return Cursor(self)

    def _check(self) -> None:
        if self._closed:
            raise InterfaceError("this connection is closed")

    def __enter__(self) -> Connection:
        return self

    def __exit__(self, *exc: object) -> None:
        self.close()


class Cursor:
    """PEP 249's cursor. One statement, one result; there are no result sets to step through."""

    def __init__(self, connection: Connection) -> None:
        self.connection = connection
        self.arraysize = 1
        self._rows: list[tuple[Any, ...]] = []
        self._at = 0
        self._description: tuple[tuple[Any, ...], ...] | None = None
        self._rowcount = -1
        self._closed = False

    # ----------------------------------------------------------------------------------------

    @property
    def description(self) -> tuple[tuple[Any, ...], ...] | None:
        """Seven-tuples, of which this server can fill in two.

        `name` comes from the answer's `columns`. `type_code` is inferred from the first
        non-null cell in each column, because the result set does not carry types. The other
        five are `None`: the server does not report them, and inventing them would be worse
        than admitting it.
        """
        return self._description

    @property
    def rowcount(self) -> int:
        """Rows returned, or rows written for an `INSERT`. `-1` before `execute`."""
        return self._rowcount

    @property
    def lastrowid(self) -> None:
        """Always `None`. The record id is the engine's coordinate, not a key handed out."""
        return None

    # ----------------------------------------------------------------------------------------

    def execute(self, operation: str, parameters: Sequence[Any] | None = None) -> Cursor:
        """One statement, with `?` placeholders bound to `parameters`."""
        self._check()
        return self._send(bind(operation, parameters))

    def executemany(
        self,
        operation: str,
        seq_of_parameters: Iterable[Sequence[Any]],
    ) -> Cursor:
        """Many rows, in as few statements as the caps allow.

        **Not a loop.** The server commits once per request, so a thousand small statements
        cost a thousand commits: `contrib/big-message/readme.md` measures 250 statements at
        3.36 s against 5 at 1.43 s over the same four million facts. When the statement ends
        with a `VALUES` tuple, the tuples are appended into one statement under
        `config.max_bytes` and `config.max_rows` - the same two ceilings `big-message`'s
        batching uses, and bytes is the one an ordinary batch reaches.

        Anything that is not shaped that way falls back to one `execute` per row, which is
        correct and slow rather than clever and wrong.
        """
        self._check()
        rows = [tuple(p) for p in seq_of_parameters]
        if not rows:
            self._rows, self._at, self._rowcount, self._description = [], 0, 0, None
            return self

        tail = _values_tail(operation)
        if tail is None:
            written = 0
            for row in rows:
                self._send(bind(operation, row))
                written += max(self._rowcount, 0)
            self._rowcount = written
            return self

        head, template = tail
        config = self.connection.client._config
        written = 0
        batch: list[str] = []
        size = len(head)
        for row in rows:
            rendered = _fill(template, row)
            cost = len(rendered) + 1
            if batch and (size + cost > config.max_bytes or len(batch) >= config.max_rows):
                self._send(head + ",".join(batch))
                written += max(self._rowcount, 0)
                batch, size = [], len(head)
            batch.append(rendered)
            size += cost
        if batch:
            self._send(head + ",".join(batch))
            written += max(self._rowcount, 0)
        self._rowcount = written
        return self

    def _send(self, statement: str) -> Cursor:
        result = self.connection.client.sql(statement)
        if isinstance(result, TextResult):
            # Parsing it back into rows would be a second CSV reader disagreeing with the
            # writer in `crates/big-sql/src/shape.rs` - and a caller who asked for CSV asked
            # because something downstream reads CSV.
            raise NotSupportedError(
                "a FORMAT TSV/CSV statement answers with text, which a cursor has no shape "
                "for; use Client.sql() and read the TextResult"
            )
        self._absorb(result)
        return self

    def _absorb(self, result: SqlResult) -> None:
        self._rows = list(result.rows)
        self._at = 0
        self._description = tuple(
            (name, _type_code(result, index), None, None, None, None, None)
            for index, name in enumerate(result.columns)
        )
        # An INSERT answers `{"columns":["inserted"],"rows":[[n]]}`, and reporting the count as
        # `rowcount` is what a DB-API caller expects from a write.
        if result.columns == ("inserted",) and len(result.rows) == 1:
            cell = result.rows[0][0]
            self._rowcount = cell if isinstance(cell, int) else len(self._rows)
        else:
            self._rowcount = len(self._rows)

    # ----------------------------------------------------------------------------------------

    def fetchone(self) -> tuple[Any, ...] | None:
        self._check()
        if self._at >= len(self._rows):
            return None
        row = self._rows[self._at]
        self._at += 1
        return row

    def fetchmany(self, size: int | None = None) -> list[tuple[Any, ...]]:
        self._check()
        take = self.arraysize if size is None else size
        rows = self._rows[self._at : self._at + take]
        self._at += len(rows)
        return rows

    def fetchall(self) -> list[tuple[Any, ...]]:
        self._check()
        rows = self._rows[self._at :]
        self._at = len(self._rows)
        return rows

    def nextset(self) -> None:
        """Always `None`: one statement, one result."""
        self._check()
        return None

    def setinputsizes(self, sizes: Sequence[Any]) -> None:
        """Required by the specification; nothing here is predeclared."""
        self._check()

    def setoutputsize(self, size: int, column: int | None = None) -> None:
        """Required by the specification; nothing here is streamed."""
        self._check()

    def close(self) -> None:
        self._closed = True
        self._rows = []

    def _check(self) -> None:
        if self._closed:
            raise InterfaceError("this cursor is closed")
        self.connection._check()

    def __iter__(self) -> Iterator[tuple[Any, ...]]:
        while True:
            row = self.fetchone()
            if row is None:
                return
            yield row

    def __enter__(self) -> Cursor:
        return self

    def __exit__(self, *exc: object) -> None:
        self.close()


# --------------------------------------------------------------------------------------------


def _type_code(result: SqlResult, index: int) -> Any:
    """The type object for a column, from the first cell that is not null.

    A column that is null all the way down gets `None` rather than a guess: `STRING` would be
    a lie a reader could act on, and there is nothing in the answer to justify it.
    """
    for row in result.rows:
        if index >= len(row):
            continue
        cell = row[index]
        if cell is None:
            continue
        if isinstance(cell, bool):
            return NUMBER
        if isinstance(cell, (int, float)):
            return NUMBER
        if isinstance(cell, str):
            return STRING
        if isinstance(cell, list):
            # A `topK` list. It renders as text for anything that prints it.
            return STRING
        return None
    return None


def _values_tail(operation: str) -> tuple[str, str] | None:
    """`(head, template)` when the statement ends in one `VALUES` tuple, else `None`.

    Uses the same scan `bind` uses, so the two agree about where a string literal ends. The
    conditions are deliberately narrow - one tuple, at the very end, nothing after it - because
    a wrong guess here would append tuples into the middle of somebody's statement.
    """
    upper = operation.upper()
    at = upper.rfind("VALUES")
    if at == -1:
        return None
    # `VALUES` must be outside every string and comment, which is what the scanner decides.
    outside = {offset for offset, _ in _scan_offsets(operation)}
    if not all(offset in outside for offset in range(at, at + 6)):
        return None
    # ...and it must be the whole word, not the tail of a longer one. `rfind` is a substring
    # search, so `SELECT * FROM myvalues (?)` would otherwise look like a `VALUES` clause and
    # `executemany` would append tuples after a table name.
    if at > 0 and (operation[at - 1].isalnum() or operation[at - 1] == "_"):
        return None
    after = at + 6
    if after < len(operation) and (operation[after].isalnum() or operation[after] == "_"):
        return None

    rest = operation[at + 6 :]
    stripped = rest.strip()
    if not stripped.startswith("(") or not stripped.endswith(")"):
        return None

    # Exactly one tuple: the first `(` must close at the last `)`.
    #
    # **The parens are counted with the scanner, not by walking characters.** A `(` or `)`
    # inside a string literal is text, and a naive count lets two imbalanced literals cancel
    # each other out: `VALUES ('(', ?), (?, ')')` is genuinely two tuples, but the `(` in the
    # first literal and the `)` in the last make a raw count return to zero exactly at the end,
    # so it would be accepted as one. `executemany` would then render each caller row against
    # the whole two-tuple template and write one logical row as two mismatched ones.
    begin = at + 6 + (len(rest) - len(rest.lstrip()))
    outside_tail = {offset for offset, _ in _scan_offsets(operation) if offset >= begin}
    depth = 0
    for index, char in enumerate(stripped):
        if begin + index not in outside_tail:
            continue  # inside a literal or a comment, so it is text rather than structure
        if char == "(":
            depth += 1
        elif char == ")":
            depth -= 1
            if depth == 0 and index != len(stripped) - 1:
                return None
    if depth != 0:
        return None
    return operation[: at + 6] + " ", stripped


def _fill(template: str, row: Sequence[Any]) -> str:
    """One `(?, ?)` tuple with this row's values in it."""
    spans = placeholders(template)
    if len(spans) != len(row):
        raise ProgrammingError(
            f"this statement has {len(spans)} placeholders and a row of {len(row)} was given"
        )
    out: list[str] = []
    cursor = 0
    for index, at in enumerate(spans):
        out.append(template[cursor:at])
        out.append(literal(row[index]))
        cursor = at + 1
    out.append(template[cursor:])
    return "".join(out)


def _scan_offsets(operation: str) -> list[tuple[int, str]]:
    from .sql import _scan

    return _scan(operation)
