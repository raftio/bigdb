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

//! **Roles, through the binaries an operator actually runs.**
//!
//! Everything below this layer proves a piece: `big-rbac` the decision, `big-embed` the
//! administration, `big-http` the request path. What none of them can prove is the thing an
//! operator does on their first day - a users file written with `big passwd`, a server started
//! against it, and a `GRANT` typed into `bigctl` - because that path crosses two binaries and a
//! file on disk.
//!
//! **The drill this file exists for is the lockout.** A users file names roles a fresh catalog
//! has never heard of, so on the day this feature lands every existing deployment holds nothing.
//! The way out is `superuser`, and if that does not work there is no way out at all.

use super::common::{credentials_file, until, users_file, Workspace};

/// The recovery procedure, run start to finish.
///
/// This is the one test in the tree that would catch a lockout, and every step of it is a step
/// an operator would type:
///
/// 1. a users file naming a role nobody has created - which is where an upgrade leaves everybody
/// 2. that credential holds nothing, and the server keeps answering rather than refusing to start
/// 3. `superuser` gets in without a single grant behind it
/// 4. it creates the role the file already names, and grants it something
/// 5. the original credential works, with no restart
#[test]
fn a_deployment_whose_roles_do_not_exist_yet_recovers_through_the_reserved_role() {
    let workspace = Workspace::new();
    // `analyst` is a name, not a rank. No catalog has it yet.
    let users = users_file(workspace.path(), "alice analyst\nroot superuser\n");
    let daemon = workspace.daemon(&["--users", &users.display().to_string()]);
    until("the daemon to report its users", || daemon.log().contains("users loaded"));

    let held = tempfile::tempdir().unwrap();
    let alice = credentials_file(held.path(), "alice");
    let root = credentials_file(held.path(), "root");
    let as_alice = |args: &[&str]| {
        let mut all = vec!["--credentials-file", alice.to_str().unwrap()];
        all.extend_from_slice(args);
        daemon.bigctl(&all)
    };
    let as_root = |args: &[&str]| {
        let mut all = vec!["--credentials-file", root.to_str().unwrap()];
        all.extend_from_slice(args);
        daemon.bigctl(&all)
    };

    // 1-2. The password is right and the role resolves to nothing, so she is refused the data
    // rather than refused the door - and the server started at all, which is the whole point.
    as_root(&["sql", "CREATE TABLE tx"]).expect(0);
    let refused = as_alice(&["sql", "SELECT count(*) FROM tx"]);
    assert_ne!(refused.code, 0, "a role nobody made holds nothing: {}", refused.out);

    // 3-4. The reserved role holds everything without a grant behind it, which is what breaks
    // the circle: making the first role needs a privilege, and only this role has one already.
    as_root(&["sql", "CREATE ROLE analyst"]).expect(0);
    as_root(&["sql", "GRANT SELECT ON default.* TO analyst"]).expect(0);

    // 5. And the credential that was locked out a moment ago works, with nothing restarted.
    let allowed = as_alice(&["sql", "SELECT count(*) FROM tx"]);
    assert_eq!(allowed.code, 0, "the grant is live: {} {}", allowed.out, allowed.err);
}

/// A grant reaches exactly the objects it names, checked through the real client rather than
/// against an in-process resolver.
#[test]
fn a_grant_reaches_one_database_and_not_the_other() {
    let workspace = Workspace::new();
    let users = users_file(workspace.path(), "alice analyst\nroot superuser\n");
    let daemon = workspace.daemon(&["--users", &users.display().to_string()]);
    until("the daemon to report its users", || daemon.log().contains("users loaded"));

    let held = tempfile::tempdir().unwrap();
    let alice = credentials_file(held.path(), "alice");
    let root = credentials_file(held.path(), "root");
    let as_alice = |args: &[&str]| {
        let mut all = vec!["--credentials-file", alice.to_str().unwrap()];
        all.extend_from_slice(args);
        daemon.bigctl(&all)
    };
    let as_root = |args: &[&str]| {
        let mut all = vec!["--credentials-file", root.to_str().unwrap()];
        all.extend_from_slice(args);
        daemon.bigctl(&all)
    };

    for statement in [
        "CREATE DATABASE sales",
        "CREATE DATABASE ops",
        "CREATE TABLE sales.orders",
        "CREATE TABLE ops.ledger",
        "CREATE ROLE analyst",
        "GRANT SELECT ON sales.* TO analyst",
    ] {
        as_root(&["sql", statement]).expect(0);
    }

    assert_eq!(as_alice(&["sql", "SELECT count(*) FROM sales.orders"]).code, 0);
    let refused = as_alice(&["sql", "SELECT count(*) FROM ops.ledger"]);
    assert_ne!(refused.code, 0, "the other database was never granted: {}", refused.out);

    // A privilege is per verb: reading is not most of the way to writing.
    let refused = as_alice(&["sql", "DROP TABLE sales.orders"]);
    assert_ne!(refused.code, 0, "SELECT is not DROP: {}", refused.out);
}

/// `REVOKE` takes effect on the next statement, which is what makes a grant worth putting in the
/// catalog rather than beside the passwords.
#[test]
fn a_revoke_takes_effect_without_a_restart() {
    let workspace = Workspace::new();
    let users = users_file(workspace.path(), "alice analyst\nroot superuser\n");
    let daemon = workspace.daemon(&["--users", &users.display().to_string()]);
    until("the daemon to report its users", || daemon.log().contains("users loaded"));

    let held = tempfile::tempdir().unwrap();
    let alice = credentials_file(held.path(), "alice");
    let root = credentials_file(held.path(), "root");
    let as_alice = |args: &[&str]| {
        let mut all = vec!["--credentials-file", alice.to_str().unwrap()];
        all.extend_from_slice(args);
        daemon.bigctl(&all)
    };
    let as_root = |args: &[&str]| {
        let mut all = vec!["--credentials-file", root.to_str().unwrap()];
        all.extend_from_slice(args);
        daemon.bigctl(&all)
    };

    as_root(&["sql", "CREATE TABLE tx"]).expect(0);
    as_root(&["sql", "CREATE ROLE analyst"]).expect(0);
    as_root(&["sql", "GRANT SELECT ON default.* TO analyst"]).expect(0);
    assert_eq!(as_alice(&["sql", "SELECT count(*) FROM tx"]).code, 0);

    as_root(&["sql", "REVOKE SELECT ON default.* FROM analyst"]).expect(0);
    let refused = as_alice(&["sql", "SELECT count(*) FROM tx"]);
    assert_ne!(refused.code, 0, "the revoke is live too: {}", refused.out);
}

/// `SHOW ROLES` lists the reserved role without it ever having been stored, and `SHOW GRANTS`
/// answers about the caller's own without needing a privilege to ask.
#[test]
fn the_listings_answer_what_they_promise() {
    let workspace = Workspace::new();
    let users = users_file(workspace.path(), "alice analyst\nroot superuser\n");
    let daemon = workspace.daemon(&["--users", &users.display().to_string()]);
    until("the daemon to report its users", || daemon.log().contains("users loaded"));

    let held = tempfile::tempdir().unwrap();
    let alice = credentials_file(held.path(), "alice");
    let root = credentials_file(held.path(), "root");
    let as_alice = |args: &[&str]| {
        let mut all = vec!["--credentials-file", alice.to_str().unwrap()];
        all.extend_from_slice(args);
        daemon.bigctl(&all)
    };
    let as_root = |args: &[&str]| {
        let mut all = vec!["--credentials-file", root.to_str().unwrap()];
        all.extend_from_slice(args);
        daemon.bigctl(&all)
    };

    as_root(&["sql", "CREATE ROLE analyst"]).expect(0);
    as_root(&["sql", "GRANT SELECT ON default.* TO analyst"]).expect(0);

    let roles = as_root(&["sql", "SHOW ROLES"]).expect(0);
    assert!(roles.out.contains("superuser"), "listed without being stored: {}", roles.out);
    assert!(roles.out.contains("analyst"), "{}", roles.out);

    // Her own grants, which she may read without holding `ROLES`.
    let mine = as_alice(&["sql", "SHOW GRANTS"]).expect(0);
    assert!(mine.out.contains("SELECT"), "{}", mine.out);

    // Somebody else's is administrative, and she does not hold it.
    let refused = as_alice(&["sql", "SHOW GRANTS FOR analyst"]);
    assert_ne!(refused.code, 0, "reading another role's grants needs ROLES: {}", refused.out);
}

/// **`CREATE USER` is refused by name, with what to do instead.**
///
/// The decision that people are not stored in the database, made visible at the surface an
/// operator types into rather than left in a doc comment.
#[test]
fn making_a_person_is_refused_and_says_where_people_live() {
    let workspace = Workspace::new();
    let users = users_file(workspace.path(), "root superuser\n");
    let daemon = workspace.daemon(&["--users", &users.display().to_string()]);
    until("the daemon to report its users", || daemon.log().contains("users loaded"));

    let held = tempfile::tempdir().unwrap();
    let root = credentials_file(held.path(), "root");
    let refused = daemon.bigctl(&[
        "--credentials-file",
        root.to_str().unwrap(),
        "sql",
        "CREATE USER bob IDENTIFIED BY 'x'",
    ]);
    assert_ne!(refused.code, 0);
    let said = format!("{}{}", refused.out, refused.err);
    assert!(said.contains("big passwd"), "it names the tool that does make one: {said}");
}
