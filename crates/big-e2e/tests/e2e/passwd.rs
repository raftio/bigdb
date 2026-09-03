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

//! `big passwd`, which is the only thing that writes a users file.
//!
//! Spawned rather than linked, like everything else here - and here it matters more than usual,
//! because half of what this command is about is what it does with a terminal. Standard input
//! being a pipe is exactly the path a provisioning script takes.

use super::common::{run_with_stdin, set_mode};
use std::path::Path;

/// `big passwd <file> <args…>` with `password` on standard input.
fn passwd(file: &Path, args: &[&str], password: &str) -> super::common::Run {
    let mut all = vec![String::from("passwd"), file.display().to_string()];
    all.extend(args.iter().map(|a| (*a).to_string()));
    let refs: Vec<&str> = all.iter().map(String::as_str).collect();
    run_with_stdin("big", &refs, password)
}

fn field(line: &str, n: usize) -> &str {
    line.split_whitespace().nth(n).unwrap_or("")
}

#[test]
fn a_user_is_created_with_a_hash_the_server_can_read() {
    let dir = tempfile::tempdir().unwrap();
    let users = dir.path().join("users");
    let run = passwd(&users, &["set", "alice", "--role", "admin"], "s3cret\n");
    assert_eq!(run.code, 0, "{}", run.err);
    assert!(run.said("created"), "it says the file is new: {}", run.err);

    let text = std::fs::read_to_string(&users).unwrap();
    let line = text.lines().next().expect("one line");
    assert_eq!(field(line, 0), "alice");
    assert_eq!(field(line, 1), "admin");
    // argon2id, not argon2i or argon2d: the server refuses the other two at load time so that
    // the request path never has to decide what an unsupported hash means.
    assert!(field(line, 2).starts_with("$argon2id$"), "{line}");
    // The password itself must not be recoverable from the file. Obvious, and worth one line.
    assert!(!text.contains("s3cret"), "the password is in the file in the clear: {text}");
}

#[test]
fn a_file_it_creates_is_readable_only_by_its_owner() {
    // Created at 0600 rather than created and then chmodded: between those two calls the file
    // would exist, world-readable, with hashes in it.
    let dir = tempfile::tempdir().unwrap();
    let users = dir.path().join("users");
    passwd(&users, &["set", "alice"], "s3cret\n").expect(0);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&users).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "got {mode:04o}");
    }
}

#[test]
fn a_new_user_defaults_to_a_role_that_grants_nothing() {
    // A user created without anybody saying what they should be able to do should be able to do
    // the least, not the most - and with roles being names the catalog resolves, the least is a
    // name no catalog has. It authenticates and holds nothing until somebody grants it
    // something, which is the fail-closed direction.
    let dir = tempfile::tempdir().unwrap();
    let users = dir.path().join("users");
    passwd(&users, &["set", "alice"], "s3cret\n").expect(0);
    let text = std::fs::read_to_string(&users).unwrap();
    let role = field(&text, 1);
    assert_eq!(role, "none");
    assert_ne!(role, "superuser", "never the one that holds everything");
}

#[test]
fn setting_a_password_again_replaces_the_line_rather_than_adding_one() {
    let dir = tempfile::tempdir().unwrap();
    let users = dir.path().join("users");
    passwd(&users, &["set", "alice", "--role", "admin"], "first\n").expect(0);
    let before = std::fs::read_to_string(&users).unwrap();

    passwd(&users, &["set", "alice", "--role", "admin"], "second\n").expect(0);
    let after = std::fs::read_to_string(&users).unwrap();

    assert_eq!(after.lines().count(), 1, "still one user: {after}");
    assert_ne!(before, after, "the hash changed");
}

#[test]
fn comments_and_order_survive_an_edit() {
    // An operator's `# ops team` heading has to outlive a password change. A tool that
    // reformatted the file is a tool nobody runs twice.
    let dir = tempfile::tempdir().unwrap();
    let users = dir.path().join("users");
    passwd(&users, &["set", "alice", "--role", "admin"], "pw\n").expect(0);
    passwd(&users, &["set", "bob", "--role", "read"], "pw\n").expect(0);

    // A heading in the middle, the way somebody would actually write one.
    let text = std::fs::read_to_string(&users).unwrap();
    let mut lines: Vec<&str> = text.lines().collect();
    lines.insert(1, "# read-only, for the dashboard");
    lines.insert(0, "# ops, 2026-03");
    std::fs::write(&users, lines.join("\n") + "\n").unwrap();
    set_mode(&users, 0o600);

    passwd(&users, &["set", "alice", "--role", "admin"], "changed\n").expect(0);

    let after = std::fs::read_to_string(&users).unwrap();
    assert!(after.contains("# ops, 2026-03"), "{after}");
    assert!(after.contains("# read-only, for the dashboard"), "{after}");
    let names: Vec<&str> =
        after.lines().filter(|l| !l.starts_with('#')).map(|l| field(l, 0)).collect();
    assert_eq!(names, ["alice", "bob"], "the order is the operator's: {after}");
}

#[test]
fn a_role_change_leaves_the_password_alone() {
    // Changing what somebody may do is not a reason to make them choose a new password.
    let dir = tempfile::tempdir().unwrap();
    let users = dir.path().join("users");
    passwd(&users, &["set", "alice", "--role", "read"], "pw\n").expect(0);
    let hash = field(&std::fs::read_to_string(&users).unwrap(), 2).to_string();

    let run = passwd(&users, &["role", "alice", "admin"], "");
    assert_eq!(run.code, 0, "{}", run.err);

    let after = std::fs::read_to_string(&users).unwrap();
    assert_eq!(field(&after, 1), "admin");
    assert_eq!(field(&after, 2), hash, "the hash is the one it already had");
}

#[test]
fn delete_removes_one_user_and_leaves_the_rest() {
    let dir = tempfile::tempdir().unwrap();
    let users = dir.path().join("users");
    passwd(&users, &["set", "alice", "--role", "admin"], "pw\n").expect(0);
    passwd(&users, &["set", "bob", "--role", "read"], "pw\n").expect(0);

    passwd(&users, &["delete", "alice"], "").expect(0);
    let after = std::fs::read_to_string(&users).unwrap();
    assert!(!after.contains("alice"), "{after}");
    assert!(after.contains("bob"), "{after}");
}

#[test]
fn deleting_somebody_who_is_not_there_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let users = dir.path().join("users");
    passwd(&users, &["set", "alice"], "pw\n").expect(0);
    let run = passwd(&users, &["delete", "nobody"], "").expect(2);
    assert!(run.said("no user `nobody`"), "{}", run.err);
}

#[test]
fn list_prints_names_and_roles_and_never_a_hash() {
    // A hash on a terminal ends up in a scrollback and then in a paste, and an argon2 hash in a
    // paste is an offline attack somebody has been handed.
    let dir = tempfile::tempdir().unwrap();
    let users = dir.path().join("users");
    passwd(&users, &["set", "alice", "--role", "admin"], "pw\n").expect(0);

    let run = passwd(&users, &["list"], "").expect(0);
    assert!(run.out.contains("alice"), "{}", run.out);
    assert!(run.out.contains("admin"), "{}", run.out);
    assert!(!run.out.contains("argon2"), "a hash reached the terminal: {}", run.out);
}

#[test]
fn a_users_file_anyone_can_read_is_refused_before_it_is_edited() {
    // The same rule `big serve` applies, so this refuses a file the server would refuse - rather
    // than editing one happily and then failing to start.
    let dir = tempfile::tempdir().unwrap();
    let users = dir.path().join("users");
    passwd(&users, &["set", "alice"], "pw\n").expect(0);
    set_mode(&users, 0o644);

    let run = passwd(&users, &["set", "bob"], "pw\n").expect(2);
    assert!(run.said("chmod 600"), "{}", run.err);
}

/// **A role this command has never heard of is accepted, on purpose.**
///
/// Roles live in the catalog and are made with `CREATE ROLE`. This command edits a file on disk
/// and may run with no server up at all, so it cannot check - and refusing would make it
/// impossible to write the line before creating the role it names, which is precisely the order
/// a fresh database has to be set up in. A name the catalog does not have is no privileges.
#[test]
fn a_role_the_catalog_may_not_have_yet_is_accepted() {
    let dir = tempfile::tempdir().unwrap();
    let users = dir.path().join("users");
    passwd(&users, &["set", "alice", "--role", "not-made-yet"], "pw\n").expect(0);
    assert_eq!(field(&std::fs::read_to_string(&users).unwrap(), 1), "not-made-yet");

    // `superuser` above all: it is the reserved name a locked-out operator recovers through, so
    // writing it has to work before any database exists.
    passwd(&users, &["role", "alice", "superuser"], "").expect(0);
    assert_eq!(field(&std::fs::read_to_string(&users).unwrap(), 1), "superuser");
}

/// What is refused is a name no catalog record could hold, because that one can never resolve
/// however many roles are created later.
#[test]
fn a_role_name_no_record_could_hold_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let users = dir.path().join("users");
    // A `.` is the separator in a qualified name.
    let run = passwd(&users, &["set", "alice", "--role", "a.b"], "pw\n").expect(2);
    assert!(run.said("a.b"), "{}", run.err);
    assert!(!users.exists(), "nothing was written: {}", users.display());
}

#[test]
fn there_is_no_password_flag() {
    // The invariant this whole command is arranged around. An argument is visible in `ps` and in
    // shell history, and a password in either has already leaked.
    let dir = tempfile::tempdir().unwrap();
    let users = dir.path().join("users");
    let run = passwd(&users, &["set", "alice", "--password", "s3cret"], "");
    assert_ne!(run.code, 0, "a --password flag was accepted: {}", run.err);
    assert!(run.said("unknown option --password"), "{}", run.err);
}

#[test]
fn the_temporary_file_does_not_survive_a_successful_write() {
    // The write is a temporary file plus a rename. A leftover `.tmp` beside the real one would
    // be a second copy of every hash, at whatever mode the crash left it.
    let dir = tempfile::tempdir().unwrap();
    let users = dir.path().join("users");
    passwd(&users, &["set", "alice"], "pw\n").expect(0);
    assert!(!users.with_extension("tmp").exists(), "a .tmp was left behind");
}
