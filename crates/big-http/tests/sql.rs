// Copyright 2026 Bany
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! `POST /sql` over a real socket, including every status a refusal can produce.
//!
//! The statuses are the part worth testing rather than assuming. A code in a body is what a
//! client branches on, but a status is what a proxy and a retry policy act on without reading
//! the body at all - and "refused forever" and "try again" must not look alike.

mod common;
use common::{send, spawn};

use std::net::SocketAddr;

/// Builds the table the rest of the file queries, and returns the address.
fn stocked(requests: usize) -> SocketAddr {
    let addr = spawn(requests + 4);
    assert_eq!(send(addr, "POST", "/table/tx", "").0, 200);
    assert_eq!(send(addr, "POST", "/table/tx/field/amount?kind=int&bit_depth=32", "").0, 200);
    assert_eq!(send(addr, "POST", "/table/tx/field/country?kind=set", "").0, 200);
    let (status, _) = send(
        addr,
        "POST",
        "/table/tx/import",
        "amount 1 100\ncountry 1 GB\namount 2 900\ncountry 2 US\namount 3 500\ncountry 3 GB\n",
    );
    assert_eq!(status, 200);
    addr
}

#[test]
fn every_answer_shape_comes_back_as_columns_and_rows() {
    let addr = stocked(5);

    let (status, body) = send(addr, "POST", "/sql", "SELECT count(*) FROM tx WHERE amount >= 500");
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, r#"{"columns":["count"],"rows":[[2]]}"#);

    let (_, body) = send(addr, "POST", "/sql", "SELECT sum(amount) AS total FROM tx");
    assert_eq!(body, r#"{"columns":["total"],"rows":[[1500]]}"#);

    let (_, body) = send(addr, "POST", "/sql", "SELECT country, count(*) FROM tx GROUP BY country");
    assert_eq!(body, r#"{"columns":["country","count"],"rows":[["GB",2],["US",1]]}"#);

    let (_, body) = send(addr, "POST", "/sql", "SELECT count(DISTINCT country) FROM tx");
    assert_eq!(body, r#"{"columns":["count"],"rows":[[2]]}"#);

    // `SELECT *` is record ids, under a column that says so.
    let (_, body) = send(addr, "POST", "/sql", "SELECT * FROM tx WHERE country = 'GB'");
    assert_eq!(body, r#"{"columns":["_record_id"],"rows":[[1],[3]]}"#);
}

/// A ranking is cut by the plan, and `LIMIT` on an unranked grouping cuts the shape.
#[test]
fn limit_applies_where_the_statement_put_it() {
    let addr = stocked(2);
    let (_, body) = send(
        addr,
        "POST",
        "/sql",
        "SELECT country, count(*) AS n FROM tx GROUP BY country ORDER BY n DESC LIMIT 1",
    );
    assert_eq!(body, r#"{"columns":["country","n"],"rows":[["GB",2]]}"#);

    let (_, body) = send(addr, "POST", "/sql", "SELECT * FROM tx LIMIT 2");
    assert_eq!(body, r#"{"columns":["_record_id"],"rows":[[1],[2]]}"#);
}

/// Nothing matched is `null`, not zero: a minimum over an empty set is absent, and a result set
/// that printed `0` would be a wrong number rather than a missing one.
#[test]
fn an_aggregate_over_nothing_answers_null() {
    let addr = stocked(1);
    let (_, body) = send(addr, "POST", "/sql", "SELECT min(amount) FROM tx WHERE amount > 10000");
    assert_eq!(body, r#"{"columns":["min"],"rows":[[null]]}"#);
}

/// The statuses, which are what a proxy sees. A refusal is permanent and says `400`; a table
/// that is not there is `404` because it is the thing being addressed; a field that is not
/// there is `422` because the body is well formed and describes something absent.
#[test]
fn a_refusal_and_a_schema_mistake_get_different_statuses() {
    let addr = stocked(6);

    // A join on a keyed column is answered, so the refusal here is the pairing that has no
    // key, which is what a comma between tables is.
    let (status, body) = send(addr, "POST", "/sql", "SELECT count(*) FROM tx, other");
    assert_eq!(status, 400);
    assert!(body.contains(r#""code":"sql_no_joins""#), "{body}");
    assert!(body.contains("comma between tables"), "{body}");

    let (status, body) = send(addr, "POST", "/sql", "SELECT amount FROM tx");
    assert_eq!(status, 400);
    assert!(body.contains(r#""code":"sql_projection_unsupported""#), "{body}");

    let (status, body) = send(addr, "POST", "/sql", "DELETE FROM tx");
    assert_eq!(status, 400);
    assert!(body.contains(r#""code":"sql_read_only""#), "{body}");

    let (status, body) = send(addr, "POST", "/sql", "SELECT count(*) FROM nope");
    assert_eq!(status, 404, "{body}");
    assert!(body.contains(r#""code":"unknown_table""#), "{body}");

    let (status, body) = send(addr, "POST", "/sql", "SELECT count(*) FROM tx WHERE nope = 1");
    assert_eq!(status, 422, "{body}");
    assert!(body.contains(r#""code":"unknown_field""#), "{body}");

    let (status, body) = send(addr, "POST", "/sql", "not a statement");
    assert_eq!(status, 400);
    assert!(body.contains(r#""code":"parse_error""#), "{body}");
}

/// The route exists at the top level and nowhere else: a `SELECT` names its own table.
#[test]
fn the_route_does_not_hang_off_a_table() {
    let addr = stocked(1);
    assert_eq!(send(addr, "POST", "/table/tx/sql", "SELECT count(*) FROM tx").0, 404);
}

/// `SELECT DISTINCT c` answers with the keys, and with the same rows `GROUP BY c` gives.
#[test]
fn select_distinct_lists_the_keys() {
    let addr = stocked(2);

    let (status, body) = send(addr, "POST", "/sql", "SELECT DISTINCT country FROM tx");
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, r#"{"columns":["country"],"rows":[["GB"],["US"]]}"#);

    // The narrowed form, to show the `WHERE` reaches it: only GB has a record under 500.
    let (_, body) =
        send(addr, "POST", "/sql", "SELECT DISTINCT country FROM tx WHERE amount < 500");
    assert_eq!(body, r#"{"columns":["country"],"rows":[["GB"]]}"#);
}

/// `HAVING` drops groups by their own count.
#[test]
fn having_drops_groups_by_their_count() {
    let addr = stocked(3);

    // GB holds two records and US one.
    let (status, body) = send(
        addr,
        "POST",
        "/sql",
        "SELECT country, count(*) FROM tx GROUP BY country HAVING count(*) > 1",
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, r#"{"columns":["country","count"],"rows":[["GB",2]]}"#);

    let (_, body) = send(
        addr,
        "POST",
        "/sql",
        "SELECT country, count(*) FROM tx GROUP BY country HAVING count(*) = 1",
    );
    assert_eq!(body, r#"{"columns":["country","count"],"rows":[["US",1]]}"#);

    // A predicate nothing satisfies is an empty answer, not an error.
    let (status, body) = send(
        addr,
        "POST",
        "/sql",
        "SELECT country, count(*) FROM tx GROUP BY country HAVING count(*) > 99",
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, r#"{"columns":["country","count"],"rows":[]}"#);
}

/// `HAVING` runs before `LIMIT`, which is the whole reason the ranking's cut moves to the shape.
///
/// GB has two records and US one, so ranked by count the order is GB then US. Asking for the
/// groups with fewer than two records, ranked, limited to one, must answer US: if the plan had
/// been allowed to keep `TopN(n=1)` it would have picked GB first and the predicate would then
/// have emptied the answer.
#[test]
fn having_runs_before_the_limit_it_shares_a_statement_with() {
    let addr = stocked(1);
    let (status, body) = send(
        addr,
        "POST",
        "/sql",
        "SELECT country, count(*) AS n FROM tx GROUP BY country HAVING count(*) < 2 \
         ORDER BY n DESC LIMIT 1",
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, r#"{"columns":["country","n"],"rows":[["US",1]]}"#);
}

/// The ordering the plan cannot carry, done on the merged answer — and it is a different
/// answer from the one the ranking gives.
///
/// **The assertion that earns its place is that the two disagree.** `GB` holds the most
/// records and `US` holds the largest total, so a `ORDER BY sum(amount) DESC` that quietly fell
/// back on the count ranking would answer `GB` first and look entirely plausible.
#[test]
fn ordering_by_the_aggregate_is_not_the_count_ranking() {
    let addr = stocked(4);

    // GB: two records, 600. US: one record, 900.
    let (status, body) = send(
        addr,
        "POST",
        "/sql",
        "SELECT country, sum(amount) FROM tx GROUP BY country ORDER BY sum(amount) DESC",
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, r#"{"columns":["country","sum"],"rows":[["US",900],["GB",600]]}"#);

    // The same statement ranked by count puts them the other way round.
    let (_, body) = send(
        addr,
        "POST",
        "/sql",
        "SELECT country, count(*) FROM tx GROUP BY country ORDER BY count(*) DESC",
    );
    assert_eq!(body, r#"{"columns":["country","count"],"rows":[["GB",2],["US",1]]}"#);

    // Ascending by count, which `TopN` cannot do and the shape can.
    let (_, body) = send(
        addr,
        "POST",
        "/sql",
        "SELECT country, count(*) FROM tx GROUP BY country ORDER BY count(*) ASC",
    );
    assert_eq!(body, r#"{"columns":["country","count"],"rows":[["US",1],["GB",2]]}"#);

    // Descending by the grouped column, which is the order groups arrive in, reversed.
    let (_, body) = send(
        addr,
        "POST",
        "/sql",
        "SELECT country, count(*) FROM tx GROUP BY country ORDER BY country DESC",
    );
    assert_eq!(body, r#"{"columns":["country","count"],"rows":[["US",1],["GB",2]]}"#);
}

/// `OFFSET` pages the groups, and the ranking it pages into is cut to `offset + limit` rather
/// than to `limit`.
///
/// The second request is the one that would break under a plan cut to `n = limit`: `TopN(n=1)`
/// hands back one group, and skipping one of them answers with nothing at all.
#[test]
fn an_offset_pages_the_groups() {
    let addr = stocked(3);

    let (status, body) = send(
        addr,
        "POST",
        "/sql",
        "SELECT country, count(*) FROM tx GROUP BY country ORDER BY count(*) DESC LIMIT 1",
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, r#"{"columns":["country","count"],"rows":[["GB",2]]}"#);

    let (_, body) = send(
        addr,
        "POST",
        "/sql",
        "SELECT country, count(*) FROM tx GROUP BY country ORDER BY count(*) DESC \
         LIMIT 1 OFFSET 1",
    );
    assert_eq!(body, r#"{"columns":["country","count"],"rows":[["US",1]]}"#);

    // Past the end is an empty answer, not an error.
    let (_, body) = send(addr, "POST", "/sql", "SELECT DISTINCT country FROM tx LIMIT 5 OFFSET 5");
    assert_eq!(body, r#"{"columns":["country"],"rows":[]}"#);
}

/// A `HAVING` on an aggregate compares in the units the field stores, which is the conversion
/// `WHERE` already does.
///
/// **The whole point of the test is the decimal.** `price` has two decimal places, so a total
/// of `12.50` is stored as `1250`. A threshold that skipped the conversion would compare `1250`
/// against `12` and keep every group — off by a factor of a hundred, with both numbers valid
/// and nothing in the answer able to show it.
#[test]
fn a_having_threshold_is_converted_the_way_a_where_bound_is() {
    let addr = spawn(8);
    assert_eq!(send(addr, "POST", "/table/s", "").0, 200);
    assert_eq!(
        send(addr, "POST", "/table/s/field/price?kind=decimal&bit_depth=32&scale=2", "").0,
        200
    );
    assert_eq!(send(addr, "POST", "/table/s/field/shop?kind=set", "").0, 200);
    // `a` totals 12.50, `b` totals 3.00.
    let (status, _) = send(
        addr,
        "POST",
        "/table/s/import",
        "price 1 1000
shop 1 a
price 2 250
shop 2 a
price 3 300
shop 3 b
",
    );
    assert_eq!(status, 200);

    // Written as the field is written, and it keeps only the shop above it.
    let (status, body) = send(
        addr,
        "POST",
        "/sql",
        "SELECT shop, sum(price) FROM s GROUP BY shop HAVING sum(price) >= 10.00",
    );
    assert_eq!(status, 200, "{body}");
    // And the total comes back as the value it is, not as the integer it is stored as: the
    // threshold was converted on the way in, so the answer is converted on the way out.
    assert_eq!(body, r#"{"columns":["shop","sum"],"rows":[["a",12.50]]}"#);

    // A whole number is the same conversion: `10` on a two-place field is `1000` units, which
    // `b`'s total of `300` is still under.
    let (_, body) = send(
        addr,
        "POST",
        "/sql",
        "SELECT shop, sum(price) FROM s GROUP BY shop HAVING sum(price) >= 10",
    );
    // And the total comes back as the value it is, not as the integer it is stored as: the
    // threshold was converted on the way in, so the answer is converted on the way out.
    assert_eq!(body, r#"{"columns":["shop","sum"],"rows":[["a",12.50]]}"#);

    // More digits than the field stores is refused rather than rounded, exactly as in a
    // `WHERE`, and with the same code and the same status - because it is the same code
    // deciding, one layer up.
    let (status, body) = send(
        addr,
        "POST",
        "/sql",
        "SELECT shop, sum(price) FROM s GROUP BY shop HAVING sum(price) >= 10.005",
    );
    assert_eq!(status, 400, "{body}");
    assert!(body.contains(r#""code":"too_precise""#), "{body}");

    // And the biggest totals first, over the same decimal.
    let (_, body) = send(
        addr,
        "POST",
        "/sql",
        "SELECT shop, sum(price) FROM s GROUP BY shop ORDER BY sum(price) DESC LIMIT 1",
    );
    // And the total comes back as the value it is, not as the integer it is stored as: the
    // threshold was converted on the way in, so the answer is converted on the way out.
    assert_eq!(body, r#"{"columns":["shop","sum"],"rows":[["a",12.50]]}"#);
}

/// Several aggregates in one statement come back as one row of cells.
///
/// Each is its own plan, fanned out and merged on its own; the row is assembled last, at the
/// coordinator. What the client sees is one result set, which is the whole point.
#[test]
fn one_statement_answers_several_questions() {
    let addr = stocked(3);

    // 100 + 900 + 500 over three records.
    let (status, body) = send(
        addr,
        "POST",
        "/sql",
        "SELECT count(*), sum(amount), min(amount), max(amount) FROM tx",
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, r#"{"columns":["count","sum","min","max"],"rows":[[3,1500,100,900]]}"#);

    // Aliases name the columns, and the `WHERE` reaches every one of them.
    let (_, body) = send(
        addr,
        "POST",
        "/sql",
        "SELECT count(*) AS n, sum(amount) AS total FROM tx WHERE country = 'GB'",
    );
    assert_eq!(body, r#"{"columns":["n","total"],"rows":[[2,600]]}"#);

    // A distinct count sits beside the others, still counted after the merge.
    let (_, body) = send(addr, "POST", "/sql", "SELECT count(*), count(DISTINCT country) FROM tx");
    assert_eq!(body, r#"{"columns":["count","count"],"rows":[[3,2]]}"#);
}

/// `avg` is a sum over a count, divided after both have been merged.
///
/// **The division cannot happen any earlier.** A ratio of one node's share is not a share of
/// the ratio, so an average computed per node and then combined would be an average of
/// averages - a different number, and one no client could tell apart from the right one.
#[test]
fn an_average_is_a_sum_over_a_count_taken_after_the_merge() {
    let addr = stocked(4);

    // 1500 over 3.
    let (status, body) = send(addr, "POST", "/sql", "SELECT avg(amount) FROM tx");
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, r#"{"columns":["avg"],"rows":[[500.0]]}"#);

    // 600 over 2, which is exact, and still rendered with a point so the column is one kind of
    // number rather than two.
    let (_, body) = send(addr, "POST", "/sql", "SELECT avg(amount) FROM tx WHERE country = 'GB'");
    assert_eq!(body, r#"{"columns":["avg"],"rows":[[300.0]]}"#);

    // No records is no average: `null`, not a division by zero and not a zero.
    let (_, body) = send(addr, "POST", "/sql", "SELECT avg(amount) FROM tx WHERE amount > 10000");
    assert_eq!(body, r#"{"columns":["avg"],"rows":[[null]]}"#);

    // Beside the halves it is made of, which share their plans with it rather than repeat them.
    let (_, body) = send(addr, "POST", "/sql", "SELECT count(*), sum(amount), avg(amount) FROM tx");
    assert_eq!(body, r#"{"columns":["count","sum","avg"],"rows":[[3,1500,500.0]]}"#);
}

/// A grouped answer with more than one aggregate is several grouped plans, joined on the
/// group's row id after each has been merged.
#[test]
fn a_group_can_carry_more_than_one_number() {
    let addr = stocked(3);

    // GB: two records, 600, average 300. US: one, 900, average 900.
    let (status, body) = send(
        addr,
        "POST",
        "/sql",
        "SELECT country, count(*), sum(amount) FROM tx GROUP BY country",
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, r#"{"columns":["country","count","sum"],"rows":[["GB",2,600],["US",1,900]]}"#);

    let (_, body) = send(
        addr,
        "POST",
        "/sql",
        "SELECT country, count(*), avg(amount) FROM tx GROUP BY country",
    );
    assert_eq!(
        body,
        r#"{"columns":["country","count","avg"],"rows":[["GB",2,300.0],["US",1,900.0]]}"#
    );

    // And the ordering picks out one of the several numbers, not whichever came first.
    let (_, body) = send(
        addr,
        "POST",
        "/sql",
        "SELECT country, count(*), sum(amount) FROM tx GROUP BY country \
         ORDER BY sum(amount) DESC",
    );
    assert_eq!(body, r#"{"columns":["country","count","sum"],"rows":[["US",1,900],["GB",2,600]]}"#);
}

/// A `HAVING` over a grouping with several aggregates filters on the one it names.
#[test]
fn having_picks_out_the_aggregate_it_names() {
    let addr = stocked(3);

    // Filtering on the total keeps US, which has the fewest records.
    let (status, body) = send(
        addr,
        "POST",
        "/sql",
        "SELECT country, count(*), sum(amount) FROM tx GROUP BY country HAVING sum(amount) > 700",
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, r#"{"columns":["country","count","sum"],"rows":[["US",1,900]]}"#);

    // Filtering on the count keeps GB, which has the smaller total. Same statement otherwise.
    let (_, body) = send(
        addr,
        "POST",
        "/sql",
        "SELECT country, count(*), sum(amount) FROM tx GROUP BY country HAVING count(*) > 1",
    );
    assert_eq!(body, r#"{"columns":["country","count","sum"],"rows":[["GB",2,600]]}"#);

    // An average is fractional and this comparison is not, so a `HAVING` on one is refused
    // rather than rounded into an answer next to the one that was asked for.
    let (status, body) = send(
        addr,
        "POST",
        "/sql",
        "SELECT country, avg(amount) FROM tx GROUP BY country HAVING avg(amount) > 300",
    );
    assert_eq!(status, 400, "{body}");
    assert!(body.contains(r#""code":"sql_unsupported""#), "{body}");
}

/// `FILTER (WHERE ...)` narrows one aggregate and leaves its neighbours alone.
///
/// This is the clause that turns one statement into a dashboard row: a total and a share of it,
/// over one `WHERE`, in one result set. Each filtered aggregate is a plan of its own, which is
/// why it costs a fan-out rather than an expression evaluator.
#[test]
fn a_filter_narrows_one_aggregate_and_not_its_neighbours() {
    let addr = stocked(4);

    // Three records: 100/GB, 900/US, 500/GB.
    let (status, body) = send(
        addr,
        "POST",
        "/sql",
        "SELECT count(*) AS all_of_them, count(*) FILTER (WHERE amount >= 500) AS big FROM tx",
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, r#"{"columns":["all_of_them","big"],"rows":[[3,2]]}"#);

    // The statement's own `WHERE` still applies to both, and the `FILTER` narrows further.
    let (_, body) = send(
        addr,
        "POST",
        "/sql",
        "SELECT count(*) AS n, count(*) FILTER (WHERE amount >= 500) AS big FROM tx \
         WHERE country = 'GB'",
    );
    assert_eq!(body, r#"{"columns":["n","big"],"rows":[[2,1]]}"#);

    // Sums as well as counts, and two different filters side by side.
    let (_, body) = send(
        addr,
        "POST",
        "/sql",
        "SELECT sum(amount) FILTER (WHERE country = 'GB') AS gb, \
                sum(amount) FILTER (WHERE country = 'US') AS us FROM tx",
    );
    assert_eq!(body, r#"{"columns":["gb","us"],"rows":[[600,900]]}"#);

    // A filter nothing satisfies. A count is zero and a sum is zero - this engine has no
    // absent total, and `sum` over an empty `WHERE` has always answered `0` - while a `min`
    // over nothing is the `null` it has always been.
    let (_, body) = send(
        addr,
        "POST",
        "/sql",
        "SELECT count(*) FILTER (WHERE amount > 10000) AS n, \
                sum(amount) FILTER (WHERE amount > 10000) AS s, \
                min(amount) FILTER (WHERE amount > 10000) AS m FROM tx",
    );
    assert_eq!(body, r#"{"columns":["n","s","m"],"rows":[[0,0,null]]}"#);
}

/// A `FILTER` inside a `GROUP BY` leaves the plans describing different groups, and the join
/// has to say the right thing about the ones only some of them know.
///
/// **This is the case the two group cells exist for.** `US` holds no record under 500, so the
/// filtered plan has no group for it at all — and the answer there is `0` for a count and
/// `null` for a sum, which is what SQL says and not the same thing. A join that rendered
/// whatever the first plan happened to hold would get one of them wrong.
#[test]
fn a_filtered_group_that_matched_nothing_is_zero_for_a_count_and_absent_for_a_sum() {
    let addr = stocked(2);

    let (status, body) = send(
        addr,
        "POST",
        "/sql",
        "SELECT country, count(*) AS n, count(*) FILTER (WHERE amount < 500) AS small FROM tx \
         GROUP BY country",
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, r#"{"columns":["country","n","small"],"rows":[["GB",2,1],["US",1,0]]}"#);

    // `US` holds no record under 500, so the filtered plan has no group for it at all. The row
    // is still there - the group set comes from the `WHERE`, not from the filter - and it reads
    // as the number that plan would have produced over no records.
    let (_, body) = send(
        addr,
        "POST",
        "/sql",
        "SELECT country, sum(amount) FILTER (WHERE amount < 500) AS small, \
                min(amount) FILTER (WHERE amount < 500) AS lowest FROM tx \
         GROUP BY country",
    );
    assert_eq!(
        body,
        r#"{"columns":["country","small","lowest"],"rows":[["GB",100,100],["US",0,null]]}"#
    );
}

/// Selecting stored values, which this surface refused until the cut that bounds the cost
/// became part of the statement.
///
/// A projection reconstructs a value per record per column out of the bit planes it is stored
/// in, so the number of records **is** what it costs. That is why the `LIMIT` is required and
/// why it lives in the plan rather than in the shape: a cut applied to the answer would be a
/// cut applied after paying for it.
#[test]
fn stored_values_can_be_selected_under_a_limit() {
    let addr = stocked(6);

    // Records 1, 2, 3 hold 100, 900, 500, and come back in record order.
    let (status, body) = send(addr, "POST", "/sql", "SELECT amount FROM tx LIMIT 10");
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, r#"{"columns":["amount"],"rows":[[100],[900],[500]]}"#);

    // The `WHERE` selects which records are read, so the reads are only paid for the matches.
    let (_, body) =
        send(addr, "POST", "/sql", "SELECT amount FROM tx WHERE country = 'GB' LIMIT 10");
    assert_eq!(body, r#"{"columns":["amount"],"rows":[[100],[500]]}"#);

    // The cut happens before the reads, not after them.
    let (_, body) = send(addr, "POST", "/sql", "SELECT amount AS a FROM tx LIMIT 2");
    assert_eq!(body, r#"{"columns":["a"],"rows":[[100],[900]]}"#);

    // A keyed column comes back as the string it was interned from, because this table keeps
    // its values in column segments. On a table that does not, there is no read from a record
    // to its key at all and the planner refuses it - see
    // `a_keyed_column_is_refused_on_a_table_that_stores_no_values`.
    let (status, body) = send(addr, "POST", "/sql", "SELECT country FROM tx LIMIT 10");
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, r#"{"columns":["country"],"rows":[["GB"],["US"],["GB"]]}"#);

    // Without a limit there is no bound on what it costs, so the statement is refused.
    let (status, body) = send(addr, "POST", "/sql", "SELECT amount FROM tx");
    assert_eq!(status, 400, "{body}");
    assert!(body.contains(r#""code":"sql_projection_unsupported""#), "{body}");
    assert!(body.contains("LIMIT"), "{body}");

    // And a value beside a number about the whole set is two answers of different heights.
    let (status, _) = send(addr, "POST", "/sql", "SELECT amount, count(*) FROM tx LIMIT 10");
    assert_eq!(status, 400);
}

/// Several columns at once, and a record that holds no value in one of them.
#[test]
fn a_projection_reads_every_column_it_names_and_says_so_when_there_is_nothing_there() {
    let addr = spawn(7);
    assert_eq!(send(addr, "POST", "/table/p", "").0, 200);
    assert_eq!(send(addr, "POST", "/table/p/field/qty?kind=int&bit_depth=16", "").0, 200);
    assert_eq!(
        send(addr, "POST", "/table/p/field/price?kind=decimal&bit_depth=32&scale=2", "").0,
        200
    );
    // Record 2 holds a quantity and no price.
    let (status, _) = send(addr, "POST", "/table/p/import", "qty 1 3\nprice 1 1250\nqty 2 7\n");
    assert_eq!(status, 200);

    // Column order follows the select list, and a decimal comes back as the value it is: the
    // field keeps two digits, so the 1250 units it stores are 12.50.
    let (status, body) = send(addr, "POST", "/sql", "SELECT qty, price FROM p LIMIT 10");
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, r#"{"columns":["qty","price"],"rows":[[3,12.50],[7,null]]}"#);

    let (_, body) = send(addr, "POST", "/sql", "SELECT price, qty FROM p LIMIT 10");
    assert_eq!(body, r#"{"columns":["price","qty"],"rows":[[12.50,3],[null,7]]}"#);

    // **The query language answers in units, and that is not a disagreement.** It has no schema
    // in the statement and no scale in the answer - `Project` names fields, not types - so what
    // it hands back is what is stored. SQL names its columns and knows their fields, so it can
    // put the point back. The plan underneath is the same one either way.
    let (status, body) =
        send(addr, "POST", "/table/p/query", "Project(All(), field=qty, field=price, n=10)");
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, r#"{"rows":[{"record":1,"values":[3,1250]},{"record":2,"values":[7,null]}]}"#);
}

/// A join, answered as arithmetic over what each side already counts.
///
/// **The number is checked against the join written out by hand.** `orders` holds 3 records
/// under `GB`, 1 under `US` and 1 under `FR`; `shops` holds 2 under `GB`, 2 under `US` and none
/// under `DE`. The inner join is `3·2 + 1·2 = 8` rows — not 5, not 4, and not the 10 a
/// cross join would give. A test that only asserted "it answered" would pass on all four.
#[test]
fn a_join_pairs_records_through_the_key_two_tables_share() {
    let addr = spawn(12);
    assert_eq!(send(addr, "POST", "/table/orders", "").0, 200);
    assert_eq!(send(addr, "POST", "/table/orders/field/country?kind=set", "").0, 200);
    assert_eq!(send(addr, "POST", "/table/orders/field/amount?kind=int&bit_depth=32", "").0, 200);
    assert_eq!(send(addr, "POST", "/table/shops", "").0, 200);
    assert_eq!(send(addr, "POST", "/table/shops/field/country?kind=set", "").0, 200);
    assert_eq!(send(addr, "POST", "/table/shops/field/staff?kind=int&bit_depth=16", "").0, 200);

    let (status, _) = send(
        addr,
        "POST",
        "/table/orders/import",
        "country 1 GB\namount 1 100\ncountry 2 GB\namount 2 200\ncountry 3 GB\namount 3 300\n\
         country 4 US\namount 4 400\ncountry 5 FR\namount 5 500\n",
    );
    assert_eq!(status, 200);
    let (status, _) = send(
        addr,
        "POST",
        "/table/shops/import",
        "country 1 GB\nstaff 1 7\ncountry 2 GB\nstaff 2 3\n\
         country 3 US\nstaff 3 5\ncountry 4 US\nstaff 4 9\ncountry 5 DE\nstaff 5 1\n",
    );
    assert_eq!(status, 200);

    // 3·2 + 1·2 = 8. `FR` and `DE` are each held by one side only and pair with nothing.
    let (status, body) = send(
        addr,
        "POST",
        "/sql",
        "SELECT count(*) FROM orders o JOIN shops s ON o.country = s.country",
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, r#"{"columns":["count"],"rows":[[8]]}"#);

    // Two countries are in the join, not the three or four either table holds.
    let (_, body) = send(
        addr,
        "POST",
        "/sql",
        "SELECT count(DISTINCT o.country) FROM orders o JOIN shops s ON o.country = s.country",
    );
    assert_eq!(body, r#"{"columns":["count"],"rows":[[2]]}"#);

    // One row per key, which is where the arithmetic is easiest to check: GB is 3·2 and US 1·2.
    let (_, body) = send(
        addr,
        "POST",
        "/sql",
        "SELECT o.country, count(*) FROM orders o JOIN shops s ON o.country = s.country \
         GROUP BY o.country",
    );
    assert_eq!(body, r#"{"columns":["country","count"],"rows":[["GB",6],["US",2]]}"#);
}

/// A sum over a join is scaled by the other side's count, and an extreme is not.
///
/// **That difference is the test.** `GB` holds orders of 100, 200 and 300 against two shops, so
/// the joined rows carry each amount twice: the sum is `(100+200+300)·2 = 1200`, and the
/// smallest amount is still `100` however many times it appears. A `min` that had been scaled
/// the way the sum is would answer `200`, which is a number nothing in the data equals.
#[test]
fn a_total_over_a_join_repeats_and_an_extreme_does_not() {
    let addr = spawn(9);
    assert_eq!(send(addr, "POST", "/table/orders", "").0, 200);
    assert_eq!(send(addr, "POST", "/table/orders/field/country?kind=set", "").0, 200);
    assert_eq!(send(addr, "POST", "/table/orders/field/amount?kind=int&bit_depth=32", "").0, 200);
    assert_eq!(send(addr, "POST", "/table/shops", "").0, 200);
    assert_eq!(send(addr, "POST", "/table/shops/field/country?kind=set", "").0, 200);
    assert_eq!(send(addr, "POST", "/table/shops/field/staff?kind=int&bit_depth=16", "").0, 200);
    send(
        addr,
        "POST",
        "/table/orders/import",
        "country 1 GB\namount 1 100\ncountry 2 GB\namount 2 200\ncountry 3 GB\namount 3 300\n",
    );
    send(addr, "POST", "/table/shops/import", "country 1 GB\nstaff 1 7\ncountry 2 GB\nstaff 2 3\n");

    let (status, body) = send(
        addr,
        "POST",
        "/sql",
        "SELECT sum(o.amount), min(o.amount), max(o.amount), sum(s.staff) \
         FROM orders o JOIN shops s ON o.country = s.country",
    );
    assert_eq!(status, 200, "{body}");
    // 600·2 = 1200 for the orders' total, 100 and 300 unscaled, and the shops' 10 seen once per
    // order: 10·3 = 30.
    assert_eq!(body, r#"{"columns":["sum","min","max","sum"],"rows":[[1200,100,300,30]]}"#);
}

/// A `WHERE` over a join narrows each table on its own, and a `HAVING` filters the keys.
#[test]
fn a_join_takes_a_where_per_table_and_a_having_over_the_keys() {
    let addr = spawn(13);
    assert_eq!(send(addr, "POST", "/table/orders", "").0, 200);
    assert_eq!(send(addr, "POST", "/table/orders/field/country?kind=set", "").0, 200);
    assert_eq!(send(addr, "POST", "/table/orders/field/amount?kind=int&bit_depth=32", "").0, 200);
    assert_eq!(send(addr, "POST", "/table/shops", "").0, 200);
    assert_eq!(send(addr, "POST", "/table/shops/field/country?kind=set", "").0, 200);
    assert_eq!(send(addr, "POST", "/table/shops/field/staff?kind=int&bit_depth=16", "").0, 200);
    send(
        addr,
        "POST",
        "/table/orders/import",
        "country 1 GB\namount 1 100\ncountry 2 GB\namount 2 900\ncountry 3 US\namount 3 900\n",
    );
    send(addr, "POST", "/table/shops/import", "country 1 GB\nstaff 1 7\ncountry 2 US\nstaff 2 3\n");

    // Without a filter: GB is 2·1 and US 1·1, so three rows.
    let (_, body) = send(
        addr,
        "POST",
        "/sql",
        "SELECT count(*) FROM orders o JOIN shops s ON o.country = s.country",
    );
    assert_eq!(body, r#"{"columns":["count"],"rows":[[3]]}"#);

    // The filter names one table and narrows only it: GB keeps one order, US keeps one.
    let (status, body) = send(
        addr,
        "POST",
        "/sql",
        "SELECT count(*) FROM orders o JOIN shops s ON o.country = s.country \
         WHERE o.amount >= 900",
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, r#"{"columns":["count"],"rows":[[2]]}"#);

    // A filter on the other side removes a key from the join entirely.
    let (_, body) = send(
        addr,
        "POST",
        "/sql",
        "SELECT count(*) FROM orders o JOIN shops s ON o.country = s.country WHERE s.staff > 5",
    );
    assert_eq!(body, r#"{"columns":["count"],"rows":[[2]]}"#);

    // `HAVING` over the keys, and a ranking of them.
    let (_, body) = send(
        addr,
        "POST",
        "/sql",
        "SELECT o.country, count(*) FROM orders o JOIN shops s ON o.country = s.country \
         GROUP BY o.country HAVING count(*) > 1",
    );
    assert_eq!(body, r#"{"columns":["country","count"],"rows":[["GB",2]]}"#);

    let (_, body) = send(
        addr,
        "POST",
        "/sql",
        "SELECT o.country, count(*) FROM orders o JOIN shops s ON o.country = s.country \
         GROUP BY o.country ORDER BY count(*) DESC LIMIT 1",
    );
    assert_eq!(body, r#"{"columns":["country","count"],"rows":[["GB",2]]}"#);
}

/// A bitmap-only table has no read from a record back to its key, so a keyed column is refused
/// by name - by the planner, which is the layer that knows both what kind of field it is and
/// whether the table stores its values.
///
/// This is the one thing the engine changes about *what can be asked* rather than about what it
/// costs, so it gets a test that names both halves.
#[test]
fn a_keyed_column_is_refused_on_a_table_that_stores_no_values() {
    let addr = spawn(5);
    assert_eq!(send(addr, "POST", "/table/idx?engine=bitmap", "").0, 200);
    assert_eq!(send(addr, "POST", "/table/idx/field/country?kind=set", "").0, 200);
    assert_eq!(send(addr, "POST", "/table/idx/import", "country 1 GB\n").0, 200);

    let (status, body) = send(addr, "POST", "/sql", "SELECT country FROM idx LIMIT 10");
    assert_eq!(status, 400, "{body}");
    assert!(body.contains(r#""code":"operator_not_allowed""#), "{body}");
    assert!(body.contains("a projection"), "{body}");
}

/// `CREATE TABLE` over `/sql`, with and without an engine.
#[test]
fn a_table_can_be_created_from_sql() {
    let addr = spawn(6);

    let (status, body) = send(addr, "POST", "/sql", "CREATE TABLE plain");
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, r#"{"columns":["table"],"rows":[[0]]}"#);

    // Bare for the names that lex as one word, quoted for the one with a `+` in it.
    assert_eq!(send(addr, "POST", "/sql", "CREATE TABLE cols ENGINE = columnar").0, 200);
    assert_eq!(send(addr, "POST", "/sql", "CREATE TABLE both ENGINE = 'bitmap+columnar'").0, 200);

    let (_, body) = send(addr, "GET", "/schema", "");
    // No engine named means the default, not the narrow one.
    assert!(body.contains(r#""name":"plain","engine":"bitmap+columnar""#), "{body}");
    assert!(body.contains(r#""name":"cols","engine":"columnar""#), "{body}");
    assert!(body.contains(r#""name":"both","engine":"bitmap+columnar""#), "{body}");
}

/// An engine name nobody has is refused where the engine list actually lives, which is not the
/// SQL crate: that one takes the name as written and never learns which are real.
#[test]
fn an_unknown_engine_in_sql_is_refused() {
    let addr = spawn(1);
    let (status, body) = send(addr, "POST", "/sql", "CREATE TABLE t ENGINE = mergetree");
    assert_eq!(status, 400, "{body}");
    assert!(body.contains(r#""code":"unknown_engine_name""#), "{body}");
}

/// A column list creates the fields it declares, as the field changes it already is.
///
/// End to end on purpose: what this claims is not that the parser read `TEXT`, which
/// `big-sql`'s own suite says, but that a field created this way is indistinguishable from one
/// created over `POST /table/{t}/field/{f}` - same kind, same depth, same scale in `/schema`.
#[test]
fn a_column_list_creates_the_fields_it_declares() {
    let addr = spawn(4);

    let (status, body) = send(
        addr,
        "POST",
        "/sql",
        "CREATE TABLE events (
           country TEXT,
           device  MUTEX,
           active  BOOL,
           amount  INT,
           small   SMALLINT,
           delta   BIGINT SIGNED,
           price   DECIMAL(10, 2),
           visit   TIMEQUANTUM
         ) ENGINE = 'bitmap+columnar'",
    );
    assert_eq!(status, 200, "{body}");
    // Still the table's id in one cell, because a column list does not change what a `CREATE`
    // answers with - only how much of the schema one statement says.
    assert_eq!(body, r#"{"columns":["table"],"rows":[[0]]}"#);

    let (_, body) = send(addr, "GET", "/schema", "");
    assert!(body.contains(r#""name":"events","engine":"bitmap+columnar""#), "{body}");
    for field in [
        r#""name":"country","kind":"set""#,
        r#""name":"device","kind":"mutex""#,
        r#""name":"active","kind":"bool""#,
        r#""name":"amount","kind":"int","bit_depth":32"#,
        r#""name":"small","kind":"int","bit_depth":16"#,
        r#""name":"delta","kind":"signedint","bit_depth":64"#,
        // Ten digits need 34 bits, which is what the precision bought.
        r#""name":"price","kind":"decimal","bit_depth":34,"scale":2"#,
        r#""name":"visit","kind":"timequantum""#,
    ] {
        assert!(body.contains(field), "missing {field} in {body}");
    }

    // And the fields work: a fact written against them lands and counts.
    assert_eq!(send(addr, "POST", "/table/events/import", "country 1 vn\namount 1 4200\n").0, 200);
    let (_, body) = send(addr, "POST", "/sql", "SELECT count(*) FROM events WHERE country = 'vn'");
    assert_eq!(body, r#"{"columns":["count"],"rows":[[1]]}"#);
}

/// The column list is optional, and the fields it declares are still the field routes' to add.
#[test]
fn a_column_list_is_one_way_to_say_a_schema_and_not_the_only_one() {
    let addr = spawn(4);
    assert_eq!(send(addr, "POST", "/sql", "CREATE TABLE t (a SET)").0, 200);
    // A field added afterwards over the route it has always had.
    assert_eq!(send(addr, "POST", "/table/t/field/b?kind=int&bit_depth=8", "").0, 200);

    let (_, body) = send(addr, "GET", "/schema", "");
    assert!(body.contains(r#""name":"a","kind":"set""#), "{body}");
    assert!(body.contains(r#""name":"b","kind":"int","bit_depth":8"#), "{body}");
}

/// What a column list will not take, refused before anything is created.
///
/// The second half is the claim worth having: a statement refused for its column list leaves
/// no table behind, because every kind, depth and scale is decided while the statement is
/// parsed and the first change goes out only after all of them are.
#[test]
fn a_column_list_is_judged_before_the_table_is_created() {
    let addr = spawn(7);

    for (sql, code) in [
        ("CREATE TABLE t (a FLOAT)", "sql_unknown_column_type"),
        ("CREATE TABLE t (a INT NOT NULL)", "sql_no_constraints"),
        ("CREATE TABLE t (a DECIMAL)", "sql_decimal_scale"),
        ("CREATE TABLE t (a UINT(65))", "sql_bit_depth"),
    ] {
        let (status, body) = send(addr, "POST", "/sql", sql);
        assert_eq!(status, 400, "{body}");
        assert!(body.contains(&format!(r#""code":"{code}""#)), "{sql}: {body}");
    }

    // Nothing was created by any of them.
    let (_, body) = send(addr, "GET", "/schema", "");
    assert!(!body.contains(r#""name":"t""#), "{body}");

    // A table nobody has is a 404, judged here before any node is told to drop anything -
    // and `IF EXISTS` makes it a request that was already satisfied.
    let (status, body) = send(addr, "POST", "/sql", "DROP TABLE t");
    assert_eq!(status, 404, "{body}");
    assert!(body.contains(r#""code":"unknown_table""#), "{body}");
    let (status, body) = send(addr, "POST", "/sql", "DROP TABLE IF EXISTS t");
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, r#"{"columns":["dropped"],"rows":[[0]]}"#);
}

/// `ALTER TABLE` adds and drops fields, which is the whole of what the engine can do to one.
///
/// End to end, because the claim is not that the parser read `ADD COLUMN` - `big-sql`'s suite
/// says that - but that the field it creates is the field the route creates, and that a fact
/// written against it lands.
#[test]
fn alter_table_adds_and_drops_the_fields_it_names() {
    let addr = spawn(7);
    let (s0, b0) = send(addr, "POST", "/sql", "CREATE TABLE events (country TEXT, legacy INT)");
    assert_eq!(s0, 200, "{b0}");

    let (status, body) = send(
        addr,
        "POST",
        "/sql",
        "ALTER TABLE events
           ADD COLUMN price DECIMAL(10, 2),
           ADD COLUMN visit TIMEQUANTUM,
           DROP COLUMN legacy",
    );
    assert_eq!(status, 200, "{body}");
    // How many fields changed, not an id: three changes have no single id to answer with.
    assert_eq!(body, r#"{"columns":["fields"],"rows":[[3]]}"#);

    let (_, body) = send(addr, "GET", "/schema", "");
    assert!(body.contains(r#""name":"price","kind":"decimal","bit_depth":34,"scale":2"#), "{body}");
    assert!(body.contains(r#""name":"visit","kind":"timequantum""#), "{body}");
    assert!(!body.contains(r#""name":"legacy""#), "{body}");

    // The added field works like any other: a decimal fact is its units, and the planner
    // resolves the literal in the query against the scale the column declared.
    assert_eq!(send(addr, "POST", "/table/events/import", "price 1 1250\n").0, 200);
    let (_, body) = send(addr, "POST", "/sql", "SELECT count(*) FROM events WHERE price > 5");
    assert_eq!(body, r#"{"columns":["count"],"rows":[[1]]}"#);
}

/// Every clause is judged before the first one is applied.
#[test]
fn an_alter_that_names_a_field_wrongly_changes_nothing() {
    let addr = spawn(7);
    assert_eq!(send(addr, "POST", "/sql", "CREATE TABLE t (a SET)").0, 200);

    // The second clause is the bad one, and the first must not have happened. `422` because
    // the name that is wrong is in the body, which is where `status::db` puts a field.
    let (status, body) =
        send(addr, "POST", "/sql", "ALTER TABLE t ADD COLUMN b INT, DROP COLUMN nope");
    assert_eq!(status, 422, "{body}");
    assert!(body.contains(r#""code":"unknown_field""#), "{body}");

    // Adding a field that is already there, and altering a table that is not.
    let (status, body) = send(addr, "POST", "/sql", "ALTER TABLE t ADD COLUMN a SET");
    assert_eq!(status, 409, "{body}");
    assert!(body.contains(r#""code":"field_redefined""#), "{body}");
    let (status, body) = send(addr, "POST", "/sql", "ALTER TABLE nope ADD COLUMN a SET");
    assert_eq!(status, 404, "{body}");
    assert!(body.contains(r#""code":"unknown_table""#), "{body}");

    let (_, body) = send(addr, "GET", "/schema", "");
    assert!(!body.contains(r#""name":"b""#), "{body}");

    // What the engine cannot do to a field, refused with what to do instead.
    let (status, body) = send(addr, "POST", "/sql", "ALTER TABLE t MODIFY a BIGINT");
    assert_eq!(status, 400, "{body}");
    assert!(body.contains(r#""code":"sql_no_alter_column""#), "{body}");
    assert!(body.contains("copy into it"), "{body}");
}

/// `INSERT`, end to end: the facts it writes are the facts a query then counts.
#[test]
fn rows_written_by_a_statement_are_read_back_by_one() {
    let addr = spawn(8);
    assert_eq!(send(addr, "POST", "/sql", "CREATE TABLE tx (amount INT, country TEXT)").0, 200);

    // The answer is the rows the client wrote, not the facts they came to - that number is the
    // statement's cost rather than its meaning.
    let (status, body) = send(
        addr,
        "POST",
        "/sql",
        "INSERT INTO tx (_record_id, amount, country) VALUES (1, 100, 'GB'), (2, 900, 'US')",
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, r#"{"columns":["inserted"],"rows":[[2]]}"#);

    let (_, body) = send(addr, "POST", "/sql", "SELECT count(*) FROM tx WHERE country = 'GB'");
    assert_eq!(body, r#"{"columns":["count"],"rows":[[1]]}"#);
    let (_, body) = send(addr, "POST", "/sql", "SELECT sum(amount) FROM tx");
    assert_eq!(body, r#"{"columns":["sum"],"rows":[[1000]]}"#);
    // The ids the statement named are the records that exist, because the id *is* the record.
    let (_, body) = send(addr, "POST", "/sql", "SELECT * FROM tx");
    assert_eq!(body, r#"{"columns":["_record_id"],"rows":[[1],[2]]}"#);
    // Writing the same id again writes about the same record, exactly as two import lines do.
    assert_eq!(
        send(addr, "POST", "/sql", "INSERT INTO tx (_record_id, amount) VALUES (1, 700)").0,
        200
    );
    let (_, body) = send(addr, "POST", "/sql", "SELECT count(*) FROM tx");
    assert_eq!(body, r#"{"columns":["count"],"rows":[[2]]}"#);
}

/// **The test that makes one shared `big_api::fact` a checkable claim rather than a refactor.**
///
/// The import route and an `INSERT` write into the same fields, and a field's kind is what
/// decides how a value is read. If the two disagreed - about `true`, about `key@seconds`, about
/// how many units `12.50` is - one table would hold two conventions and no query could tell
/// which line wrote which. So the same record is written both ways and asked about once.
#[test]
fn a_value_written_as_a_statement_reads_the_way_an_imported_one_does() {
    let addr = spawn(12);
    assert_eq!(
        send(
            addr,
            "POST",
            "/sql",
            "CREATE TABLE t (n INT, balance SIGNED, price DECIMAL(10, 2),
                             country TEXT, active BOOL, visit TIMEQUANTUM)"
        )
        .0,
        200
    );

    // Record 1 the old way. A decimal takes the units it stores, which is what a line of an
    // import has always meant.
    let (status, body) = send(
        addr,
        "POST",
        "/table/t/import",
        "n 1 100\nbalance 1 -5\nprice 1 1250\ncountry 1 GB\nactive 1 true\nvisit 1 home@1750000000\n",
    );
    assert_eq!(status, 200, "{body}");

    // Record 2 the new way. A literal carries its own scale, so `12.50` is written as the same
    // 1250 units - which is also what `WHERE price = 12.50` compares against.
    let (status, body) = send(
        addr,
        "POST",
        "/sql",
        "INSERT INTO t (_record_id, n, balance, price, country, active, visit)
         VALUES (2, 100, -5, 12.50, 'GB', true, 'home@1750000000')",
    );
    assert_eq!(status, 200, "{body}");

    // Every field answers for both records, which is the claim.
    for (sql, expected) in [
        ("SELECT count(*) FROM t WHERE n = 100", 2),
        ("SELECT count(*) FROM t WHERE balance = -5", 2),
        ("SELECT count(*) FROM t WHERE price = 12.50", 2),
        ("SELECT count(*) FROM t WHERE country = 'GB'", 2),
        ("SELECT count(*) FROM t WHERE active = true", 2),
        ("SELECT count(*) FROM t WHERE visit = 'home'", 2),
        // The moment landed too, in the views a window reads - which is the half of a time
        // quantum fact that is easy to write and lose.
        ("SELECT count(*) FROM t WHERE visit = 'home' AND visit BETWEEN 1749000000 AND 1751000000", 2),
    ] {
        let (status, body) = send(addr, "POST", "/sql", sql);
        assert_eq!(status, 200, "{sql}: {body}");
        assert_eq!(body, format!(r#"{{"columns":["count"],"rows":[[{expected}]]}}"#), "{sql}");
    }
}

/// A statement that names anything wrongly writes none of it.
#[test]
fn an_insert_that_names_a_column_wrongly_writes_nothing() {
    let addr = spawn(8);
    assert_eq!(send(addr, "POST", "/sql", "CREATE TABLE t (n INT)").0, 200);

    // A field nobody has: `422`, because the name that is wrong is in the body.
    let (status, body) =
        send(addr, "POST", "/sql", "INSERT INTO t (_record_id, nope) VALUES (1, 5)");
    assert_eq!(status, 422, "{body}");
    assert!(body.contains(r#""code":"unknown_field""#), "{body}");
    // A table nobody has.
    let (status, body) = send(addr, "POST", "/sql", "INSERT INTO nope (_record_id) VALUES (1)");
    assert_eq!(status, 404, "{body}");
    assert!(body.contains(r#""code":"unknown_table""#), "{body}");
    // A value the field cannot hold, reported the way the import route reports the same
    // mistake - the two read it with one function, so they say one thing about it.
    let (status, body) =
        send(addr, "POST", "/sql", "INSERT INTO t (_record_id, n) VALUES (1, 'five')");
    assert_eq!(status, 400, "{body}");
    assert!(body.contains(r#""code":"malformed_line""#), "{body}");
    assert!(body.contains("needs a number"), "{body}");
    // The second row is the bad one, and the first must not have landed: the whole batch is
    // resolved before any of it is written.
    let (status, _) =
        send(addr, "POST", "/sql", "INSERT INTO t (_record_id, n) VALUES (1, 5), (2, 'x')");
    assert_eq!(status, 400);

    let (_, body) = send(addr, "POST", "/sql", "SELECT count(*) FROM t");
    assert_eq!(body, r#"{"columns":["count"],"rows":[[0]]}"#);
}

/// `IF NOT EXISTS` is about the fields as much as the table, which is why it is a flag on the
/// statement rather than a shrug at whatever error came back.
#[test]
fn if_not_exists_leaves_a_table_and_its_fields_alone() {
    let addr = spawn(8);
    assert_eq!(send(addr, "POST", "/sql", "CREATE TABLE t (a SET)").0, 200);

    // A second run of a setup script that has since grown a column. **The table is left exactly
    // as it is**: `b` is not created, and the answer is that nothing was.
    let (status, body) = send(addr, "POST", "/sql", "CREATE TABLE IF NOT EXISTS t (a SET, b INT)");
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, r#"{"columns":["table"],"rows":[[0]]}"#);
    let (_, body) = send(addr, "GET", "/schema", "");
    assert!(!body.contains(r#""name":"b""#), "{body}");

    // Without the clause the same statement *adds* `b`, because a declaration identical to what
    // is already there is idempotent below and a new field is simply a new field. Which is the
    // difference the flag names: "make sure this exists" and "leave it alone if it does" are two
    // requests, and only one of them is safe to run against a table somebody has since altered.
    let (status, body) = send(addr, "POST", "/sql", "CREATE TABLE t (a SET, b INT)");
    assert_eq!(status, 200, "{body}");
    let (_, body) = send(addr, "GET", "/schema", "");
    assert!(body.contains(r#""name":"b""#), "{body}");

    // A declaration that contradicts what is there is refused either way: `IF NOT EXISTS` says
    // what to do about a table that exists, not what a field means.
    let (status, body) = send(addr, "POST", "/sql", "CREATE TABLE t (a INT)");
    assert_eq!(status, 409, "{body}");
    assert!(body.contains(r#""code":"field_redefined""#), "{body}");
}

/// `DROP TABLE`, which reaches exactly what `DELETE /table/{t}` reaches.
#[test]
fn a_table_dropped_by_statement_is_gone_and_dropping_it_again_is_idempotent() {
    let addr = spawn(7);
    assert_eq!(send(addr, "POST", "/sql", "CREATE TABLE t (a SET)").0, 200);
    assert_eq!(send(addr, "POST", "/sql", "INSERT INTO t (_record_id, a) VALUES (1, 'x')").0, 200);

    let (status, body) = send(addr, "POST", "/sql", "DROP TABLE t");
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, r#"{"columns":["dropped"],"rows":[[1]]}"#);
    let (_, body) = send(addr, "GET", "/schema", "");
    assert!(!body.contains(r#""name":"t""#), "{body}");

    // Twice is an error; twice with `IF EXISTS` is a request that was already satisfied.
    let (status, body) = send(addr, "POST", "/sql", "DROP TABLE t");
    assert_eq!(status, 404, "{body}");
    let (status, body) = send(addr, "POST", "/sql", "DROP TABLE IF EXISTS t");
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, r#"{"columns":["dropped"],"rows":[[0]]}"#);
}

/// `DESCRIBE` and `SHOW`, which read the catalog every node holds.
#[test]
fn the_catalog_answers_in_sql_what_the_schema_route_answers_in_json() {
    let addr = spawn(6);
    assert_eq!(
        send(
            addr,
            "POST",
            "/sql",
            "CREATE TABLE t (n INT, price DECIMAL(10, 2), visit TIMEQUANTUM)"
        )
        .0,
        200
    );

    // One row per field, and the two numbers that are absent rather than zero: a scale on a
    // field that stores no decimal, a granularity on a field with no views by time.
    let (status, body) = send(addr, "POST", "/sql", "DESCRIBE t");
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body,
        r#"{"columns":["name","kind","bit_depth","scale","granularity"],"rows":[["n","int",32,null,null],["price","decimal",34,2,null],["visit","timequantum",0,null,null]]}"#
    );

    let (_, body) = send(addr, "POST", "/sql", "SHOW TABLES");
    assert_eq!(
        body,
        r#"{"columns":["name","engine","fields"],"rows":[["t","bitmap+columnar",3]]}"#
    );

    // `FORMAT` is the same clause a `SELECT` takes, and means the same thing.
    let (_, body) = send(addr, "POST", "/sql", "SHOW TABLES FORMAT TSV");
    assert_eq!(body, "t\tbitmap+columnar\t3\n");

    let (status, body) = send(addr, "POST", "/sql", "DESCRIBE nope");
    assert_eq!(status, 404, "{body}");
    assert!(body.contains(r#""code":"unknown_table""#), "{body}");
}

/// **`SHOW CREATE TABLE` answers with a statement that creates the same table.**
///
/// Not "a statement that looks right": it is posted back to a fresh server and the two schemas
/// are compared. That is what keeps the renderer and the parser's type table inverses of each
/// other across a layer boundary, where a unit test cannot see both ends.
#[test]
fn a_shown_create_statement_recreates_the_table_it_describes() {
    let source = spawn(3);
    let declared = "CREATE TABLE t (a SET, b MUTEX, c BOOL, d TIMEQUANTUM,
                                    n UINT(12), s SIGNED(20), price DECIMAL(10, 2))";
    assert_eq!(send(source, "POST", "/sql", declared).0, 200);

    let (status, body) = send(source, "POST", "/sql", "SHOW CREATE TABLE t");
    assert_eq!(status, 200, "{body}");
    let statement = body
        .split_once(r#""rows":[[""#)
        .and_then(|(_, rest)| rest.rsplit_once(r#""]]}"#))
        .map(|(s, _)| s.replace("\\n", "\n"))
        .unwrap_or_else(|| panic!("no statement in {body}"));

    let fresh = spawn(2);

    let (status, body) = send(fresh, "POST", "/sql", &statement);
    assert_eq!(status, 200, "{statement} -> {body}");

    let (_, from_source) = send(source, "GET", "/schema", "");
    let (_, from_fresh) = send(fresh, "GET", "/schema", "");
    assert_eq!(from_fresh, from_source, "recreated from `{statement}`");
}

/// A statement that names no `id` is given one, and the ids keep going up across statements.
///
/// End to end because the allocation is not the parser's: it is one past the highest id
/// anywhere, asked of the schema leader before a fact is sent. What that means on one node is
/// what this checks; what it means on several is that the same leader answers.
#[test]
fn a_record_id_is_allocated_when_the_statement_does_not_name_one() {
    let addr = spawn(9);
    assert_eq!(send(addr, "POST", "/sql", "CREATE TABLE t (n INT, country TEXT)").0, 200);

    // An empty table starts at zero, and a run is contiguous and in the order written.
    let (status, body) =
        send(addr, "POST", "/sql", "INSERT INTO t (n, country) VALUES (10, 'GB'), (20, 'US')");
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, r#"{"columns":["inserted"],"rows":[[2]]}"#);
    let (_, body) = send(addr, "POST", "/sql", "SELECT * FROM t");
    assert_eq!(body, r#"{"columns":["_record_id"],"rows":[[0],[1]]}"#);

    // The next statement carries on above them rather than starting again.
    assert_eq!(send(addr, "POST", "/sql", "INSERT INTO t (n) VALUES (30)").0, 200);
    let (_, body) = send(addr, "POST", "/sql", "SELECT * FROM t");
    assert_eq!(body, r#"{"columns":["_record_id"],"rows":[[0],[1],[2]]}"#);

    // **Above an id written by hand, too.** Allocation is one past the highest that exists, not
    // a counter of its own - so a statement that names an id cannot be overwritten by one that
    // does not, whichever order they arrive in.
    assert_eq!(send(addr, "POST", "/sql", "INSERT INTO t (_record_id, n) VALUES (100, 40)").0, 200);
    assert_eq!(send(addr, "POST", "/sql", "INSERT INTO t (n) VALUES (50)").0, 200);
    let (_, body) = send(addr, "POST", "/sql", "SELECT * FROM t");
    assert_eq!(body, r#"{"columns":["_record_id"],"rows":[[0],[1],[2],[100],[101]]}"#);

    // Each row is its own record: the values went where the ids say they did.
    let (_, body) = send(addr, "POST", "/sql", "SELECT count(*) FROM t WHERE country = 'GB'");
    assert_eq!(body, r#"{"columns":["count"],"rows":[[1]]}"#);
}
