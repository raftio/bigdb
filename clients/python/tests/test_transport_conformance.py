"""One table of cases, run against both transports.

This file is the proof that the seam in `transport/base.py` held. If the sync and async
transports ever behave differently here, that is a bug in one of them rather than a difference
in design - so the parametrisation is over the transport itself, not two copies of the tests.
"""

from __future__ import annotations

import asyncio
import socket
import time

import pytest
from fake import FakeServer, reply

from bigdb.address import Address
from bigdb.config import Config
from bigdb.errors import NotSent, ProtocolError, Unknown
from bigdb.transport.aio import AsyncTransport
from bigdb.transport.sync import SyncTransport

SHORT = Config(connect_timeout=2.0, io_timeout=2.0)


def drive(kind, address, config=SHORT, **kw):
    """A transport of either kind behind one blocking calling convention.

    The async one is driven on **one** event loop held for the life of the driver, not through
    `asyncio.run` per call. That is not a convenience: a kept-alive stream belongs to the loop
    that opened it, so a fresh loop per request would quietly defeat the very keep-alive these
    tests exist to prove.
    """
    if kind is SyncTransport:
        transport = SyncTransport(address, config=config, **kw)

        class Sync:
            def roundtrip(self, method, target, body=b""):
                return transport.roundtrip(method, target, body)

            def close(self):
                transport.close()

        return Sync()

    loop = asyncio.new_event_loop()
    transport = loop.run_until_complete(_make_async(address, config, kw))

    class Async:
        def roundtrip(self, method, target, body=b""):
            return loop.run_until_complete(transport.roundtrip(method, target, body))

        def close(self):
            loop.run_until_complete(transport.close())
            loop.close()

    return Async()


async def _make_async(address, config, kw):
    """Built inside the loop, because `asyncio.Lock` binds to the running one."""
    return AsyncTransport(address, config=config, **kw)


BOTH = pytest.mark.parametrize("kind", [SyncTransport, AsyncTransport], ids=["sync", "async"])


@BOTH
def test_an_ordinary_exchange(kind):
    with FakeServer([reply(200, b'{"count":2}')]) as server:
        t = drive(kind, Address.parse(server.addr))
        raw = t.roundtrip("POST", "/table/tx/query", b"Count(All())")
        t.close()
    assert raw.status == 200
    assert raw.body == b'{"count":2}'
    assert raw.content_type == "application/json"
    assert raw.request_id == "test-1"


@BOTH
def test_the_request_carries_what_the_server_needs(kind):
    with FakeServer([reply()]) as server:
        t = drive(kind, Address.parse(server.addr), user="alice", password="s3cret")
        t.roundtrip("POST", "/sql", b"SELECT 1")
        t.close()
    sent = server.requests[0]
    assert sent.startswith(b"POST /sql HTTP/1.1\r\n")
    # Keep-alive is opt-in on this server: without this header every request costs a TCP
    # handshake, and nothing errors to say so.
    assert b"Connection: keep-alive\r\n" in sent
    assert b"Content-Length: 8\r\n" in sent
    assert b"Authorization: Basic YWxpY2U6czNjcmV0\r\n" in sent
    assert sent.endswith(b"\r\n\r\nSELECT 1")


@BOTH
def test_no_credential_means_no_authorization_header(kind):
    with FakeServer([reply()]) as server:
        t = drive(kind, Address.parse(server.addr))
        t.roundtrip("GET", "/health")
        t.close()
    assert b"Authorization" not in server.requests[0]


@BOTH
def test_one_connection_serves_many_requests(kind):
    """The direct port of `stocked(1)` in contrib/big-message/tests/producer.rs.

    The server accepts exactly one connection. A transport that opened a socket per request
    would block for ever on the second, so this makes keep-alive a proof rather than a
    coincidence.
    """
    with FakeServer([reply() for _ in range(5)], max_connections=1) as server:
        t = drive(kind, Address.parse(server.addr))
        for _ in range(5):
            assert t.roundtrip("GET", "/health").status == 200
        t.close()
    assert server.connections == 1
    assert len(server.requests) == 5


@BOTH
def test_a_connection_is_retired_at_the_request_cap(kind):
    """Ours is 900 against the server's 1000; here it is 2, so the test is quick."""
    config = Config(connect_timeout=2.0, io_timeout=2.0, keepalive_requests=2)
    with FakeServer([reply() for _ in range(4)]) as server:
        t = drive(kind, Address.parse(server.addr), config=config)
        for _ in range(4):
            t.roundtrip("GET", "/health")
        t.close()
    # Retired after every second request, so four requests took two connections.
    assert server.connections == 2


@BOTH
def test_a_connection_close_from_the_server_is_honoured(kind):
    with FakeServer([reply(connection="close"), reply()], close_after_each=True) as server:
        t = drive(kind, Address.parse(server.addr))
        assert t.roundtrip("GET", "/health").closing
        # The next request must open a new connection rather than write onto the closed one.
        assert t.roundtrip("GET", "/health").status == 200
        t.close()
    assert server.connections == 2


@BOTH
def test_a_chunked_answer_is_refused_rather_than_decoded(kind):
    """big never sends it; a chunked answer means something is rewriting the response."""
    body = b"5\r\nhello\r\n0\r\n\r\n"
    head = (
        b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n"
        b"Transfer-Encoding: chunked\r\nConnection: keep-alive\r\n\r\n"
    )
    with FakeServer([head + body]) as server:
        t = drive(kind, Address.parse(server.addr))
        with pytest.raises(ProtocolError, match="Transfer-Encoding"):
            t.roundtrip("GET", "/health")
        t.close()


@BOTH
def test_a_body_shorter_than_content_length_is_an_unknown_outcome(kind):
    """The Python equivalent of read_exact: a truncated answer is not a shorter answer."""
    with FakeServer([reply(200, b'{"imported":3}', content_length=999)]) as server:
        t = drive(kind, Address.parse(server.addr))
        with pytest.raises(Unknown):
            t.roundtrip("POST", "/sql", b"INSERT INTO t VALUES (1)")
        t.close()


@BOTH
def test_a_connection_dropped_before_the_status_line_is_an_unknown_outcome(kind):
    """The request was written in full. Committed-then-died is indistinguishable from this."""
    with FakeServer([b""]) as server:
        t = drive(kind, Address.parse(server.addr))
        with pytest.raises(Unknown):
            t.roundtrip("POST", "/sql", b"INSERT INTO t VALUES (1)")
        t.close()


@BOTH
def test_a_refused_connection_provably_never_arrived(kind):
    # **A socket that never listened**, rather than a `FakeServer` that was closed. A listener
    # on its way out can still accept - which is a connection that reaches somebody, reads the
    # request in full and drops it, and that is `Unknown` rather than the `NotSent` this test
    # is about. Nothing can ever be accepted on a socket that was never listening.
    probe = socket.socket()
    probe.bind(("127.0.0.1", 0))
    port = probe.getsockname()[1]
    probe.close()
    addr = Address.parse(f"127.0.0.1:{port}")
    t = drive(kind, addr)
    with pytest.raises(NotSent):
        t.roundtrip("GET", "/health")
    t.close()


@BOTH
def test_a_first_line_that_is_not_http_is_a_protocol_error(kind):
    with FakeServer([b"not http at all\r\n\r\n"]) as server:
        t = drive(kind, Address.parse(server.addr))
        with pytest.raises((ProtocolError, Unknown)):
            t.roundtrip("GET", "/health")
        t.close()


@BOTH
def test_a_503_is_returned_for_the_caller_to_decide_about(kind):
    """The transport does not retry - `retry.decide` does, one layer up."""
    with FakeServer(
        [reply(503, b'{"error":"busy","code":"server_busy"}', extra="Retry-After: 1\r\n")]
    ) as server:
        t = drive(kind, Address.parse(server.addr))
        raw = t.roundtrip("GET", "/health")
        t.close()
    assert raw.status == 503
    assert raw.retry_after == 1.0


@BOTH
def test_an_empty_body_response_is_fine(kind):
    with FakeServer([reply(200, b"")]) as server:
        t = drive(kind, Address.parse(server.addr))
        assert t.roundtrip("GET", "/health").body == b""
        t.close()


# --------------------------------------------------------------------------------------------
# Malformed answers that are not I/O failures. Both transports must refuse them the same way,
# and neither may leave a half-read connection in the pool for the next request to misread.
# --------------------------------------------------------------------------------------------


@BOTH
def test_a_negative_content_length_is_refused(kind):
    head = (
        b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n"
        b"Content-Length: -1\r\nConnection: keep-alive\r\n\r\n"
    )
    with FakeServer([head]) as server:
        t = drive(kind, Address.parse(server.addr))
        with pytest.raises((ProtocolError, Unknown)):
            t.roundtrip("GET", "/health")
        t.close()


@BOTH
def test_a_header_line_longer_than_the_server_ever_sends_is_refused(kind):
    """8 KiB is `big_http::MAX_LINE`; a longer line means something else is answering.

    It must not escape as a raw `asyncio.LimitOverrunError`: that is neither `NotSent` nor
    `Unknown`, so the retry policy could not classify it, and the oversized bytes would be
    left sitting in the reader for the next request to read as its own answer.
    """
    head = (
        b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n"
        b"X-Huge: " + b"z" * 20000 + b"\r\nContent-Length: 0\r\nConnection: keep-alive\r\n\r\n"
    )
    with FakeServer([head]) as server:
        t = drive(kind, Address.parse(server.addr))
        with pytest.raises((ProtocolError, Unknown)):
            t.roundtrip("GET", "/health")
        t.close()


@BOTH
def test_a_missing_content_length_is_refused(kind):
    head = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n"
    with FakeServer([head]) as server:
        t = drive(kind, Address.parse(server.addr))
        with pytest.raises((ProtocolError, Unknown)):
            t.roundtrip("GET", "/health")
        t.close()


@BOTH
def test_io_timeout_governs_the_read_and_not_just_the_connect(kind):
    """`io_timeout` has to be a knob that works, on both transports.

    `http.client` puts its constructor timeout on the socket, where it governs reads too - so
    a transport that only passed `connect_timeout` would leave `io_timeout` doing nothing, and
    a caller following this client's own advice ("raise io_timeout for a slow query") would
    still time out at the connect value.
    """
    # A server that accepts, reads the request, and then says nothing for far longer than
    # `io_timeout` but well under `connect_timeout`.
    with FakeServer([b""], hold=1.5) as server:
        config = Config(connect_timeout=10.0, io_timeout=0.3)
        t = drive(kind, Address.parse(server.addr), config=config)
        started = time.monotonic()
        with pytest.raises(Unknown):
            t.roundtrip("POST", "/sql", b"SELECT 1")
        elapsed = time.monotonic() - started
        t.close()
    assert elapsed < 1.2, f"gave up after {elapsed:.2f}s; io_timeout of 0.3s did not govern"
