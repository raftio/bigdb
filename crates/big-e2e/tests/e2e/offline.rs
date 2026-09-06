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

//! What survives the process, and the offline tool that works on the file directly.
//!
//! **Nothing below this file ever restarted a daemon.** The cluster tests build an
//! `Api::in_memory()` and the server tests bind and drop one inside a single process, so
//! "commit, exit, come back" - the sequence every operator performs on every upgrade - had no
//! test. The meta page flip is the only atomic point in the design; this is where that claim
//! meets a second process.

use crate::common::*;

/// The schema and facts both halves of a restart test agree about.
fn stock(daemon: &Daemon) {
    daemon.bigctl(&["create", "table", "tx"]).expect(0);
    daemon
        .bigctl(&["create", "field", "tx", "amount", "--kind", "int", "--bit-depth", "20"])
        .expect(0);
    daemon.bigctl(&["create", "field", "tx", "country", "--kind", "set"]).expect(0);
    daemon
        .bigctl_stdin(
            &["import", "tx", "-"],
            "country 1 GB\ncountry 2 US\ncountry 3 GB\namount 1 100\namount 2 250\namount 3 75\n",
        )
        .expect(0);
}

fn count(daemon: &Daemon) -> String {
    daemon.bigctl(&["--format", "json", "sql", "SELECT count(*) FROM tx"]).expect(0).out
}

#[test]
fn what_was_committed_is_there_after_the_daemon_is_restarted() {
    // The whole of recovery, which is that there is none: a commit writes its pages, fsyncs,
    // flips the meta page and fsyncs again. There is no state in between and nothing to replay.
    let workspace = Workspace::new();
    let first = workspace.daemon(&[]);
    stock(&first);
    assert!(count(&first).contains("[3]"));

    // Killed, not asked politely: a daemon that only survives a clean shutdown is a daemon
    // whose durability claim is about shutdown rather than about commits. Stopped rather than
    // dropped, because the next one cannot open the file until this one has been reaped.
    first.stop();

    let second = workspace.daemon(&[]);

    assert!(count(&second).contains("[3]"), "the facts came back: {}", count(&second));
    let schema = second.bigctl(&["--format", "json", "schema"]).expect(0);
    assert!(schema.out.contains("country"), "and so did the schema: {}", schema.out);
}

#[test]
fn a_backup_taken_offline_is_an_ordinary_database() {
    // Restoring is opening it. There is no restore format and no conversion step, which this
    // checks by serving the backup rather than by inspecting it.
    let workspace = Workspace::new();
    let daemon = workspace.daemon(&[]);
    stock(&daemon);
    let source = daemon.path.clone();
    daemon.stop();

    let out = Workspace::new();
    let copy = out.join("backup.big");
    let taken = run("big", &["backup", &source.display().to_string(), &copy.display().to_string()])
        .expect(0);
    assert!(taken.out.contains("backed up"), "{:?}", taken.out);

    let served = Workspace::new();
    std::fs::copy(&copy, served.join("data.big")).unwrap();
    let restored = served.daemon(&[]);

    assert!(count(&restored).contains("[3]"), "the backup answers: {}", count(&restored));
}

#[test]
fn a_backup_refuses_to_overwrite_what_is_already_there() {
    // The caller who typed the wrong name is exactly the caller who needed the previous one.
    let workspace = Workspace::new();
    let daemon = workspace.daemon(&[]);
    stock(&daemon);
    let source = daemon.path.clone();
    daemon.stop();

    let out = Workspace::new();
    let dest = out.join("taken.big");
    std::fs::write(&dest, b"something already here").unwrap();

    let run = run("big", &["backup", &source.display().to_string(), &dest.display().to_string()]);

    assert_ne!(run.code, 0, "it refuses: {}{}", run.out, run.err);
    assert_eq!(
        std::fs::read(&dest).unwrap(),
        b"something already here",
        "and leaves what was there"
    );
}

#[test]
fn the_offline_tool_cannot_touch_a_file_a_daemon_is_serving() {
    // **Structural rather than incidental.** Every subcommand opens the file through
    // `MmapPager`, which takes an exclusive lock, so a second process cannot open a served file
    // at all. "A backup is safe alongside a writer" is true of the *walk* - it holds a read
    // transaction, which pins the reclaim horizon - and only inside one process. `runbook.md`
    // used to say it without that qualifier; this test is what keeps the two honest.
    let daemon = Daemon::start();
    let out = Workspace::new();
    let dest = out.join("backup.big");

    let run =
        run("big", &["backup", &daemon.path.display().to_string(), &dest.display().to_string()]);

    assert_ne!(run.code, 0, "the lock stops it: {}{}", run.out, run.err);
    assert!(!dest.exists(), "and nothing half-written is left behind");
}

#[test]
fn compact_returns_space_and_keeps_the_answers() {
    // Compaction is the copy path plus an atomic rename, which is why it is the same walk a
    // backup is - and why a test of one is a test of the other's page classes.
    let workspace = Workspace::new();
    let daemon = workspace.daemon(&[]);
    stock(&daemon);
    daemon.bigctl(&["drop", "field", "tx", "country"]).expect(0);
    let path = daemon.path.clone();
    daemon.stop();

    let run = run("big", &["compact", &path.display().to_string()]).expect(0);
    assert!(!run.out.is_empty() || !run.err.is_empty(), "it reports what it did");

    let after = workspace.daemon(&[]);

    assert!(count(&after).contains("[3]"), "the records are still there: {}", count(&after));
    let schema = after.bigctl(&["--format", "json", "schema"]).expect(0);
    assert!(!schema.out.contains("country"), "and the dropped field is gone: {}", schema.out);
}

#[test]
fn the_offline_tool_says_what_it_takes_when_given_nothing() {
    let run = run("big", &[]).expect(2);

    assert!(run.said("Usage: big"), "{}", run.err);
    assert!(run.said("backup") && run.said("compact") && run.said("verify"), "{}", run.err);
}

#[test]
fn a_file_that_is_not_a_database_is_refused_rather_than_opened() {
    // A file large enough to hold a meta page is checked against the magic and refused, which
    // is the behaviour the whole format-version policy rests on: "a file from another version
    // reports that fact rather than collapsing into `this file is damaged`".
    let dir = Workspace::new();
    let junk = dir.join("not-a-database.big");
    std::fs::write(&junk, vec![b'x'; 64 << 10]).unwrap();

    let run = run("big", &["verify", &junk.display().to_string()]);

    assert_ne!(run.code, 0, "it does not open: {}{}", run.out, run.err);
}

/// A file too short to hold a meta page is somebody else's, not an empty path.
///
/// **This used to destroy the file.** Anything under two pages holds zero *pages*, so it looked
/// exactly like a path nothing had created yet, and every `big` subcommand initialised a fresh
/// database over it - returning zero and printing `txn 0, 2 pages` while the operator's file
/// went away. A larger file was already refused by the magic in its meta page; this is the range
/// where there was no magic to check.
#[test]
fn a_short_file_that_is_not_a_database_is_not_overwritten() {
    let dir = Workspace::new();
    let notes = dir.join("notes.txt");
    let original = b"somebody else's file, four thousand two hundred bytes short of a page pair";
    std::fs::write(&notes, original).unwrap();

    let run = run("big", &["verify", &notes.display().to_string()]);

    assert_ne!(run.code, 0, "it does not open: {}{}", run.out, run.err);
    assert_eq!(std::fs::read(&notes).unwrap(), original, "and it does not touch the file");
}
