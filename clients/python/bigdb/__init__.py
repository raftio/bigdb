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

"""A Python client for bigdb, written against its HTTP surface.

    import bigdb

    with bigdb.Client("127.0.0.1:7654") as db:
        db.create_table("tx")
        db.create_field("tx", "amount", kind="int", bit_depth=20)
        db.import_facts("tx", [bigdb.Fact("amount", 0, 1250)])
        print(db.sql("SELECT sum(amount) FROM tx").scalar())

# Scope

The data plane and schema, which is what a program talks to: `/sql`, `/table/{t}/query`,
`/table/{t}/import`, `/table/{t}/delete`, `/table/{t}/records`, `/schema`, `/health`, `/ready`,
and the DDL routes. Not the operator surface - `/metrics`, `/verify`, `/repair`,
`/admin/backup`, `/cluster/*` - which `bigctl` covers and which wants a person reading the
answers.

# No dependencies

`[project.dependencies]` is empty and stays empty, which is the same claim
`contrib/big-message` makes and for the same reason: an empty manifest is the only form of it a
build can check. There is a CI job that checks it two ways.

# The three things worth knowing before writing against this

**Writes are at-least-once.** `/table/{t}/import` is idempotent because the caller chooses the
record id, so a chunk sent twice writes the same bits twice - which is writing them once. An
allocating `INSERT` over `/sql` is not: sending it twice writes two records, silently. When a
request was written in full and the answer never came, you get `bigdb.Unknown`, and it is never
retried automatically. If you need exactly-once, choose the ids yourself and use
`import_facts`.

**One `Client` is one connection and one thread.** Use a client per thread, or the async one.

**A slow query needs `io_timeout` raised too.** The socket timeout defaults to 30 seconds to
match the server's own. If an operator raised `--query-timeout` past that, raise
`Config(io_timeout=...)` with it - otherwise a long query fails as `Unknown` rather than as the
`504 query_timeout` it actually is, which is the worst way for a slow query to fail.
"""

from __future__ import annotations

__version__ = "0.1.0"

from .address import Address
from .aio import AsyncClient
from .client import Client
from .config import (
    DEFAULT_ADDR,
    FIELD_KINDS_READ,
    FIELD_KINDS_WRITE,
    MAX_BODY,
    TABLE_ENGINES,
    Config,
    Credential,
)
from .errors import (
    BadGateway,
    BadRequest,
    BigError,
    ClientClosed,
    ConfigError,
    Conflict,
    DatabaseError,
    DataError,
    Error,
    Forbidden,
    InsecureCredentialWarning,
    IntegrityError,
    InterfaceError,
    InternalError,
    NotFound,
    NotSent,
    NotSupportedError,
    OperationalError,
    PartiallyApplied,
    PayloadTooLarge,
    ProgrammingError,
    ProtocolError,
    QueryTimeout,
    RequestTooLarge,
    ServerError,
    ServerFault,
    TransportError,
    Unauthenticated,
    Unavailable,
    Unknown,
    Unprocessable,
    Unsupported,
    ValueRefused,
)
from .facts import Fact
from .results import (
    Count,
    Extreme,
    FieldInfo,
    Group,
    Groups,
    PqlAnswer,
    ProjectionRow,
    ProjectionRows,
    Ready,
    RecordPage,
    Schema,
    SqlResult,
    Sum,
    TableInfo,
    TextResult,
    TupleGroup,
    Tuples,
    WriteResult,
)

__all__ = [
    "DEFAULT_ADDR",
    "FIELD_KINDS_READ",
    "FIELD_KINDS_WRITE",
    "MAX_BODY",
    "TABLE_ENGINES",
    "Address",
    "AsyncClient",
    "BadGateway",
    "BadRequest",
    "BigError",
    "Client",
    "ClientClosed",
    "Config",
    "ConfigError",
    "Conflict",
    "Count",
    "Credential",
    "DataError",
    "DatabaseError",
    "Error",
    "Extreme",
    "Fact",
    "FieldInfo",
    "Forbidden",
    "Group",
    "Groups",
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
    "PqlAnswer",
    "ProgrammingError",
    "ProjectionRow",
    "ProjectionRows",
    "ProtocolError",
    "QueryTimeout",
    "Ready",
    "RecordPage",
    "RequestTooLarge",
    "Schema",
    "ServerError",
    "ServerFault",
    "SqlResult",
    "Sum",
    "TableInfo",
    "TextResult",
    "TransportError",
    "TupleGroup",
    "Tuples",
    "Unauthenticated",
    "Unavailable",
    "Unknown",
    "Unprocessable",
    "Unsupported",
    "ValueRefused",
    "WriteResult",
    "__version__",
    "connect",
]


def connect(*args: object, **kwargs: object) -> object:
    """PEP 249's entry point, from `bigdb.dbapi`.

    A function rather than a re-export so `import bigdb` does not pull in the DB-API layer for
    the callers who do not want it.
    """
    from .dbapi import connect as _connect

    return _connect(*args, **kwargs)  # type: ignore[arg-type]
