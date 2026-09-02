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

//! The catalog as rows, and a written value as the fact its field makes it.
//!
//! Both are pure functions over a schema, which is the point of them being functions: no pager,
//! no socket, and a table of cases instead of a fixture. What they answer is what `DESCRIBE`,
//! `SHOW` and `INSERT` answer, so the claims here are the ones those statements rest on.

use big_embed::fact::{from_literal, from_text, ValueError};
use big_embed::{introspect, Datum, Fact, FieldInfo, FieldKind, TableEngine, TableInfo};
use big_db::Granularity;
use big_plan::Literal;

fn field(name: &str, kind: FieldKind, bit_depth: u32, scale: i8) -> FieldInfo {
    FieldInfo { name: name.to_string(), kind, bit_depth, scale, granularity: Vec::new() }
}

fn schema() -> Vec<TableInfo> {
    vec![
        TableInfo {
            database: "default".to_string(),
            name: "tx".to_string(),
            engine: TableEngine::default(),
            fields: vec![
                field("n", FieldKind::Int, 32, 0),
                field("price", FieldKind::Decimal, 34, 2),
                field("country", FieldKind::Set, 0, 0),
                FieldInfo {
                    granularity: vec![Granularity::Day],
                    ..field("visit", FieldKind::TimeQuantum, 0, 0)
                },
            ],
        },
        TableInfo {
            database: "default".to_string(),
            name: "empty".to_string(),
            engine: TableEngine::default(),
            fields: Vec::new(),
        },
    ]
}

/// One row per field, and the two numbers that are absent rather than zero.
#[test]
fn describing_a_table_is_one_row_per_field() {
    let set = introspect::describe(&schema(), &[], "tx").unwrap();
    assert_eq!(set.columns, ["name", "kind", "bit_depth", "scale", "granularity"]);
    assert_eq!(set.rows.len(), 4);
    assert_eq!(
        set.rows[0],
        [
            Datum::Text("n".to_string()),
            Datum::Text("int".to_string()),
            Datum::Int(32),
            // A field that stores no decimal has no scale, and saying `0` would be saying it
            // has one that happens to be zero.
            Datum::Null,
            Datum::Null,
        ]
    );
    assert_eq!(set.rows[1][3], Datum::Int(2), "a decimal reports the scale it stores");
    assert_eq!(set.rows[3][4], Datum::Keys(vec!["D".to_string()]), "views by day");

    // A table with no fields is a real thing here, and describes as no rows rather than an
    // error - `POST /table/{t}` creates exactly that.
    assert!(introspect::describe(&schema(), &[], "empty").unwrap().rows.is_empty());
    assert_eq!(introspect::describe(&schema(), &[], "nope").unwrap_err().code(), "unknown_table");
    assert_eq!(
        introspect::show_create(&schema(), &[], "nope", false).unwrap_err().code(),
        "unknown_table"
    );
}

/// The listing, and the statement that recreates one table.
#[test]
fn the_catalog_lists_itself_and_writes_itself_back_out() {
    let set = introspect::show_tables(&schema(), &[], None);
    // `type` says `BASE TABLE` or `VIEW`, which is what a JDBC driver asks for. Tables and
    // views share one namespace, so they share one sorted listing - `empty` sorts before `tx`.
    assert_eq!(set.columns, ["name", "type", "engine", "fields"]);
    assert_eq!(set.rows[0][0], Datum::Text("empty".to_string()));
    assert_eq!(set.rows[0][1], Datum::Text("BASE TABLE".to_string()));
    assert_eq!(set.rows[0][3], Datum::Int(0));
    assert_eq!(set.rows[1][0], Datum::Text("tx".to_string()));
    assert_eq!(set.rows[1][3], Datum::Int(4));

    let set = introspect::show_create(&schema(), &[], "tx", false).unwrap();
    assert_eq!(set.columns, ["statement"]);
    let Datum::Text(statement) = &set.rows[0][0] else { panic!("a statement is text") };
    // The native spelling of each kind, because that is the field that exists - `TEXT` and
    // `SET` create one thing, and a schema answers with the thing.
    for expected in [
        "CREATE TABLE tx",
        "n UINT(32)",
        "price DECIMAL(10, 2)",
        "country SET",
        "visit TIMEQUANTUM",
    ] {
        assert!(statement.contains(expected), "`{expected}` missing from `{statement}`");
    }
    // A table with no fields renders without a column list, which is the statement that
    // creates it.
    let set = introspect::show_create(&schema(), &[], "empty", false).unwrap();
    let Datum::Text(statement) = &set.rows[0][0] else { panic!("a statement is text") };
    assert!(!statement.contains('('), "{statement}");
}

/// **The two write paths read a value the same way.**
///
/// One function per spelling, one table of meanings between them: an import line and a SQL
/// literal that say the same thing must become the same fact, or one table would hold two
/// conventions.
#[test]
fn a_line_and_a_literal_that_say_the_same_thing_become_the_same_fact() {
    let cases: [(FieldInfo, &str, Literal); 5] = [
        (field("n", FieldKind::Int, 32, 0), "100", Literal::Int(100)),
        (field("b", FieldKind::SignedInt, 32, 0), "-5", Literal::Sint(-5)),
        (field("f", FieldKind::Bool, 0, 0), "true", Literal::Bool(true)),
        (field("c", FieldKind::Set, 0, 0), "GB", Literal::Str("GB".to_string())),
        (
            field("v", FieldKind::TimeQuantum, 0, 0),
            "home@1750000000",
            Literal::Str("home@1750000000".to_string()),
        ),
    ];
    for (info, text, literal) in &cases {
        let from_line = from_text(&info.name, info, 1, text).unwrap();
        let from_sql = from_literal(&info.name, info, 1, literal).unwrap();
        assert_eq!(from_line, from_sql, "`{text}` and `{literal:?}` on a {:?} field", info.kind);
    }

    // **The one place they differ, and it is the one place they have to.** An import line
    // carries no scale and has always sent the units a decimal stores; a literal carries its
    // own, so `12.50` is scaled by the same conversion `WHERE price >= 12.50` is scaled by.
    let price = field("price", FieldKind::Decimal, 34, 2);
    let stored = Fact::Int { field: "price", record: 1, value: 1250 };
    assert_eq!(from_text("price", &price, 1, "1250").unwrap(), stored);
    assert_eq!(
        from_literal("price", &price, 1, &Literal::Dec { units: 1250, scale: 2 }).unwrap(),
        stored
    );
    // A whole number written into a decimal field means whole units of the value, not of the
    // storage: `12` is 1200, exactly as `WHERE price = 12` asks about 1200.
    assert_eq!(
        from_literal("price", &price, 1, &Literal::Int(12)).unwrap(),
        Fact::Int { field: "price", record: 1, value: 1200 }
    );
}

/// What each kind refuses, and the sentence it refuses with.
#[test]
fn a_value_a_field_cannot_hold_says_what_the_field_wanted() {
    let n = field("n", FieldKind::Int, 32, 0);
    assert_eq!(from_text("n", &n, 1, "five"), Err(ValueError::NeedsNumber));
    assert_eq!(
        from_literal("n", &n, 1, &Literal::Str("five".to_string())),
        Err(ValueError::NeedsNumber)
    );
    // An unsigned field is not a signed one: `-1` is a mistake worth naming rather than a very
    // large number.
    assert_eq!(from_literal("n", &n, 1, &Literal::Sint(-1)), Err(ValueError::NeedsNumber));

    let b = field("b", FieldKind::SignedInt, 32, 0);
    assert_eq!(from_text("b", &b, 1, "x"), Err(ValueError::NeedsSignedNumber));
    let f = field("f", FieldKind::Bool, 0, 0);
    assert_eq!(from_text("f", &f, 1, "yes"), Err(ValueError::NeedsBool));
    assert_eq!(from_literal("f", &f, 1, &Literal::Int(1)), Err(ValueError::NeedsBool));
    let v = field("v", FieldKind::TimeQuantum, 0, 0);
    assert_eq!(from_text("v", &v, 1, "home@soon"), Err(ValueError::NeedsSeconds));
    let c = field("c", FieldKind::Set, 0, 0);
    assert_eq!(from_literal("c", &c, 1, &Literal::Int(1)), Err(ValueError::NeedsKey));
    // **More digits after the point than the field keeps is refused, never rounded.** Silently
    // dropping a digit would answer a question nobody asked, and both numbers would be valid.
    let price = field("price", FieldKind::Decimal, 34, 2);
    let too_precise = from_literal("price", &price, 1, &Literal::Dec { units: 12505, scale: 3 });
    assert_eq!(too_precise, Err(ValueError::TooPrecise { written: 3, scale: 2 }));
    // And it is reported as the planner's own refusal, because it is the same mistake
    // `WHERE price = 12.505` makes: one code and one sentence, whichever half wrote the number.
    let e = too_precise.unwrap_err().into_error("price", "12.505");
    assert_eq!(e.code(), "too_precise");
    assert_eq!(e.to_string(), "`price` stores 2 decimal places, but the value has 3");

    // The sentence names the field and what it was given, and it is the same sentence whichever
    // spelling the value arrived in.
    assert_eq!(ValueError::NeedsNumber.why("n", "five"), "`n` needs a number, got `five`");
    assert_eq!(big_embed::fact::written(&Literal::Dec { units: 1250, scale: 2 }), "12.50");
    assert_eq!(big_embed::fact::written(&Literal::Dec { units: 5, scale: 3 }), "0.005");
    assert_eq!(big_embed::fact::written(&Literal::Sint(-5)), "-5");
}
