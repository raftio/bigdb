"""A server that replays raw bytes, so transport behaviour can be asserted at the byte level.

Not an HTTP server: it reads enough of each request to know when one ended and writes back
whatever the script says next. That is the point - a real server would hide exactly the things
these tests are for (a truncated body, a chunked answer, a connection dropped mid-exchange).
"""

from __future__ import annotations

import contextlib
import socket
import threading
import time


class FakeServer:
    """Replays a script of raw responses, and records the request bytes it was sent."""

    def __init__(
        self,
        script: list[bytes],
        *,
        max_connections: int | None = None,
        close_after_each: bool = False,
        hold: float = 0.0,
    ) -> None:
        self.script = list(script)
        #: When set, the listener accepts at most this many connections. `max_connections=1` is
        #: how keep-alive is proved rather than assumed: a client that opened a socket per
        #: request would hang on the second one. It is the port of `stocked(1)` in
        #: `contrib/big-message/tests/producer.rs`.
        self.max_connections = max_connections
        self.close_after_each = close_after_each
        #: Seconds to wait after reading a request before answering, for timeout tests.
        self.hold = hold
        self.requests: list[bytes] = []
        self.connections = 0
        self._sock = socket.socket()
        self._sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        self._sock.bind(("127.0.0.1", 0))
        self._sock.listen(8)
        # **A timeout, so `close` can actually stop it.** The accept loop below blocks in
        # `accept()`; closing the socket from another thread does not reliably wake a thread
        # already parked there, so without this the listener can outlive `close()` and accept a
        # connection on its way out - which is a client getting a reset where it was promised a
        # refusal.
        self._sock.settimeout(0.05)
        self.port = self._sock.getsockname()[1]
        self._stop = threading.Event()
        self._thread = threading.Thread(target=self._serve, daemon=True)
        self._thread.start()

    @property
    def addr(self) -> str:
        return f"127.0.0.1:{self.port}"

    def _serve(self) -> None:
        while not self._stop.is_set():
            if self.max_connections is not None and self.connections >= self.max_connections:
                return
            try:
                conn, _ = self._sock.accept()
            except TimeoutError:
                # Nothing arrived in this slice. Round again and re-read `_stop`.
                continue
            except OSError:
                return
            self.connections += 1
            threading.Thread(target=self._handle, args=(conn,), daemon=True).start()

    def _handle(self, conn: socket.socket) -> None:
        with conn:
            while not self._stop.is_set():
                request = _read_request(conn)
                if request is None:
                    return
                self.requests.append(request)
                if self.hold:
                    time.sleep(self.hold)
                if not self.script:
                    return
                reply = self.script.pop(0)
                if reply == b"":
                    # An empty script entry means "drop the connection without answering" -
                    # the ambiguous failure, where the request was written in full.
                    return
                try:
                    conn.sendall(reply)
                except OSError:
                    return
                if self.close_after_each:
                    return

    def close(self) -> None:
        """Stops listening, and does not return until that is true.

        The join is the point. `close` used to set the flag and shut the socket, leaving the
        accept thread parked in `accept()`; a client connecting in that window was accepted by
        a server that was supposed to be gone, read in full, and dropped - which reads to the
        client as `Unknown` rather than the `NotSent` a closed port owes it.
        """
        self._stop.set()
        self._thread.join(timeout=2.0)
        with contextlib.suppress(OSError):
            self._sock.close()

    def __enter__(self) -> FakeServer:
        return self

    def __exit__(self, *exc: object) -> None:
        self.close()


def _read_request(conn: socket.socket) -> bytes | None:
    """Headers, then exactly as many body bytes as `Content-Length` promised."""
    buffer = b""
    while b"\r\n\r\n" not in buffer:
        chunk = conn.recv(4096)
        if not chunk:
            return None
        buffer += chunk
    head, _, rest = buffer.partition(b"\r\n\r\n")
    length = 0
    for line in head.split(b"\r\n")[1:]:
        name, _, value = line.partition(b":")
        if name.strip().lower() == b"content-length":
            length = int(value.strip())
    while len(rest) < length:
        chunk = conn.recv(4096)
        if not chunk:
            return None
        rest += chunk
    return head + b"\r\n\r\n" + rest


def reply(
    status: int = 200,
    body: bytes = b"{}",
    *,
    content_type: str = "application/json",
    extra: str = "",
    connection: str = "keep-alive",
    content_length: int | None = None,
) -> bytes:
    """One well-formed response, with every part overridable so a test can malform it."""
    length = len(body) if content_length is None else content_length
    reason = {200: "OK", 400: "Bad Request", 503: "Service Unavailable"}.get(status, "Status")
    head = (
        f"HTTP/1.1 {status} {reason}\r\n"
        f"Content-Type: {content_type}\r\n"
        f"Content-Length: {length}\r\n"
        f"Connection: {connection}\r\n"
        f"X-Request-Id: test-1\r\n"
        f"{extra}"
        f"\r\n"
    )
    return head.encode() + body
