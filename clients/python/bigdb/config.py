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

"""Every number this client is tuned by, and the server ceiling each one sits under.

One module, because a constant whose ceiling lives in another file is a constant that drifts
away from it. Each one below names the server value it is under and the margin it leaves; no
other module in this package reads a number that is not defined here.
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Final

__all__ = [
    "DEFAULT_ADDR",
    "DEFAULT_CONFIG",
    "DEFAULT_FIRST_BACKOFF",
    "DEFAULT_IO_TIMEOUT",
    "DEFAULT_MAX_BACKOFF",
    "DEFAULT_MAX_BYTES",
    "DEFAULT_MAX_ROWS",
    "DEFAULT_PAGE",
    "DEFAULT_RETRIES",
    "FIELD_KINDS_READ",
    "FIELD_KINDS_WRITE",
    "KEEPALIVE_IDLE",
    "KEEPALIVE_REQUESTS",
    "MAX_BODY",
    "SERVER_KEEPALIVE_IDLE",
    "SERVER_KEEPALIVE_REQUESTS",
    "TABLE_ENGINES",
    "Config",
    "Credential",
]

#: `crates/big-bin/src/serve.rs` and `crates/big-bin/src/client/args.rs` agree on this one.
DEFAULT_ADDR: Final[str] = "127.0.0.1:7654"

#: `big_http::MAX_BODY`, checked against `Content-Length` before the body is read.
MAX_BODY: Final[int] = 8 << 20

#: The cap this client actually sends under. The one-megabyte margin is the same one
#: `bigctl import`'s `DEFAULT_CHUNK_BYTES` and `big-message`'s `DEFAULT_MAX_BYTES` leave, so a
#: body that fits here fits there.
DEFAULT_MAX_BYTES: Final[int] = 7 << 20

#: `big_sql::MAX_INSERT_ROWS` is 1,000,000. This is the backstop for rows so narrow that seven
#: megabytes still holds an unreasonable number of them; bytes is the cap an ordinary batch
#: reaches, and raising this one on its own changes nothing.
DEFAULT_MAX_ROWS: Final[int] = 900_000

#: `ServerConfig::max_keepalive_requests` and `keepalive_idle`.
SERVER_KEEPALIVE_REQUESTS: Final[int] = 1_000
SERVER_KEEPALIVE_IDLE: Final[float] = 5.0

#: Ours, deliberately under the server's, so the connection is always retired *between*
#: requests by us rather than closed underneath one by the server. See `lifetime.py` for why
#: that race is made unreachable instead of handled.
KEEPALIVE_REQUESTS: Final[int] = 900
KEEPALIVE_IDLE: Final[float] = 3.0

#: Matching the server's own `read_timeout` / `write_timeout`, so the client never gives up
#: before the server would. Raise it with `--query-timeout`: a long query that outruns this
#: turns into `Unknown`, which is the worst way for a slow query to fail.
DEFAULT_IO_TIMEOUT: Final[float] = 30.0

#: How many records `iter_records` asks for at a time. The server has no default limit -
#: absent means "everything" (`crates/big-http/src/json.rs::Page`) - so paging is opting in.
DEFAULT_PAGE: Final[int] = 1_000

DEFAULT_RETRIES: Final[int] = 3
DEFAULT_FIRST_BACKOFF: Final[float] = 0.05
DEFAULT_MAX_BACKOFF: Final[float] = 1.0

#: What `/schema` answers, from `FieldKind`'s `Debug` lowercased
#: (`crates/big-http/src/json.rs` over `crates/big-engine/src/base/field_kind.rs`).
FIELD_KINDS_READ: Final[tuple[str, ...]] = (
    "set",
    "mutex",
    "bool",
    "int",
    "decimal",
    "timequantum",
    "signedint",
    "float32",
    "float64",
    "date",
    "datetime",
)

#: What `?kind=` accepts, from `routes/mod.rs::parse_kind`. **Not the same list**: a signed
#: integer is written `signed` and read back `signedint`. These are for autocompletion only -
#: `create_field` passes `kind` through unvalidated, so a kind the engine gains later needs no
#: release of this client.
FIELD_KINDS_WRITE: Final[tuple[str, ...]] = (
    "int",
    "signed",
    "decimal",
    "set",
    "mutex",
    "bool",
    "timequantum",
    "float32",
    "float64",
    "date",
    "datetime",
)

TABLE_ENGINES: Final[tuple[str, ...]] = ("bitmap", "bitmap+columnar", "columnar")


@dataclass(frozen=True, slots=True)
class Credential:
    """A user and a password, on their way into an `Authorization: Basic` header."""

    user: str
    password: str

    def __repr__(self) -> str:
        # `crates/big-http/src/request.rs`'s `Basic` derives no `Debug` for this reason: a
        # password in a traceback is a password in a log.
        return f"Credential(user={self.user!r}, password=<redacted>)"


@dataclass(frozen=True, slots=True)
class Config:
    """How this client behaves. Frozen; build a new one to change anything."""

    connect_timeout: float = DEFAULT_IO_TIMEOUT
    io_timeout: float = DEFAULT_IO_TIMEOUT
    max_bytes: int = DEFAULT_MAX_BYTES
    max_rows: int = DEFAULT_MAX_ROWS
    keepalive_requests: int = KEEPALIVE_REQUESTS
    keepalive_idle: float = KEEPALIVE_IDLE
    retries: int = DEFAULT_RETRIES
    first_backoff: float = DEFAULT_FIRST_BACKOFF
    max_backoff: float = DEFAULT_MAX_BACKOFF
    warn_on_plaintext_credentials: bool = True


#: The one every client uses when it is not given another.
#:
#: A shared instance rather than a `Config()` in each signature: `Config` is frozen, so one
#: value cannot be mutated by whoever holds it, and a default that is constructed per call is a
#: default that shows up as a different object in every traceback.
DEFAULT_CONFIG: Final[Config] = Config()
