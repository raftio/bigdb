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

//! The refusal list, which is the part of this surface that does the work.

use super::common::*;

/// The refusal list, which is the part of this surface that does the work.
///
/// One assertion per variant of [`Refused`], which is where the list lives. A construct that
/// stops being refused should break a test here rather than start being answered.
#[test]
fn every_refusal_names_itself() {
    // A join on a keyed column is answered - any number of tables, so long as they are one star
    // around one key. What is refused is a pairing this engine has no key for, and a join
    // condition that is not one equality.
    assert_eq!(code("SELECT count(*) FROM t, u"), "sql_no_joins");
    // The chain: `u` keyed on `a` by one join and on `b` by the other, which would mean
    // grouping it by both at once.
    assert_eq!(
        code("SELECT count(*) FROM t JOIN u ON t.a = u.a JOIN v ON u.b = v.b"),
        "sql_no_joins"
    );
    assert_eq!(code("SELECT count(*) FROM t LEFT JOIN u ON t.a = u.a"), "sql_no_outer_joins");
    // A cross join and a natural join name no key at all, which is what a comma between tables
    // is - so they get that refusal and not the outer join one, whose sentence is about
    // producing a row for a record with no partner.
    assert_eq!(code("SELECT count(*) FROM t CROSS JOIN u"), "sql_no_joins");
    assert_eq!(code("SELECT count(*) FROM t NATURAL JOIN u"), "sql_no_joins");
    assert_eq!(code("SELECT count(*) FROM t JOIN u USING (a)"), "sql_join_condition");
    assert_eq!(code("SELECT count(*) FROM t JOIN u ON t.a > u.a"), "sql_join_condition");
    assert_eq!(
        code("SELECT count(*) FROM t JOIN u ON t.a = u.a AND t.b = u.b"),
        "sql_join_condition"
    );
    // With two tables in scope, a bare column names neither of them.
    assert_eq!(code("SELECT sum(amount) FROM t JOIN u ON t.a = u.a"), "sql_ambiguous_column");
    assert_eq!(code("SELECT sum(x.amount) FROM t JOIN u ON t.a = u.a"), "sql_ambiguous_column");
    // A `WHERE` term that mixes both tables under an `OR` selects records neither side can be
    // filtered to on its own.
    assert_eq!(
        code("SELECT count(*) FROM t JOIN u ON t.a = u.a WHERE t.amount > 1 OR u.amount > 1"),
        "sql_join_filter"
    );
    // And what a join cannot answer, which is a *row* of one rather than an aggregate over it:
    // a pair of records has no identity this engine stores.
    assert_eq!(code("SELECT * FROM t JOIN u ON t.a = u.a"), "sql_unsupported");
    assert_eq!(code("SELECT count(*) FROM t WHERE amount IN (SELECT x FROM u)"), "sql_unsupported");
    assert_eq!(code("WITH x AS (SELECT 1) SELECT count(*) FROM t"), "sql_unsupported");
    // `UNION ALL` stacks two answers and is answered; a plain `UNION` removes duplicate rows,
    // which would mean comparing two rendered answers.
    assert_eq!(code("SELECT count(*) FROM t UNION SELECT count(*) FROM t"), "sql_union");
    assert_eq!(code("SELECT count(*) FROM t UNION DISTINCT SELECT count(*) FROM t"), "sql_union");
    // Branches that do not line up have no answer between them.
    assert_eq!(
        code("SELECT count(*) FROM t UNION ALL SELECT category, count(*) FROM t GROUP BY category"),
        "sql_union"
    );
    assert_eq!(code("SELECT count(*) FROM t INTERSECT SELECT count(*) FROM t"), "sql_unsupported");
    // `HAVING` on a count over a grouping is answered; the refusal is for the shapes that have
    // no count to filter.
    assert_eq!(
        code("SELECT category, sum(amount) FROM t GROUP BY category HAVING count(*) > 5"),
        "sql_unsupported"
    );
    assert_eq!(code("SELECT count(*) FROM t HAVING count(*) > 5"), "sql_unsupported");
    assert_eq!(code("SELECT row_number() OVER () FROM t"), "sql_unsupported");
    assert_eq!(code("SELECT count(*) FROM t LIMIT 1 OFFSET 5"), "sql_unsupported");
    assert_eq!(code("SELECT amount * 2 FROM t"), "sql_unsupported");
    // One column and two are both answered; three would be a pass over the third per pair of
    // the first two.
    assert_eq!(code("SELECT DISTINCT category, country, active FROM t"), "sql_unsupported");
    assert_eq!(
        code("SELECT category, count(*) FROM t GROUP BY category, country, active"),
        "sql_unsupported"
    );
    assert_eq!(code("SELECT DISTINCT count(*) FROM t"), "sql_unsupported");
    assert_eq!(code("SELECT DISTINCT category FROM t GROUP BY category"), "sql_unsupported");
    assert_eq!(code("SELECT count(*) FROM t WHERE amount IS NULL"), "sql_no_nulls");
    assert_eq!(code("SELECT count(*) FROM t WHERE amount = NULL"), "sql_no_nulls");
    // A projection without the cut that bounds what it costs, and one with a cut past the cap.
    assert_eq!(code("SELECT amount FROM t"), "sql_projection_unsupported");
    assert_eq!(code("SELECT country FROM t WHERE amount > 5"), "sql_projection_unsupported");
    assert_eq!(code("SELECT amount FROM t LIMIT 0"), "sql_projection_unsupported");
    assert_eq!(
        code(&format!("SELECT amount FROM t LIMIT {}", big_sql::MAX_PROJECTION + 1)),
        "sql_projection_unsupported"
    );
    // An `INSERT` is answered now - see `writes` - so what is refused is one that does not
    // name the record it writes about.
    // A column list is still required - without one the values would be positional against a
    // field order the statement does not carry. The `id` column is not: leaving it out asks the
    // server to allocate one.
    assert_eq!(code("INSERT INTO t VALUES (1, 2)"), "sql_insert_shape");
    assert_eq!(code("INSERT INTO t (_record_id, amount) VALUES ('seven', 2)"), "sql_insert_shape");
    assert_eq!(code("INSERT INTO t (amount) SELECT amount FROM u"), "sql_unsupported");
    // `id` is what a record is called, so a *field* of that name is one no `INSERT` could ever
    // fill - refused where it is declared rather than where it silently answers nothing.
    assert_eq!(code("CREATE TABLE t (_record_id UINT(32), a SET)"), "sql_id_column");
    assert_eq!(code("ALTER TABLE t ADD COLUMN _record_id UINT(32)"), "sql_id_column");
    assert_eq!(code("DELETE FROM t"), "sql_read_only");
    assert_eq!(code("DELETE FROM t WHERE amount > 5"), "sql_read_only");
    // A column list is answered now - see `schema` - so what is refused is a type name that
    // names nothing this engine stores, and the SQL that comes attached to one.
    assert_eq!(code("CREATE TABLE t (a FLOAT)"), "sql_unknown_column_type");
    assert_eq!(code("CREATE TABLE t (a INT NOT NULL)"), "sql_no_constraints");
    assert_eq!(code("CREATE TABLE t (a DECIMAL)"), "sql_decimal_scale");
    assert_eq!(code("CREATE TABLE t (a UINT(0))"), "sql_bit_depth");
    assert_eq!(code("CREATE INDEX i ON t (a)"), "sql_read_only");
    // Databases are answered now - see `database.test`. What is still refused is a *session*:
    // one statement is one request, so the database arrives with the request rather than being
    // something `USE` can leave behind for the next one.
    assert_eq!(code("USE d"), "sql_use_unsupported");
    assert_eq!(code("DROP VIEW v"), "sql_no_views");
    assert_eq!(code("CREATE VIEW v AS SELECT count(*) FROM t"), "sql_no_views");
    assert_eq!(code("CREATE MATERIALIZED VIEW v AS SELECT count(*) FROM t"), "sql_no_views");
    // The three shapes of computation this dialect has no evaluator for, each named by what it
    // asked for rather than by the evaluator that is missing.
    assert_eq!(code("SELECT CASE WHEN amount > 5 THEN 1 ELSE 0 END FROM t"), "sql_unsupported");
    assert_eq!(code("SELECT multiIf(amount > 5, 1, 0) FROM t"), "sql_unsupported");
    assert_eq!(code("SELECT cast(amount AS BIGINT) FROM t"), "sql_unsupported");
    assert_eq!(code("SELECT toString(amount) FROM t"), "sql_unsupported");
    assert_eq!(code("SELECT argMax(amount, price) FROM t"), "sql_unsupported");
    assert_eq!(code("SELECT stddevPop(amount) FROM t"), "sql_unsupported");
    assert_eq!(code("SELECT corr(amount, price) FROM t"), "sql_unsupported");
    assert_eq!(code("SELECT count(*) FROM t WHERE country LIKE 'G%'"), "sql_unsupported");
    // Two aggregates are two plans, which is now answered. What is still refused is a select
    // list that is not one answer: a star beside an aggregate, or a bare column beside one.
    assert_eq!(code("SELECT *, count(*) FROM t"), "sql_unsupported");
    assert_eq!(code("SELECT category, count(*) FROM t"), "sql_unsupported");
    // A column that is neither grouped nor aggregated.
    assert_eq!(code("SELECT country, count(*) FROM t GROUP BY category"), "sql_unsupported");
    // Ordering by the aggregate the select list asked for is answered; ordering by a second
    // one names a number the answer does not hold.
    assert_eq!(
        code("SELECT category, sum(amount) FROM t GROUP BY category ORDER BY min(amount) DESC"),
        "sql_unsupported_order"
    );
    assert_eq!(
        code("SELECT category, sum(amount) FROM t GROUP BY category ORDER BY count(*) DESC"),
        "sql_unsupported_order"
    );
    assert_eq!(
        code("SELECT category, count(*) FROM t GROUP BY category ORDER BY country"),
        "sql_unsupported_order"
    );
    assert_eq!(code("SELECT * FROM t ORDER BY amount"), "sql_unsupported_order");
    assert_eq!(code("SELECT count(*) FROM t ORDER BY count(*) DESC"), "sql_unsupported_order");
    // An offset into a record listing, which is paged with a cursor instead.
    assert_eq!(code("SELECT * FROM t LIMIT 10 OFFSET 5"), "sql_unsupported");
    // A count threshold that is not a whole number of records.
    assert_eq!(
        code("SELECT category FROM t GROUP BY category HAVING count(*) > 5.5"),
        "sql_unsupported"
    );
    // A column that is neither of the two grouped ones still has no single value per pair.
    assert_eq!(
        code("SELECT balance, count(*) FROM t GROUP BY category, country"),
        "sql_unsupported"
    );
    // More plans than one statement may fan out. Refused at translation, before any of them is
    // resolved, so the field names here need not exist.
    let many =
        (0..=big_sql::MAX_CALLS).map(|i| format!("sum(f{i})")).collect::<Vec<_>>().join(", ");
    assert_eq!(code(&format!("SELECT {many} FROM t")), "sql_too_many_aggregates");
}

/// A refusal points at the construct, not at wherever the parser happened to give up.
#[test]
fn a_refusal_says_what_it_is_and_what_exists_instead() {
    let e = translate("SELECT count(*) FROM t, u").unwrap_err();
    let SqlError::Refused { what, .. } = e else { panic!("expected a refusal, got {e:?}") };
    assert_eq!(what, Refused::Joins);
    // The sentence has to name the thing that was actually written - a comma - and the two
    // spellings that mean the same, since all three arrive here.
    assert!(what.why().contains("comma between tables"), "{}", what.why());
    assert!(what.why().contains("CROSS JOIN"), "{}", what.why());
    assert!(what.why().contains("NATURAL JOIN"), "{}", what.why());

    // A projection is answered now, so what this refusal names is the missing cut rather than
    // the clause: the sentence has to say both what it costs and what to write instead.
    let e = translate("SELECT amount FROM t").unwrap_err();
    assert_eq!(e.code(), "sql_projection_unsupported");
    assert!(e.to_string().contains("point read"), "{e}");
    assert!(e.to_string().contains("LIMIT"), "{e}");
    // And the same statement with one is not a refusal at all.
    assert!(translate("SELECT amount FROM t LIMIT 100").is_ok());
}

#[test]
fn text_that_is_not_a_statement_is_a_parse_error_not_a_refusal() {
    assert_eq!(code("hello"), "parse_error");
    assert_eq!(code("SELECT count(*) FROM"), "parse_error");
    assert_eq!(code("SELECT count(*) FROM t WHERE amount >"), "parse_error");
    assert_eq!(code("SELECT count(*) FROM t WHERE country = 'unterminated"), "parse_error");
    // `FROM t trailing` is a table with an alias, which is what SQL says it is. Trailing
    // text is text after a clause that has already ended.
    assert_eq!(code("SELECT count(*) FROM t LIMIT 5 trailing"), "parse_error");
    assert_eq!(
        code("SELECT count(*) FROM t WHERE amount > 99999999999999999999999"),
        "parse_error"
    );
    assert_eq!(code("SELECT count(*) FROM t WHERE price > -12.50"), "parse_error");
}

/// The stack bound, which is not a taste in conditions: a recursive descent that runs out of
/// stack aborts the process rather than returning an error.
#[test]
fn a_condition_deeper_than_the_limit_is_refused_rather_than_overflowing() {
    let deep =
        format!("SELECT count(*) FROM t WHERE {}amount > 1{}", "(".repeat(300), ")".repeat(300));
    assert_eq!(code(&deep), "query_too_deep");
}

/// The codes a client sees for a schema mistake are the query language's own, because the
/// mistake is the same one and a client should not need to know which surface it came through.
#[test]
fn schema_failures_keep_the_codes_they_have_in_the_other_language() {
    let s = translate("SELECT count(*) FROM nope").unwrap();
    let e = big_plan::plan(&s.calls[0].table, &s.calls[0].call, &Stub).unwrap_err();
    assert_eq!(e.code(), "unknown_table");

    let s = translate("SELECT count(*) FROM t WHERE missing > 1").unwrap();
    let e = big_plan::plan(&s.calls[0].table, &s.calls[0].call, &Stub).unwrap_err();
    assert_eq!(e.code(), "unknown_field");

    let s = translate("SELECT sum(category) FROM t").unwrap();
    let e = big_plan::plan(&s.calls[0].table, &s.calls[0].call, &Stub).unwrap_err();
    assert_eq!(e.code(), "operator_not_allowed");
}
