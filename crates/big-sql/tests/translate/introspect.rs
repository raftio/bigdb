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

//! `DESCRIBE` and `SHOW`: asking the catalog, and writing a schema back out as SQL.

use super::common::*;
use big_sql::{Column, ColumnKind, Format, Shown};

/// Four spellings of one question, because they are four dialects' words for it and there is
/// nothing to gain from knowing which one was typed.
#[test]
fn describing_a_table_has_four_spellings_and_one_meaning() {
    let columns = Shown::Columns { database: None, table: "events".to_string() };
    for sql in [
        "DESCRIBE events",
        "DESC events",
        "DESCRIBE TABLE events",
        "SHOW COLUMNS FROM events",
        "SHOW FIELDS FROM events",
    ] {
        assert_eq!(show(sql).what, columns, "{sql}");
    }
    assert_eq!(show("SHOW TABLES").what, Shown::Tables { database: None });
    assert_eq!(show("SHOW VIEWS").what, Shown::Views { database: None });
    let create = Shown::Create { database: None, table: "events".to_string(), view: false };
    assert_eq!(show("SHOW CREATE TABLE events").what, create);
    // Neither word is required, and a bare `SHOW CREATE` looks the name up as either - which is
    // the same question, so it is the same `Shown`.
    assert_eq!(show("SHOW CREATE events").what, create);
    // `VIEW` is not decoration: it says a table under that name is the wrong object rather than
    // the answer, which the layer holding a catalog is what acts on.
    assert_eq!(
        show("SHOW CREATE VIEW events").what,
        Shown::Create { database: None, table: "events".to_string(), view: true }
    );
}

/// `FORMAT` is about the bytes and nothing else, which is why it is the same clause a `SELECT`
/// takes rather than a second one that means the same thing.
#[test]
fn a_listing_is_written_in_whichever_format_was_asked_for() {
    assert_eq!(show("SHOW TABLES").format, Format::Json);
    assert_eq!(show("SHOW TABLES FORMAT TSV").format, Format::Tsv);
    assert_eq!(show("DESCRIBE events FORMAT CSVWithNames").format, Format::CsvWithNames);
    assert_eq!(code("SHOW TABLES FORMAT Arrow"), "sql_unknown_format");
}

/// What `SHOW` will not answer.
#[test]
fn the_catalog_questions_that_have_no_answer_here() {
    // Each of these is a surface of its own, and none is one this statement grows by accident.
    // `SHOW GRANTS` was on this list until roles arrived, and is now answered - which is why
    // the list is a list rather than a comment: the surface grows, and what it refuses shrinks.
    for sql in ["SHOW INDEX FROM t", "DESCRIBE", "SHOW"] {
        assert_eq!(code(sql), "parse_error", "{sql}");
    }
    // `SHOW PROCESSLIST` was on this list until the running-query registry arrived, and is now
    // answered - which is the list doing what the comment above says it does. It demands
    // `Operate`, because reading somebody else's running statements is an administrative
    // question and the text of one can name tables the reader may not read.
    let listed = big_sql::translate("SHOW PROCESSLIST").unwrap();
    assert_eq!(
        listed.demands().iter().map(|d| d.privilege).collect::<Vec<_>>(),
        vec![big_sql::Privilege::Operate]
    );
}

/// **The test that keeps `render` and `column_type` from drifting.**
///
/// They are the two halves of one table - a type name onto a field kind, and back - and an
/// inverse that is only checked by eye is an inverse that stops being one. Every column here
/// came from a column list, so every one of them round-trips exactly.
#[test]
fn a_rendered_schema_reads_back_as_the_schema_it_came_from() {
    for sql in [
        "CREATE TABLE t (a SET, b MUTEX, c BOOL, d TIMEQUANTUM)",
        "CREATE TABLE t (a UINT(1), b UINT(12), c UINT(64))",
        "CREATE TABLE t (a SIGNED, b SIGNED(12))",
        "CREATE TABLE t (a DECIMAL(10, 2), b NUMERIC(4, 4), c DECIMAL(19, 0))",
        // The SQL spellings render as the native name of the thing they created, which is the
        // point: `TEXT` and `SET` are one field, and the schema answers with what exists.
        "CREATE TABLE t (a TEXT, b BIGINT, c BOOLEAN, d TIMESTAMP)",
        "CREATE TABLE t ()",
    ] {
        let written = big_sql::render::create_table("t", None, &columns_of(sql));
        assert_eq!(columns(&written), columns(sql), "{sql} -> {written}");
    }
    // The engine comes back too, quoted when it has to be: `+` is not a character the lexer
    // has a token for, so an unquoted one would not read back.
    for engine in ["columnar", "bitmap+columnar"] {
        let written =
            big_sql::render::create_table("t", Some(engine), &columns_of("CREATE TABLE t (a SET)"));
        let big_sql::Ddl::CreateTable { database: None, engine: read_back, .. } = ddl(&written)
        else {
            panic!("a CREATE is a CreateTable")
        };
        assert_eq!(read_back.as_deref(), Some(engine), "{written}");
    }
}

/// A decimal field created over the field route names its bits directly, and there may be no
/// precision that derives exactly that many - so the rendered statement asks for the smallest
/// precision that covers it, which holds every value the field can.
#[test]
fn a_decimal_that_came_from_the_field_route_renders_wide_enough() {
    let column = Column {
        name: "price".to_string(),
        kind: ColumnKind::Decimal,
        bit_depth: 20,
        scale: Some(2),
    };
    let written = big_sql::render::create_table("t", None, &[column]);
    let read_back = columns(&written);
    let [(name, kind, bit_depth, scale)] = read_back.as_slice() else { panic!("one column") };
    assert_eq!((name.as_str(), *kind, *scale), ("price", "decimal", Some(2)));
    assert!(*bit_depth >= 20, "{written} narrowed a field from 20 bits to {bit_depth}");
}

/// The columns of a `CREATE TABLE`, as the structs the renderer takes.
fn columns_of(sql: &str) -> Vec<Column> {
    let big_sql::Ddl::CreateTable { database: None, columns, .. } = ddl(sql) else {
        panic!("`{sql}` is not a CREATE TABLE")
    };
    columns
}
