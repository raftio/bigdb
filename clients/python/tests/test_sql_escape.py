"""The assertions in `contrib/big-message/src/sql.rs`'s test module, ported.

Ported rather than reinvented on purpose: a Python client with its own idea of what escaping
means is a client that agrees with the server until it does not. These vectors hold the two
implementations to each other.
"""

from __future__ import annotations

import datetime
import decimal

import pytest

from bigdb.errors import ProgrammingError, ValueRefused
from bigdb.sql import (
    MAX_SCALE,
    bind,
    check_column,
    keyed,
    literal,
    placeholders,
    quote_ident,
    quote_table,
    quote_text,
)

U64_MAX = (1 << 64) - 1
I64_MIN = -(1 << 63)


def test_a_quote_in_a_value_cannot_end_the_statement():
    assert literal("O'Brien") == "'O''Brien'"
    assert literal("x'); DROP TABLE t; --") == "'x''); DROP TABLE t; --'"
    # The empty string is a key like any other, and it is not the absence of one.
    assert literal("") == "''"
    # A run of quotes doubles every one of them, rather than the first.
    assert literal("'''") == "''''''''"


def test_a_quote_in_a_name_cannot_end_the_identifier():
    assert quote_ident('we"ird') == '"we""ird"'


def test_a_name_that_spells_a_keyword_is_still_a_name():
    # The reason every identifier is quoted: unquoted, this is `Tok::Word("values")` and the
    # parser reads it as the keyword.
    assert quote_ident("values") == '"values"'
    assert quote_ident("select") == '"select"'


def test_a_qualified_table_is_quoted_in_two_pieces():
    assert quote_table("sales.orders") == '"sales"."orders"'
    assert quote_table("orders") == '"orders"'


def test_an_empty_name_is_refused():
    with pytest.raises(ValueRefused):
        quote_ident("")


def test_the_record_id_column_is_refused_however_it_is_spelled():
    for spelling in ("_record_id", "_RECORD_ID", "_Record_Id"):
        with pytest.raises(ValueRefused):
            check_column(spelling)
    check_column("record_id")  # an ordinary column that merely reads like it
    check_column("id")  # `id` belongs to whoever is writing the table
    with pytest.raises(ValueRefused):
        check_column("")


def test_a_float_that_is_not_a_number_is_refused_before_it_is_sent():
    for bad in (float("nan"), float("inf"), float("-inf")):
        with pytest.raises(ValueRefused):
            literal(bad)


def test_a_float_is_written_in_the_only_notation_this_dialect_has():
    # Ordinary magnitudes come straight out of `repr`.
    assert literal(2.75) == "2.75"
    assert literal(-0.5) == "-0.5"
    assert literal(0.0) == "0.0"

    # `repr` gives `1e-07`, which the lexer cannot read, so a fixed spelling is found.
    small = literal(1e-7)
    assert "e" not in small, f"{small} still carries an exponent"
    assert float(small) == 1e-7

    # Large but inside what a `u64` of units holds.
    large = literal(1e18)
    assert "e" not in large, f"{large} still carries an exponent"
    assert float(large) == 1e18


def test_a_float_with_no_spelling_is_refused_rather_than_rounded():
    # 1e300 written out is 301 digits and `units` is a u64. Refusing is the honest answer;
    # rounding it to something that fits would be writing a different number than was sent.
    with pytest.raises(ValueRefused):
        literal(1e300)


def test_a_decimal_is_checked_against_the_grammar_the_lexer_has():
    assert literal(decimal.Decimal("12.50")) == "12.50"
    assert literal(decimal.Decimal("-12.50")) == "-12.50"
    assert literal(decimal.Decimal("0")) == "0"

    for bad in ("", ".5", "5.", "1e3", "1,5", "12.5.0", "abc", "-", "- 1", "+1"):
        with pytest.raises(ValueRefused):
            # Through the same checker a Decimal goes through, driven by text the way
            # `Value::Decimal(&str)` is in the Rust crate.
            from bigdb.sql import _checked

            _checked(bad)


def test_a_number_with_more_digits_than_a_literal_holds_is_refused():
    from bigdb.sql import _checked

    # One past `u64::MAX`, which is where `lex::number` answers `NumberTooLarge`.
    with pytest.raises(ValueRefused):
        _checked("18446744073709551616")
    # The same digits with a point in them are the same `units`, so the same refusal.
    with pytest.raises(ValueRefused):
        _checked("1844674407370955161.6")
    # And just inside it is fine.
    assert _checked("18446744073709551615") == "18446744073709551615"


def test_a_scale_past_what_a_literal_carries_is_refused():
    from bigdb.sql import _checked

    assert _checked("0." + "0" * MAX_SCALE)
    with pytest.raises(ValueRefused):
        _checked("0." + "0" * (MAX_SCALE + 1))


def test_a_keyed_value_carries_its_moment_and_is_still_quoted():
    assert keyed("gb", 1_750_000_000) == "'gb@1750000000'"
    # The server splits on the last `@`, so a key holding one survives the round trip.
    assert keyed("a@b", 1) == "'a@b@1'"
    # And a key holding a quote is doubled like any other string.
    assert keyed("o'b", 1) == "'o''b@1'"


def test_a_bool_is_written_the_way_the_parser_eats_it():
    assert literal(True) == "TRUE"
    assert literal(False) == "FALSE"


def test_a_bool_is_not_an_integer_even_though_python_says_it_is():
    # `isinstance(True, int)` is True in Python. Rendering `1` here would write a different
    # value than the caller sent into a BOOL field.
    assert literal(True) != "1"


def test_a_signed_value_keeps_its_sign_and_an_unsigned_one_has_none():
    assert literal(-1) == "-1"
    assert literal(I64_MIN) == str(I64_MIN)
    assert literal(U64_MAX) == str(U64_MAX)
    with pytest.raises(ValueRefused):
        literal(U64_MAX + 1)
    with pytest.raises(ValueRefused):
        literal(I64_MIN - 1)


def test_none_is_refused_because_the_dialect_has_no_null():
    with pytest.raises(ValueRefused, match="NULL"):
        literal(None)


def test_bytes_have_no_spelling():
    with pytest.raises(ValueRefused):
        literal(b"\x00")


def test_dates_are_written_as_the_text_the_server_reads():
    assert literal(datetime.date(2024, 1, 15)) == "'2024-01-15'"
    assert literal(datetime.datetime(2024, 1, 15, 12, 0, 0)) == "'2024-01-15 12:00:00'"


@pytest.mark.parametrize(
    "text",
    ["", "'", "''", "'''", "a'b", "''''", "a''b'", "\\'", "'; --"],
)
def test_a_quoted_value_never_has_an_odd_run_of_quotes_inside_it(text):
    """The invariant behind the doubling rule, stated as a property.

    Between the outer quotes, every run of `'` has even length - which is what makes it
    impossible for any input to terminate the literal early.
    """
    inner = quote_text(text)[1:-1]
    run = 0
    for char in inner + "x":
        if char == "'":
            run += 1
        else:
            assert run % 2 == 0, f"{text!r} produced an odd run of quotes"
            run = 0


# --------------------------------------------------------------------------------------------
# Binding.
# --------------------------------------------------------------------------------------------


def test_a_placeholder_is_replaced_and_a_literal_question_mark_is_not():
    assert bind("SELECT * FROM t WHERE a = ?", [1]) == "SELECT * FROM t WHERE a = 1"
    # Inside a string literal, `?` is text.
    assert placeholders("SELECT '?' FROM t") == []
    # Inside a quoted identifier too.
    assert placeholders('SELECT "?" FROM t') == []
    # And inside a comment, which is `--` to end of line and nothing else.
    assert placeholders("SELECT 1 -- ?\nFROM t") == []
    # A `?` after a comment's newline is a placeholder again.
    assert len(placeholders("SELECT 1 -- x\nWHERE a = ?")) == 1


def test_a_doubled_quote_does_not_end_the_literal():
    # `'a''?'` is one literal holding `a'?`. A scanner that treated the second quote as the end
    # would see the `?` as a placeholder and substitute into the middle of a string.
    assert placeholders("SELECT 'a''?' FROM t") == []
    assert placeholders('SELECT "a""?" FROM t') == []


def test_arity_is_checked_in_both_directions():
    with pytest.raises(ProgrammingError, match="2 placeholders and 1 parameter"):
        bind("SELECT ?, ?", [1])
    with pytest.raises(ProgrammingError, match="1 placeholder and 2 parameters"):
        bind("SELECT ?", [1, 2])
    with pytest.raises(ProgrammingError):
        bind("SELECT 1", [1])


def test_no_placeholders_and_no_parameters_is_the_statement_unchanged():
    assert bind("SELECT 1", None) == "SELECT 1"
    assert bind("SELECT 1", []) == "SELECT 1"


def test_a_bound_value_that_looks_like_sql_stays_one_value():
    bound = bind("SELECT * FROM t WHERE country = ?", ["x'); DROP TABLE t; --"])
    assert bound == "SELECT * FROM t WHERE country = 'x''); DROP TABLE t; --'"
    # And the statement still has exactly the one literal it had.
    assert placeholders(bound) == []


# --------------------------------------------------------------------------------------------
# The scanner, against an independently written oracle.
# --------------------------------------------------------------------------------------------


def _oracle(statement: str) -> list[int]:
    """A second implementation of the same rule, written from the lexer's description.

    Deliberately not a refactor of `_scan`: the point of a differential test is that two
    people reading `crates/big-sql/src/lex.rs` would have to make the *same* mistake for a bug
    to survive it.
    """
    out: list[int] = []
    at, end = 0, len(statement)
    while at < end:
        char = statement[at]
        if char in "'\"":
            quote, at = char, at + 1
            while at < end:
                if statement[at] == quote:
                    if at + 1 < end and statement[at + 1] == quote:
                        at += 2
                        continue
                    at += 1
                    break
                at += 1
        elif char == "-" and statement.startswith("--", at):
            newline = statement.find("\n", at)
            at = end if newline == -1 else newline + 1
        else:
            if char == "?":
                out.append(at)
            at += 1
    return out


def test_the_placeholder_scanner_agrees_with_an_independent_oracle():
    """Randomised over the characters that actually change state.

    A `?` mistaken for a placeholder inside a literal would substitute into the middle of
    somebody's string; one missed outside would leave an unbound `?` for the server. Both are
    the kind of bug a handful of hand-written cases walks straight past.
    """
    import random

    random.seed(20260904)
    alphabet = "?'\"-\n abc(),xyz"
    for _ in range(20_000):
        statement = "".join(random.choice(alphabet) for _ in range(random.randint(0, 24)))
        assert placeholders(statement) == _oracle(statement), repr(statement)


def test_a_quoted_value_is_never_more_than_one_literal():
    """The property behind every injection assertion above, over awkward characters.

    A backslash is in the alphabet on purpose: this dialect has **no** backslash escape, so
    `\\'` is a backslash followed by a quote that must still be doubled. A client that copied
    an escaping rule from a dialect that does have one would fail here.
    """
    import random

    random.seed(1)
    characters = ["'", '"', "\\", "\n", "A", "中", "😀", "-", "?"]
    for _ in range(10_000):
        value = "".join(random.choice(characters) for _ in range(random.randint(0, 12)))
        quoted = quote_text(value)
        # It contains no placeholder the binder would see...
        assert placeholders(quoted) == [], repr(value)
        # ...and the scanner agrees it is exactly one literal with nothing after it.
        assert _oracle(quoted + "?") == [len(quoted)], repr(value)
