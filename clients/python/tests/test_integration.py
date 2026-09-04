"""Against a real `big serve`. Skipped unless one is there - see `conftest.py`.

What belongs here is what a fixture would keep agreeing about long after the two had parted
company: the escaping actually round-trips, the import format is actually read as facts, the
schema vocabulary is actually what this client decodes. A unit test with a canned body proves
this client is self-consistent; only these prove it agrees with the server.
"""

from __future__ import annotations

import uuid

import pytest

import bigdb
from bigdb.results import Count, RecordPage, TextResult

pytestmark = pytest.mark.integration


@pytest.fixture
def db(server):
    with bigdb.Client(server) as client:
        yield client


@pytest.fixture
def table(db):
    """A fresh table with one field of each interesting kind.

    Dropped on the way out, which is only safe because the schema-snapshot bug this suite found
    is fixed - see `test_dropping_a_table_leaves_the_others_alone`. The name is unique per test
    regardless, so a failure mid-test leaves nothing that another test can trip over.
    """
    name = f"t{uuid.uuid4().hex[:8]}"
    db.create_table(name)
    db.create_field(name, "amount", kind="int", bit_depth=20)
    db.create_field(name, "country", kind="set")
    db.create_field(name, "price", kind="decimal", bit_depth=32, scale=2)
    db.create_field(name, "delta", kind="signed", bit_depth=32)
    yield name
    db.drop_table(name)


def test_health_and_ready_need_no_credential(db):
    assert db.health() is True
    ready = db.ready()
    assert ready.status
    assert ready.version


def test_a_value_that_looks_like_sql_is_stored_as_the_text_it_is(db, table):
    """The assertion from contrib/big-message/tests/producer.rs, ported.

    If quoting were wrong, the statement would end early and the second half would run as SQL -
    so the table would be gone. Asserting it is still there is the half that matters.
    """
    nasty = "x'); DROP TABLE " + table + "; --"
    db.import_facts(table, [bigdb.Fact("country", 0, nasty)])

    got = db.sql(
        f"SELECT count(*) FROM {table} WHERE country = ?".replace(
            "?", "'" + nasty.replace("'", "''") + "'"
        )
    )
    assert got.scalar() == 1
    # And the table survived, which is the real assertion.
    assert db.schema().table(table) is not None


def test_parameters_bound_through_the_dbapi_are_data_not_sql(server, table):
    conn = bigdb.connect(server)
    try:
        cur = conn.cursor()
        cur.execute(
            f"SELECT count(*) FROM {table} WHERE country = ?", ["x'); DROP TABLE " + table + "; --"]
        )
        assert cur.fetchone() == (0,)
    finally:
        conn.close()
    # The table is still here; the injected statement never ran.
    with bigdb.Client(server) as db:
        assert db.schema().table(table) is not None


def test_the_import_format_is_read_as_facts_including_spaces_in_a_value(db, table):
    result = db.import_facts(
        table,
        [
            bigdb.Fact("amount", 0, 1250),
            bigdb.Fact("country", 0, "GB"),
            bigdb.Fact("country", 1, "United States of America"),
        ],
    )
    assert result.written == 3
    assert result.complete

    answer = db.query(table, "Count(All())")
    assert isinstance(answer, Count)
    assert answer.value == 2


def test_a_malformed_line_names_the_line_it_was_on(db, table):
    with pytest.raises(bigdb.BadRequest) as caught:
        db.import_facts(table, b"amount 0 1\nnotafact\namount 2 3\n")
    assert caught.value.code == "malformed_line"
    assert "line 2" in caught.value.message


def test_an_unknown_field_is_a_422(db, table):
    with pytest.raises(bigdb.Unprocessable) as caught:
        db.import_facts(table, [bigdb.Fact("nosuchfield", 0, 1)])
    assert caught.value.code == "unknown_field"


def test_the_schema_kinds_are_the_spellings_this_client_decodes(db, table):
    """The guard against the console's stale vocabulary creeping back in.

    `?kind=signed` on the way in comes back as `signedint`, and `types.ts` says `signed_int`.
    This is the test that would catch a well-meaning normalisation.
    """
    info = db.schema().table(table)
    kinds = {f.name: f.kind for f in info.fields}
    assert kinds["amount"] == "int"
    assert kinds["country"] == "set"
    assert kinds["price"] == "decimal"
    assert kinds["delta"] == "signedint", "written `signed`, read back `signedint`"
    assert info.field_named("price").scale == 2
    # And scale is absent on anything else.
    assert info.field_named("amount").scale is None


def test_writing_a_kind_by_its_read_spelling_is_refused(db, table):
    # The two vocabularies really are two: this is what makes normalising them wrong.
    with pytest.raises(bigdb.BadRequest):
        db.create_field(table, "d2", kind="signedint")


def test_records_and_paging(db, table):
    db.import_facts(table, [bigdb.Fact("amount", i, i + 1) for i in range(5)])
    page = db.records(table, limit=2)
    assert isinstance(page, RecordPage)
    assert len(page.records) == 2
    assert page.next is not None
    assert sorted(db.iter_records(table, page=2)) == [0, 1, 2, 3, 4]


def test_paging_a_pql_answer_that_is_not_records_is_not_pageable(db, table):
    db.import_facts(table, [bigdb.Fact("amount", 0, 1)])
    with pytest.raises(bigdb.Unprocessable) as caught:
        db.query(table, "Count(All())", limit=10)
    assert caught.value.code == "not_pageable"


def test_a_format_csv_statement_comes_back_as_text(db, table):
    db.import_facts(table, [bigdb.Fact("amount", 0, 5)])
    got = db.sql(f"SELECT amount FROM {table} FORMAT CSVWithNames")
    assert isinstance(got, TextResult)
    assert got.content_type == "text/csv"
    assert got.text.startswith("amount\n")


def test_deleting_records(db, table):
    db.import_facts(table, [bigdb.Fact("amount", i, 1) for i in range(3)])
    result = db.delete_records(table, [0, 1])
    assert result.written >= 0
    assert sorted(db.iter_records(table)) == [2]


def test_a_body_past_the_cap_is_refused_locally_before_it_is_sent(db, table):
    """The client's cap is 7 MiB under the server's 8; raising it reaches the server's."""
    with pytest.raises(bigdb.RequestTooLarge):
        db.sql("-- " + "x" * (8 << 20))


def test_an_unknown_table_is_a_404_with_a_code_that_says_which_kind(db):
    with pytest.raises(bigdb.NotFound) as caught:
        db.query("nosuchtable", "Count(All())")
    assert caught.value.code
    assert caught.value.request_id is not None


def test_databases_and_cascade(fresh_server):
    """Its own daemon, because it drops - see the `fresh_server` fixture."""
    with bigdb.Client(fresh_server) as db:
        name = f"d{uuid.uuid4().hex[:8]}"
        assert db.create_database(name) is True
        # A second create is not an error; it just did not create anything.
        assert db.create_database(name) is False
        assert db.drop_database(name) == name


def test_the_connection_survives_many_requests_on_one_socket(db):
    """Keep-alive is opt-in on this server; this is the end-to-end version of that check."""
    for _ in range(50):
        assert db.health() is True


def test_the_dbapi_layer_end_to_end(server, table):
    conn = bigdb.connect(server)
    try:
        cur = conn.cursor()
        cur.executemany(
            f"INSERT INTO {table} (amount, country) VALUES (?, ?)",
            [(10, "GB"), (20, "US"), (30, "GB")],
        )
        assert cur.rowcount == 3

        cur.execute(f"SELECT country, sum(amount) FROM {table} GROUP BY country")
        rows = dict(cur.fetchall())
        assert rows["GB"] == 40
        assert rows["US"] == 20
        assert [d[0] for d in cur.description] == ["country", "sum"] or len(cur.description) == 2
    finally:
        conn.close()


def test_a_table_outside_the_default_database_is_reachable_both_ways(fresh_server):
    """Both spellings work on every table route, and mean the same thing.

    This test used to pin the opposite. `?database=` reached the RBAC guard on every route but
    was then dropped by `/import`, `/delete`, `/records` and `/query`, and `/import` additionally
    compared the raw path segment against the bare `TableInfo::name` - so a table outside the
    default database was reachable by qualified path on three routes and by nothing at all on
    `/import`. Fixed server-side; `crates/big-http/tests/server.rs` carries the route-level
    version.
    """
    with bigdb.Client(fresh_server) as db:
        database = f"d{uuid.uuid4().hex[:8]}"
        table = f"t{uuid.uuid4().hex[:8]}"
        qualified = f"{database}.{table}"
        db.create_database(database)
        db.create_table(qualified)
        db.create_field(qualified, "amount", kind="int")

        # Qualified in the path.
        assert db.import_facts(qualified, [bigdb.Fact("amount", 0, 1)]).written == 1
        assert db.query(qualified, "Count(All())").value == 1
        assert db.records(qualified).records == (0,)

        # The same table, named by parameter instead.
        assert db.import_facts(table, [bigdb.Fact("amount", 1, 2)], database=database).written == 1
        assert db.query(table, "Count(All())", database=database).value == 2
        assert db.records(table, database=database).records == (0, 1)
        assert db.sql(f"SELECT count(*) FROM {table}", database=database).scalar() == 2
        assert db.delete_records(table, [0], database=database).written == 1

        # And a bare name still means the default database, where this table is not.
        with pytest.raises(bigdb.NotFound):
            db.import_facts(table, [bigdb.Fact("amount", 9, 9)])


def test_a_client_wide_database_scopes_every_route(fresh_server):
    """`Client(database=...)` is the same parameter, set once."""
    database = f"d{uuid.uuid4().hex[:8]}"
    table = f"t{uuid.uuid4().hex[:8]}"
    with bigdb.Client(fresh_server) as setup:
        setup.create_database(database)
        setup.create_table(f"{database}.{table}")
        setup.create_field(f"{database}.{table}", "amount", kind="int")

    with bigdb.Client(fresh_server, database=database) as db:
        assert db.import_facts(table, [bigdb.Fact("amount", 0, 1)]).written == 1
        assert db.query(table, "Count(All())").value == 1
        assert db.sql(f"SELECT count(*) FROM {table}").scalar() == 1


def test_dropping_a_table_leaves_the_others_alone(fresh_server):
    """This was an `xfail` against a server bug, and is now the assertion it should have been.

    `big_embed::schema::snapshot` walked table ids from zero and stopped at the first gap, so a
    drop hid every table with a higher id - from `/schema`, and from `/import`, which resolves a
    name through the same snapshot. The data was readable throughout, which is what made it look
    like a listing quirk. Fixed by iterating the catalog's tables instead of counting upwards;
    `crates/big-embed/tests/api.rs` carries the unit-level version.
    """
    with bigdb.Client(fresh_server) as db:
        keep = f"t{uuid.uuid4().hex[:8]}"
        goes = f"t{uuid.uuid4().hex[:8]}"
        after = f"t{uuid.uuid4().hex[:8]}"
        db.create_table(keep)
        db.create_table(goes)
        assert {t.name for t in db.schema().tables} == {keep, goes}

        db.drop_table(goes)
        assert {t.name for t in db.schema().tables} == {keep}

        # And a table created after the drop is both visible and writable - the half of this
        # that showed up as `404 unknown_table` on a table the server had just created.
        db.create_table(after)
        db.create_field(after, "n", kind="int")
        assert {t.name for t in db.schema().tables} == {keep, after}
        assert db.import_facts(after, [bigdb.Fact("n", 0, 1)]).written == 1
