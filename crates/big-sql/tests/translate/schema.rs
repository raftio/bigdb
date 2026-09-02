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

//! `CREATE TABLE`, and what a type name in its column list means.
//!
//! This is the one file in the suite about a statement that is not a query, and the one place
//! the crate decides what a word means rather than passing it on for a schema to resolve. So
//! the claims here are a table: this spelling, that kind, that many bits.

use super::common::*;
use big_sql::{Alter, Column, ColumnKind, Ddl};

/// The statement without a column list, which is what it was before there was one.
#[test]
fn a_table_is_still_a_name_and_an_engine() {
    assert_eq!(
        ddl("CREATE TABLE events"),
        Ddl::CreateTable {
            database: None,
            table: "events".to_string(),
            engine: None,
            columns: Vec::new(),
            if_not_exists: false,
        }
    );
    assert_eq!(
        ddl("CREATE TABLE events ENGINE = columnar"),
        Ddl::CreateTable {
            database: None,
            table: "events".to_string(),
            engine: Some("columnar".to_string()),
            columns: Vec::new(),
            if_not_exists: false,
        }
    );
    // Quoted because `+` is not a character the lexer has a token for.
    assert_eq!(
        ddl("CREATE TABLE events ENGINE = 'bitmap+columnar'"),
        Ddl::CreateTable {
            database: None,
            table: "events".to_string(),
            engine: Some("bitmap+columnar".to_string()),
            columns: Vec::new(),
            if_not_exists: false,
        }
    );
    // An empty list is a table with no fields written the long way, not a statement in doubt.
    assert_eq!(columns("CREATE TABLE events ()"), []);
}

/// What each spelling means, which is the whole of the dialect this file is about.
#[test]
fn every_type_name_maps_onto_a_field_kind() {
    // The native spellings, which are the kind names the field routes take.
    assert_eq!(
        columns("CREATE TABLE t (a SET, b MUTEX, c BOOL, d TIMEQUANTUM, e SIGNED, f UINT(12))"),
        [
            ("a".to_string(), "set", 32, None),
            ("b".to_string(), "mutex", 32, None),
            ("c".to_string(), "bool", 32, None),
            ("d".to_string(), "timequantum", 32, None),
            ("e".to_string(), "signed", 32, None),
            ("f".to_string(), "int", 12, None),
        ]
    );
    // A string lands in a keyed field, so every SQL spelling of one is a set.
    assert_eq!(
        columns("CREATE TABLE t (a TEXT, b VARCHAR, c CHAR, d STRING)"),
        [
            ("a".to_string(), "set", 32, None),
            ("b".to_string(), "set", 32, None),
            ("c".to_string(), "set", 32, None),
            ("d".to_string(), "set", 32, None),
        ]
    );
    // The widths the integer names have always meant, which is what a bit depth decides here:
    // every plane is one more bitmap a range query intersects.
    assert_eq!(
        columns("CREATE TABLE t (a TINYINT, b SMALLINT, c INT, d INTEGER, e BIGINT)"),
        [
            ("a".to_string(), "int", 8, None),
            ("b".to_string(), "int", 16, None),
            ("c".to_string(), "int", 32, None),
            ("d".to_string(), "int", 32, None),
            ("e".to_string(), "int", 64, None),
        ]
    );
    // The temporal kinds are three things, not one. `TIMESTAMP` and `DATETIME` are the same
    // scalar field; `DATE` is the same shape over days; a `TIMEQUANTUM` is the keyed field that
    // is viewed by day, and is what the first three used to be.
    assert_eq!(
        columns("CREATE TABLE t (a BOOLEAN, b TIMESTAMP, c DATETIME, d DATE, e TIMEQUANTUM)"),
        [
            ("a".to_string(), "bool", 32, None),
            ("b".to_string(), "datetime", 64, None),
            ("c".to_string(), "datetime", 64, None),
            ("d".to_string(), "date", 32, None),
            ("e".to_string(), "timequantum", 32, None),
        ]
    );
    // The float widths are in their names, and every SQL spelling lands on one of the two.
    assert_eq!(
        columns("CREATE TABLE t (a FLOAT, b REAL, c FLOAT32, d DOUBLE, e FLOAT64)"),
        [
            ("a".to_string(), "float32", 32, None),
            ("b".to_string(), "float32", 32, None),
            ("c".to_string(), "float32", 32, None),
            ("d".to_string(), "float64", 64, None),
            ("e".to_string(), "float64", 64, None),
        ]
    );
    // Case is not significant, here as everywhere else in this dialect.
    assert_eq!(columns("create table t (a bigint)"), [("a".to_string(), "int", 64, None)]);
}

/// `SIGNED` and `UNSIGNED` after an integer name, which say what the name already implies.
#[test]
fn the_sign_word_picks_the_kind_and_the_name_keeps_the_width() {
    assert_eq!(
        columns("CREATE TABLE t (a INT UNSIGNED, b BIGINT SIGNED, c SMALLINT SIGNED)"),
        [
            ("a".to_string(), "int", 32, None),
            ("b".to_string(), "signed", 64, None),
            ("c".to_string(), "signed", 16, None),
        ]
    );
    // The native spelling takes its depth in brackets, because a bit-sliced field is cheaper
    // the narrower it is and no SQL name is 12 bits wide.
    assert_eq!(columns("CREATE TABLE t (a SIGNED(12))"), [("a".to_string(), "signed", 12, None)]);
}

/// The precision buys the bit depth, and the scale is never guessed at.
#[test]
fn a_decimal_takes_both_of_its_numbers() {
    // 10^p - 1 needs ceil(p * log2(10)) bits, and this asks for no fewer.
    assert_eq!(
        columns("CREATE TABLE t (a DECIMAL(10, 2), b NUMERIC(4, 4), c DECIMAL(19, 0))"),
        [
            ("a".to_string(), "decimal", 34, Some(2)),
            ("b".to_string(), "decimal", 14, Some(4)),
            ("c".to_string(), "decimal", 64, Some(0)),
        ]
    );
    // One argument is SQL for a precision, and reading it as a scale would turn `price > 5`
    // into a question about ten-billionths. So it is refused rather than given a new meaning.
    assert_eq!(code("CREATE TABLE t (a DECIMAL(10))"), "sql_decimal_scale");
    assert_eq!(code("CREATE TABLE t (a DECIMAL)"), "sql_decimal_scale");
    assert_eq!(code("CREATE TABLE t (a NUMERIC)"), "sql_decimal_scale");
    // More digits after the point than there are digits.
    assert_eq!(code("CREATE TABLE t (a DECIMAL(2, 5))"), "sql_decimal_scale");
    assert_eq!(code("CREATE TABLE t (a DECIMAL(0, 0))"), "sql_decimal_scale");
    // Wider than the integer a value is read back into.
    assert_eq!(code("CREATE TABLE t (a DECIMAL(25, 2))"), "sql_bit_depth");
}

/// A bit depth is bounded where a bit-sliced value is bounded.
#[test]
fn a_bit_depth_is_one_to_sixty_four() {
    assert_eq!(columns("CREATE TABLE t (a UINT(1))"), [("a".to_string(), "int", 1, None)]);
    assert_eq!(columns("CREATE TABLE t (a UINT(64))"), [("a".to_string(), "int", 64, None)]);
    assert_eq!(code("CREATE TABLE t (a UINT(0))"), "sql_bit_depth");
    assert_eq!(code("CREATE TABLE t (a UINT(65))"), "sql_bit_depth");
    assert_eq!(code("CREATE TABLE t (a SIGNED(65))"), "sql_bit_depth");
    // A depth is written literally. A `WITH` binding has no business standing in for one, and
    // a `CREATE` has no `WITH` to bind it in anyway.
    assert_eq!(code("CREATE TABLE t (a UINT(x))"), "parse_error");
}

/// What the column list will not take, each refused where it is written.
#[test]
fn the_sql_a_column_list_does_not_answer() {
    // A type this engine has nothing to store: no floats, no documents. `DATE` is *not* on this
    // list - it is a time quantum, alongside `TIMESTAMP` and `DATETIME`, and is checked below.
    for sql in [
        "CREATE TABLE t (a FLOAT(10, 2))",
        "CREATE TABLE t (a DATETIME(3))",
        "CREATE TABLE t (a UUID)",
        "CREATE TABLE t (a JSON)",
        "CREATE TABLE t (a BLOB)",
    ] {
        assert_eq!(code(sql), "sql_unknown_column_type", "{sql}");
    }
    // Constraints are promises about rows, and a fact at (field, record) is not a row.
    for sql in [
        "CREATE TABLE t (a INT NOT NULL)",
        "CREATE TABLE t (a INT NULL)",
        "CREATE TABLE t (a INT PRIMARY KEY)",
        "CREATE TABLE t (a INT UNIQUE)",
        "CREATE TABLE t (a INT DEFAULT 0)",
        "CREATE TABLE t (a INT AUTO_INCREMENT)",
        "CREATE TABLE t (a INT REFERENCES u (b))",
        "CREATE TABLE t (a INT CHECK (a > 0))",
        "CREATE TABLE t (a TEXT COMMENT 'why')",
    ] {
        assert_eq!(code(sql), "sql_no_constraints", "{sql}");
    }
    // A length on a key bounds nothing: keys are stored whole, with nothing to truncate.
    assert_eq!(code("CREATE TABLE t (a VARCHAR(255))"), "sql_no_constraints");
    // A display width is not a bit depth. `INT(11)` is eleven digits where it was written and
    // would be eleven bits here, which stops at 2047 - so it is refused, not reinterpreted.
    assert_eq!(code("CREATE TABLE t (a INT(11))"), "sql_unknown_column_type");
    assert_eq!(code("CREATE TABLE t (a BIGINT(20))"), "sql_unknown_column_type");
    // Still every other schema change, by name.
    assert_eq!(code("CREATE INDEX i ON t (a)"), "sql_read_only");
    assert_eq!(code("CREATE TEMPORARY TABLE t (a SET)"), "sql_read_only");
}

/// The list is punctuation like any other, and a malformed one is a syntax error rather than a
/// refusal: nobody meant `(a SET,)`, and telling them what this engine will not do would be
/// answering a question they did not ask.
#[test]
fn a_malformed_list_is_a_syntax_error() {
    for sql in [
        "CREATE TABLE t (a SET,)",
        "CREATE TABLE t (a SET",
        "CREATE TABLE t (SET)",
        "CREATE TABLE t (a)",
        "CREATE TABLE t (a SET) ENGINE",
        "CREATE TABLE t (a SET) trailing",
    ] {
        assert_eq!(code(sql), "parse_error", "{sql}");
    }
}

/// The whole statement, in the order somebody writes it.
#[test]
fn a_column_list_comes_before_the_engine() {
    let Ddl::CreateTable { database: None, table, engine, columns, if_not_exists } =
        ddl("CREATE TABLE events (
           country TEXT,
           amount  INT,
           price   DECIMAL(10, 2),
           visit   TIMEQUANTUM
         ) ENGINE = 'bitmap+columnar'")
    else {
        panic!("a CREATE is a CreateTable")
    };
    assert_eq!(table, "events");
    assert_eq!(engine.as_deref(), Some("bitmap+columnar"));
    assert_eq!(columns.len(), 4);
    assert_eq!(columns[2].scale, Some(2));
    assert!(!if_not_exists);
}

/// A comment inside the list is a comment, because the lexer reads one anywhere.
#[test]
fn a_column_list_is_lexed_like_everything_else() {
    assert_eq!(
        columns("CREATE TABLE t (a SET, -- the key\n b INT)"),
        [("a".to_string(), "set", 32, None), ("b".to_string(), "int", 32, None)]
    );
    // A quoted identifier is never a keyword, so a column may be called `set`.
    assert_eq!(columns("CREATE TABLE t (\"set\" SET)"), [("set".to_string(), "set", 32, None)]);
}

/// `ALTER TABLE`, which is the two changes the engine below can make to a field.
#[test]
fn alter_table_adds_and_drops_fields() {
    assert_eq!(
        ddl("ALTER TABLE events ADD COLUMN region TEXT"),
        Ddl::AlterTable {
            database: None,
            table: "events".to_string(),
            changes: vec![Alter::Add(Column {
                name: "region".to_string(),
                kind: ColumnKind::Set,
                bit_depth: 32,
                scale: None,
            })],
        }
    );
    // `COLUMN` is optional, as it is in every dialect that has the word at all.
    assert_eq!(ddl("ALTER TABLE t ADD a SET"), ddl("ALTER TABLE t ADD COLUMN a SET"));
    assert_eq!(ddl("ALTER TABLE t DROP a"), ddl("ALTER TABLE t DROP COLUMN a"));

    let Ddl::AlterTable { database: None, table, changes } = ddl("ALTER TABLE events
           ADD COLUMN price DECIMAL(10, 2),
           ADD COLUMN visit TIMEQUANTUM,
           DROP COLUMN legacy")
    else {
        panic!("an ALTER is an AlterTable")
    };
    assert_eq!(table, "events");
    assert_eq!(changes.len(), 3);
    assert!(matches!(&changes[2], Alter::Drop(f) if f == "legacy"));
    // A column in an `ADD` is the same column a `CREATE TABLE` list holds, read by the same
    // rule - so the type names and their refusals are the ones above, not a second set.
    let Alter::Add(price) = &changes[0] else { panic!("the first change is an ADD") };
    assert_eq!((price.kind, price.bit_depth, price.scale), (ColumnKind::Decimal, 34, Some(2)));
}

/// What `ALTER` will not do, each refused at the word that says it.
///
/// These are the claims worth having, because each one is a thing the engine cannot do rather
/// than a thing this dialect has not got round to: a kind is how every fact in a field was
/// routed, a name is what resolves one, and an engine is what the facts already there were
/// written under.
#[test]
fn the_alters_the_engine_cannot_make() {
    // A kind or a depth is what a field's bit planes are.
    for sql in [
        "ALTER TABLE t MODIFY a BIGINT",
        "ALTER TABLE t MODIFY COLUMN a BIGINT",
        "ALTER TABLE t ALTER COLUMN a TYPE BIGINT",
        "ALTER TABLE t CHANGE a b BIGINT",
        "ALTER TABLE t ADD COLUMN a SET, MODIFY b BIGINT",
    ] {
        assert_eq!(code(sql), "sql_no_alter_column", "{sql}");
    }
    // Nothing below renames a field or a table.
    assert_eq!(code("ALTER TABLE t RENAME TO u"), "sql_no_rename");
    assert_eq!(code("ALTER TABLE t RENAME COLUMN a TO b"), "sql_no_rename");
    // The engine is fixed when the table is created.
    assert_eq!(code("ALTER TABLE t ENGINE = columnar"), "sql_no_alter_engine");
    // Constraints and indexes are refused where a constraint in a column list is.
    for sql in [
        "ALTER TABLE t ADD PRIMARY KEY (a)",
        "ALTER TABLE t ADD CONSTRAINT c CHECK (a > 0)",
        "ALTER TABLE t ADD INDEX i (a)",
        "ALTER TABLE t DROP CONSTRAINT c",
        "ALTER TABLE t DROP PRIMARY KEY",
    ] {
        assert_eq!(code(sql), "sql_no_constraints", "{sql}");
    }
    // Everything an `ALTER` is not.
    assert_eq!(code("ALTER USER bob SET PASSWORD 'x'"), "sql_read_only");
    // A database is not a level this catalog has, which is a different sentence from "this
    // surface does not write".
    // A database carries a name and nothing else, so there is nothing about one to alter -
    // which is the same answer every other change this surface does not make gets.
    assert_eq!(code("ALTER DATABASE d OWNER TO bob"), "sql_read_only");
    // A type name in an `ADD` is judged by the rule the column list is judged by.
    assert_eq!(code("ALTER TABLE t ADD COLUMN a BLOB"), "sql_unknown_column_type");
    assert_eq!(code("ALTER TABLE t ADD COLUMN a DECIMAL"), "sql_decimal_scale");
    assert_eq!(code("ALTER TABLE t ADD COLUMN a INT NOT NULL"), "sql_no_constraints");
    // An `ALTER` that changes nothing is not a statement.
    assert_eq!(code("ALTER TABLE t"), "parse_error");
    assert_eq!(code("ALTER TABLE t ADD"), "parse_error");
    assert_eq!(code("ALTER TABLE t ADD COLUMN a SET,"), "parse_error");
}

/// `DROP TABLE`, which reaches exactly what `DELETE /table/{t}` reaches.
#[test]
fn a_table_is_dropped_by_name_and_at_most_one_at_a_time() {
    assert_eq!(
        ddl("DROP TABLE events"),
        Ddl::DropTable { database: None, table: "events".to_string(), if_exists: false }
    );
    assert_eq!(
        ddl("DROP TABLE IF EXISTS events"),
        Ddl::DropTable { database: None, table: "events".to_string(), if_exists: true }
    );
    // Two drops are two changes, each travelling to the leader and then to every node, and a
    // list would promise an atomicity nothing below here has.
    assert_eq!(code("DROP TABLE a, b"), "parse_error");
    // Half a clause is a syntax error rather than a table called `IF`, which is what reading
    // the word as a name would create.
    assert_eq!(code("DROP TABLE IF events"), "parse_error");
    assert_eq!(code("DROP TABLE"), "parse_error");
    // A field is dropped by `ALTER`, and `DROP COLUMN` on its own is not a statement.
    assert_eq!(code("DROP COLUMN a"), "sql_read_only");
}

/// **`_record_id` is a record, not a field**, so a column list may not declare one.
///
/// The trap this closes: an `INSERT` reads that column as the record to write *about*, so a
/// field of that name could never be written through this surface. It would sit empty while
/// `SELECT *` listed the very records it was meant to hold, and a condition on it would answer
/// nothing - which is a wrong answer rather than a refusal, and the one thing this surface must
/// not produce. The underscore is what keeps `id` out of this.
#[test]
fn a_field_cannot_be_called_record_id() {
    assert_eq!(code("CREATE TABLE t (_record_id UINT(32))"), "sql_id_column");
    assert_eq!(code("CREATE TABLE t (a SET, _RECORD_ID INT)"), "sql_id_column");
    assert_eq!(code("CREATE TABLE t (\"_record_id\" SET)"), "sql_id_column");
    assert_eq!(code("ALTER TABLE t ADD _record_id SET"), "sql_id_column");
    // Dropping one is still allowed: a table that already has such a field - created over
    // `POST /table/{t}/field/{f}`, which reserves no name - has to be fixable.
    assert_eq!(
        ddl("ALTER TABLE t DROP COLUMN _record_id"),
        Ddl::AlterTable {
            database: None,
            table: "t".to_string(),
            changes: vec![Alter::Drop("_record_id".to_string())],
        }
    );
    // A name that merely contains it is a name like any other.
    assert_eq!(
        columns("CREATE TABLE t (user_id UINT(32))"),
        [("user_id".to_string(), "int", 32, None)]
    );
}

/// `IF NOT EXISTS`, which is about the fields as much as about the table.
#[test]
fn if_not_exists_is_part_of_the_statement() {
    let Ddl::CreateTable { database: None, table, if_not_exists, columns, .. } =
        ddl("CREATE TABLE IF NOT EXISTS events (a SET)")
    else {
        panic!("a CREATE is a CreateTable")
    };
    assert_eq!(table, "events");
    assert!(if_not_exists);
    assert_eq!(columns.len(), 1);
    // `IF EXISTS` on a create and `IF NOT EXISTS` on a drop are each the other statement's
    // clause, and neither is read as the one that was meant.
    assert_eq!(code("CREATE TABLE IF EXISTS events (a SET)"), "parse_error");
    assert_eq!(code("DROP TABLE IF NOT EXISTS events"), "parse_error");
}
