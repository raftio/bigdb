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

//! Every error an operator can be shown, checked as text.
//!
//! `Display` used to be `{self:?}` everywhere, so nothing about these messages was ever
//! asserted and a broken one would only be noticed by whoever was already having a bad day.
//! These are cheap to keep honest, so they are kept honest.

use big_db::*;

/// Every variant, so adding one without a message is a compile error here first.
fn every_error() -> Vec<DbError> {
    vec![
        DbError::Store(big_pager::StoreError::Locked),
        DbError::Store(big_pager::StoreError::NoValidMeta),
        DbError::Store(big_pager::StoreError::MapSizeExhausted { need: 9, mapsize: 4 }),
        DbError::Store(big_pager::StoreError::Page(big_pager::PageError::UnsupportedVersion(9))),
        DbError::Store(big_pager::StoreError::Page(big_pager::PageError::ChecksumMismatch {
            stored: 1,
            computed: 2,
        })),
        DbError::Field(big_engine::bitmap::field::FieldError::ValueTooWide {
            value: 9,
            bit_depth: 2,
        }),
        DbError::Key(big_keys::KeyError::TooLong { len: 500 }),
        DbError::Tree(big_btree::BTreeError::TooDeep { root: 3 }),
        DbError::UnknownTable("tx".into()),
        DbError::UnknownField { table: "tx".into(), field: "amount".into() },
        DbError::WrongFieldKind { field: "amount".into(), expected: "int" },
        DbError::NameTooLong { name: "x".into(), max: 104 },
        DbError::NameTaken("tx".into()),
        DbError::BackupDestinationExists("/tmp/x.big".into()),
        DbError::BackupDestinationNotEmpty,
        DbError::QueryTooLarge { limit: 10, needed: 500, unit: "records" },
        DbError::FieldRedefined { table: "tx".into(), field: "amount".into() },
        DbError::QueryTimeout { elapsed_ms: 900, limit_ms: 500 },
        DbError::QueryCancelled,
        DbError::UnknownFieldKind { table: 1, field: 2, kind: 200 },
        DbError::SignedValueOutOfRange { value: 900, min: -128, max: 127 },
    ]
}

#[test]
fn no_error_message_is_a_debug_dump() {
    for e in every_error() {
        let text = e.to_string();
        assert!(!text.is_empty(), "{e:?} has no message");
        // A debug dump gives itself away by the variant name and its struct punctuation.
        assert!(
            !text.contains('{') && !text.contains(" { "),
            "{e:?} still prints its debug form: {text}"
        );
        assert!(
            text.chars().next().is_some_and(|c| c.is_lowercase() || c.is_ascii_digit() || c == '/'),
            "{e:?} reads like a type name rather than a sentence: {text}"
        );
    }
}

#[test]
fn no_error_message_has_mangled_whitespace() {
    // A `\` line continuation in a Rust string keeps the leading whitespace of the next line
    // unless the source lines up exactly, and the result is a message with a gap in it. This
    // has already happened once.
    for e in every_error() {
        let text = e.to_string();
        assert!(!text.contains("  "), "{e:?} has a run of spaces: {text:?}");
        assert!(!text.contains('\n'), "{e:?} spans lines: {text:?}");
        assert_eq!(text.trim(), text, "{e:?} has stray edge whitespace: {text:?}");
    }
}

#[test]
fn the_messages_that_carry_an_action_say_what_it_is() {
    let version =
        DbError::Store(big_pager::StoreError::Page(big_pager::PageError::UnsupportedVersion(9)))
            .to_string();
    assert!(version.contains("migrat"), "a version mismatch must point at the tool: {version}");

    let damaged = DbError::Store(big_pager::StoreError::NoValidMeta).to_string();
    assert!(damaged.contains("backup"), "a damaged file must point at the backup: {damaged}");

    let locked = DbError::Store(big_pager::StoreError::Locked).to_string();
    assert!(locked.contains("process"), "a lock refusal must say who is holding it: {locked}");
}
