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

"""Names and parameters, written as the URL the server's router reads back.

A port of `crates/big-bin/src/client/mod.rs::escape`, kept byte-identical to it so a table that
arrives at `bigctl` is the table that arrives here. It is not a general percent-encoder: it
keeps the unreserved set and encodes everything else, which covers what the router splits on
and what a query string is delimited by.

The server splits path segments on `/` **before** percent-decoding them
(`crates/big-http/src/request.rs::segments`, with its own test that an encoded slash stays
inside one segment), so a table named `a/b` round-trips through `%2F`.
"""

from __future__ import annotations

__all__ = ["escape_segment", "query_string"]

#: RFC 3986's unreserved set, which is what `escape` in the Rust client keeps.
_UNRESERVED = frozenset("ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~")


def escape_segment(s: str) -> str:
    """One path segment or parameter value, with everything reserved percent-encoded.

    Note `.` is kept literal, exactly as the Rust encoder keeps it. A table whose own name
    contains a dot is therefore unaddressable through the path in the same way it is
    unaddressable in SQL, where `db.table` splits on the first one. One cost, in one place.
    """
    out: list[str] = []
    for char in s:
        if char in _UNRESERVED:
            out.append(char)
        else:
            out.extend(f"%{byte:02X}" for byte in char.encode("utf-8"))
    return "".join(out)


def query_string(params: dict[str, str | int | bool | None]) -> str:
    """`?a=1&b=2`, or an empty string when nothing is set.

    A `None` value is left out entirely rather than sent empty: the server reads an absent
    parameter and an empty one differently on several routes (`after=` is not `after=0`).
    A `bool` is written `true` / `false`, which is the spelling `?cascade=true` wants.
    """
    parts: list[str] = []
    for name, value in params.items():
        if value is None:
            continue
        text = ("true" if value else "false") if isinstance(value, bool) else str(value)
        parts.append(f"{escape_segment(name)}={escape_segment(text)}")
    return f"?{'&'.join(parts)}" if parts else ""
