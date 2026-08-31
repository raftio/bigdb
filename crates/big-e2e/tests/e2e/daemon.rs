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

//! `bigd`, started the way an operator starts it.
//!
//! **Every decision here happens before the first request.** The argument parser, the token
//! file, the refusal to serve a public port with no authentication, the exclusive lock - none
//! of it is reachable from a test that calls `Server::bind`, and none of it had a test until
//! this file. The parser had exactly one caller, which was `main`.

use crate::common::*;

#[test]
fn no_arguments_is_a_usage_error_rather_than_a_default_database() {
    // Not "open ./data.big". A daemon that invents a path creates a file somewhere the operator
    // did not choose, and finds out later.
    let run = run("bigd", &[]).expect(2);

    assert!(run.said("a database file is required"), "says what is missing: {}", run.err);
    assert!(run.said("usage: bigd"), "and repeats the usage: {}", run.err);
}

#[test]
fn help_is_not_a_failure() {
    // Usage to stdout and exit zero, so `bigd --help | less` works and a script does not treat
    // it as an error. The split every tool here makes.
    let run = run("bigd", &["--help"]).expect(0);

    assert!(run.out.contains("usage: bigd"), "usage on stdout: {:?}", run.out);
    assert!(run.err.is_empty(), "and nothing on stderr: {:?}", run.err);
}

#[test]
fn an_unknown_option_names_the_option_it_did_not_know() {
    let run = run("bigd", &["/tmp/nothing.big", "--wat"]).expect(2);

    assert!(run.said("--wat"), "names the option: {}", run.err);
}

#[test]
fn a_flag_without_its_value_says_which_flag() {
    // The difference between a usable error and "invalid arguments".
    let run = run("bigd", &["/tmp/nothing.big", "--tokens"]).expect(2);

    assert!(run.said("--tokens needs a value"), "{}", run.err);
}

#[test]
fn a_durability_that_is_not_one_of_the_three_is_refused_with_the_three() {
    let run = run("bigd", &["/tmp/nothing.big", "--durability", "sometimes"]).expect(2);

    assert!(run.said("full, barrier or none"), "lists what it takes: {}", run.err);
    assert!(run.said("sometimes"), "and repeats what it got: {}", run.err);
}

#[test]
fn a_public_port_with_no_authentication_is_refused() {
    // **The load-bearing refusal.** There is no TLS and there will not be, so what keeps that
    // from being an excuse is this: a port anyone can reach, with no token file, does not open.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("data.big");
    let run = run("bigd", &[&path.display().to_string(), "0.0.0.0:0"]).expect(2);

    assert!(run.said("refusing to serve"), "{}", run.err);
    // All three ways out, because a refusal that does not say how to proceed is a wall.
    assert!(run.said("--tokens"), "offers a token file: {}", run.err);
    assert!(run.said("reverse proxy"), "offers a proxy: {}", run.err);
    assert!(run.said("--insecure-no-auth"), "offers the override: {}", run.err);
}

#[test]
fn loopback_with_no_authentication_is_allowed_and_says_so() {
    // Allowed, because a loopback port is not reachable from anywhere else - and announced,
    // because an operator reading a log should not have to infer it from silence.
    let daemon = Daemon::start();
    until("the daemon to announce itself", || !daemon.log().is_empty());

    let log = daemon.log();
    assert!(!log.contains("refusing to serve"), "loopback is not refused: {log}");
    assert!(log.contains("no authentication"), "and it says the port is open: {log}");
}

#[test]
fn a_token_file_anyone_can_read_is_refused() {
    // A bearer token in a world-readable file is a token every process on the box has. Refused
    // rather than warned about: a warning on startup is a line nobody reads twice.
    let dir = tempfile::tempdir().unwrap();
    let tokens = token_file(dir.path(), "sekrit admin\n");
    set_mode(&tokens, 0o644);
    let path = dir.path().join("data.big");

    let run = run(
        "bigd",
        &[&path.display().to_string(), "127.0.0.1:0", "--tokens", &tokens.display().to_string()],
    );

    assert_ne!(run.code, 0, "it does not start: {}", run.err);
    assert!(run.said("644") || run.said("readable"), "says why: {}", run.err);
}

#[test]
fn a_daemon_with_tokens_says_how_many_it_loaded() {
    let workspace = Workspace::new();
    let tokens = token_file(workspace.path(), "alpha admin\nbeta read\n");
    let daemon = workspace.daemon(&["--tokens", &tokens.display().to_string()]);

    until("the daemon to report its tokens", || daemon.log().contains("tokens loaded"));
    assert!(daemon.log().contains("2 tokens loaded"), "{}", daemon.log());

    // The probes stay open with authentication on, which is what makes this observable at all.
    assert_eq!(daemon.bigc(&["health"]).code, 0);
    // And an unauthenticated request for data does not get through.
    let refused = daemon.bigc(&["schema"]);
    assert_ne!(refused.code, 0, "no token, no schema: {:?}", refused.out);
}

#[test]
fn a_second_daemon_on_the_same_file_is_refused_by_the_lock() {
    // One process per file. The engine takes an exclusive lock, so the second daemon fails at
    // startup rather than two of them writing to one file and finding out afterwards.
    let first = Daemon::start();

    let second = run("bigd", &[&first.path.display().to_string(), "127.0.0.1:0"]);

    assert_ne!(second.code, 0, "the second daemon does not start: {}", second.err);
    assert!(second.said("could not open"), "and says which file: {}", second.err);
    // The first is untouched by the attempt.
    assert_eq!(first.bigc(&["health"]).code, 0);
}

#[test]
fn durability_is_announced_on_every_start_not_only_when_it_is_relaxed() {
    // An operator reading a log after an incident needs to know what the setting *was*, and a
    // line that only appears sometimes is one they have to remember the absence of. So both the
    // relaxed setting and the default one have to say what they are.
    let relaxed = Daemon::with(&["--durability", "none"]);
    until("the relaxed daemon to announce itself", || relaxed.log().contains("durability"));
    assert!(relaxed.log().contains("durability none"), "{}", relaxed.log());

    let default = Daemon::start();
    until("the default daemon to announce itself", || default.log().contains("durability"));
    assert!(default.log().contains("durability full"), "{}", default.log());
}
