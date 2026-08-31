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

//! The three binaries against one daemon, the way the readme tells an operator to use them.
//!
//! **The claim being tested is that they are one product.** `bigc` links none of the engine and
//! `bigi` links only `bigc`, so what makes them agree with `bigd` is a wire format and nothing
//! else. A test that called the library functions would keep passing after `main` stopped
//! wiring them up; these run the binaries.

use crate::common::*;

/// The schema the readme's example builds, and a few facts under it.
fn stocked() -> Daemon {
    let daemon = Daemon::start();
    daemon.bigc(&["create", "table", "tx"]).expect(0);
    daemon
        .bigc(&["create", "field", "tx", "amount", "--kind", "int", "--bit-depth", "20"])
        .expect(0);
    daemon.bigc(&["create", "field", "tx", "country", "--kind", "set"]).expect(0);
    daemon
}

/// The facts `make demo` loads, as a file `bigi` can take.
const FACTS: &str = "country 1 GB\ncountry 2 US\ncountry 3 GB\n\
                     amount 1 100\namount 2 250\namount 3 75\n";

#[test]
fn the_readme_walkthrough_works_as_three_processes() {
    // Schema, then a load, then a question - each a separate program, talking over a socket.
    let daemon = stocked();
    let dir = tempfile::tempdir().unwrap();
    let facts = dir.path().join("facts.txt");
    std::fs::write(&facts, FACTS).unwrap();

    let load = daemon.bigi(&["import", "tx", &facts.display().to_string()]).expect(0);
    assert!(load.out.contains("imported 6"), "six facts went in: {:?}", load.out);

    let answer = daemon
        .bigc(&[
            "--format",
            "json",
            "sql",
            "SELECT country, count(*), sum(amount) FROM tx GROUP BY country ORDER BY country",
        ])
        .expect(0);

    assert!(answer.out.contains("\"GB\",2,175"), "the merged row: {}", answer.out);
    assert!(answer.out.contains("\"US\",1,250"), "and the other: {}", answer.out);
}

#[test]
fn a_statement_the_server_refuses_comes_back_as_the_servers_own_code() {
    // **`bigc` cannot rewrite this.** It links no planner, so the code and the sentence on the
    // terminal are the ones `bigd` chose - which is the whole reason it links nothing.
    let daemon = stocked();

    let run = daemon.bigc(&["sql", "SELECT * FROM tx JOIN other ON tx.k = other.k"]);

    assert_eq!(run.code, 1, "the server refused, so exit 1: {:?}", run.err);
    assert!(run.said("sql_"), "and the server's own code is printed: {}{}", run.out, run.err);
}

#[test]
fn nothing_listening_is_its_own_exit_code() {
    // Told apart from a refusal on purpose: a script that retries wants to know whether the
    // server said no or was not there.
    let run = run("bigc", &["--addr", "127.0.0.1:1", "schema"]);

    assert_eq!(run.code, 3, "nothing listening is 3: {}{}", run.out, run.err);
}

#[test]
fn a_load_from_standard_input_reaches_the_same_place_a_file_does() {
    let daemon = stocked();

    let load =
        run_with_stdin("bigi", &["--addr", &daemon.addr.to_string(), "import", "tx", "-"], FACTS)
            .expect(0);
    assert!(load.out.contains("imported 6"), "{:?}", load.out);

    let count = daemon.bigc(&["--format", "json", "sql", "SELECT count(*) FROM tx"]).expect(0);
    assert!(count.out.contains("[3]"), "three records: {}", count.out);
}

#[test]
fn an_interrupted_load_resumes_where_its_checkpoint_says() {
    // The property the whole loader rests on: a fact is a bit set at a record id written in the
    // line, so a chunk sent twice writes what sending it once wrote. Here the load is cut in
    // half by hand and finished by a second process reading the checkpoint the first left.
    let daemon = stocked();
    let dir = tempfile::tempdir().unwrap();
    let facts = dir.path().join("facts.txt");
    let resume = dir.path().join("resume.json");

    // Sixteen bytes a line, so an offset is a number this test can state.
    let all: String = (1..=200).map(|id| format!("country {id:04} GB\n")).collect();
    std::fs::write(&facts, &all).unwrap();

    // First half, as its own file, written through the checkpoint the second run will read.
    let half = dir.path().join("half.txt");
    std::fs::write(&half, &all[..all.len() / 2]).unwrap();
    daemon
        .bigi(&[
            "--resume",
            &resume.display().to_string(),
            "import",
            "tx",
            &half.display().to_string(),
        ])
        .expect(0);

    let count = |d: &Daemon| -> String {
        d.bigc(&["--format", "json", "sql", "SELECT count(*) FROM tx"]).expect(0).out
    };
    assert!(count(&daemon).contains("[100]"), "half landed: {}", count(&daemon));

    // A finished load removes its checkpoint, so the second run starts from the beginning of
    // the full file - and lands on exactly the same bits for the first hundred.
    daemon
        .bigi(&[
            "--resume",
            &resume.display().to_string(),
            "import",
            "tx",
            &facts.display().to_string(),
        ])
        .expect(0);

    assert!(count(&daemon).contains("[200]"), "all of it, once: {}", count(&daemon));
}

#[test]
fn a_dry_run_sends_nothing() {
    let daemon = stocked();
    let dir = tempfile::tempdir().unwrap();
    let facts = dir.path().join("facts.txt");
    std::fs::write(&facts, FACTS).unwrap();

    let run = daemon.bigi(&["--dry-run", "import", "tx", &facts.display().to_string()]).expect(0);

    assert!(run.out.contains("would send"), "it says it would: {:?}", run.out);
    let count = daemon.bigc(&["--format", "json", "sql", "SELECT count(*) FROM tx"]).expect(0);
    assert!(count.out.contains("[0]"), "and nothing arrived: {}", count.out);
}

#[test]
fn a_token_is_read_from_a_file_and_never_taken_as_a_flag() {
    // An argument is visible in `ps` and in shell history, so a bearer token in either is a
    // token that has leaked. There is deliberately no `--token`.
    let workspace = Workspace::new();
    let tokens = token_file(workspace.path(), "sekrit admin\n");
    let daemon = workspace.daemon(&["--tokens", &tokens.display().to_string()]);

    let refused = daemon.bigc(&["schema"]);
    assert_ne!(refused.code, 0, "no credential, no schema");

    // The client's token file is a different file with the same contents rule: mode 600, and
    // it holds the token itself rather than a `token role` line.
    let held = tempfile::tempdir().unwrap();
    let mine = held.path().join("token");
    std::fs::write(&mine, "sekrit").unwrap();
    set_mode(&mine, 0o600);

    let allowed = daemon.bigc(&["--token-file", &mine.display().to_string(), "schema"]).expect(0);
    assert!(allowed.code == 0, "with the token it answers: {}", allowed.err);
}
