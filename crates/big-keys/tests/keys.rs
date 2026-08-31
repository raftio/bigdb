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

use big_keys::*;
use proptest::prelude::*;

#[test]
fn interning_is_stable_and_scoped_per_field() {
    let mut k = KeyStore::new();
    assert_eq!(k.intern(1, 1, "vn").unwrap(), 0);
    assert_eq!(k.intern(1, 1, "jp").unwrap(), 1);
    assert_eq!(k.intern(1, 1, "vn").unwrap(), 0, "same key, same row");

    // A different field has its own row space.
    assert_eq!(k.intern(1, 2, "vn").unwrap(), 0);
    assert_eq!(k.id(1, 1, "jp"), Some(1));
    assert_eq!(k.id(1, 2, "jp"), None);
    assert_eq!(k.name(1, 1, 1), Some("jp"));
}

#[test]
fn an_overlong_key_is_refused_not_truncated() {
    let mut k = KeyStore::new();
    let long = "x".repeat(MAX_KEY_LEN + 1);
    assert_eq!(k.intern(1, 1, &long), Err(KeyError::TooLong { len: MAX_KEY_LEN + 1 }));
    assert!(k.intern(1, 1, &"x".repeat(MAX_KEY_LEN)).is_ok());
}

#[test]
fn round_trips_through_catalog_records() {
    let mut k = KeyStore::new();
    for (t, f, n) in [(1u32, 1u32, "a"), (1, 1, "b"), (2, 7, "c")] {
        k.intern(t, f, n).unwrap();
    }
    let back = KeyStore::from_entries(&k.encode());

    assert_eq!(back.len(), 3);
    assert_eq!(back.id(1, 1, "b"), Some(1));
    assert_eq!(back.id(2, 7, "c"), Some(0));
    assert_eq!(back.rows(1, 1).collect::<Vec<_>>(), vec![(0, "a"), (1, "b")]);
}

/// Ids are never reused, so reloading and adding more must not collide with existing rows.
#[test]
fn reloading_continues_the_id_sequence() {
    let mut k = KeyStore::new();
    k.intern(1, 1, "a").unwrap();
    k.intern(1, 1, "b").unwrap();

    let mut back = KeyStore::from_entries(&k.encode());
    assert_eq!(back.intern(1, 1, "c").unwrap(), 2, "must not restart at 0");
    assert_eq!(back.id(1, 1, "a"), Some(0));
}

#[test]
fn records_of_other_kinds_are_ignored() {
    let mut k = KeyStore::new();
    k.intern(1, 1, "a").unwrap();
    let mut entries = k.encode();
    let mut foreign = vec![0u8; entries[0].len()];
    foreign[0] = 99;
    entries.push(foreign);

    assert_eq!(KeyStore::from_entries(&entries).len(), 1);
}

proptest! {
    #[test]
    fn every_key_survives_a_round_trip(
        names in proptest::collection::btree_set("[a-z]{1,20}", 0..80)
    ) {
        let mut k = KeyStore::new();
        for n in &names {
            k.intern(3, 4, n).unwrap();
        }
        let back = KeyStore::from_entries(&k.encode());
        for n in &names {
            prop_assert_eq!(back.id(3, 4, n), k.id(3, 4, n));
            let id = back.id(3, 4, n).unwrap();
            prop_assert_eq!(back.name(3, 4, id), Some(n.as_str()));
        }
    }
}
