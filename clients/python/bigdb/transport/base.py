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

"""The seam between what this client knows and how it gets bytes.

`RawResponse` is the whole of the boundary. Everything about the protocol - how a request is
built, how an answer is read, which failures are retried, how a value becomes SQL - lives above
it and is written once. Everything about sockets lives below it and is written twice, once for
`http.client` and once for `asyncio`, because there is no way to write it once.

The proof that the seam held is `tests/test_transport_conformance.py`, which runs one table of
cases against both implementations. Anything that behaves differently is a bug in one of them,
not a difference in design.
"""

from __future__ import annotations

import base64
from dataclasses import dataclass
from typing import Protocol

from ..errors import ProtocolError

__all__ = [
    "MAX_HEADERS",
    "MAX_HEADER_BYTES",
    "MAX_LINE",
    "RawResponse",
    "Transport",
    "basic",
    "refuse_oversized_headers",
]

#: The server's own reader limits, from `crates/big-http/src/request.rs`. Applied to responses
#: here so this client is not a softer target than the thing it talks to.
MAX_LINE = 8 * 1024
MAX_HEADERS = 64
MAX_HEADER_BYTES = 16 * 1024


@dataclass(frozen=True, slots=True)
class RawResponse:
    """One answer, before anything knows what it means."""

    status: int
    #: Names lowercased, arrival order kept - the same normalisation
    #: `crates/big-http/src/request.rs` does on the way in.
    headers: tuple[tuple[str, str], ...]
    body: bytes

    def header(self, name: str) -> str | None:
        """The first header by that name, matching the server's own first-occurrence-wins."""
        wanted = name.lower()
        return next((value for key, value in self.headers if key == wanted), None)

    @property
    def content_type(self) -> str:
        """The media type without its parameters, lowercased.

        This is how `Client.sql` tells a JSON result set from a `FORMAT CSV` one. Deciding by
        the answer rather than by scanning the statement keeps a SQL parser out of this client.
        """
        raw = self.header("content-type") or ""
        return raw.split(";", 1)[0].strip().lower()

    @property
    def request_id(self) -> str | None:
        """`X-Request-Id`, which the server puts on every response.

        Worth carrying into every exception: a body at 500 or above is redacted to a sentence
        that says to read the server log, and this is what joins the two.
        """
        return self.header("x-request-id")

    @property
    def retry_after(self) -> float | None:
        """`Retry-After` in seconds, when the server sent a number it can read as one."""
        raw = self.header("retry-after")
        if raw is None:
            return None
        try:
            return max(0.0, float(raw.strip()))
        except ValueError:
            # An HTTP-date is legal in the header and the server never sends one. Treating it
            # as absent lets the ordinary backoff apply rather than failing over a header.
            return None

    @property
    def closing(self) -> bool:
        """Whether the server said it is closing this connection after answering."""
        return (self.header("connection") or "").lower() == "close"

    @property
    def ok(self) -> bool:
        return 200 <= self.status < 300


class Transport(Protocol):
    """One request, one answer. The only thing the two implementations promise."""

    def roundtrip(self, method: str, target: str, body: bytes) -> RawResponse: ...

    def close(self) -> None: ...


def basic(user: str, password: str) -> str:
    """An `Authorization: Basic` value.

    The server only reads `Basic` - `Bearer` is treated as absent
    (`crates/big-http/src/request.rs::basic`), and the token flavours were removed outright.
    Split on the first colon per RFC 7617, so a password holding one is fine and a username
    holding one is not; that is the server's rule and this side must not be more permissive.
    """
    if ":" in user:
        raise ValueError("a username cannot contain a colon: the header splits on the first one")
    return "Basic " + base64.b64encode(f"{user}:{password}".encode()).decode("ascii")


def refuse_oversized_headers(headers: tuple[tuple[str, str], ...]) -> None:
    """The server's own header bounds, applied to its answers.

    Here rather than in either transport because the two would otherwise disagree: `http.client`
    enforces its own, much larger limits (100 headers, 64 KiB a line) and the asyncio reader
    enforces whatever `limit` it was opened with. A conformance test that sends an oversized
    header should get the same refusal from both, and this is what makes that true.

    The point is not memory - an answer this large is already in hand by the time the sync
    transport sees it - but agreement: a proxy rewriting responses should be reported, not
    tolerated by one transport and refused by the other.
    """
    if len(headers) > MAX_HEADERS:
        raise ProtocolError(
            f"this answer carries {len(headers)} headers; big never sends more than {MAX_HEADERS}"
        )
    # `+ 4` for the `: ` and the CRLF each line costs on the wire.
    total = sum(len(name) + len(value) + 4 for name, value in headers)
    if total > MAX_HEADER_BYTES:
        raise ProtocolError(
            f"this answer carries {total} bytes of headers; big never sends more than "
            f"{MAX_HEADER_BYTES}"
        )
