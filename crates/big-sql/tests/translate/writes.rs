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

//! `INSERT`, which is the one statement here that carries values rather than questions.
//!
//! The claims in this file are about what survives translation. Every value is handed to the
//! layer that holds a schema exactly as written, because what a value *means* is the field's
//! kind to decide - so a test that a literal reaches the other side unchanged is a test that
//! this crate did not quietly decide something it has no schema to decide with.

use super::common::*;
use big_plan::Literal;

/// The shape of the statement: which columns, which record, which values.
#[test]
fn an_insert_is_its_columns_and_its_rows() {
    let i = insert(
        "INSERT INTO tx (_record_id, country, amount) VALUES (1, 'GB', 100), (2, 'US', 900)",
    );
    assert_eq!(i.table, "tx");
    assert_eq!(i.columns, ["_record_id", "country", "amount"]);
    assert_eq!(i.id_at, Some(0));
    assert_eq!(i.values().len(), 2);
    assert_eq!(i.record(&i.values()[0]), Some(1));
    assert_eq!(i.record(&i.values()[1]), Some(2));
    // Two rows of two fields each, which is what the statement costs in facts.
    assert_eq!(i.fact_count(), 4);
    assert_eq!(
        i.facts(&i.values()[0]).collect::<Vec<_>>(),
        [("country", &Literal::Str("GB".to_string())), ("amount", &Literal::Int(100))]
    );
    // `INTO` is optional, as it is everywhere else it appears.
    assert_eq!(insert("INSERT tx (_record_id) VALUES (1)").table, "tx");
    // And `VALUE` is MySQL's spelling of the same word.
    assert_eq!(insert("INSERT INTO tx (_record_id) VALUE (1)").values().len(), 1);
}

/// The id is a column like any other, and may be written anywhere in the list.
#[test]
fn the_record_id_is_found_wherever_it_was_written() {
    let i = insert("INSERT INTO tx (country, _record_id, amount) VALUES ('GB', 7, 100)");
    assert_eq!(i.id_at, Some(1));
    assert_eq!(i.record(&i.values()[0]), Some(7));
    // Everything but the id is a field, whichever position the id took.
    assert_eq!(
        i.facts(&i.values()[0]).map(|(name, _)| name).collect::<Vec<_>>(),
        ["country", "amount"]
    );
    // Case is not significant here either, and a quoted name is the same column.
    assert_eq!(insert("INSERT INTO tx (_RECORD_ID) VALUES (1)").id_at, Some(0));
    assert_eq!(insert("INSERT INTO tx (\"_record_id\") VALUES (1)").id_at, Some(0));
}

/// A statement may leave the record to the server, which is a different statement rather than
/// an incomplete one.
#[test]
fn a_statement_without_an_id_asks_the_server_for_one() {
    let i = insert("INSERT INTO tx (country, amount) VALUES ('GB', 100), ('US', 900)");
    assert_eq!(i.id_at, None);
    // `None` per row is the whole of the difference: nothing downstream searches the column
    // list to find out which of the two forms it was handed.
    assert_eq!(i.record(&i.values()[0]), None);
    assert_eq!(i.field_count(), 2);
    assert_eq!(i.fact_count(), 4);
    // Every column is a field when none of them is the id.
    assert_eq!(
        i.facts(&i.values()[0]).map(|(name, _)| name).collect::<Vec<_>>(),
        ["country", "amount"]
    );
    // And with an id there is one fewer field than there are columns.
    let named = insert("INSERT INTO tx (_record_id, country) VALUES (1, 'GB')");
    assert_eq!(named.field_count(), 1);
}

/// **The load-bearing claim of this file.** Each literal reaches the other side as written.
///
/// The layer above resolves these against a field's kind: `'GB'` is a key to intern, `12.50` is
/// 1250 units on a decimal of scale 2, `-5` is a signed value and a mistake on an unsigned
/// field. A parser that normalised any of them - a decimal into a float, a negative into an
/// unsigned - would be deciding something it has no schema to decide, and the layer above would
/// resolve the wrong thing.
#[test]
fn every_literal_survives_translation_as_written() {
    let i = insert(
        "INSERT INTO tx (_record_id, n, neg, price, key, flag)
         VALUES (1, 100, -5, 12.50, 'GB', true)",
    );
    assert_eq!(
        i.values()[0],
        [
            Literal::Int(1),
            Literal::Int(100),
            Literal::Sint(-5),
            Literal::Dec { units: 1250, scale: 2 },
            Literal::Str("GB".to_string()),
            Literal::Bool(true),
        ]
    );
    // A `WITH` binding is a constant substituted wherever a literal is expected - but a `WITH`
    // introduces a `SELECT`, so a statement cannot bind one and then insert it.
    assert_eq!(code("WITH 7 AS n INSERT INTO tx (_record_id) VALUES (n)"), "parse_error");
}

/// What an `INSERT` will not take, each refused where it is written.
#[test]
fn the_writes_an_insert_does_not_make() {
    // No column list: positional against a field order the statement does not carry, and that
    // the next `ALTER TABLE ... ADD` would move underneath it.
    assert_eq!(code("INSERT INTO tx VALUES (1, 'GB')"), "sql_insert_shape");
    // Omitting the id is no longer a mistake - it asks the server to allocate one - so what
    // is left refused is an id that is not an address.
    // An id that is not a whole number, judged at the value - which needs no schema, since a
    // record id is a number on every write path this engine has.
    assert_eq!(code("INSERT INTO tx (_record_id) VALUES ('seven')"), "sql_insert_shape");
    assert_eq!(code("INSERT INTO tx (_record_id) VALUES (-1)"), "sql_insert_shape");
    assert_eq!(code("INSERT INTO tx (_record_id) VALUES (1.5)"), "sql_insert_shape");
    // An answer is not records to copy, so there is nothing to write back.
    assert_eq!(code("INSERT INTO tx (_record_id) SELECT n FROM other"), "sql_unsupported");
    assert_eq!(code("INSERT INTO tx (n) VALUES (1) ON DUPLICATE KEY UPDATE n = 2"), "parse_error");
}

/// A statement is bounded by what it holds in memory, twice over.
///
/// Sized from the constant rather than from a number written here, so that moving the ceiling
/// moves the test with it. The statement built is large - that is the point of the ceiling -
/// which is why this is the one test in the file that costs anything to run.
#[test]
fn an_insert_is_refused_past_the_row_ceiling() {
    let rows = |n: usize| {
        let mut out = String::with_capacity(n * 10 + 40);
        out.push_str("INSERT INTO tx (_record_id) VALUES ");
        for i in 1..=n {
            if i > 1 {
                out.push(',');
            }
            out.push('(');
            out.push_str(&i.to_string());
            out.push(')');
        }
        out
    };
    assert_eq!(insert(&rows(big_sql::MAX_INSERT_ROWS)).values().len(), big_sql::MAX_INSERT_ROWS);
    assert_eq!(code(&rows(big_sql::MAX_INSERT_ROWS + 1)), "sql_insert_too_large");
}

/// A malformed statement is a syntax error, not a refusal: nothing about the engine makes a
/// row of the wrong width impossible, the statement is simply not saying what it means.
#[test]
fn a_malformed_insert_is_a_syntax_error() {
    for sql in [
        "INSERT INTO tx (_record_id, country) VALUES (1)",
        "INSERT INTO tx (_record_id, country) VALUES (1, 'GB', 2)",
        "INSERT INTO tx (_record_id) (1)",
        "INSERT INTO tx (_record_id) VALUES 1",
        "INSERT INTO tx (_record_id,) VALUES (1)",
        "INSERT INTO tx (_record_id) VALUES (1),",
        "INSERT INTO tx (_record_id) VALUES (1) extra",
        "INSERT INTO (_record_id) VALUES (1)",
    ] {
        assert_eq!(code(sql), "parse_error", "{sql}");
    }
}
