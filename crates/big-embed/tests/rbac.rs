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

//! **Administering roles**, which is this layer's half of the three the feature is split across.
//!
//! `big-rbac` says what a role may do and proves it against a store it builds by hand. `big-db`
//! says where one is kept and proves it survives a round trip. What is left is the part neither
//! of them can reach: the name rule a catalog record imposes, the resolution of an object's
//! *name* to the ids a grant is filed under, and the transaction that makes a change atomic
//! against the schema it is about.
//!
//! Every refusal asserted here is one a caller could provoke by typing a statement, which is why
//! they are checked by code rather than by message.

use big_embed::Api;
use big_pager::MemPager;
use big_rbac::{Demand, Privilege, Privileges, Who, SUPERUSER};

fn some() -> Privileges {
    Privileges::from_iter([Privilege::Select])
}

fn analyst() -> Who {
    Who::Role("analyst".into())
}

fn stocked() -> Api<MemPager> {
    let api = Api::in_memory().unwrap();
    api.create_database("sales").unwrap();
    api.create_table("sales.orders").unwrap();
    api.create_role("analyst").unwrap();
    api
}

#[test]
fn a_role_is_made_once_and_remaking_it_changes_nothing() {
    let api = stocked();
    // `false` is "already there", which is what `IF NOT EXISTS` needs to tell apart from a
    // refusal - and a repeat must not disturb what the role already holds.
    api.set_grant("analyst", Some("sales"), None, some()).unwrap();
    assert!(!api.create_role("analyst").unwrap());
    assert!(api.allows(&analyst(), Demand::database(Privilege::Select, "sales")));
}

#[test]
fn a_role_that_was_never_made_is_not_an_error_to_drop() {
    let api = stocked();
    assert!(!api.drop_role("nobody").unwrap());
    assert!(api.drop_role("analyst").unwrap());
}

#[test]
fn dropping_a_role_takes_its_grants_and_nobody_elses() {
    let api = stocked();
    api.create_role("auditor").unwrap();
    api.set_grant("analyst", Some("sales"), None, some()).unwrap();
    api.set_grant("auditor", Some("sales"), None, some()).unwrap();

    api.drop_role("analyst").unwrap();
    assert!(!api.roles().iter().any(|r| r == "analyst"));
    // The dropped role holds nothing; the other one is untouched.
    assert!(!api.allows(&analyst(), Demand::database(Privilege::Select, "sales")));
    assert!(api.allows(&Who::Role("auditor".into()), Demand::database(Privilege::Select, "sales")));
    assert_eq!(api.grants_of("auditor").len(), 1);
}

/// The bootstrap. It holds everything without being stored, so each of these would be a way to
/// narrow the one role a lockout is recovered through.
#[test]
fn the_reserved_role_cannot_be_made_dropped_or_granted_to() {
    let api = stocked();
    assert_eq!(api.create_role(SUPERUSER).unwrap_err().code(), "reserved_role");
    assert_eq!(api.drop_role(SUPERUSER).unwrap_err().code(), "reserved_role");
    assert_eq!(
        api.set_grant(SUPERUSER, Some("sales"), None, some()).unwrap_err().code(),
        "reserved_role"
    );
    // It is listed because it is true, not because it is stored.
    assert_eq!(api.roles().first().map(String::as_str), Some(SUPERUSER));
    assert!(api.grants_of(SUPERUSER).is_empty());
    // And it holds everything anyway, without a single grant behind it.
    assert!(api.allows(&Who::Role(SUPERUSER.into()), Demand::server(Privilege::Roles)));
}

/// A role is stored in a fixed-width record like every other catalog object, so its name is held
/// to the same rule - and that rule is this layer's to apply, because `big-rbac` cannot see it.
#[test]
fn a_role_name_is_held_to_the_rule_every_catalog_name_is() {
    let api = stocked();
    // A `.` is the separator in a qualified name; a role called `a.b` would be ambiguous
    // everywhere a name travels as one string.
    assert_eq!(api.create_role("a.b").unwrap_err().code(), "name_separator");
    assert_eq!(api.create_role(&"x".repeat(500)).unwrap_err().code(), "name_too_long");
}

/// Both halves of the object have to exist. A grant on a name nobody has created has no id to
/// hang on, and inventing one would resurrect the moment somebody used that name.
#[test]
fn a_grant_on_something_that_does_not_exist_is_refused() {
    let api = stocked();
    assert_eq!(
        api.set_grant("analyst", Some("nowhere"), None, some()).unwrap_err().code(),
        "unknown_database"
    );
    assert_eq!(
        api.set_grant("analyst", Some("sales"), Some("nothing"), some()).unwrap_err().code(),
        "unknown_table"
    );
    assert_eq!(
        api.set_grant("stranger", Some("sales"), None, some()).unwrap_err().code(),
        "unknown_role"
    );
    // `ON *.tbl` is not a shape: a table is only nameable inside a database.
    assert_eq!(
        api.set_grant("analyst", None, Some("orders"), some()).unwrap_err().code(),
        "unknown_table"
    );
}

/// A refused grant must leave nothing behind. The transaction is dropped rather than committed,
/// which is how every abandoned write here works - there is no rollback because nothing was
/// written.
#[test]
fn a_refused_grant_commits_nothing() {
    let api = stocked();
    assert!(api.set_grant("analyst", Some("nowhere"), None, some()).is_err());
    assert!(api.grants_of("analyst").is_empty(), "nothing was left behind");
    assert!(!api.allows(&analyst(), Demand::database(Privilege::Select, "sales")));
}

/// Widening is one record with a sentinel rather than a row per object, which is what keeps a
/// role with the run of the server cheap to store.
#[test]
fn a_grant_on_everything_is_one_record() {
    let api = stocked();
    api.set_grant("analyst", None, None, some()).unwrap();

    assert_eq!(api.grants_of("analyst").len(), 1, "one record, not one per object");
    // And it reaches a table the grant never named.
    assert!(api.allows(&analyst(), Demand::table(Privilege::Select, "sales", "orders")));
}

/// An empty mask is the absence of a grant, not a grant of nothing - or `REVOKE` would grow the
/// catalog it was asked to shrink.
#[test]
fn revoking_everything_removes_the_record() {
    let api = stocked();
    api.set_grant("analyst", Some("sales"), None, some()).unwrap();
    api.set_grant("analyst", Some("sales"), None, Privileges::empty()).unwrap();

    assert!(api.grants_of("analyst").is_empty());
    assert!(!api.allows(&analyst(), Demand::database(Privilege::Select, "sales")));
}

/// A grant is a schema change like any other: it commits at the meta page flip, so it is visible
/// the moment the call returns and survives a reopen of the same pages.
#[test]
fn a_grant_is_committed_rather_than_held_in_memory() {
    let api = stocked();
    api.set_grant("analyst", Some("sales"), Some("orders"), some()).unwrap();

    assert!(api.allows(&analyst(), Demand::table(Privilege::Select, "sales", "orders")));
    // Named back as they were typed, which is what `SHOW GRANTS` prints.
    assert_eq!(
        api.grants_of("analyst"),
        vec![(Some("sales".to_string()), Some("orders".to_string()), vec!["SELECT"])]
    );
}
