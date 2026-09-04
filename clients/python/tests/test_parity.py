"""The two client surfaces are one surface.

`AsyncClient` is `Client` with awaits in it. Nothing enforces that except this file, so it
compares the method sets and the signatures rather than trusting that whoever edits one will
remember the other.
"""

from __future__ import annotations

import asyncio
import inspect
import json

import pytest
from fake import FakeServer, reply

import bigdb
from bigdb import AsyncClient, Client
from bigdb.transport.base import RawResponse

#: Named differently on purpose: `close` is blocking, `aclose` is not, and PEP 8's convention
#: for an async teardown is the `a` prefix.
RENAMED = {"close": "aclose"}


def public(cls):
    return {
        name
        for name, member in inspect.getmembers(cls, callable)
        if not name.startswith("_") and getattr(member, "__qualname__", "").startswith(cls.__name__)
    }


def test_the_two_clients_have_the_same_methods():
    sync = {RENAMED.get(name, name) for name in public(Client)}
    assert sync == public(AsyncClient)


@pytest.mark.parametrize("name", sorted(public(Client) - {"close"}))
def test_each_method_takes_the_same_arguments(name):
    """Same names, same order, same defaults - so code ports between them by adding `await`."""
    sync = inspect.signature(getattr(Client, name))
    async_ = inspect.signature(getattr(AsyncClient, name))
    assert list(sync.parameters) == list(async_.parameters), name
    for param, value in sync.parameters.items():
        assert value.default == async_.parameters[param].default, f"{name}.{param}"
        assert value.kind == async_.parameters[param].kind, f"{name}.{param}"


def test_every_async_method_is_actually_async():
    for name in public(AsyncClient) - {"aclose"}:
        member = getattr(AsyncClient, name)
        assert inspect.iscoroutinefunction(member) or inspect.isasyncgenfunction(member), name


def test_the_constructors_agree():
    assert list(inspect.signature(Client).parameters) == list(
        inspect.signature(AsyncClient).parameters
    )


# --------------------------------------------------------------------------------------------
# And the async client actually works, end to end, against the fake server.
# --------------------------------------------------------------------------------------------


def run(coro):
    return asyncio.new_event_loop().run_until_complete(coro)


def test_the_async_client_reads_an_answer():
    body = json.dumps({"columns": ["n"], "rows": [[2]]}).encode()
    with FakeServer([reply(200, body)]) as server:

        async def go():
            async with AsyncClient(server.addr) as db:
                return await db.sql("SELECT count(*) FROM tx")

        result = run(go())
    assert result.rows == ((2,),)
    assert result.scalar() == 2


def test_the_async_client_pages_records_over_one_connection():
    pages = [
        reply(200, json.dumps({"records": [0, 1], "next": 1}).encode()),
        reply(200, json.dumps({"records": [2], "next": None}).encode()),
    ]
    with FakeServer(pages, max_connections=1) as server:

        async def go():
            async with AsyncClient(server.addr) as db:
                return [r async for r in db.iter_records("tx", page=2)]

        assert run(go()) == [0, 1, 2]
    assert server.connections == 1


def test_the_async_client_raises_the_same_exceptions_the_sync_one_does():
    refusal = json.dumps({"error": "no such table", "code": "unknown_table"}).encode()
    with FakeServer([reply(404, refusal)]) as server:

        async def go():
            async with AsyncClient(server.addr) as db:
                await db.sql("SELECT 1 FROM nope")

        with pytest.raises(bigdb.NotFound) as caught:
            run(go())
    assert caught.value.code == "unknown_table"


def test_the_async_client_retries_what_never_arrived():
    ok_body = json.dumps({"imported": 1}).encode()
    # First connection is dropped without an answer; because `/import` is idempotent the
    # client sends it again on a new connection.
    with FakeServer([b"", reply(200, ok_body)]) as server:

        async def go():
            async with AsyncClient(server.addr, config=bigdb.Config(first_backoff=0.0)) as db:
                return await db.import_facts("tx", [bigdb.Fact("amount", 0, 1)])

        assert run(go()).written == 1
    assert len(server.requests) == 2


def test_raw_response_is_what_both_sides_speak():
    # The seam itself: neither client knows anything about sockets, only about this.
    assert RawResponse(200, (), b"").ok
