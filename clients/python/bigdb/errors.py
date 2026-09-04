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

"""One exception tree, serving two vocabularies.

PEP 249 requires a database module to expose nine exception classes with prescribed names and
a prescribed inheritance. A client that also wants exceptions a caller can read has two
choices: two trees with a translation layer between them, or one tree where PEP 249's classes
are the bases and this client's are the leaves.

**It is one tree.** `except bigdb.NotFound` and `except bigdb.dbapi.ProgrammingError` catch the
same object, nothing is translated, and nothing is re-raised - so a traceback names the thing
that actually happened rather than the thing a translation layer decided it resembled.

# Why the split is by what the caller can do

`NotSent` and `Unknown` are two classes rather than one class with a flag, because they are the
two different things a caller can do about a failed write. This is `contrib/big-message`'s
`Failure` enum, ported with its reasoning:

- Everything up to *and including* the write and flush is `NotSent`. The server holds fewer
  bytes than `Content-Length` promised, and `big_http::Request::read` calls `read_exact` on the
  declared length - so it cannot parse a statement from them and will never run one. Retrying
  is provably safe.
- Everything from reading the status line onward is `Unknown`. A read timeout, a reset and an
  EOF where a response should be are identical whether the server died before running the
  statement or after committing it. There is no safe recovery, so it is never retried.

# Why the mapping is on `code` and never on prose

`big_http::json::error` writes `{"error": <sentence>, "code": <code>}`. The sentence is written
for a person and is improved when someone finds a better one; the code is the stable half. A
client that matched on prose would break when the prose got better.
"""

from __future__ import annotations

from typing import Any, Final

__all__ = [
    "BadGateway",
    "BadRequest",
    "BigError",
    "ClientClosed",
    "ConfigError",
    "Conflict",
    "DataError",
    "DatabaseError",
    "Error",
    "Forbidden",
    "InsecureCredentialWarning",
    "IntegrityError",
    "InterfaceError",
    "InternalError",
    "NotFound",
    "NotSent",
    "NotSupportedError",
    "OperationalError",
    "PartiallyApplied",
    "PayloadTooLarge",
    "ProgrammingError",
    "ProtocolError",
    "QueryTimeout",
    "RequestTooLarge",
    "ServerError",
    "ServerFault",
    "TransportError",
    "Unauthenticated",
    "Unavailable",
    "Unknown",
    "Unprocessable",
    "Unsupported",
    "ValueRefused",
    "Warning",
    "classify",
]


# --------------------------------------------------------------------------------------------
# PEP 249's nine, which are the bases of everything below.
# --------------------------------------------------------------------------------------------


class Warning(Exception):
    """PEP 249 requires the name. Nothing in this client raises it."""


class Error(Exception):
    """The root of everything this client raises."""


#: The name to reach for when `Error` would read as the builtin.
BigError = Error


class InterfaceError(Error):
    """Something about the client or the connection, rather than the database."""


class DatabaseError(Error):
    """Something the database said or would say."""


class DataError(DatabaseError):
    """A value that will not survive the trip."""


class OperationalError(DatabaseError):
    """The database is there but cannot do this now."""


class IntegrityError(DatabaseError):
    """A constraint on the data refused it."""


class InternalError(DatabaseError):
    """The database found itself in a state it does not explain."""


class ProgrammingError(DatabaseError):
    """The request is wrong and will be wrong again."""


class NotSupportedError(DatabaseError):
    """This dialect or this route does not have the thing being asked for."""


# --------------------------------------------------------------------------------------------
# Things that happen before, or instead of, an answer.
# --------------------------------------------------------------------------------------------


class ConfigError(InterfaceError):
    """The client was configured in a way that has no meaning.

    A scheme this client does not speak, or a `ca_file` on a plaintext address - the same two
    refusals `bigctl` makes in `crates/big-bin/src/client/http.rs::transport`.
    """


class ValueRefused(DataError):
    """A value with no spelling in this dialect, named before a batch carries it.

    Refusing here rather than at the server is the whole point: the server would refuse a batch
    of eight thousand rows and its sentence would name none of them.
    """


class RequestTooLarge(DataError):
    """A body past the cap, refused before a socket is opened.

    `big_http::MAX_BODY` is checked against `Content-Length` *before* the body is read
    (`crates/big-http/src/request.rs`), so an over-large request is a clean 413 rather than a
    partial write. Raising it here saves the round trip and, for the fact routes, names which
    line pushed it over - `at` - because a producer that has sent a million of them needs to
    know which one rather than that one exists.
    """

    def __init__(self, length: int, cap: int, at: int | None = None) -> None:
        where = "" if at is None else f" (reached at item {at})"
        super().__init__(f"a body of {length} bytes is past the {cap}-byte cap{where}")
        self.length = length
        self.cap = cap
        self.at = at


class TransportError(InterfaceError):
    """A request that did not come back as an answer."""

    def __init__(self, message: str, *, operation: str = "", target: str = "") -> None:
        super().__init__(message)
        self.operation = operation
        self.target = target


class NotSent(TransportError):
    """The request provably never reached the server. Safe to retry, whatever it was."""


class Unknown(TransportError):
    """The request was written in full and the outcome is not known.

    Never retried on anything but an idempotent route. For a route that allocates - an
    `INSERT` over `/sql` - retrying turns "possibly written once" into "possibly written twice",
    which is a worse thing to be unsure about.
    """

    def __str__(self) -> str:
        return f"{super().__str__()}; the request was written in full and the outcome is not known"


class ProtocolError(InterfaceError):
    """Not an HTTP response this client reads.

    Its own class because it says something different about the deployment - a proxy in the
    path, or a build mismatch - but its *outcome* is as unknown as `Unknown`, and the retry
    policy treats the two identically.
    """


class InsecureCredentialWarning(UserWarning):
    """A password on its way over a plaintext connection that is not loopback.

    Basic auth is base64, which is not encryption. Refusing would break `127.0.0.1:7654`, which
    is a perfectly ordinary way to run this - so loopback is silent and everything else warns
    once. Suppressible with `Config(warn_on_plaintext_credentials=False)`.
    """


# --------------------------------------------------------------------------------------------
# The server answered, and said no.
# --------------------------------------------------------------------------------------------

#: Codes whose failure is transient: the map moved, an owner was unreachable, or the request was
#: shed before a worker read its body. From `crates/big-cluster/src/error.rs::status`.
RETRYABLE_CODES: Final[frozenset[str]] = frozenset(
    {
        "stale_route",
        "range_moving",
        "owner_unreachable",
        "not_serving",
        "schema_leader_unreachable",
        "server_busy",
        "busy_authenticating",
    }
)


class ServerError(DatabaseError):
    """A refusal the server wrote, carrying the code to branch on."""

    def __init__(
        self,
        *,
        status: int,
        code: str,
        message: str,
        request_id: str | None = None,
        retry_after: float | None = None,
        challenge: str | None = None,
    ) -> None:
        super().__init__(message)
        self.status = status
        self.code = code
        self.message = message
        self.request_id = request_id
        self.retry_after = retry_after
        self.challenge = challenge

    def __str__(self) -> str:
        # The code first, because it is the half that is stable enough to branch on. The request
        # id always, because a body at 500 or above is redacted to a sentence that says to read
        # the server log (`crates/big-http/src/status.rs`), and this is what joins the two.
        head = f"{self.status} {self.code}"
        body = f": {self.message}" if self.message else ""
        tail = f" (X-Request-Id: {self.request_id})" if self.request_id else ""
        return f"{head}{body}{tail}"

    @property
    def retryable(self) -> bool:
        """Whether sending this again could get a different answer.

        `status == 503` is the belt to the code's braces and matches what
        `contrib/big-message/src/producer.rs` does. It is safe even for a non-idempotent write:
        a 503 is a refusal with nothing written.
        """
        return self.status == 503 or self.code in RETRYABLE_CODES


class BadRequest(ServerError, ProgrammingError):
    """400."""


class Unauthenticated(ServerError, OperationalError):
    """401. `challenge` carries the `WWW-Authenticate` header."""


class Forbidden(ServerError, OperationalError):
    """403."""


class NotFound(ServerError, ProgrammingError):
    """404.

    A typo in a target and a table that does not exist both arrive here - there is no 405, the
    router matches method and path as one tuple. `code` is what tells them apart:
    `no_such_route` against `unknown_table` or `unknown_database`.
    """


class Conflict(ServerError, IntegrityError):
    """409."""


class PayloadTooLarge(ServerError, DataError):
    """413. The server's own `request_too_large`, when a body got past the client-side cap."""


class Unprocessable(ServerError, ProgrammingError):
    """422. Read the request, could not act on it - `unknown_field`, `not_pageable`."""


class ClientClosed(ServerError, OperationalError):
    """499. The client hung up; nginx's code, which the server borrows."""


class ServerFault(ServerError, InternalError):
    """500. The body is redacted; `request_id` is how to find out what happened."""


class PartiallyApplied(ServerFault):
    """500 `partially_applied`, and the one 500 with a procedure attached.

    A batch spanning two shard ranges is two commits with no cross-range atomicity. **Not
    retryable** - `crates/big-cluster/src/error.rs` puts it at 500 for exactly that reason.
    Catching this class is the repair path: inspect what landed, then fix it.
    """


class Unsupported(ServerError, NotSupportedError):
    """501."""


class BadGateway(ServerError, OperationalError):
    """502. Version skew between peers; not retryable."""


class Unavailable(ServerError, OperationalError):
    """503. Retryable, and `Retry-After` says how soon."""


class QueryTimeout(ServerError, OperationalError):
    """504.

    Not retryable, and not the same thing as a socket timeout. A socket timeout is `Unknown` -
    we do not know whether the server committed. This is a definite answer that the statement
    did not complete, and `crates/big-http/src/status.rs` says why sending it again is pointless:
    a query that passed its deadline will pass it again. What has to change is the query.
    """


#: Status to class. The primary mapping: a status this build has never seen still lands on
#: `ServerError` rather than on nothing.
_BY_STATUS: Final[dict[int, type[ServerError]]] = {
    400: BadRequest,
    401: Unauthenticated,
    403: Forbidden,
    404: NotFound,
    409: Conflict,
    413: PayloadTooLarge,
    422: Unprocessable,
    499: ClientClosed,
    500: ServerFault,
    501: Unsupported,
    502: BadGateway,
    503: Unavailable,
    504: QueryTimeout,
}

#: Code to class, for the few codes whose class is not implied by the status. Deliberately
#: small: a code this build has never heard of should fall through to the status class rather
#: than need a client release.
_BY_CODE: Final[dict[str, type[ServerError]]] = {
    "partially_applied": PartiallyApplied,
    "request_too_large": PayloadTooLarge,
}


def envelope(body: bytes) -> tuple[str, str]:
    """`(code, message)` out of an error body, without ever raising.

    The server writes `{"error": <sentence>, "code": <code>}`
    (`crates/big-http/src/json.rs`). `message` is accepted as a fallback key because the
    console's type declarations use it, and a body that is not JSON at all - a proxy's error
    page, an empty 502 - still has to produce an exception rather than a second failure while
    building the first.
    """
    import json

    try:
        decoded = json.loads(body)
    except (ValueError, UnicodeDecodeError):
        return "", body.decode("utf-8", "replace").strip()[:200]
    if not isinstance(decoded, dict):
        return "", ""
    code = decoded.get("code")
    message = decoded.get("error", decoded.get("message"))
    return (
        code if isinstance(code, str) else "",
        message if isinstance(message, str) else "",
    )


def classify(
    status: int,
    body: bytes,
    *,
    request_id: str | None = None,
    retry_after: float | None = None,
    challenge: str | None = None,
) -> ServerError:
    """The exception for one refusal.

    `code` first for the handful it decides, then status, then `ServerError`. Never prose.
    """
    code, message = envelope(body)
    cls: type[ServerError] = _BY_CODE.get(code) or _BY_STATUS.get(status) or ServerError
    return cls(
        status=status,
        code=code,
        message=message,
        request_id=request_id,
        retry_after=retry_after,
        challenge=challenge,
    )


def __getattr__(name: str) -> Any:  # pragma: no cover - import-time convenience only
    raise AttributeError(f"module {__name__!r} has no attribute {name!r}")
