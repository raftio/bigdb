"""The pure core: addresses, escaping, facts, ops, decoding, errors, retry, lifetime.

No sockets anywhere in this file. Given a route call it asserts the exact bytes; given exact
bytes it asserts the exact typed answer or exception. That is the whole of Phase 1's contract.
"""

from __future__ import annotations

import json

import pytest

from bigdb import decode, ops
from bigdb.address import Address
from bigdb.errors import (
    BadRequest,
    ConfigError,
    NotFound,
    NotSent,
    PartiallyApplied,
    PayloadTooLarge,
    ProtocolError,
    QueryTimeout,
    RequestTooLarge,
    ServerError,
    ServerFault,
    Unauthenticated,
    Unavailable,
    Unknown,
    Unprocessable,
    ValueRefused,
    classify,
)
from bigdb.escape import escape_segment, query_string
from bigdb.facts import Fact, chunk_facts, render_facts, render_records
from bigdb.lifetime import Lifetime
from bigdb.results import Count, Extreme, Groups, ProjectionRows, RecordPage, Sum, Tuples
from bigdb.retry import RetryPolicy, decide
from bigdb.transport.base import RawResponse, basic


def response(status=200, body=b"", headers=(("content-type", "application/json"),)):
    return RawResponse(status=status, headers=tuple(headers), body=body)


def js(obj, status=200, **kw):
    return response(status, json.dumps(obj).encode(), **kw)


# --------------------------------------------------------------------------------------------
# Address.
# --------------------------------------------------------------------------------------------


def test_tls_is_chosen_by_scheme_and_never_guessed():
    assert Address.parse("127.0.0.1:7654").tls is False
    assert Address.parse("http://example:7654").tls is False
    assert Address.parse("https://example:7654").tls is True
    # A port that is conventionally TLS changes nothing: guessing is the failure a caller
    # cannot see.
    assert Address.parse("example:443").tls is False


def test_a_scheme_this_client_does_not_speak_is_refused_by_name():
    with pytest.raises(ConfigError, match="ftp"):
        Address.parse("ftp://example:7654")


def test_a_ca_file_only_means_something_with_https():
    Address.parse("https://example:7654", ca_file="/tmp/ca.pem")
    with pytest.raises(ConfigError, match="ca_file"):
        Address.parse("example:7654", ca_file="/tmp/ca.pem")


def test_a_missing_port_is_refused_rather_than_defaulted():
    with pytest.raises(ConfigError, match="port"):
        Address.parse("example")


@pytest.mark.parametrize("bad", ["", "https://", "example:0", "example:70000", "example:abc"])
def test_addresses_that_name_nothing_usable(bad):
    with pytest.raises(ConfigError):
        Address.parse(bad)


def test_ipv6_is_bracketed_in_the_host_header():
    addr = Address.parse("[::1]:7654")
    assert addr.host == "::1"
    assert addr.port == 7654
    assert addr.authority == "[::1]:7654"


def test_loopback_is_recognised_so_a_password_over_it_is_silent():
    assert Address.parse("127.0.0.1:7654").is_loopback
    assert Address.parse("[::1]:7654").is_loopback
    assert Address.parse("localhost:7654").is_loopback
    assert not Address.parse("10.0.0.1:7654").is_loopback


# --------------------------------------------------------------------------------------------
# Escaping. The last two vectors are the server's own, from request.rs's decode tests.
# --------------------------------------------------------------------------------------------


def test_a_slash_in_a_name_stays_inside_one_segment():
    # The server splits segments on `/` before decoding them, so this round-trips.
    assert escape_segment("a/b") == "a%2Fb"


def test_the_servers_own_percent_decoding_vectors_round_trip():
    from urllib.parse import unquote

    for name in ("báo cáo", "bitmap+columnar", "a b", "100%", "x?y&z=1"):
        assert unquote(escape_segment(name)) == name


def test_a_dot_is_kept_literal_like_the_rust_encoder_keeps_it():
    assert escape_segment("sales.orders") == "sales.orders"


def test_a_query_string_leaves_out_what_is_not_set():
    assert query_string({"a": None, "b": 1}) == "?b=1"
    assert query_string({"a": None}) == ""
    assert query_string({"cascade": True}) == "?cascade=true"
    assert query_string({"cascade": False}) == "?cascade=false"


def test_the_authorization_header_matches_rfc_4648():
    # The same vector `contrib/big-message/src/http.rs`'s base64 tests use. An encoder that
    # disagreed here would fail as "wrong password", which is the least helpful way to break.
    assert basic("alice", "s3cret") == "Basic YWxpY2U6czNjcmV0"
    with pytest.raises(ValueError):
        basic("a:b", "x")


# --------------------------------------------------------------------------------------------
# Facts.
# --------------------------------------------------------------------------------------------


def test_a_fact_line_is_field_record_value():
    assert render_facts([Fact("amount", 0, 1250)]) == b"amount 0 1250\n"


def test_a_value_keeps_its_spaces_because_the_server_splits_on_the_first_two():
    assert render_facts([Fact("city", 7, "New York City")]) == b"city 7 New York City\n"


def test_a_field_name_with_a_space_is_refused_here_rather_than_misread_there():
    # The server would read this as field `amount`, record `paid`, and complain about the
    # record id - a sentence that names the wrong problem.
    with pytest.raises(ValueRefused, match="field name"):
        render_facts([Fact("amount paid", 0, 1)])


def test_a_newline_in_a_value_is_refused_because_it_would_end_the_line():
    with pytest.raises(ValueRefused, match="newline"):
        render_facts([Fact("note", 0, "two\nlines")])


def test_a_bool_fact_is_written_the_way_the_engine_reads_it():
    assert render_facts([Fact("active", 1, True)]) == b"active 1 true\n"


@pytest.mark.parametrize("bad", [-1, 1 << 64, "3", 1.5, True])
def test_a_record_id_is_a_u64(bad):
    with pytest.raises(ValueRefused):
        render_facts([Fact("f", bad, 1)])


def test_no_facts_is_an_empty_body_not_a_lone_newline():
    assert render_facts([]) == b""
    assert render_records([]) == b""


def test_chunking_never_splits_a_line_and_rebuilds_the_body():
    """The Python twin of `splitting_never_cuts_a_line_in_half` in routes/query.rs."""
    facts = [Fact("amount", i, i) for i in range(5000)]
    whole = render_facts(facts)
    for cap in (32, 100, 1024, 1 << 20):
        chunks = list(chunk_facts(facts, max_bytes=cap))
        assert b"".join(body for _, body in chunks) == whole, f"cap={cap}"
        for _, body in chunks:
            assert body.endswith(b"\n"), f"cap={cap}: a chunk ended mid-line"
            assert len(body) <= cap or body.count(b"\n") == 1


def test_chunk_offsets_are_fact_indices_so_a_caller_can_resume():
    facts = [Fact("amount", i, i) for i in range(10)]
    chunks = list(chunk_facts(facts, max_bytes=32))
    assert chunks[0][0] == 0
    # Each offset is where the next chunk's first fact sits in the input.
    seen = 0
    for offset, body in chunks:
        assert offset == seen
        seen += body.count(b"\n")


def test_one_fact_too_large_for_the_cap_is_named_by_index():
    facts = [Fact("f", 0, "x"), Fact("f", 1, "y" * 500)]
    with pytest.raises(RequestTooLarge) as caught:
        list(chunk_facts(facts, max_bytes=64))
    assert caught.value.at == 1


# --------------------------------------------------------------------------------------------
# Ops: the exact method, target and body for every route.
# --------------------------------------------------------------------------------------------


def test_every_route_builds_the_target_the_server_routes_on():
    assert (ops.health().method, ops.health().target) == ("GET", "/health")
    assert ops.ready().target == "/ready"
    assert ops.schema().target == "/schema"
    assert ops.sql("SELECT 1").target == "/sql"
    assert ops.sql("SELECT 1").body == b"SELECT 1"
    assert ops.query("tx", "Count(All())").target == "/table/tx/query"
    assert ops.query("tx", "Count(All())").body == b"Count(All())"
    assert ops.records("tx").target == "/table/tx/records"
    assert ops.import_facts("tx", b"amount 0 1\n").target == "/table/tx/import"
    assert ops.delete_records("tx", [1, 2]).body == b"1\n2\n"
    assert ops.create_table("tx", engine="bitmap+columnar").target == (
        "/table/tx?engine=bitmap%2Bcolumnar"
    )
    dropping = ops.drop_table("tx")
    assert (dropping.method, dropping.target, dropping.body) == ("DELETE", "/table/tx", b"")
    assert dropping.decode is decode.dropped
    assert ops.create_field("tx", "amount", kind="int", bit_depth=20).target == (
        "/table/tx/field/amount?kind=int&bit_depth=20"
    )
    assert ops.create_field("tx", "p", kind="decimal", scale=2).target == (
        "/table/tx/field/p?kind=decimal&scale=2"
    )
    assert ops.drop_field("tx", "amount").target == "/table/tx/field/amount"
    assert ops.create_database("d").target == "/database/d"
    assert ops.drop_database("d", cascade=True).target == "/database/d?cascade=true"
    # Without cascade the parameter is left out entirely rather than sent false.
    assert ops.drop_database("d").target == "/database/d"


def test_database_and_paging_reach_the_query_string():
    assert ops.sql("SELECT 1", database="sales").target == "/sql?database=sales"
    assert ops.records("tx", after=10, limit=5).target == "/table/tx/records?after=10&limit=5"
    assert ops.query("tx", "All()", database="sales", limit=2).target == (
        "/table/tx/query?database=sales&limit=2"
    )
    # after=0 is not the same as absent, and must survive.
    assert "after=0" in ops.records("tx", after=0).target


def test_a_table_name_the_router_would_split_is_encoded():
    assert ops.records("a/b").target.startswith("/table/a%2Fb/")


def test_only_import_and_the_reads_are_idempotent():
    assert ops.import_facts("t", b"").idempotent
    assert ops.delete_records("t", []).idempotent
    assert ops.query("t", "Count(All())").idempotent
    assert ops.records("t").idempotent
    assert ops.schema().idempotent
    # `/sql` is not, because telling a SELECT from an allocating INSERT needs a SQL parser.
    assert not ops.sql("SELECT 1").idempotent
    # Nor is any DDL: a full write that landed as `partially_applied` should be inspected once.
    assert not ops.create_table("t").idempotent
    assert not ops.drop_database("d").idempotent


def test_a_body_past_the_cap_is_refused_before_a_socket_is_opened():
    with pytest.raises(RequestTooLarge) as caught:
        ops.sql("x" * 200, cap=100)
    assert caught.value.cap == 100
    assert caught.value.length == 200


# --------------------------------------------------------------------------------------------
# Decoding, with bodies in the shapes json.rs writes.
# --------------------------------------------------------------------------------------------


def test_a_sql_result_set():
    result = decode.sql(js({"columns": ["country", "count"], "rows": [["GB", 41]]}))
    assert result.columns == ("country", "count")
    assert result.rows == (("GB", 41),)
    assert result.dicts() == [{"country": "GB", "count": 41}]


def test_a_one_cell_result_reads_as_a_scalar():
    assert decode.sql(js({"columns": ["count"], "rows": [[2]]})).scalar() == 2
    with pytest.raises(ValueError):
        decode.sql(js({"columns": ["a", "b"], "rows": [[1, 2]]})).scalar()


def test_a_format_csv_answer_comes_back_as_text_not_parsed():
    raw = response(200, b"country,count\nGB,41\n", [("content-type", "text/csv")])
    result = decode.sql(raw)
    assert result.content_type == "text/csv"
    assert result.text == "country,count\nGB,41\n"


@pytest.mark.parametrize(
    ("body", "kind"),
    [
        ({"count": 2}, Count),
        ({"sum": 41}, Sum),
        ({"sum": 3.5}, Sum),
        ({"value": None}, Extreme),
        ({"value": 7}, Extreme),
        ({"records": [0, 1], "next": None}, RecordPage),
        ({"tuples": [{"keys": ["GB", "x"], "value": {"count": 1}}]}, Tuples),
        ({"rows": [{"record": 3, "values": [1, "a"]}]}, ProjectionRows),
        ({"groups": [{"key": "GB", "row": 4, "value": {"count": 9}}]}, Groups),
    ],
)
def test_every_pql_shape_json_rs_can_write(body, kind):
    assert isinstance(decode.pql(js(body)), kind)


def test_a_pql_shape_this_build_has_never_heard_of_says_so():
    with pytest.raises(ProtocolError, match="no shape this client knows"):
        decode.pql(js({"histogram": [1, 2, 3]}))


def test_a_nested_group_value_is_decoded_too():
    answer = decode.pql(js({"groups": [{"key": None, "row": 4, "value": {"sum": 12}}]}))
    assert isinstance(answer, Groups)
    assert answer.groups[0].key is None
    assert isinstance(answer.groups[0].value, Sum)
    assert answer.groups[0].value.value == 12


def test_a_record_page_carries_its_cursor():
    page = decode.records(js({"records": [0, 1, 2], "next": 2}))
    assert page.records == (0, 1, 2)
    assert page.next == 2


def test_a_write_answer_with_and_without_missed_copies():
    plain = decode.wrote("imported")(js({"imported": 3}))
    assert plain.written == 3 and plain.complete

    partial = decode.wrote("deleted")(js({"deleted": 2, "missed": ["a-spare (0..1) (refused)"]}))
    assert partial.written == 2
    assert partial.missed == ("a-spare (0..1) (refused)",)
    assert not partial.complete


def test_the_schema_kinds_are_the_servers_spellings_not_the_consoles():
    raw = js(
        {
            "tables": [
                {
                    "name": "tx",
                    "engine": "bitmap+columnar",
                    "fields": [
                        {"name": "amount", "kind": "int", "bit_depth": 20},
                        {"name": "price", "kind": "decimal", "bit_depth": 32, "scale": 2},
                        {
                            "name": "t",
                            "kind": "timequantum",
                            "bit_depth": 32,
                            "granularity": ["Y", "M"],
                        },
                        {"name": "delta", "kind": "signedint", "bit_depth": 32},
                    ],
                }
            ]
        }
    )
    schema = decode.schema(raw)
    table = schema.table("tx")
    assert table is not None and table.engine == "bitmap+columnar"
    # `signedint`, not `signed_int`: the console's types.ts is stale and this pins it.
    assert [f.kind for f in table.fields] == ["int", "decimal", "timequantum", "signedint"]
    assert table.field_named("price").scale == 2
    assert table.field_named("t").granularity == ("Y", "M")
    # scale is absent for anything but a decimal.
    assert table.field_named("amount").scale is None


def test_ready_on_a_single_node_and_in_a_cluster():
    single = decode.ready(
        js(
            {
                "status": "ready",
                "tables": 1,
                "txn_id": 4,
                "pages": 9,
                "node": "a",
                "shards": "0..64",
                "version": "0.1.0",
                "wire": 6,
            }
        )
    )
    assert single.status == "ready" and single.leader is None and single.behind == ()

    clustered = decode.ready(
        js(
            {
                "status": "ready",
                "tables": 1,
                "txn_id": 4,
                "pages": 9,
                "node": "a",
                "shards": "0..64",
                "version": "0.1.0",
                "wire": 6,
                "serving": True,
                "term": 3,
                "leader": "b",
                "behind": ["a-spare"],
            }
        )
    )
    assert clustered.serving is True and clustered.leader == "b"
    assert clustered.behind == ("a-spare",)


def test_ddl_answers():
    assert decode.created_table(js({"table": 1})) == 1
    assert decode.created_field(js({"field": 2})) == 2
    assert decode.created_database(js({"database": "d", "created": True})) is True
    assert decode.created_database(js({"database": "d", "created": False})) is False
    assert decode.dropped(js({"dropped": "tx"})) == "tx"


def test_a_2xx_that_is_not_json_is_a_protocol_error():
    with pytest.raises(ProtocolError):
        decode.schema(response(200, b"<html>a proxy said hello</html>"))


# --------------------------------------------------------------------------------------------
# Errors.
# --------------------------------------------------------------------------------------------


def err(status, code, message="no", **headers):
    body = json.dumps({"error": message, "code": code}).encode()
    return classify(status, body, **headers)


@pytest.mark.parametrize(
    ("status", "kind"),
    [
        (400, BadRequest),
        (401, Unauthenticated),
        (404, NotFound),
        (413, PayloadTooLarge),
        (422, Unprocessable),
        (500, ServerFault),
        (503, Unavailable),
        (504, QueryTimeout),
    ],
)
def test_a_status_lands_on_its_class(status, kind):
    assert isinstance(err(status, "whatever"), kind)


def test_the_envelope_key_is_error_and_message_is_only_a_fallback():
    # json.rs writes `{"error": ..., "code": ...}`. The console's types.ts says `message`.
    assert err(400, "bad_request", "the sentence").message == "the sentence"
    fallback = classify(400, json.dumps({"message": "old shape", "code": "x"}).encode())
    assert fallback.message == "old shape"


def test_a_code_decides_the_class_where_the_status_cannot():
    # 500, but with a repair procedure attached, so it gets its own class to catch.
    assert isinstance(err(500, "partially_applied"), PartiallyApplied)
    assert isinstance(err(500, "partially_applied"), ServerFault)


def test_a_code_this_build_has_never_heard_of_falls_through_to_the_status():
    assert isinstance(err(409, "some_future_code"), ServerError)
    assert err(409, "some_future_code").code == "some_future_code"


def test_an_unparseable_or_empty_body_still_produces_an_exception():
    assert classify(502, b"<html>bad gateway</html>").code == ""
    assert classify(500, b"").message == ""
    assert isinstance(classify(500, b""), ServerFault)


def test_the_request_id_is_in_the_message_because_a_500_body_is_redacted():
    e = err(500, "internal", "see the server log", request_id="abc-123")
    assert "abc-123" in str(e)
    assert str(e).startswith("500 internal")


def test_only_the_transient_refusals_are_retryable():
    assert err(503, "server_busy").retryable
    assert err(503, "busy_authenticating").retryable
    assert err(503, "stale_route").retryable
    assert err(503, "owner_unreachable").retryable
    # A 504 is a definite answer that the statement did not complete.
    assert not err(504, "query_timeout").retryable
    # And a partial write is the one 500 you must not repeat.
    assert not err(500, "partially_applied").retryable
    assert not err(400, "bad_request").retryable
    assert not err(401, "unauthenticated").retryable


def test_a_401_carries_its_challenge():
    e = err(401, "unauthenticated", challenge='Basic realm="big"')
    assert isinstance(e, Unauthenticated)
    assert e.challenge == 'Basic realm="big"'


def test_a_not_found_prints_its_code_first_because_that_is_what_tells_them_apart():
    # There is no 405: a typo in a target and a missing table both arrive as 404.
    assert str(err(404, "no_such_route")).startswith("404 no_such_route")
    assert str(err(404, "unknown_table")).startswith("404 unknown_table")


# --------------------------------------------------------------------------------------------
# Retry, as a pure table. Nothing here sleeps.
# --------------------------------------------------------------------------------------------


POLICY = RetryPolicy(retries=3, first_backoff=0.05, max_backoff=1.0)


def test_a_request_that_never_arrived_is_always_retried():
    assert decide(POLICY, 0, NotSent("refused"), idempotent=False) is not None


def test_an_unknown_outcome_is_retried_only_on_an_idempotent_route():
    # `/import` writes the same bits twice, which is writing them once.
    assert decide(POLICY, 0, Unknown("timeout"), idempotent=True) is not None
    # An allocating INSERT writes two records; "possibly twice" is worse than "possibly once".
    assert decide(POLICY, 0, Unknown("timeout"), idempotent=False) is None


def test_a_protocol_error_is_treated_like_an_unknown_outcome():
    assert decide(POLICY, 0, ProtocolError("chunked"), idempotent=True) is not None
    assert decide(POLICY, 0, ProtocolError("chunked"), idempotent=False) is None


def test_a_503_is_retried_even_for_a_write_because_nothing_was_written():
    assert decide(POLICY, 0, err(503, "server_busy"), idempotent=False) is not None


def test_a_refusal_the_server_understood_is_never_retried():
    for e in (err(400, "bad_request"), err(404, "unknown_table"), err(504, "query_timeout")):
        assert decide(POLICY, 0, e, idempotent=True) is None


def test_a_client_side_refusal_is_never_retried():
    assert decide(POLICY, 0, ValueRefused("NaN"), idempotent=True) is None
    assert decide(POLICY, 0, RequestTooLarge(9, 8), idempotent=True) is None


def test_the_budget_runs_out():
    assert decide(POLICY, 2, NotSent("x"), idempotent=True) is not None
    assert decide(POLICY, 3, NotSent("x"), idempotent=True) is None


def test_the_backoff_doubles_and_is_capped():
    delays = [decide(POLICY, n, NotSent("x"), idempotent=True) for n in range(3)]
    assert delays == [0.05, 0.1, 0.2]
    tight = RetryPolicy(retries=9, first_backoff=1.0, max_backoff=2.0)
    assert decide(tight, 5, NotSent("x"), idempotent=True) == 2.0


def test_retry_after_is_a_floor_and_may_exceed_the_cap():
    e = err(503, "server_busy", retry_after=5.0)
    assert decide(POLICY, 0, e, idempotent=True) == 5.0


def test_a_retry_after_that_is_not_a_number_is_treated_as_absent():
    raw = response(503, b"{}", [("retry-after", "Wed, 21 Oct 2026 07:28:00 GMT")])
    assert raw.retry_after is None


# --------------------------------------------------------------------------------------------
# Lifetime. Nothing here sleeps either.
# --------------------------------------------------------------------------------------------


def test_a_connection_is_retired_before_the_server_would_close_it():
    life = Lifetime(max_requests=900, max_idle=3.0)
    assert not life.stale(now=0.0)  # opened, never used
    for n in range(899):
        life.record(now=float(n))
    assert not life.stale(now=899.0)
    life.record(now=899.0)
    assert life.stale(now=899.0), "the request cap is ours, under the server's 1000"


def test_an_idle_connection_is_retired_before_the_server_times_it_out():
    life = Lifetime(max_requests=900, max_idle=3.0)
    life.record(now=100.0)
    assert not life.stale(now=102.9)
    assert life.stale(now=103.0), "the idle cap is ours, under the server's 5s"


def test_a_fresh_connection_starts_again_from_nothing():
    life = Lifetime()
    life.record(now=1.0)
    life.reset()
    assert life.sent == 0
    assert not life.stale(now=1_000_000.0)


# --------------------------------------------------------------------------------------------
# RawResponse.
# --------------------------------------------------------------------------------------------


def test_the_first_header_wins_like_the_servers_own_reader():
    raw = RawResponse(200, (("x-request-id", "first"), ("x-request-id", "second")), b"")
    assert raw.request_id == "first"


def test_a_content_type_is_read_without_its_parameters():
    raw = response(200, b"", [("content-type", "text/plain; version=0.0.4; charset=utf-8")])
    assert raw.content_type == "text/plain"
