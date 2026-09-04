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

"""The awaitable client.

`client.Client` with `await` in it. Same method names, same keyword arguments, same answers -
`tests/test_parity.py` asserts the two surfaces are equal so they cannot drift, and the twelve
lines of `_run` below are the only logic this file does not share with the blocking one.

The connection belongs to the loop that opened it. An `AsyncClient` built on one loop and used
on another will not reuse its connection, which is worth knowing before wrapping one in a
thread pool.
"""

from __future__ import annotations

import asyncio
from collections.abc import AsyncIterator, Awaitable, Callable, Iterable

from . import ops
from .address import Address
from .client import _warn_if_exposed
from .config import DEFAULT_ADDR, DEFAULT_CONFIG, DEFAULT_PAGE, Config
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
from .transport.aio import AsyncTransport

__all__ = ["AsyncClient"]


class AsyncClient:
    """One server, one connection, one event loop."""

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
        self._transport = AsyncTransport(self._address, config=config, user=user, password=password)

    # ----------------------------------------------------------------------------------------
    # The loop. `Client._run` is these lines without the awaits; the *policy* is shared.
    # ----------------------------------------------------------------------------------------

    async def _run(self, op: ops.Op[T]) -> T:
        attempt = 0
        while True:
            try:
                raw = await self._transport.roundtrip(op.method, op.target, op.body)
                return op.decode(raw)
            except Exception as failure:
                delay = decide(self._policy, attempt, failure, idempotent=op.idempotent)
                if delay is None:
                    raise
                await asyncio.sleep(delay)
                attempt += 1

    def _db(self, database: str | None) -> str | None:
        return database if database is not None else self._database

    # ----------------------------------------------------------------------------------------
    # Data plane.
    # ----------------------------------------------------------------------------------------

    async def sql(self, statement: str, *, database: str | None = None) -> SqlResult | TextResult:
        """One statement. See `Client.sql`."""
        return await self._run(
            ops.sql(statement, database=self._db(database), cap=self._config.max_bytes)
        )

    async def query(
        self,
        table: str,
        pql: str,
        *,
        database: str | None = None,
        after: int | None = None,
        limit: int | None = None,
    ) -> PqlAnswer:
        """One PQL call. See `Client.query`."""
        return await self._run(
            ops.query(
                table,
                pql,
                database=self._db(database),
                after=after,
                limit=limit,
                cap=self._config.max_bytes,
            )
        )

    async def records(
        self,
        table: str,
        *,
        after: int | None = None,
        limit: int | None = None,
        database: str | None = None,
    ) -> RecordPage:
        """One page of record ids. See `Client.records`."""
        return await self._run(
            ops.records(table, database=self._db(database), after=after, limit=limit)
        )

    async def iter_records(
        self,
        table: str,
        *,
        page: int = DEFAULT_PAGE,
        database: str | None = None,
    ) -> AsyncIterator[int]:
        """Every record id, a page at a time. See `Client.iter_records`."""
        after: int | None = None
        while True:
            got = await self.records(table, after=after, limit=page, database=database)
            for record in got.records:
                yield record
            if got.next is None:
                return
            after = got.next

    async def import_facts(
        self,
        table: str,
        facts: Iterable[Fact] | bytes | str,
        *,
        database: str | None = None,
    ) -> WriteResult:
        """One batch of facts. Idempotent - see `Client.import_facts`."""
        return await self._run(
            ops.import_facts(table, facts, database=self._db(database), cap=self._config.max_bytes)
        )

    async def import_stream(
        self,
        table: str,
        facts: Iterable[Fact],
        *,
        database: str | None = None,
        max_bytes: int | None = None,
        on_chunk: Callable[[int, WriteResult], Awaitable[None] | None] | None = None,
    ) -> WriteResult:
        """Any number of facts, chunked under the body cap.

        `on_chunk` may be a coroutine function here - a caller checkpointing an offset is
        usually writing it somewhere, and forcing that to be blocking inside an async client
        would be the wrong default.
        """
        cap = max_bytes if max_bytes is not None else self._config.max_bytes
        written = 0
        missed: list[str] = []
        for offset, body in chunk_facts(facts, max_bytes=cap):
            result = await self.import_facts(table, body, database=database)
            written += result.written
            missed.extend(result.missed)
            if on_chunk is not None:
                outcome = on_chunk(offset, result)
                if asyncio.iscoroutine(outcome):
                    await outcome
        return WriteResult(written=written, missed=tuple(dict.fromkeys(missed)))

    async def delete_records(
        self,
        table: str,
        records: Iterable[int],
        *,
        database: str | None = None,
    ) -> WriteResult:
        """Remove records by id. See `Client.delete_records`."""
        return await self._run(
            ops.delete_records(
                table, records, database=self._db(database), cap=self._config.max_bytes
            )
        )

    # ----------------------------------------------------------------------------------------
    # Schema and DDL.
    # ----------------------------------------------------------------------------------------

    async def schema(self) -> Schema:
        return await self._run(ops.schema())

    async def create_table(self, table: str, *, engine: str | None = None) -> int:
        return await self._run(ops.create_table(table, engine=engine))

    async def drop_table(self, table: str) -> str:
        return await self._run(ops.drop_table(table))

    async def create_field(
        self,
        table: str,
        field: str,
        *,
        kind: str,
        bit_depth: int | None = None,
        scale: int | None = None,
    ) -> int:
        """`kind` is the write vocabulary - `signed`, not `signedint`. See `Client`."""
        return await self._run(
            ops.create_field(table, field, kind=kind, bit_depth=bit_depth, scale=scale)
        )

    async def drop_field(self, table: str, field: str) -> str:
        return await self._run(ops.drop_field(table, field))

    async def create_database(self, database: str) -> bool:
        return await self._run(ops.create_database(database))

    async def drop_database(self, database: str, *, cascade: bool = False) -> str:
        return await self._run(ops.drop_database(database, cascade=cascade))

    # ----------------------------------------------------------------------------------------
    # Probes.
    # ----------------------------------------------------------------------------------------

    async def health(self) -> bool:
        return await self._run(ops.health())

    async def ready(self) -> Ready:
        return await self._run(ops.ready())

    # ----------------------------------------------------------------------------------------

    async def aclose(self) -> None:
        await self._transport.close()

    async def __aenter__(self) -> AsyncClient:
        return self

    async def __aexit__(self, *exc: object) -> None:
        await self.aclose()

    def __repr__(self) -> str:
        return f"AsyncClient({str(self._address)!r})"
