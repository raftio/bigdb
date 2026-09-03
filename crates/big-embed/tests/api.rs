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

//! The facade end to end, including through a real file.

use big_db::catalog::FieldKind;
use big_embed::*;
use big_exec::Value;

fn stocked() -> Api<big_pager::MemPager> {
    let api = Api::in_memory().unwrap();
    api.create_table("tx").unwrap();
    api.create_field("tx", "amount", FieldKind::Int, 32).unwrap();
    api.create_field("tx", "country", FieldKind::Set, 0).unwrap();
    api.import(
        "tx",
        &[
            Fact::Int { field: "amount", record: 1, value: 100 },
            Fact::Key { field: "country", record: 1, value: "GB" },
            Fact::Int { field: "amount", record: 2, value: 900 },
            Fact::Key { field: "country", record: 2, value: "US" },
        ],
    )
    .unwrap();
    api
}

#[test]
fn a_batch_lands_and_can_be_queried_back() {
    let api = stocked();
    let Value::Count(n) = api.query("tx", r#"Count(Row(country="GB"))"#).unwrap() else {
        panic!("expected a count")
    };
    assert_eq!(n, 1);
    assert_eq!(api.query("tx", r#"Sum(All(), field="amount")"#).unwrap().as_sum(), Some(1000));
}

/// One transaction for the whole batch: a fact the engine refuses must take the rest with it,
/// or a caller has no way to know what actually landed.
#[test]
fn a_batch_that_fails_partway_lands_nothing() {
    let api = stocked();
    let before = api.query("tx", "Count(All())").unwrap().as_count();

    let err = api.import(
        "tx",
        &[
            Fact::Int { field: "amount", record: 9, value: 1 },
            // A key into an integer field: refused, and the record before it must not survive.
            Fact::Key { field: "amount", record: 9, value: "nope" },
        ],
    );
    assert!(err.is_err());
    assert_eq!(api.query("tx", "Count(All())").unwrap().as_count(), before);
}

#[test]
fn the_schema_comes_back_as_owned_values() {
    let api = stocked();
    let schema = api.schema();
    assert_eq!(schema.len(), 1);
    assert_eq!(schema[0].name, "tx");

    let names: Vec<&str> = schema[0].fields.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names, vec!["amount", "country"]);
    assert_eq!(schema[0].fields[0].kind, FieldKind::Int);
}

#[test]
fn a_bad_query_is_an_error_not_a_panic() {
    let api = stocked();
    assert!(api.query("tx", "Row(nope > 1)").is_err());
    assert!(api.query("tx", "Row(amount >").is_err());
    assert!(api.query("no_such_table", "All()").is_err());
}

#[cfg(unix)]
#[test]
fn a_file_backed_database_survives_being_reopened() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("api.big");
    {
        let api = Api::open(&path).unwrap();
        api.create_table("tx").unwrap();
        api.create_field("tx", "amount", FieldKind::Int, 32).unwrap();
        api.import("tx", &[Fact::Int { field: "amount", record: 7, value: 42 }]).unwrap();
    }

    let api = Api::open(&path).unwrap();
    assert_eq!(api.query("tx", r#"Sum(All(), field="amount")"#).unwrap().as_sum(), Some(42));
    assert_eq!(api.schema()[0].fields[0].name, "amount");
}

/// The engine takes an exclusive lock on the file, so this is a clear error rather than two
/// writers quietly disagreeing.
#[cfg(unix)]
#[test]
fn a_second_handle_on_one_file_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("locked.big");
    let _first = Api::open(&path).unwrap();
    assert!(Api::open(&path).is_err());
}

/// The row-key ceiling, seen from the facade.
///
/// It is here rather than only in `big-keys` because the number an operator sets is set on a
/// database, and what they need to know is that reaching it refuses a *write* rather than
/// corrupting one - and that everything already written stays readable.
#[test]
fn a_row_key_ceiling_refuses_new_keys_and_leaves_the_old_ones_readable() {
    let api = Api::in_memory().unwrap();
    api.create_table("tx").unwrap();
    api.create_field("tx", "country", FieldKind::Set, 0).unwrap();

    api.import("tx", &[Fact::Key { field: "country", record: 1, value: "GB" }]).unwrap();
    api.set_key_limit(Some(1));

    let stats = api.key_stats();
    assert_eq!(stats.count, 1);
    assert_eq!(stats.limit, Some(1));
    assert!(stats.resident_bytes > 0);

    // A second distinct key is refused.
    let refused = api.import("tx", &[Fact::Key { field: "country", record: 2, value: "US" }]);
    assert!(refused.is_err(), "the ceiling let a second key through");

    // The one that exists still works, for reading and for writing more records against it.
    api.import("tx", &[Fact::Key { field: "country", record: 3, value: "GB" }]).unwrap();
    assert_eq!(api.key_stats().count, 1);

    // Lifting the ceiling lets the dictionary grow again: it refuses, it does not poison.
    api.set_key_limit(None);
    api.import("tx", &[Fact::Key { field: "country", record: 4, value: "US" }]).unwrap();
    assert_eq!(api.key_stats().count, 2);
}
