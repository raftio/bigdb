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

//! The ClickHouse spellings, against the standard ones they mean.
//!
//! **Every test here asks the same question twice** - once the way ClickHouse writes it, once
//! the way this surface already did - and asserts the two answers are identical. A test that
//! only checked the ClickHouse form returned *something* would pass on a translation that had
//! quietly picked a different question.
//!
//! What is deliberately not identical is `uniq` and `topK`: ClickHouse's are approximate and
//! these are exact, because the plans behind them are a grouping and a ranking rather than a
//! sketch. That is a different answer, and it is asserted as one.

mod common;
use common::{send, spawn};

use std::net::SocketAddr;

/// Five records: amounts 100/900/500/700/200, countries GB/US/GB/FR/US, categories a/b/a/b/a,
/// active except #2.
fn stocked(requests: usize) -> SocketAddr {
    let addr = spawn(requests + 6);
    assert_eq!(send(addr, "POST", "/table/tx", "").0, 200);
    assert_eq!(send(addr, "POST", "/table/tx/field/amount?kind=int&bit_depth=32", "").0, 200);
    assert_eq!(send(addr, "POST", "/table/tx/field/country?kind=set", "").0, 200);
    assert_eq!(send(addr, "POST", "/table/tx/field/category?kind=set", "").0, 200);
    assert_eq!(send(addr, "POST", "/table/tx/field/active?kind=bool", "").0, 200);
    let (status, _) = send(
        addr,
        "POST",
        "/table/tx/import",
        "amount 1 100\ncountry 1 GB\ncategory 1 a\nactive 1 true\n\
         amount 2 900\ncountry 2 US\ncategory 2 b\nactive 2 false\n\
         amount 3 500\ncountry 3 GB\ncategory 3 a\nactive 3 true\n\
         amount 4 700\ncountry 4 FR\ncategory 4 b\nactive 4 true\n\
         amount 5 200\ncountry 5 US\ncategory 5 a\nactive 5 true\n",
    );
    assert_eq!(status, 200);
    addr
}

fn sql(addr: SocketAddr, text: &str) -> String {
    let (status, body) = send(addr, "POST", "/sql", text);
    assert_eq!(status, 200, "`{text}` was refused: {body}");
    body
}

/// `-If` is `FILTER (WHERE ...)` under another name, and the two must not drift.
#[test]
fn the_if_combinator_is_the_filter_clause_it_already_had() {
    let addr = stocked(8);

    let cells = |body: &str| body.split("\"rows\":").nth(1).unwrap().to_string();
    for (clickhouse, standard) in [
        (
            "SELECT countIf(amount >= 500) FROM tx",
            "SELECT count(*) FILTER (WHERE amount >= 500) FROM tx",
        ),
        (
            "SELECT sumIf(amount, country = 'GB') FROM tx",
            "SELECT sum(amount) FILTER (WHERE country = 'GB') FROM tx",
        ),
        (
            "SELECT minIf(amount, active) FROM tx",
            "SELECT min(amount) FILTER (WHERE active = true) FROM tx",
        ),
        (
            "SELECT maxIf(amount, active = false) FROM tx",
            "SELECT max(amount) FILTER (WHERE active = false) FROM tx",
        ),
    ] {
        assert_eq!(cells(&sql(addr, clickhouse)), cells(&sql(addr, standard)), "{clickhouse}");
    }
}

/// A column standing on its own is `= TRUE`, which is how `countIf(active)` reads.
#[test]
fn a_bare_boolean_column_is_a_predicate() {
    let addr = stocked(3);
    // Four of the five records are active.
    assert_eq!(
        sql(addr, "SELECT count(*) FROM tx WHERE active"),
        r#"{"columns":["count"],"rows":[[4]]}"#
    );
    assert_eq!(
        sql(addr, "SELECT count(*) FROM tx WHERE active AND amount >= 500"),
        r#"{"columns":["count"],"rows":[[2]]}"#
    );
    assert_eq!(
        sql(addr, "SELECT countIf(active) FROM tx"),
        r#"{"columns":["count"],"rows":[[4]]}"#
    );
}

/// `uniq` and its five approximate cousins all answer the exact count, and say so by agreeing
/// with `count(DISTINCT x)` to the digit.
#[test]
fn every_spelling_of_uniq_is_the_exact_distinct_count() {
    let addr = stocked(8);
    let want = r#"{"columns":["count"],"rows":[[3]]}"#;
    for spelling in [
        "count(DISTINCT country)",
        "uniq(country)",
        "uniqExact(country)",
        "uniqCombined(country)",
        "uniqCombined64(country)",
        "uniqHLL12(country)",
        "uniqTheta(country)",
    ] {
        assert_eq!(sql(addr, &format!("SELECT {spelling} FROM tx")), want, "{spelling}");
    }
    // And it narrows with the statement, which a sketch of the whole column would not.
    assert_eq!(
        sql(addr, "SELECT uniq(country) FROM tx WHERE amount >= 500"),
        r#"{"columns":["count"],"rows":[[3]]}"#
    );
}

/// `topK` is the ranking a `TopN` already answered, rendered as a list in one cell.
#[test]
fn top_k_is_the_ranking_rendered_as_a_list() {
    let addr = stocked(4);
    // GB and US hold two records each, FR one; ties break on the key, so GB comes first.
    assert_eq!(
        sql(addr, "SELECT topK(2)(country) FROM tx"),
        r#"{"columns":["topK"],"rows":[[["GB","US"]]]}"#
    );
    // The default takes every key there is, which here is fewer than ten.
    assert_eq!(
        sql(addr, "SELECT topK(country) FROM tx"),
        r#"{"columns":["topK"],"rows":[[["GB","US","FR"]]]}"#
    );
    // It reads the same keys the row form does, in the same order.
    assert_eq!(
        sql(addr, "SELECT country, count(*) AS n FROM tx GROUP BY country ORDER BY n DESC LIMIT 2"),
        r#"{"columns":["country","n"],"rows":[["GB",2],["US",2]]}"#
    );
    // A ranking per group would be a grouping over two columns, which nothing stored.
    let (status, _) =
        send(addr, "POST", "/sql", "SELECT country, topK(2)(country) FROM tx GROUP BY country");
    assert_eq!(status, 400);
}

/// `PREWHERE` selects what `WHERE` selects, because here there is no row to read.
#[test]
fn prewhere_is_the_same_set_and_says_so() {
    let addr = stocked(3);
    // 900, 500 and 700 are at or above the floor.
    let want = r#"{"columns":["count"],"rows":[[3]]}"#;
    assert_eq!(sql(addr, "SELECT count(*) FROM tx PREWHERE amount >= 500"), want);
    assert_eq!(sql(addr, "SELECT count(*) FROM tx WHERE amount >= 500"), want);
    // Both at once are combined, not one ignored.
    assert_eq!(
        sql(addr, "SELECT count(*) FROM tx PREWHERE amount >= 500 WHERE country = 'GB'"),
        r#"{"columns":["count"],"rows":[[1]]}"#
    );
}

/// `LIMIT n WITH TIES` keeps the rows the ordering cannot tell apart from the last one.
#[test]
fn with_ties_keeps_what_the_ordering_cannot_separate() {
    let addr = stocked(4);
    // GB and US both hold two records. `LIMIT 1` cuts one of them off; `WITH TIES` does not.
    assert_eq!(
        sql(addr, "SELECT country, count(*) AS n FROM tx GROUP BY country ORDER BY n DESC LIMIT 1"),
        r#"{"columns":["country","n"],"rows":[["GB",2]]}"#
    );
    assert_eq!(
        sql(
            addr,
            "SELECT country, count(*) AS n FROM tx GROUP BY country ORDER BY n DESC \
             LIMIT 1 WITH TIES"
        ),
        r#"{"columns":["country","n"],"rows":[["GB",2],["US",2]]}"#
    );
    // Nothing ties with FR, so the third row does not drag a fourth in.
    assert_eq!(
        sql(
            addr,
            "SELECT country, count(*) AS n FROM tx GROUP BY country ORDER BY n ASC \
             LIMIT 1 WITH TIES"
        ),
        r#"{"columns":["country","n"],"rows":[["FR",1]]}"#
    );
    // Without an ordering there is nothing to tie on, and that is refused rather than read as
    // a plain limit.
    let (status, _) =
        send(addr, "POST", "/sql", "SELECT DISTINCT country FROM tx LIMIT 1 WITH TIES");
    assert_eq!(status, 400);
}

/// `WITH <constant> AS <name>` binds a value the statement can use by name.
#[test]
fn a_with_clause_binds_a_constant() {
    let addr = stocked(4);
    assert_eq!(
        sql(addr, "WITH 500 AS floor SELECT count(*) FROM tx WHERE amount >= floor"),
        r#"{"columns":["count"],"rows":[[3]]}"#
    );
    // Several bindings, and one used twice.
    assert_eq!(
        sql(
            addr,
            "WITH 500 AS floor, 'GB' AS home \
             SELECT count(*), countIf(country = home) FROM tx WHERE amount >= floor"
        ),
        r#"{"columns":["count","count"],"rows":[[3,1]]}"#
    );
    // A binding whose body is a select is a real CTE, which is a subquery.
    let (status, body) = send(addr, "POST", "/sql", "WITH x AS (SELECT 1) SELECT count(*) FROM tx");
    assert_eq!(status, 400);
    assert!(body.contains(r#""code":"sql_unsupported""#), "{body}");
}

/// `FORMAT` changes the bytes and the content type, and nothing about the answer.
#[test]
fn a_format_clause_changes_the_bytes_and_not_the_answer() {
    let addr = stocked(6);
    let q = "SELECT country, count(*) AS n FROM tx GROUP BY country";

    assert_eq!(sql(addr, q), r#"{"columns":["country","n"],"rows":[["FR",1],["GB",2],["US",2]]}"#);
    assert_eq!(sql(addr, &format!("{q} FORMAT TSV")), "FR\t1\nGB\t2\nUS\t2\n");
    assert_eq!(sql(addr, &format!("{q} FORMAT TSVWithNames")), "country\tn\nFR\t1\nGB\t2\nUS\t2\n");
    assert_eq!(sql(addr, &format!("{q} FORMAT CSVWithNames")), "country,n\nFR,1\nGB,2\nUS,2\n");

    // A format nobody here writes is refused by name rather than answered as JSON.
    let (status, body) = send(addr, "POST", "/sql", &format!("{q} FORMAT Parquet"));
    assert_eq!(status, 400);
    assert!(body.contains(r#""code":"sql_unknown_format""#), "{body}");
}

/// `UNION ALL` stacks two answers, and stacks them without the engine learning anything.
///
/// Each branch is a whole statement with its own plans; what makes them one answer is that the
/// rows are written one after the other. **The test that earns its place is the third**: two
/// branches over the same table with different filters, whose rows are the two halves of a
/// question neither branch could ask alone.
#[test]
fn union_all_stacks_two_answers() {
    let addr = stocked(6);

    assert_eq!(
        sql(addr, "SELECT count(*) FROM tx UNION ALL SELECT count(*) FROM tx WHERE active"),
        r#"{"columns":["count"],"rows":[[5],[4]]}"#
    );

    // The columns are named by the first branch, whatever the others called theirs.
    assert_eq!(
        sql(addr, "SELECT count(*) AS n FROM tx UNION ALL SELECT sum(amount) AS total FROM tx"),
        r#"{"columns":["n"],"rows":[[5],[2400]]}"#
    );

    // Two groupings, stacked: each branch keeps its own rows and its own order.
    assert_eq!(
        sql(
            addr,
            "SELECT country, count(*) FROM tx WHERE amount >= 500 GROUP BY country \
             UNION ALL \
             SELECT country, count(*) FROM tx WHERE amount < 500 GROUP BY country"
        ),
        r#"{"columns":["country","count"],"rows":[["FR",1],["GB",1],["US",1],["GB",1],["US",1]]}"#
    );

    // Three branches, and a `FORMAT` after the last one covers all of them.
    assert_eq!(
        sql(
            addr,
            "SELECT count(*) FROM tx WHERE country = 'GB' \
             UNION ALL SELECT count(*) FROM tx WHERE country = 'US' \
             UNION ALL SELECT count(*) FROM tx WHERE country = 'FR' FORMAT TSV"
        ),
        "2\n2\n1\n"
    );

    // A branch of a different width has no answer between it and the first.
    let (status, body) = send(
        addr,
        "POST",
        "/sql",
        "SELECT count(*) FROM tx UNION ALL SELECT country, count(*) FROM tx GROUP BY country",
    );
    assert_eq!(status, 400);
    assert!(body.contains(r#""code":"sql_union""#), "{body}");
}

/// A window over a time quantum field, end to end.
///
/// **The number that matters is the one the window excludes.** All four visits are `home`, so a
/// query that ignored the bounds would answer 4 and look entirely reasonable. Two of them fall
/// inside the day asked for.
#[test]
fn a_time_window_reads_the_days_it_asked_for() {
    let addr = spawn(9);
    assert_eq!(send(addr, "POST", "/table/v", "").0, 200);
    assert_eq!(send(addr, "POST", "/table/v/field/visit?kind=timequantum", "").0, 200);

    // 2023-11-14 is 1699920000; a day is 86400.
    const DAY: i64 = 86_400;
    const BASE: i64 = 1_699_920_000;
    let facts = format!(
        "visit 1 home@{}\nvisit 2 home@{}\nvisit 3 home@{}\nvisit 4 home@{}\n",
        BASE + 3600,
        BASE + 7200,
        BASE + DAY + 3600,
        BASE + 2 * DAY + 3600,
    );
    let (status, body) = send(addr, "POST", "/table/v/import", &facts);
    assert_eq!(status, 200, "{body}");

    // Every visit, ignoring when.
    assert_eq!(
        sql(addr, "SELECT count(*) FROM v WHERE visit = 'home'"),
        r#"{"columns":["count"],"rows":[[4]]}"#
    );

    // The first day only: two of the four.
    assert_eq!(
        sql(
            addr,
            &format!(
                "SELECT count(*) FROM v WHERE visit = 'home' AND visit BETWEEN {BASE} AND {}",
                BASE + DAY - 1
            )
        ),
        r#"{"columns":["count"],"rows":[[2]]}"#
    );

    // From the second day onwards: the other two.
    assert_eq!(
        sql(
            addr,
            &format!("SELECT count(*) FROM v WHERE visit = 'home' AND visit >= {}", BASE + DAY)
        ),
        r#"{"columns":["count"],"rows":[[2]]}"#
    );

    // A window over a column with no views by time is refused rather than answered with the
    // empty set those used to produce. This is the bug the field classes were split to see.
    assert_eq!(send(addr, "POST", "/table/v/field/tag?kind=set", "").0, 200);
    let (status, body) = send(
        addr,
        "POST",
        "/sql",
        "SELECT count(*) FROM v WHERE tag = 'x' AND tag BETWEEN 1 AND 2",
    );
    assert_eq!(status, 400, "{body}");
    assert!(body.contains("a time window"), "{body}");
}

/// `GROUP BY a, b` — one row per combination, and the numbers add up to the ungrouped ones.
///
/// **The check that earns its place is the second**: the pair counts must sum to the whole
/// table. A grouping that dropped a combination, or counted one twice, would still look like a
/// perfectly reasonable table.
#[test]
fn grouping_by_two_columns_covers_every_combination() {
    let addr = stocked(6);

    // FR/b: record 4. GB/a: records 1 and 3. US/a: record 5. US/b: record 2.
    assert_eq!(
        sql(addr, "SELECT country, category, count(*) FROM tx GROUP BY country, category"),
        r#"{"columns":["country","category","count"],"rows":[["FR","b",1],["GB","a",2],["US","a",1],["US","b",1]]}"#
    );

    // 1 + 2 + 1 + 1, which is every record and none of them twice.
    assert_eq!(sql(addr, "SELECT count(*) FROM tx"), r#"{"columns":["count"],"rows":[[5]]}"#);

    // An aggregate per pair, not just a count: 700, 100+500, 200, 900.
    assert_eq!(
        sql(addr, "SELECT country, category, sum(amount) FROM tx GROUP BY country, category"),
        r#"{"columns":["country","category","sum"],"rows":[["FR","b",700],["GB","a",600],["US","a",200],["US","b",900]]}"#
    );

    // `SELECT DISTINCT a, b` is that grouping with the counts not rendered, which is what
    // standard SQL says it is.
    assert_eq!(
        sql(addr, "SELECT DISTINCT country, category FROM tx"),
        r#"{"columns":["country","category"],"rows":[["FR","b"],["GB","a"],["US","a"],["US","b"]]}"#
    );

    // Ordered by the number rather than by the key, and cut.
    assert_eq!(
        sql(
            addr,
            "SELECT country, category, count(*) AS n FROM tx GROUP BY country, category \
             ORDER BY n DESC LIMIT 1"
        ),
        r#"{"columns":["country","category","n"],"rows":[["GB","a",2]]}"#
    );

    // And a `HAVING` over the pairs.
    assert_eq!(
        sql(
            addr,
            "SELECT country, category, count(*) FROM tx GROUP BY country, category \
             HAVING count(*) > 1"
        ),
        r#"{"columns":["country","category","count"],"rows":[["GB","a",2]]}"#
    );
}

/// Quantiles, found by moving a bound until the count lands on the rank.
///
/// **Exact, and checked against the sorted values written out.** The amounts are
/// 100, 200, 500, 700, 900. The median is the third of five, and every level below is a
/// specific one of them - a sketch would be close to these and not equal to them.
#[test]
fn a_quantile_is_the_value_at_the_rank_and_it_is_exact() {
    let addr = stocked(11);

    // ceil(0.5 * 5) = 3, so the median is the third smallest: 500.
    assert_eq!(
        sql(addr, "SELECT median(amount) FROM tx"),
        r#"{"columns":["quantile"],"rows":[[500]]}"#
    );
    assert_eq!(
        sql(addr, "SELECT quantile(0.5)(amount) FROM tx"),
        r#"{"columns":["quantile"],"rows":[[500]]}"#
    );

    // The ends, and two in between: ranks 1, 2, 4 and 5.
    for (level, want) in [("0", 100), ("0.2", 100), ("0.4", 200), ("0.8", 700), ("1", 900)] {
        assert_eq!(
            sql(addr, &format!("SELECT quantile({level})(amount) FROM tx")),
            format!(r#"{{"columns":["quantile"],"rows":[[{want}]]}}"#),
            "quantile({level})"
        );
    }

    // It narrows with the statement: over GB the values are 100 and 500, so the median is the
    // first of the two.
    assert_eq!(
        sql(addr, "SELECT median(amount) FROM tx WHERE country = 'GB'"),
        r#"{"columns":["quantile"],"rows":[[100]]}"#
    );

    // No records is no quantile, which is the `null` a `min` over nothing already answers.
    assert_eq!(
        sql(addr, "SELECT median(amount) FROM tx WHERE amount > 10000"),
        r#"{"columns":["quantile"],"rows":[[null]]}"#
    );

    // Beside the aggregates that are calls rather than searches.
    assert_eq!(
        sql(addr, "SELECT count(*), median(amount), max(amount) FROM tx"),
        r#"{"columns":["count","quantile","max"],"rows":[[5,500,900]]}"#
    );

    // A level finer than this resolves names a place in a distribution nothing here holds.
    let (status, body) = send(addr, "POST", "/sql", "SELECT quantile(0.9999)(amount) FROM tx");
    assert_eq!(status, 400);
    assert!(body.contains(r#""code":"sql_quantile_level""#), "{body}");
}
