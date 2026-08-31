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
    assert_eq!(body, r#"{"columns":["id"],"rows":[[1],[3]]}"#);
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
    assert_eq!(body, r#"{"columns":["id"],"rows":[[1],[2]]}"#);
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
    // key: a comma between tables is a cross join.
    let (status, body) = send(addr, "POST", "/sql", "SELECT count(*) FROM tx, other");
    assert_eq!(status, 400);
    assert!(body.contains(r#""code":"sql_no_joins""#), "{body}");
    assert!(body.contains("cross join"), "{body}");

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
    assert_eq!(body, r#"{"columns":["shop","sum"],"rows":[["a",1250]]}"#);

    // A whole number is the same conversion: `10` on a two-place field is `1000` units, which
    // `b`'s total of `300` is still under.
    let (_, body) = send(
        addr,
        "POST",
        "/sql",
        "SELECT shop, sum(price) FROM s GROUP BY shop HAVING sum(price) >= 10",
    );
    assert_eq!(body, r#"{"columns":["shop","sum"],"rows":[["a",1250]]}"#);

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
    assert_eq!(body, r#"{"columns":["shop","sum"],"rows":[["a",1250]]}"#);
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

    // Column order follows the select list, and a decimal comes back in the units it is stored
    // in - exactly as a `sum` over it does.
    let (status, body) = send(addr, "POST", "/sql", "SELECT qty, price FROM p LIMIT 10");
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, r#"{"columns":["qty","price"],"rows":[[3,1250],[7,null]]}"#);

    let (_, body) = send(addr, "POST", "/sql", "SELECT price, qty FROM p LIMIT 10");
    assert_eq!(body, r#"{"columns":["price","qty"],"rows":[[1250,3],[null,7]]}"#);

    // The same projection in the query language, which is where SQL's translation has to land
    // for it to be a translation at all.
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

/// A column list is refused by name, with the sentence the writer needs: fields are declared
/// separately here because a field kind may be a set, a mutex or a time quantum, none of which
/// a SQL type names.
#[test]
fn a_column_list_on_create_table_is_refused_by_name() {
    let addr = spawn(2);
    let (status, body) = send(addr, "POST", "/sql", "CREATE TABLE t (amount int)");
    assert_eq!(status, 400, "{body}");
    assert!(body.contains(r#""code":"sql_no_column_list""#), "{body}");

    // Everything else that writes is still refused as it was.
    let (status, body) = send(addr, "POST", "/sql", "DROP TABLE t");
    assert_eq!(status, 400, "{body}");
    assert!(body.contains(r#""code":"sql_read_only""#), "{body}");
}
