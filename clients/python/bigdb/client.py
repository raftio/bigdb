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

"""The blocking client.

Thin on purpose. Every request is built by `ops`, every answer read by `decode`, every retry
decided by `retry` - so what is left here is the loop that turns an `Op` into an answer, plus
the two conveniences (`import_stream`, `iter_records`) that need more than one request.

`AsyncClient` in `bigdb.aio` is this file with `await` in it, and `tests/test_parity.py`
asserts the two have the same methods so they cannot drift.
"""

from __future__ import annotations

import time
import warnings
from collections.abc import Callable, Iterable, Iterator

from . import ops
from .address import Address
from .config import DEFAULT_ADDR, DEFAULT_CONFIG, DEFAULT_PAGE, Config
from .errors import InsecureCredentialWarning
from .facts import Fact, chunk_facts
from .ops import T
from .results import (
    PqlAnswer,
    Ready,
    RecordPage,
    Schema,
    SqlResult,
    TextResult,
    WriteResult,
)
from .retry import RetryPolicy, decide
from .transport.sync import SyncTransport

__all__ = ["Client"]


class Client:
    """One server, one connection, one thread.

    **Not thread-safe, deliberately.** The connection carries a request counter and no lock, so
    two threads would interleave requests on one stream. A lock here would serialise the calls
    without saying so; a client per thread is the honest shape, and it is what
    `dbapi.threadsafety = 1` reports.
    """

    def __init__(
        self,
        addr: str = DEFAULT_ADDR,
        *,
        user: str | None = None,
        password: str | None = None,
        database: str | None = None,
        ca_file: str | None = None,
        insecure_skip_verify: bool = False,
        config: Config = DEFAULT_CONFIG,
    ) -> None:
        self._address = Address.parse(
            addr, ca_file=ca_file, insecure_skip_verify=insecure_skip_verify
        )
        self._config = config
        self._database = database
        self._policy = RetryPolicy(
            retries=config.retries,
            first_backoff=config.first_backoff,
            max_backoff=config.max_backoff,
        )
        _warn_if_exposed(self._address, user, config)
        self._transport = SyncTransport(self._address, config=config, user=user, password=password)

    # ----------------------------------------------------------------------------------------
    # The loop. `AsyncClient` differs from this file in these twelve lines and nowhere else.
    # ----------------------------------------------------------------------------------------

    def _run(self, op: ops.Op[T]) -> T:
        attempt = 0
        while True:
            try:
                return op.decode(self._transport.roundtrip(op.method, op.target, op.body))
            except Exception as failure:
                delay = decide(self._policy, attempt, failure, idempotent=op.idempotent)
                if delay is None:
                    raise
                time.sleep(delay)
                attempt += 1

    def _db(self, database: str | None) -> str | None:
        """Per-call database, falling back to the client's."""
        return database if database is not None else self._database

    # ----------------------------------------------------------------------------------------
    # Data plane.
    # ----------------------------------------------------------------------------------------

    def sql(self, statement: str, *, database: str | None = None) -> SqlResult | TextResult:
        """One statement. No session, no `USE` - pass `database=` instead.

        Returns a `SqlResult` for the ordinary JSON answer, and a `TextResult` when the
        statement asked for `FORMAT TSV|TSVWithNames|CSV|CSVWithNames`. Which one is decided by
        the answer's content type, never by reading the statement.
        """
        return self._run(
            ops.sql(statement, database=self._db(database), cap=self._config.max_bytes)
        )

    def query(
        self,
        table: str,
        pql: str,
        *,
        database: str | None = None,
        after: int | None = None,
        limit: int | None = None,
    ) -> PqlAnswer:
        """One PQL call. `after`/`limit` on an answer that is not records is a 422."""
        return self._run(
            ops.query(
                table,
                pql,
                database=self._db(database),
                after=after,
                limit=limit,
                cap=self._config.max_bytes,
            )
        )

    def records(
        self,
        table: str,
        *,
        after: int | None = None,
        limit: int | None = None,
        database: str | None = None,
    ) -> RecordPage:
        """One page of record ids. Absent `limit` means all of them."""
        return self._run(ops.records(table, database=self._db(database), after=after, limit=limit))

    def iter_records(
        self,
        table: str,
        *,
        page: int = DEFAULT_PAGE,
        database: str | None = None,
    ) -> Iterator[int]:
        """Every record id, a page at a time.

        `next` is set only when a page came back exactly as long as the limit, so it means
        "there may be more" - the loop stops on the page that proves there are not.
        """
        after: int | None = None
        while True:
            got = self.records(table, after=after, limit=page, database=database)
            yield from got.records
            if got.next is None:
                return
            after = got.next

    def import_facts(
        self,
        table: str,
        facts: Iterable[Fact] | bytes | str,
        *,
        database: str | None = None,
    ) -> WriteResult:
        """One batch of facts, in one request.

        Idempotent: every fact is a bit set at an address the caller chose, so this is the one
        write that is safe to send again after an ambiguous failure. For more than
        `config.max_bytes` of them, use `import_stream`.
        """
        return self._run(
            ops.import_facts(table, facts, database=self._db(database), cap=self._config.max_bytes)
        )

    def import_stream(
        self,
        table: str,
        facts: Iterable[Fact],
        *,
        database: str | None = None,
        max_bytes: int | None = None,
        on_chunk: Callable[[int, WriteResult], None] | None = None,
    ) -> WriteResult:
        """Any number of facts, chunked under the body cap.

        `on_chunk(offset, result)` is called after each chunk lands, where `offset` is the index
        of that chunk's first fact. Recording it is a resume point - the same thing
        `bigctl import --resume` checkpoints - and it is only meaningful because this route is
        idempotent: re-sending from a checkpoint writes the same bits again, which is writing
        them once.
        """
        cap = max_bytes if max_bytes is not None else self._config.max_bytes
        written = 0
        missed: list[str] = []
        for offset, body in chunk_facts(facts, max_bytes=cap):
            result = self.import_facts(table, body, database=database)
            written += result.written
            missed.extend(result.missed)
            if on_chunk is not None:
                on_chunk(offset, result)
        # Deduplicated but order-preserving: the same copy behind for every chunk is one copy
        # behind, and a caller reading this wants the set of nodes, not the count of chunks.
        return WriteResult(written=written, missed=tuple(dict.fromkeys(missed)))

    def delete_records(
        self,
        table: str,
        records: Iterable[int],
        *,
        database: str | None = None,
    ) -> WriteResult:
        """Remove records by id. The whole batch is parsed before any of it is removed."""
        return self._run(
            ops.delete_records(
                table, records, database=self._db(database), cap=self._config.max_bytes
            )
        )

    # ----------------------------------------------------------------------------------------
    # Schema and DDL.
    # ----------------------------------------------------------------------------------------

    def schema(self) -> Schema:
        """Every table this node knows about, with its fields."""
        return self._run(ops.schema())

    def create_table(self, table: str, *, engine: str | None = None) -> int:
        """The new table's id. `engine` is passed through unvalidated."""
        return self._run(ops.create_table(table, engine=engine))

    def drop_table(self, table: str) -> str:
        return self._run(ops.drop_table(table))

    def create_field(
        self,
        table: str,
        field: str,
        *,
        kind: str,
        bit_depth: int | None = None,
        scale: int | None = None,
    ) -> int:
        """The new field's id.

        `kind` is the **write** vocabulary: `signed`, not the `signedint` that `/schema` answers
        with. `bigdb.FIELD_KINDS_WRITE` lists what the server accepts today.
        """
        return self._run(
            ops.create_field(table, field, kind=kind, bit_depth=bit_depth, scale=scale)
        )

    def drop_field(self, table: str, field: str) -> str:
        return self._run(ops.drop_field(table, field))

    def create_database(self, database: str) -> bool:
        """`True` when this call created it, `False` when it was already there."""
        return self._run(ops.create_database(database))

    def drop_database(self, database: str, *, cascade: bool = False) -> str:
        """Without `cascade`, a database holding tables is a 409."""
        return self._run(ops.drop_database(database, cascade=cascade))

    # ----------------------------------------------------------------------------------------
    # Probes. Neither is authenticated.
    # ----------------------------------------------------------------------------------------

    def health(self) -> bool:
        """Whether the process is alive. A constant; it says nothing about the data."""
        return self._run(ops.health())

    def ready(self) -> Ready:
        """Whether the engine answers, and what it is holding."""
        return self._run(ops.ready())

    # ----------------------------------------------------------------------------------------

    def close(self) -> None:
        self._transport.close()

    def __enter__(self) -> Client:
        return self

    def __exit__(self, *exc: object) -> None:
        self.close()

    def __repr__(self) -> str:
        return f"Client({str(self._address)!r})"


def _warn_if_exposed(address: Address, user: str | None, config: Config) -> None:
    """One warning for a password about to cross a network in the clear.

    Basic auth is base64, which is not encryption. Refusing outright would break
    `127.0.0.1:7654`, which is a perfectly ordinary way to run this - so loopback is silent and
    everything else says so once.
    """
    if user is None or address.tls or address.is_loopback:
        return
    if not config.warn_on_plaintext_credentials:
        return
    warnings.warn(
        f"sending a password to {address} in the clear: Basic auth is base64, not encryption. "
        f"Use an https:// address, or pass Config(warn_on_plaintext_credentials=False) to "
        f"silence this.",
        InsecureCredentialWarning,
        stacklevel=3,
    )
