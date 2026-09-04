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

"""One keep-alive connection, over `asyncio` streams.

# Why the HTTP is written by hand here

The alternative is a dependency, and this package has none. The protocol that has to be read is
`Content-Length`-framed with no chunked encoding and no trailers - `crates/big-http/src/
response.rs::encode` writes `Content-Length` unconditionally - so a complete reader is about
ninety lines and every branch in it is reachable from a test. That is a better trade than a
transitive tree for a client whose whole claim is that it has none.

The limits below are the server's own, from `crates/big-http/src/request.rs`. They are here so
this client is not a softer target than the thing it talks to: a peer that can make `bigd` refuse
an oversized header should not be able to make this allocate one.

The `NotSent` / `Unknown` boundary is the same as in `sync.py` and is in the same place: through
`drain()` is `NotSent`, from the first byte read onward is `Unknown`.
"""

from __future__ import annotations

import asyncio
import ssl

from ..address import Address
from ..config import Config
from ..errors import NotSent, ProtocolError, Unknown
from ..lifetime import Lifetime
from .base import (
    MAX_HEADER_BYTES,
    MAX_HEADERS,
    MAX_LINE,
    RawResponse,
    basic,
    refuse_oversized_headers,
)

__all__ = ["AsyncTransport"]


class AsyncTransport:
    """Holds at most one connection, guarded by a lock.

    The lock is needed where `Client`'s thread rule is not: two coroutines on one event loop
    can interleave on one stream without any of the ceremony threads need, so `roundtrip` has
    to be a critical section or two requests end up spliced together.
    """

    def __init__(
        self,
        address: Address,
        *,
        config: Config,
        user: str | None = None,
        password: str | None = None,
        user_agent: str = "bigdb-python",
    ) -> None:
        self._address = address
        self._config = config
        self._user_agent = user_agent
        self._auth = basic(user, password or "") if user is not None else None
        self._life = Lifetime(
            max_requests=config.keepalive_requests,
            max_idle=config.keepalive_idle,
        )
        self._io: tuple[asyncio.StreamReader, asyncio.StreamWriter] | None = None
        self._lock = asyncio.Lock()

    # ----------------------------------------------------------------------------------------

    async def roundtrip(self, method: str, target: str, body: bytes) -> RawResponse:
        async with self._lock:
            return await self._exchange(method, target, body)

    async def _exchange(self, method: str, target: str, body: bytes) -> RawResponse:
        loop = asyncio.get_running_loop()
        if self._io is not None and self._life.stale(loop.time()):
            await self._drop()
        if self._io is None:
            self._io = await self._open()
        _, writer = self._io

        request = self._head(method, target, len(body)).encode("latin-1") + body
        try:
            writer.write(request)
            await asyncio.wait_for(writer.drain(), self._config.io_timeout)
        except (OSError, asyncio.TimeoutError, ssl.SSLError) as e:
            await self._drop()
            raise NotSent(f"{e or type(e).__name__}", operation=method, target=target) from e

        # ---- the request is on the wire in full; everything past here is Unknown ----
        try:
            raw = await asyncio.wait_for(self._read(), self._config.io_timeout)
        except ProtocolError:
            await self._drop()
            raise
        except (asyncio.LimitOverrunError, ValueError) as e:
            # Neither an I/O failure nor a shape this reader understands: an oversized line
            # (`LimitOverrunError`, which is *not* an `OSError`) or a length that is not a
            # usable number. Dropping is not optional - `readuntil` deliberately leaves the
            # oversized chunk in the buffer for reuse, so keeping this connection would let
            # the next request read the remains of this one as its own answer.
            await self._drop()
            raise ProtocolError(f"this answer is not one big sends: {e}") from e
        except (OSError, asyncio.TimeoutError, asyncio.IncompleteReadError, ssl.SSLError) as e:
            await self._drop()
            raise Unknown(f"{e or type(e).__name__}", operation=method, target=target) from e

        now = loop.time()
        self._life.record(now)
        if raw.closing or self._life.stale(now):
            await self._drop()
        return raw

    async def close(self) -> None:
        async with self._lock:
            await self._drop()

    # ----------------------------------------------------------------------------------------

    async def _read(self) -> RawResponse:
        """The status line, the headers, and exactly as many body bytes as were promised."""
        reader, _ = self._io if self._io is not None else (None, None)
        assert reader is not None  # only reached inside `_exchange`, which opened it

        line = await reader.readuntil(b"\r\n")
        parts = line.split(b" ", 2)
        if len(parts) < 2 or not parts[0].startswith(b"HTTP/"):
            raise ProtocolError(f"this is not an HTTP response: {line[:60]!r}")
        try:
            status = int(parts[1])
        except ValueError as e:
            raise ProtocolError(f"this response has no status code: {line[:60]!r}") from e

        headers: list[tuple[str, str]] = []
        header_bytes = 0
        while True:
            raw_line = await reader.readuntil(b"\r\n")
            if raw_line == b"\r\n":
                break
            header_bytes += len(raw_line)
            if len(headers) >= MAX_HEADERS or header_bytes > MAX_HEADER_BYTES:
                raise ProtocolError("this response has more headers than big ever sends")
            name, sep, value = raw_line.decode("latin-1").partition(":")
            if not sep:
                raise ProtocolError(f"a header with no colon in it: {raw_line[:60]!r}")
            headers.append((name.strip().lower(), value.strip()))

        refuse_oversized_headers(tuple(headers))
        found = dict(headers)
        if "transfer-encoding" in found:
            raise ProtocolError(
                "this answer used Transfer-Encoding, which big does not send; "
                "something is rewriting the response"
            )
        if "content-length" not in found:
            raise ProtocolError("this answer carries no Content-Length, which big always sends")
        try:
            length = int(found["content-length"])
        except ValueError as e:
            raise ProtocolError(f"a Content-Length that is not a number: {found!r}") from e
        if length < 0:
            # `readexactly` answers a negative length with a bare `ValueError`, which says
            # nothing about whose fault it is. Refusing here names the header instead.
            raise ProtocolError(f"a negative Content-Length: {found['content-length']!r}")

        # `readexactly` raises `IncompleteReadError` on a short body, which is the point: a
        # truncated answer is not a shorter answer, and for a write it is an unknown outcome.
        body = await reader.readexactly(length) if length else b""
        return RawResponse(status=status, headers=tuple(headers), body=body)

    def _head(self, method: str, target: str, length: int) -> str:
        auth = f"Authorization: {self._auth}\r\n" if self._auth is not None else ""
        return (
            f"{method} {target} HTTP/1.1\r\n"
            f"Host: {self._address.authority}\r\n"
            f"User-Agent: {self._user_agent}\r\n"
            f"{auth}"
            f"Content-Length: {length}\r\n"
            # Opt-in on this server, and invisible when missing.
            f"Connection: keep-alive\r\n"
            f"\r\n"
        )

    async def _open(self) -> tuple[asyncio.StreamReader, asyncio.StreamWriter]:
        """A new connection, with any failure here provably `NotSent`."""
        self._life.reset()
        context = self._context() if self._address.tls else None
        try:
            return await asyncio.wait_for(
                asyncio.open_connection(
                    self._address.host,
                    self._address.port,
                    ssl=context,
                    # The certificate is issued to a name, and `example:7654` is not one.
                    server_hostname=self._address.host if context is not None else None,
                    limit=MAX_LINE,
                ),
                self._config.connect_timeout,
            )
        except (OSError, asyncio.TimeoutError, ssl.SSLError, ValueError) as e:
            raise NotSent(f"could not reach {self._address}: {e or type(e).__name__}") from e

    def _context(self) -> ssl.SSLContext:
        context = ssl.create_default_context(cafile=self._address.ca_file)
        if self._address.insecure_skip_verify:
            context.check_hostname = False
            context.verify_mode = ssl.CERT_NONE
        return context

    async def _drop(self) -> None:
        if self._io is not None:
            _, writer = self._io
            self._io = None
            try:
                writer.close()
                await writer.wait_closed()
            except (OSError, ssl.SSLError):
                # Closing something already gone is not a failure worth reporting.
                pass
        self._life.reset()
