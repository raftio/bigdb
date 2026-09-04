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

"""Values and names, written as the statement text the server reads back.

A port of `contrib/big-message/src/sql.rs`, function for function and test for test.

# The one place a caller's bytes become SQL

Every string that reaches a statement passes through `quote_text` or `quote_ident`, and nothing
else in this package writes a quote. That is the whole of the injection argument: there is one
module to read, it has no I/O, and its tests run before anything in this package opens a socket.

# Why identifiers are always quoted

`big_sql`'s `bare_ident` takes a `Tok::Word` or a `Tok::Quoted` and treats them alike, and the
lexer builds a `Quoted` from `"..."` with `""` meaning one `"`. So quoting always is legal
everywhere a name may appear, and it removes a class of mistake rather than managing it: a
column called `values` or `select` is a `Tok::Word` the parser reads as the keyword it spells.
Quoting is not a fallback for awkward names; it is the only path.

# Why numbers are checked against the lexer's own grammar

`big_sql::lex::number` reads `[-]digits[.digits]` and nothing else - **there is no exponent
form** - and builds `units / 10^scale` with `units` a `u64` and `scale` a `u8`. So a great many
floats have no spelling in this dialect, and Python's `repr` produces one of the unspellable
ones (`1e-07`, `1e+18`) for perfectly ordinary inputs. Checking here means the caller is told
which value was wrong; leaving it to the server means one value refuses a batch of eight
thousand and the sentence names none of them.

# Why there is no NULL

`big_plan::ast::Literal` has no null variant and `parse::literal` wants "a number, string, or
true/false". A client that rendered `None` as `NULL` would be writing a syntax error. `None` is
refused with a message that says the alternative: leave the column out.
"""

from __future__ import annotations

import datetime
import decimal
from collections.abc import Sequence
from typing import Any, Final

from .errors import ProgrammingError, ValueRefused

__all__ = [
    "MAX_SCALE",
    "RECORD_COLUMN",
    "bind",
    "check_column",
    "literal",
    "quote_ident",
    "quote_table",
    "quote_text",
]

#: The column that names a record id, which this client refuses to write.
#:
#: Matched the way the parser matches it - case-insensitively, see `big_sql::parse::insert` - so
#: that `_RECORD_ID` cannot slip past a check written against the lower-case spelling and turn
#: an allocating statement into one that names its own addresses.
RECORD_COLUMN: Final[str] = "_record_id"

#: The largest scale a literal can carry, because the lexer keeps it in a `u8`.
MAX_SCALE: Final[int] = 255

_U64_MAX: Final[int] = (1 << 64) - 1
_I64_MAX: Final[int] = (1 << 63) - 1
_I64_MIN: Final[int] = -(1 << 63)


def check_column(name: str) -> None:
    """A column name this client will write.

    Two refusals, and both are about what the *caller* meant rather than about what would parse.
    An empty name parses perfectly well as `""` and names nothing; `_record_id` parses perfectly
    well and would quietly turn off the allocation an `INSERT` relies on.
    """
    if not name:
        raise ValueRefused("a column with no name")
    if name.lower() == RECORD_COLUMN:
        raise ValueRefused(
            f"{name} is the record id, which this client does not write: leave the column "
            f"out and the server allocates one"
        )


def quote_ident(name: str) -> str:
    """One name, double-quoted, with `"` doubled to mean itself."""
    if not name:
        raise ValueRefused("a name with no characters in it")
    return '"' + name.replace('"', '""') + '"'


def quote_table(name: str) -> str:
    """A table, which may be written `database.table`.

    Split on the **first** `.`, matching `big_db::TableRef::parse`, so a qualified name reaches
    the server qualified. The cost is that a table whose own name contains a dot cannot be
    addressed - the same cost every other client in this repository pays, for the same reason.
    """
    database, dot, table = name.partition(".")
    if not dot:
        return quote_ident(name)
    return f"{quote_ident(database)}.{quote_ident(table)}"


def quote_text(s: str) -> str:
    """One string literal, single-quoted, with `'` doubled to mean itself.

    This is `big_sql::lex::string`'s rule read backwards, and it is total: there is no character
    a caller can send that ends the literal early, because the only character that could is the
    one being doubled.
    """
    return "'" + s.replace("'", "''") + "'"


def literal(value: Any) -> str:
    """One value, as the literal the server will read."""
    # `bool` before `int`, because in Python a bool *is* an int and would otherwise render as
    # `1` - which is a different value in a `BOOL` field's eyes than `TRUE`.
    if value is None:
        raise ValueRefused(
            "this dialect has no NULL literal; leave the column out of the statement instead"
        )
    if isinstance(value, bool):
        # `TRUE` and `FALSE`, which `big_sql::parse` reads with `eat_word` and therefore reads
        # in any case. Upper because that is how the rest of the dialect is written.
        return "TRUE" if value else "FALSE"
    if isinstance(value, int):
        return _integer(value)
    if isinstance(value, float):
        return _float(value)
    if isinstance(value, decimal.Decimal):
        # Through `str`, not through a float: a SQL literal carries its own scale, so `12.50`
        # against a scale-2 field is the 1250 units it stores, exactly, with no round trip
        # through binary floating point to lose it.
        if not value.is_finite():
            raise ValueRefused(f"{value} cannot be written as a number: this dialect has none")
        return _checked(format(value, "f"))
    if isinstance(value, str):
        return quote_text(value)
    if isinstance(value, datetime.datetime):
        # Text, which is how the server reads a `DATETIME`. Not isoformat: a `T` separator and a
        # timezone suffix are not what `big_db`'s reader wants.
        return quote_text(value.strftime("%Y-%m-%d %H:%M:%S"))
    if isinstance(value, datetime.date):
        return quote_text(value.strftime("%Y-%m-%d"))
    raise ValueRefused(
        f"a {type(value).__name__} has no spelling in this dialect; convert it to a number, "
        f"a string or a bool first"
    )


def keyed(key: str, at: int) -> str:
    """A `TIMEQUANTUM` value, written `key@unix_seconds`.

    Built as one string and *then* quoted, rather than pushed in three pieces, because the
    quoting rule has to apply to the whole of it: a key containing a `'` must still be doubled,
    and a key containing an `@` is still safe because the server splits on the last one.
    """
    return quote_text(f"{key}@{at}")


def _integer(value: int) -> str:
    """A bare integer, inside what the lexer's `units` holds."""
    if value > _U64_MAX:
        raise ValueRefused(f"{value} has more digits than a literal holds")
    if value < _I64_MIN:
        raise ValueRefused(f"{value} has more digits than a negative literal holds")
    return str(value)


def _float(value: float) -> str:
    """A float, in the one notation this dialect has.

    `repr` is tried first because it is the shortest spelling that reads back as the same
    number - the same property Rust's `{:?}` has. When it comes out in exponent form, which it
    does for magnitudes an ordinary program still produces, a fixed spelling is searched for
    instead, shortest first, over the scales a literal can actually carry.
    """
    if value != value or value in (float("inf"), float("-inf")):
        raise ValueRefused(
            f"{value} cannot be written as a number: this dialect has no spelling for it"
        )

    short = repr(value)
    if "e" not in short and "E" not in short:
        return _checked(short)

    for scale in range(MAX_SCALE + 1):
        fixed = f"{value:.{scale}f}"
        # The first spelling that reads back as the same number is the shortest one that does,
        # because the scales are tried in order.
        if float(fixed) == value:
            return _checked(fixed)

    raise ValueRefused(
        f"{value} cannot be written as a number this dialect reads: it has no exponent form, "
        f"and a literal carries at most {MAX_SCALE} digits after the point"
    )


def _checked(text: str) -> str:
    """Whether the server's lexer would read this text as one number, and the text if so.

    `big_sql::lex::number`: an optional `-`, at least one digit, then optionally a `.` and at
    least one more digit. The digits either side of the point are concatenated into `units`,
    which is a `u64` - and an `i64` when the sign is there - and the digits after the point are
    counted into `scale`, which is a `u8`.
    """

    def refuse(why: str) -> ValueRefused:
        return ValueRefused(f"{text} is not a number this dialect reads: {why}")

    negative = text.startswith("-")
    digits = text[1:] if negative else text
    whole, point, frac = digits.partition(".")

    if not whole or not whole.isdigit() or not whole.isascii():
        raise refuse("it needs at least one digit before the point")
    if point and (not frac or not frac.isdigit() or not frac.isascii()):
        raise refuse("it needs at least one digit after the point")
    if len(frac) > MAX_SCALE:
        raise refuse(f"a literal carries at most {MAX_SCALE} digits after the point")

    # Concatenated exactly as the lexer concatenates them, so a value that overflows here is
    # the value that would have come back as `NumberTooLarge`.
    units = int(whole + frac)
    if units > _U64_MAX:
        raise refuse("it has more digits than a literal holds")
    if negative and units > _I64_MAX:
        raise refuse("it has more digits than a negative literal holds")
    return text


# --------------------------------------------------------------------------------------------
# Parameter binding.
# --------------------------------------------------------------------------------------------


def bind(operation: str, parameters: Sequence[Any] | None) -> str:
    """`?` placeholders replaced by literals, and nothing else touched.

    # Why `?`

    This dialect has no server-side parameters, so every parameter is substituted textually
    here. The marker therefore has to be a character the lexer can never produce in a valid
    statement outside a string literal or a comment. From `crates/big-sql/src/lex.rs`, the
    punctuation set is `( ) , * . = != < <= > >= ' " - + / %`:

    - **`?` is not a token.** It reaches the `other =>` arm and is a syntax error. So an
      unbound `?` cannot silently mean something, and a `?` that survives substitution is
      caught by the server with a clear message rather than changing what the statement does.
    - **`%` *is* a token** - `Tok::Arith("%")`. `format` and `pyformat` would collide with
      modulo, and worse with a `%` inside a string literal (`LIKE 'a%b'`), which would force
      every caller to double it. That is a footgun in a dialect where `%` in a string is
      ordinary text.

    The scan is a small state machine rather than a regex because it has to agree with the
    lexer about what a string is: `''` inside `'...'` is one quote and does not end it, and the
    same for `""` inside `"..."`. `--` to end of line is the *only* comment form
    (`lex.rs`), so there is no `/* */` state to carry.
    """
    if parameters is None:
        parameters = ()
    spans = placeholders(operation)
    if len(spans) != len(parameters):
        raise ProgrammingError(
            f"this statement has {len(spans)} placeholder"
            f"{'' if len(spans) == 1 else 's'} and {len(parameters)} parameter"
            f"{'' if len(parameters) == 1 else 's'} were given"
        )
    if not spans:
        return operation

    out: list[str] = []
    cursor = 0
    for index, at in enumerate(spans):
        out.append(operation[cursor:at])
        out.append(literal(parameters[index]))
        cursor = at + 1
    out.append(operation[cursor:])
    return "".join(out)


def placeholders(operation: str) -> list[int]:
    """The offsets of every `?` that is a placeholder rather than text.

    Public within the package because `dbapi.executemany` uses the same scan to find the
    `VALUES` tail, and two scanners would be two ideas about where a string ends.
    """
    return [at for at, _ in _scan(operation) if operation[at] == "?"]


def _scan(operation: str) -> list[tuple[int, str]]:
    """Every character that is outside a string literal and outside a comment, with its offset.

    One pass, four states. Returned as a list rather than yielded so callers can index into it
    without re-scanning.
    """
    out: list[tuple[int, str]] = []
    at = 0
    end = len(operation)
    while at < end:
        char = operation[at]
        if char == "'":
            at = _skip_quoted(operation, at, "'")
        elif char == '"':
            at = _skip_quoted(operation, at, '"')
        elif char == "-" and operation.startswith("--", at):
            newline = operation.find("\n", at)
            at = end if newline == -1 else newline + 1
        else:
            out.append((at, char))
            at += 1
    return out


def _skip_quoted(operation: str, at: int, quote: str) -> int:
    """Past the closing quote, treating a doubled quote as one character of content.

    An unterminated literal runs to the end and is left for the server to complain about: this
    client does not parse SQL, and refusing here would mean a second opinion about what is
    valid, kept in step with `big_sql` by hand.
    """
    at += 1
    end = len(operation)
    while at < end:
        if operation[at] == quote:
            if at + 1 < end and operation[at + 1] == quote:
                at += 2
                continue
            return at + 1
        at += 1
    return end
