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
    // A `FULL` mixed with any other kind: `(t JOIN u) FULL JOIN v` pairs on the keys `t` and
    // `u` share unioned with `v`'s, and one flag per side cannot tell that from the union of
    // all three. Every other outer join is answered - see `joins.rs`.
    assert_eq!(
        code("SELECT count(*) FROM t JOIN u ON t.a = u.a FULL JOIN v ON t.a = v.a"),
        "sql_no_outer_joins"
    );
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
    // A `HAVING` may only name a number the select list already asked for - that rule is what
    // both of these are about, and it is the same rule whether or not there is a `GROUP BY`.
    // A grouping that selected `sum(amount)` carries no count for a `HAVING count(*)` to read.
    assert_eq!(
        code("SELECT category, sum(amount) FROM t GROUP BY category HAVING count(*) > 5"),
        "sql_unsupported"
    );
    // The ungrouped case, same rule: the answer holds a count and no sum.
    assert_eq!(code("SELECT count(*) FROM t HAVING sum(amount) > 5"), "sql_unsupported");
    // And the shapes that hold no aggregate at all for one to be about.
    assert_eq!(code("SELECT amount FROM t HAVING count(*) > 5"), "sql_unsupported");
    assert_eq!(code("SELECT * FROM t HAVING count(*) > 5"), "sql_unsupported");
    assert_eq!(code("SELECT count(*) FROM t LIMIT 1 OFFSET 5"), "sql_unsupported");
    // A window used to be refused outright and share `sql_unsupported` with everything else
    // this surface had no answer for. It has one now, so what is left are the two mistakes a
    // window can be: no ordering to rank by, and a shape that holds no rows to rank.
    assert_eq!(code("SELECT row_number() OVER () FROM t"), "sql_window_frame");
    assert_eq!(
        code("SELECT category, row_number() OVER (ORDER BY amount) FROM t GROUP BY category"),
        "sql_window_shape"
    );
    // Arithmetic is answered now; what is refused is an expression that is not about one
    // column - two of them in a cell, or none at all.
    assert_eq!(code("SELECT concat(country, category) FROM t"), "sql_unsupported");
    assert_eq!(code("SELECT 1 + 1 FROM t"), "sql_unsupported");
    // Any arity up to the cap is answered now - the real bound is the passes, checked per level
    // by the executor. What the cap refuses is a statement naming more columns than an answer's
    // arity is allowed to be, which is refused at the text rather than after building a frontier.
    assert_eq!(
        code("SELECT DISTINCT category, country, active, device, city FROM t"),
        "sql_unsupported"
    );
    assert_eq!(
        code("SELECT count(*) FROM t GROUP BY category, country, active, device, city"),
        "sql_unsupported"
    );
    // A column that is neither grouped nor aggregated is still the classic SQL error.
    assert_eq!(
        code("SELECT category, count(*) FROM t GROUP BY country, active"),
        "sql_unsupported"
    );
    assert_eq!(code("SELECT DISTINCT count(*) FROM t"), "sql_unsupported");
    assert_eq!(code("SELECT DISTINCT category FROM t GROUP BY category"), "sql_unsupported");
    assert_eq!(code("SELECT count(*) FROM t WHERE amount IS NULL"), "sql_no_nulls");
    assert_eq!(code("SELECT count(*) FROM t WHERE amount = NULL"), "sql_no_nulls");
    // A projection is not refused for its limit any more - with one, without one, or with one
    // past what used to be the cap, it lowers.
    assert!(translate("SELECT amount FROM t").is_ok());
    assert!(translate("SELECT country FROM t WHERE amount > 5").is_ok());
    assert!(translate("SELECT amount FROM t LIMIT 0").is_ok());
    assert!(translate("SELECT amount FROM t LIMIT 100000").is_ok());
    // An `INSERT` is answered now - see `writes` - so what is refused is one that does not
    // name the record it writes about.
    // A column list is still required - without one the values would be positional against a
    // field order the statement does not carry. The `id` column is not: leaving it out asks the
    // server to allocate one.
    assert_eq!(code("INSERT INTO t VALUES (1, 2)"), "sql_insert_shape");
    assert_eq!(code("INSERT INTO t (_record_id, amount) VALUES ('seven', 2)"), "sql_insert_shape");
    // Any answer whose cells are values is a source - a projection, and a grouping just as
    // much, which is what a materialised rollup is here. What is not is `SELECT *`: it answers
    // with record ids, which are the address a fact is written to rather than anything stored
    // in a column. And one table on both sides has no snapshot under it.
    assert_eq!(code("INSERT INTO t (amount) SELECT * FROM u"), "sql_unsupported");
    assert_eq!(code("INSERT INTO t (amount) SELECT amount FROM t"), "sql_insert_self_read");
    // `id` is what a record is called, so a *field* of that name is one no `INSERT` could ever
    // fill - refused where it is declared rather than where it silently answers nothing.
    assert_eq!(code("CREATE TABLE t (_record_id UINT(32), a SET)"), "sql_id_column");
    assert_eq!(code("ALTER TABLE t ADD COLUMN _record_id UINT(32)"), "sql_id_column");
    // A `DELETE` with a `WHERE` is answered now, so what is left refused is the one that names
    // every record - which `TRUNCATE TABLE` says by freeing the fragments instead.
    assert_eq!(code("DELETE FROM t"), "sql_delete_all");
    assert!(big_sql::translate("DELETE FROM t WHERE amount > 5").is_ok());
    // A column list is answered now - see `schema` - so what is refused is a type name that
    // names nothing this engine stores, and the SQL that comes attached to one.
    assert_eq!(code("CREATE TABLE t (a BLOB)"), "sql_unknown_column_type");
    assert_eq!(code("CREATE TABLE t (a INT NOT NULL)"), "sql_no_constraints");
    assert_eq!(code("CREATE TABLE t (a DECIMAL)"), "sql_decimal_scale");
    assert_eq!(code("CREATE TABLE t (a UINT(0))"), "sql_bit_depth");
    assert_eq!(code("CREATE INDEX i ON t (a)"), "sql_read_only");
    // Databases are answered now - see `database.test`. What is still refused is a *session*:
    // one statement is one request, so the database arrives with the request rather than being
    // something `USE` can leave behind for the next one.
    assert_eq!(code("USE d"), "sql_use_unsupported");
    // A plain view is answered now - see `view.test`. What is refused is the materialised half,
    // which is a table plus a write path rather than a name for a statement.
    assert_eq!(
        code("CREATE MATERIALIZED VIEW v AS SELECT count(*) FROM t"),
        "sql_no_materialized_views"
    );
    assert_eq!(code("DROP MATERIALIZED VIEW v"), "sql_no_materialized_views");
    // `ALTER VIEW` gets the ordinary write refusal instead: `CREATE OR REPLACE VIEW` already
    // says the whole statement, so there is no change to a view left to name.
    assert_eq!(code("ALTER VIEW v AS SELECT a FROM t"), "sql_read_only");
    // The searched `CASE` and both `if` spellings are answered; the simple form is the one
    // refused, because it is the same tree with the comparison factored out.
    assert_eq!(code("SELECT CASE amount WHEN 5 THEN 1 ELSE 0 END FROM t"), "sql_unsupported");
    // A conversion that would have to invent a value is still one this engine cannot make:
    // widening a day count to an instant has to choose a time of day.
    assert_eq!(code("SELECT toDateTime(visit) FROM t"), "sql_unsupported");
    assert_eq!(code("SELECT argMax(amount, price) FROM t"), "sql_unsupported");
    assert_eq!(code("SELECT stddevPop(amount) FROM t"), "sql_unsupported");
    assert_eq!(code("SELECT corr(amount, price) FROM t"), "sql_unsupported");
    // `LIKE` is answered now - a pattern over a keyed column is a union of the keys that match.
    // Every other string comparison is still refused, because none of them is a set operation -
    // and the regular-expression spellings are refused by their own name, which is what carries
    // the pattern language that *is* here.
    assert_eq!(code("SELECT count(*) FROM t WHERE country SIMILAR TO 'G%'"), "sql_no_regex");
    assert_eq!(code("SELECT count(*) FROM t WHERE match(country, '^G')"), "sql_no_regex");
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
    // A projection is ordered now, so what is refused over one is an ordering the answer holds
    // nothing to sort by: a column it does not read, or a number about the set.
    assert_eq!(code("SELECT country FROM t ORDER BY amount"), "sql_unsupported_order");
    assert_eq!(code("SELECT amount FROM t ORDER BY count(*)"), "sql_unsupported_order");
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

    // A projection with no `LIMIT` is a full scan rather than a refusal, and one with an
    // `ORDER BY` is answered - at the cost of the cut leaving the plan, which is the whole of
    // what that clause changes here.
    let ordered = translate("SELECT amount FROM t ORDER BY amount LIMIT 10").unwrap();
    let Shape::Table { order, cut, .. } = &ordered.answer.shape else { panic!("a projection") };
    assert_eq!(cut, &Some(10), "the limit moves into the shape");
    assert!(order.is_some());
    // And it is gone from the plan, which is what makes the sort correct rather than a sort of
    // whichever ten records happened to be read first.
    assert!(
        !format!("{:?}", ordered.calls[0].call).contains("\"n\""),
        "the plan kept a cut it cannot carry: {:?}",
        ordered.calls[0].call
    );

    // Without an ordering the cut stays in the plan, where it bounds the point reads.
    let cut_in_plan = translate("SELECT amount FROM t LIMIT 10").unwrap();
    assert!(format!("{:?}", cut_in_plan.calls[0].call).contains("\"n\""));

    assert!(translate("SELECT amount FROM t").is_ok());
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
    // `-12.50` used to be here. It is a legal literal now - a float field holds one - so the
    // refusal moved to where the field is known; see `big-plan`'s planning tests.
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

/// The refusal whose whole value is the sentence.
///
/// **A bitmap function here is not a missing feature, and the message is the only thing that
/// says so.** Every name on that list has an answer under a different word, so a refusal that
/// merely said "unsupported" would send somebody to build what they already have. This asserts
/// the mapping is actually in the sentence - the one refusal in the dialect where the prose is
/// the deliverable rather than the decoration.
#[test]
fn the_bitmap_refusal_names_the_spelling_that_works_here() {
    let e = translate("SELECT bitmap_count(uid) FROM t").unwrap_err();
    let SqlError::Refused { what, .. } = e else { panic!("expected a refusal, got {e:?}") };
    assert_eq!(what, Refused::BitmapFunction);

    let why = what.why();
    for spelling in ["count(DISTINCT x)", "sum(x)", "BETWEEN", "NOT IN (SELECT _record_id"] {
        assert!(why.contains(spelling), "the mapping is missing `{spelling}`: {why}");
    }

    // Both dialects reach it, which is the point of listing both.
    assert_eq!(code("SELECT bitmapAnd(a, b) FROM t"), "sql_bitmap_function");
    assert_eq!(code("SELECT bsi_range(amount, 1, 9) FROM t"), "sql_bitmap_function");

    // And the approximate-distinct names are *not* on it: they are answered, exactly, which is
    // the same published decision `uniqHLL12` already rests on. A refusal here would contradict
    // the sentence above, which tells people cardinality is a popcount.
    assert!(translate("SELECT approx_count_distinct(country) FROM t").is_ok());
    assert!(translate("SELECT uniqHLL12(country) FROM t").is_ok());
}

/// The two types the engine understands and declines, which is why they do not share a code with
/// the names it simply does not know.
#[test]
fn a_declined_type_is_told_apart_from_an_unknown_one() {
    // Understood and declined.
    assert_eq!(code("CREATE TABLE t (a Nullable(String))"), "sql_nullable_type");
    assert_eq!(code("CREATE TABLE t (a BITMAP)"), "sql_bitmap_type");
    // Not understood.
    assert_eq!(code("CREATE TABLE t (a JSON)"), "sql_unknown_column_type");

    // The nested form says the same sentence. `wrapped_type` could not read this one, so
    // refusing at the word rather than after the bracket is what keeps it a refusal instead of
    // a syntax error about an inner `(`.
    assert_eq!(code("CREATE TABLE t (a Nullable(Decimal(10, 2)))"), "sql_nullable_type");

    // Each names what to write instead, which is the whole claim of the list.
    assert!(Refused::NullableType.why().contains("SELECT *"), "no answer named");
    assert!(Refused::BitmapType.why().contains("SET"), "no column kind named");
}
