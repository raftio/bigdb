"""PEP 249 conformance, and the client loop, driven over a stub transport.

No sockets: `Client` is built normally and its transport swapped for one that replays canned
answers and records what it was asked. That is enough to test the retry loop, the warning, and
the whole DB-API layer.
"""

from __future__ import annotations

import json
import warnings

import pytest

import bigdb
from bigdb import dbapi
from bigdb.client import Client
from bigdb.config import Config
from bigdb.errors import (
    InsecureCredentialWarning,
    InterfaceError,
    NotSent,
    NotSupportedError,
    ProgrammingError,
    Unavailable,
    Unknown,
)
from bigdb.transport.base import RawResponse


class StubTransport:
    """Replays a script. An entry may be a `RawResponse` or an exception to raise."""

    def __init__(self, script):
        self.script = list(script)
        self.sent: list[tuple[str, str, bytes]] = []
        self.closed = False

    def roundtrip(self, method, target, body):
        self.sent.append((method, target, body))
        item = self.script.pop(0) if self.script else ok({})
        if isinstance(item, BaseException):
            raise item
        return item

    def close(self):
        self.closed = True


def ok(obj, status=200, content_type="application/json"):
    return RawResponse(
        status=status,
        headers=(("content-type", content_type),),
        body=json.dumps(obj).encode(),
    )


def client(script, **kw):
    c = Client("127.0.0.1:7654", config=Config(first_backoff=0.0, **kw))
    c._transport = StubTransport(script)
    return c


# --------------------------------------------------------------------------------------------
# The client loop.
# --------------------------------------------------------------------------------------------


def test_a_request_that_never_arrived_is_sent_again():
    c = client([NotSent("refused"), ok({"columns": ["a"], "rows": [[1]]})])
    assert c.sql("SELECT 1").rows == ((1,),)
    assert len(c._transport.sent) == 2


def test_a_503_is_sent_again_even_for_a_write():
    busy = RawResponse(503, (("content-type", "application/json"),), b'{"code":"server_busy"}')
    c = client([busy, ok({"columns": ["inserted"], "rows": [[1]]})])
    assert c.sql("INSERT INTO t VALUES (1)").rows == ((1,),)


def test_an_unknown_outcome_stops_a_sql_write_and_is_retried_for_an_import():
    # `/sql` allocates, so "possibly written once" must not become "possibly written twice".
    c = client([Unknown("timeout"), ok({"columns": ["inserted"], "rows": [[1]]})])
    with pytest.raises(Unknown):
        c.sql("INSERT INTO t VALUES (1)")
    assert len(c._transport.sent) == 1

    # `/import` writes the same bits at the same addresses, so it is safe.
    c = client([Unknown("timeout"), ok({"imported": 3})])
    assert c.import_facts("t", b"amount 0 1\n").written == 3
    assert len(c._transport.sent) == 2


def test_the_budget_runs_out_and_the_last_failure_is_what_is_raised():
    c = client([NotSent("a"), NotSent("b"), NotSent("c"), NotSent("d")], retries=2)
    with pytest.raises(NotSent, match="c"):
        c.health()
    assert len(c._transport.sent) == 3


def test_a_refusal_is_raised_without_being_sent_again():
    refused = RawResponse(400, (("content-type", "application/json"),), b'{"code":"bad_request"}')
    c = client([refused, ok({})])
    with pytest.raises(bigdb.BadRequest):
        c.sql("NOT SQL")
    assert len(c._transport.sent) == 1


def test_the_clients_database_reaches_the_query_string():
    c = Client("127.0.0.1:7654", database="sales")
    c._transport = StubTransport([ok({"columns": [], "rows": []})])
    c.sql("SELECT 1")
    assert c._transport.sent[0][1] == "/sql?database=sales"
    # And a per-call one wins over it.
    c._transport.script.append(ok({"columns": [], "rows": []}))
    c.sql("SELECT 1", database="other")
    assert c._transport.sent[1][1] == "/sql?database=other"


def test_iter_records_follows_the_cursor_until_a_page_proves_there_is_no_more():
    c = client(
        [
            ok({"records": [0, 1], "next": 1}),
            ok({"records": [2, 3], "next": 3}),
            ok({"records": [4], "next": None}),
        ]
    )
    assert list(c.iter_records("tx", page=2)) == [0, 1, 2, 3, 4]
    assert "after=1" in c._transport.sent[1][1]


def test_import_stream_chunks_and_reports_each_offset():
    facts = [bigdb.Fact("amount", i, i) for i in range(6)]
    c = client([ok({"imported": 2}) for _ in range(3)])
    seen: list[int] = []
    result = c.import_stream("tx", facts, max_bytes=30, on_chunk=lambda o, _r: seen.append(o))
    assert result.written == 6
    assert seen == [0, 2, 4]
    assert len(c._transport.sent) == 3


def test_import_stream_collects_the_copies_that_were_behind_without_repeating_them():
    facts = [bigdb.Fact("amount", i, i) for i in range(4)]
    c = client(
        [
            ok({"imported": 2, "missed": ["a-spare (0..1)"]}),
            ok({"imported": 2, "missed": ["a-spare (0..1)"]}),
        ]
    )
    result = c.import_stream("tx", facts, max_bytes=30)
    # The same copy behind for both chunks is one copy behind.
    assert result.missed == ("a-spare (0..1)",)
    assert not result.complete


def test_a_password_over_a_plaintext_network_address_warns_once():
    with warnings.catch_warnings(record=True) as caught:
        warnings.simplefilter("always")
        Client("10.0.0.1:7654", user="alice", password="s3cret")
    assert any(issubclass(w.category, InsecureCredentialWarning) for w in caught)


def test_loopback_and_tls_and_opting_out_are_all_silent():
    with warnings.catch_warnings(record=True) as caught:
        warnings.simplefilter("always")
        Client("127.0.0.1:7654", user="alice", password="x")
        Client("https://example:7654", user="alice", password="x")
        Client(
            "10.0.0.1:7654",
            user="alice",
            password="x",
            config=Config(warn_on_plaintext_credentials=False),
        )
    assert not [w for w in caught if issubclass(w.category, InsecureCredentialWarning)]


# --------------------------------------------------------------------------------------------
# PEP 249's module-level requirements.
# --------------------------------------------------------------------------------------------


def test_the_module_globals_the_specification_requires():
    assert dbapi.apilevel == "2.0"
    assert dbapi.threadsafety == 1
    # `?`, because it is the one marker the lexer can never produce in a valid statement.
    # `%` is `Tok::Arith`, so pyformat would collide with modulo and with `LIKE 'a%b'`.
    assert dbapi.paramstyle == "qmark"


def test_the_nine_exceptions_exist_and_are_parented_as_the_specification_says():
    assert issubclass(dbapi.Warning, Exception)
    assert issubclass(dbapi.Error, Exception)
    for name in ("InterfaceError", "DatabaseError"):
        assert issubclass(getattr(dbapi, name), dbapi.Error)
    for name in (
        "DataError",
        "OperationalError",
        "IntegrityError",
        "InternalError",
        "ProgrammingError",
        "NotSupportedError",
    ):
        assert issubclass(getattr(dbapi, name), dbapi.DatabaseError)


def test_one_tree_means_a_bigdb_exception_is_also_a_pep_249_one():
    # The whole point of not having a translation layer.
    assert issubclass(bigdb.NotFound, dbapi.ProgrammingError)
    assert issubclass(bigdb.Unavailable, dbapi.OperationalError)
    assert issubclass(bigdb.Conflict, dbapi.IntegrityError)
    assert issubclass(bigdb.PartiallyApplied, dbapi.InternalError)
    assert issubclass(bigdb.ValueRefused, dbapi.DataError)
    assert issubclass(bigdb.NotSent, dbapi.InterfaceError)


def test_the_type_constructors_the_specification_requires():
    assert dbapi.Date(2024, 1, 15).isoformat() == "2024-01-15"
    assert dbapi.Timestamp(2024, 1, 15, 12, 0, 0).year == 2024
    assert dbapi.DateFromTicks(0) is not None
    assert dbapi.TimeFromTicks(0) is not None
    assert dbapi.TimestampFromTicks(0) is not None
    assert dbapi.Binary(b"x") == b"x"
    assert dbapi.NUMBER == "int"
    assert dbapi.STRING == "set"
    assert dbapi.NUMBER != "set"


# --------------------------------------------------------------------------------------------
# Connection and cursor.
# --------------------------------------------------------------------------------------------


def connection(script):
    return dbapi.Connection(client(script))


def test_a_query_through_a_cursor():
    conn = connection([ok({"columns": ["country", "n"], "rows": [["GB", 41], ["US", 9]]})])
    cur = conn.cursor()
    cur.execute("SELECT country, count(*) FROM tx GROUP BY country")
    assert cur.rowcount == 2
    assert [d[0] for d in cur.description] == ["country", "n"]
    assert cur.fetchone() == ("GB", 41)
    assert cur.fetchall() == [("US", 9)]
    assert cur.fetchone() is None


def test_parameters_are_bound_before_the_statement_is_sent():
    conn = connection([ok({"columns": ["n"], "rows": [[1]]})])
    cur = conn.cursor()
    cur.execute("SELECT count(*) FROM tx WHERE country = ?", ["x'); DROP TABLE tx; --"])
    sent = conn.client._transport.sent[0][2].decode()
    assert sent.endswith("WHERE country = 'x''); DROP TABLE tx; --'")


def test_the_description_infers_a_type_from_the_first_cell_that_is_not_null():
    conn = connection([ok({"columns": ["a", "b", "c"], "rows": [[None, "x", 1], [2, "y", 2]]})])
    cur = conn.cursor()
    cur.execute("SELECT a, b, c FROM t")
    assert cur.description[0][1] == dbapi.NUMBER  # first non-null in column a is 2
    assert cur.description[1][1] == dbapi.STRING
    assert cur.description[2][1] == dbapi.NUMBER
    # Seven members, five of them None because the server does not report them.
    assert len(cur.description[0]) == 7
    assert cur.description[0][2:] == (None, None, None, None, None)


def test_a_column_that_is_null_all_the_way_down_gets_no_type_rather_than_a_guess():
    conn = connection([ok({"columns": ["a"], "rows": [[None], [None]]})])
    cur = conn.cursor()
    cur.execute("SELECT a FROM t")
    assert cur.description[0][1] is None


def test_an_insert_reports_the_count_the_server_wrote_as_rowcount():
    conn = connection([ok({"columns": ["inserted"], "rows": [[7]]})])
    cur = conn.cursor()
    cur.execute("INSERT INTO t (a) VALUES (?)", [1])
    assert cur.rowcount == 7


def test_fetchmany_uses_arraysize_by_default():
    conn = connection([ok({"columns": ["n"], "rows": [[1], [2], [3]]})])
    cur = conn.cursor()
    cur.execute("SELECT n FROM t")
    assert cur.fetchmany() == [(1,)]
    cur.arraysize = 2
    assert cur.fetchmany() == [(2,), (3,)]
    assert cur.fetchmany() == []


def test_a_cursor_iterates():
    conn = connection([ok({"columns": ["n"], "rows": [[1], [2]]})])
    cur = conn.cursor()
    cur.execute("SELECT n FROM t")
    assert list(cur) == [(1,), (2,)]


def test_rowcount_is_minus_one_before_anything_is_executed():
    assert connection([]).cursor().rowcount == -1
    assert connection([]).cursor().description is None


def test_lastrowid_is_always_none_because_the_record_id_is_not_handed_out():
    conn = connection([ok({"columns": ["inserted"], "rows": [[1]]})])
    cur = conn.cursor()
    cur.execute("INSERT INTO t (a) VALUES (1)")
    assert cur.lastrowid is None


def test_commit_does_nothing_and_rollback_refuses():
    conn = connection([])
    conn.commit()  # every statement is its own commit
    with pytest.raises(NotSupportedError):
        conn.rollback()


def test_nextset_and_the_no_op_setters():
    cur = connection([]).cursor()
    assert cur.nextset() is None
    cur.setinputsizes([1])
    cur.setoutputsize(1)


def test_a_closed_cursor_and_a_closed_connection_refuse():
    conn = connection([ok({"columns": [], "rows": []})])
    cur = conn.cursor()
    cur.close()
    with pytest.raises(InterfaceError):
        cur.execute("SELECT 1")
    conn2 = connection([])
    conn2.close()
    with pytest.raises(InterfaceError):
        conn2.cursor()


def test_the_exception_classes_hang_off_the_connection_too():
    conn = connection([])
    assert conn.Error is bigdb.Error
    assert conn.ProgrammingError is dbapi.ProgrammingError


def test_a_format_csv_statement_says_to_use_the_client_instead():
    csv = RawResponse(200, (("content-type", "text/csv"),), b"n\n1\n")
    conn = connection([csv])
    cur = conn.cursor()
    with pytest.raises(NotSupportedError, match="TextResult"):
        cur.execute("SELECT n FROM t FORMAT CSVWithNames")


# --------------------------------------------------------------------------------------------
# executemany, which is the one place this layer earns its keep.
# --------------------------------------------------------------------------------------------


def test_executemany_sends_one_statement_not_one_per_row():
    conn = connection([ok({"columns": ["inserted"], "rows": [[3]]})])
    cur = conn.cursor()
    cur.executemany("INSERT INTO tx (a, b) VALUES (?, ?)", [(1, "x"), (2, "y"), (3, "z")])
    assert len(conn.client._transport.sent) == 1, "the server commits once per request"
    sent = conn.client._transport.sent[0][2].decode()
    # The tuple template keeps the caller's own spacing; only the `?` is replaced.
    assert sent == "INSERT INTO tx (a, b) VALUES (1, 'x'),(2, 'y'),(3, 'z')"
    assert cur.rowcount == 3


def test_executemany_splits_when_the_byte_cap_is_reached():
    # The head alone is 26 bytes and each tuple costs 4, so 36 fits two tuples per statement.
    conn = dbapi.Connection(
        client([ok({"columns": ["inserted"], "rows": [[2]]}) for _ in range(3)], max_bytes=36)
    )
    cur = conn.cursor()
    cur.executemany("INSERT INTO tx (a) VALUES (?)", [(i,) for i in range(6)])
    assert len(conn.client._transport.sent) == 3
    for _, _, body in conn.client._transport.sent:
        assert len(body) <= 36
    # Every row still arrived, split across statements rather than dropped.
    assert cur.rowcount == 6


def test_executemany_falls_back_to_one_statement_per_row_when_the_shape_is_not_values():
    conn = connection([ok({"columns": ["inserted"], "rows": [[1]]}) for _ in range(2)])
    cur = conn.cursor()
    cur.executemany("UPDATE tx SET a = ? WHERE b = 1", [(1,), (2,)])
    assert len(conn.client._transport.sent) == 2
    assert cur.rowcount == 2


def test_executemany_does_not_mistake_the_word_values_inside_a_string():
    conn = connection([ok({"columns": ["inserted"], "rows": [[1]]}) for _ in range(2)])
    cur = conn.cursor()
    # `VALUES` here is text, not the clause. Appending tuples after it would corrupt the
    # statement, so the fallback is the correct answer.
    cur.executemany("UPDATE t SET note = ? WHERE note = 'VALUES (1)'", [("a",), ("b",)])
    assert len(conn.client._transport.sent) == 2


def test_executemany_with_no_rows_sends_nothing():
    conn = connection([])
    cur = conn.cursor()
    cur.executemany("INSERT INTO tx (a) VALUES (?)", [])
    assert conn.client._transport.sent == []
    assert cur.rowcount == 0


def test_a_row_of_the_wrong_width_is_refused_by_name():
    conn = connection([ok({"columns": ["inserted"], "rows": [[1]]})])
    cur = conn.cursor()
    with pytest.raises(ProgrammingError):
        cur.executemany("INSERT INTO tx (a, b) VALUES (?, ?)", [(1,)])


def test_connect_is_reachable_from_the_package_root():
    assert callable(bigdb.connect)
    assert bigdb.connect is not dbapi.connect  # a wrapper, so `import bigdb` stays light


def test_unavailable_is_the_class_a_503_lands_on():
    assert issubclass(Unavailable, dbapi.OperationalError)


# --------------------------------------------------------------------------------------------
# `executemany`'s tail detection, which is the one place a wrong guess corrupts data.
# --------------------------------------------------------------------------------------------


def test_a_paren_inside_a_string_does_not_make_two_tuples_look_like_one():
    """Two top-level tuples must be refused even when their parens balance inside literals.

    `('(', ?), (?, ')')` is genuinely two tuples. A depth count that does not skip string
    literals sees the `(` in the first literal and the `)` in the last cancel out, returns to
    zero exactly at the final character, and accepts it as a single tuple. `executemany` then
    renders each caller row against the *whole* two-tuple template, so one logical row is
    written as two mismatched rows - silent corruption of the caller's data.
    """
    from bigdb.dbapi import _values_tail

    assert _values_tail("INSERT INTO t (a,b) VALUES ('(', ?), (?, ')')") is None
    # The honest single-tuple forms still work, literals and all.
    assert _values_tail("INSERT INTO t (a,b) VALUES (?, ')')") is not None
    assert _values_tail("INSERT INTO t (a,b) VALUES ('(', ?)") is not None


def test_a_row_is_never_split_across_two_written_rows():
    """The end-to-end statement of the same defect."""
    conn = connection([ok({"columns": ["inserted"], "rows": [[1]]}) for _ in range(2)])
    cur = conn.cursor()
    cur.executemany("INSERT INTO t (a,b) VALUES ('(', ?), (?, ')')", [[111, 222], [333, 444]])
    for _, _, body in conn.client._transport.sent:
        statement = body.decode()
        # Each caller row must appear together in one statement, not spread across tuples of
        # a template that was never a single tuple.
        assert "('(', 111), (222, ')'),('(', 333)" not in statement


def test_a_word_merely_ending_in_values_is_not_a_values_clause():
    """`rfind` is a substring search, so the keyword has to be checked as a whole word.

    Without it, `SELECT * FROM myvalues (?)` looks like a `VALUES` clause and `executemany`
    appends tuples straight after a table name.
    """
    from bigdb.dbapi import _values_tail

    assert _values_tail("INSERT INTO t (a) SELECT * FROM myvalues (?)") is None
    assert _values_tail("INSERT INTO t (a) SELECT rowvalues (?)") is None
    assert _values_tail("INSERT INTO t (a) VALUESX (?)") is None
    # A table whose name merely contains the word is still fine, because the real clause follows.
    assert _values_tail("INSERT INTO oldvalues (a) VALUES (?)") is not None
    assert _values_tail('INSERT INTO "VALUES" (a) VALUES (?)') is not None
