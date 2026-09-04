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

"""One keep-alive connection, over `http.client`.

# Where the boundary is, exactly

Everything up to and including the write is `NotSent`; everything from reading the response
onward is `Unknown`. That line is not a stylistic choice - it is the only place it can be, and
`contrib/big-message/src/http.rs` puts it in the same place for the same reason. A request that
did not finish writing leaves the server holding fewer bytes than `Content-Length` promised, and
`big_http::Request::read` calls `read_exact` on that length, so it cannot parse a statement from
them and will never run one. A request that finished writing and then failed to answer is
indistinguishable from one the server committed before dying.

# Four things `http.client` does that have to be undone or asked for

1. It sends no `Connection` header on HTTP/1.1, and this server's keep-alive is **opt-in**
   (`Request::wants_keep_alive`) - a client that says nothing gets one request and a close. So
   the header is explicit, and a conformance test asserts it is in the bytes.
2. It would happily de-chunk a `Transfer-Encoding` response. The server writes `Content-Length`
   unconditionally (`response.rs::encode`), so anything chunked means a proxy is in the path -
   worth a sentence rather than a silent decode of something this client cannot verify.
3. `HTTPResponse.read()` raises `IncompleteRead` on a truncated body, which is the Python
   equivalent of `read_exact` and the reason the body is read in one call.
4. `RemoteDisconnected` is the ambiguous case. `urllib3` retries it, which would silently
   duplicate an `INSERT`; here it is an `Unknown` and the retry policy decides.
"""

from __future__ import annotations

import contextlib
import http.client
import ssl
import time

from ..address import Address
from ..config import Config
from ..errors import NotSent, ProtocolError, Unknown
from ..lifetime import Lifetime
from .base import RawResponse, basic, refuse_oversized_headers

__all__ = ["SyncTransport"]


class SyncTransport:
    """Holds at most one connection, and replaces it before the server would."""

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
        self._conn: http.client.HTTPConnection | None = None

    # ----------------------------------------------------------------------------------------

    def roundtrip(self, method: str, target: str, body: bytes) -> RawResponse:
        """One exchange, or an exception that says whether it arrived."""
        # Checked *before* writing, where a fresh connect is provably safe. Checking after is
        # where the ambiguity lives.
        if self._conn is not None and self._life.stale(time.monotonic()):
            self._drop()
        if self._conn is None:
            self._conn = self._open()

        try:
            self._conn.request(method, target, body=body, headers=self._headers(len(body)))
        except (OSError, http.client.HTTPException) as e:
            self._drop()
            raise NotSent(f"{e}", operation=method, target=target) from e

        # ---- the request is on the wire in full; everything past here is Unknown ----
        try:
            response = self._conn.getresponse()
            payload = response.read()
        except (OSError, http.client.HTTPException) as e:
            self._drop()
            raise Unknown(f"{e}", operation=method, target=target) from e

        headers = tuple((name.lower(), value) for name, value in response.getheaders())
        if response.getheader("Transfer-Encoding") is not None:
            self._drop()
            raise ProtocolError(
                "this answer used Transfer-Encoding, which big does not send; "
                "something is rewriting the response"
            )
        try:
            refuse_oversized_headers(headers)
        except ProtocolError:
            self._drop()
            raise

        now = time.monotonic()
        self._life.record(now)
        raw = RawResponse(status=response.status, headers=headers, body=payload)
        if raw.closing or self._life.stale(now):
            self._drop()
        return raw

    def close(self) -> None:
        self._drop()

    # ----------------------------------------------------------------------------------------

    def _headers(self, length: int) -> dict[str, str]:
        headers = {
            "Host": self._address.authority,
            "User-Agent": self._user_agent,
            "Content-Length": str(length),
            # Opt-in, and invisible when missing: without it every request costs a TCP
            # handshake and nothing errors.
            "Connection": "keep-alive",
        }
        if self._auth is not None:
            headers["Authorization"] = self._auth
        return headers

    def _open(self) -> http.client.HTTPConnection:
        """A new connection, with any failure here provably `NotSent`.

        **Connected eagerly, and the socket's timeout replaced afterwards.** `http.client`
        passes its constructor `timeout` into `socket.create_connection`, which calls
        `settimeout` on the socket - and that one value then governs every later read and
        write, because nothing in `http.client` sets it again. Left alone, `connect_timeout`
        would silently be the read timeout too and `io_timeout` would be a setting that does
        nothing. That matters because this client's own guidance is to raise `io_timeout` when
        an operator raises `--query-timeout`; a knob that does not work is worse than no knob.
        """
        self._life.reset()
        try:
            conn: http.client.HTTPConnection
            if self._address.tls:
                conn = http.client.HTTPSConnection(
                    self._address.host,
                    self._address.port,
                    timeout=self._config.connect_timeout,
                    context=self._context(),
                )
            else:
                conn = http.client.HTTPConnection(
                    self._address.host,
                    self._address.port,
                    timeout=self._config.connect_timeout,
                )
            # Connect now, under `connect_timeout`, rather than letting the first request do it
            # lazily - which is also what makes a refused connection a `NotSent` raised from
            # here rather than an ambiguous one raised mid-request.
            conn.connect()
            if conn.sock is not None:
                conn.sock.settimeout(self._config.io_timeout)
            return conn
        except (OSError, ValueError, ssl.SSLError) as e:
            raise NotSent(f"could not reach {self._address}: {e}") from e

    def _context(self) -> ssl.SSLContext:
        """A TLS context, verifying unless told in as many words not to.

        The certificate is checked against the host with the port stripped, which
        `http.client` does for us by taking host and port separately - a certificate is issued
        to a name, and `example:7654` is not one.
        """
        context = ssl.create_default_context(cafile=self._address.ca_file)
        if self._address.insecure_skip_verify:
            context.check_hostname = False
            context.verify_mode = ssl.CERT_NONE
        return context

    def _drop(self) -> None:
        if self._conn is not None:
            # Closing a socket that is already gone is not a failure worth reporting; the
            # connection is being thrown away either way.
            with contextlib.suppress(OSError):
                self._conn.close()
            self._conn = None
        self._life.reset()
