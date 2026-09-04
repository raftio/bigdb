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

"""When to throw a connection away, and why it is thrown away early.

A port of `contrib/big-message/src/http.rs`, whose argument is worth restating in full because
it is the reason this module exists at all rather than being three lines inside a transport:

> A keep-alive connection can be closed by the server at the exact moment the client writes onto
> it. The write succeeds - into a socket buffer - and the read then ends having read nothing,
> which is **indistinguishable from a server that ran the statement and died before answering.**

For an idempotent route that is merely annoying. For an allocating `INSERT` there is no safe
recovery from it: the client cannot tell whether to send it again. So the race is made
unreachable rather than handled. The server allows a thousand requests and five seconds of idle
per connection; this retires its own at nine hundred and at three, so the connection is always
replaced *between* requests, where a fresh connect is provably safe.

`stale()` takes the time as an argument rather than reading a clock, so both transports share it
and the tests for it do not sleep.
"""

from __future__ import annotations

from dataclasses import dataclass

from .config import KEEPALIVE_IDLE, KEEPALIVE_REQUESTS

__all__ = ["Lifetime"]


@dataclass(slots=True)
class Lifetime:
    """How much a connection has done, and how long since it last did any of it."""

    max_requests: int = KEEPALIVE_REQUESTS
    max_idle: float = KEEPALIVE_IDLE
    sent: int = 0
    last: float | None = None

    def stale(self, now: float) -> bool:
        """Whether this connection should be replaced before the next request is written.

        Checked *before* writing, never after: after is where the ambiguity lives.
        """
        if self.sent >= self.max_requests:
            return True
        if self.last is None:
            # Opened but never used. Nothing has aged yet.
            return False
        return (now - self.last) >= self.max_idle

    def record(self, now: float) -> None:
        """One completed exchange."""
        self.sent += 1
        self.last = now

    def reset(self) -> None:
        """A new connection starts again from nothing."""
        self.sent = 0
        self.last = None
