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

//! What the corpus has to keep being, checked against the corpus rather than against a list
//! somebody maintains.
//!
//! A corpus decays in a way a test suite does not: nothing about it fails when a feature is
//! added and no case is written for it. These are the three claims that turn that from a matter
//! of discipline into a matter of the build going red.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use big_sql::{Authority, Ddl, Refused, Sql};

/// Every statement in the corpus, whatever directive it was written under.
///
/// Read out of the files rather than listed here, which is the point: a case added to a file is
/// a case these gates see.
fn statements() -> Vec<(PathBuf, usize, String)> {
    let dir = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/testdata"));
    let mut out = Vec::new();
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("no corpus at {}: {e}", dir.display()))
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "test"))
        .collect();
    files.sort();
    assert!(!files.is_empty(), "no .test files under {}", dir.display());

    for path in files {
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        let mut i = 0;
        while i < lines.len() {
            let line = lines[i];
            i += 1;
            if line.trim().is_empty() || line.trim_start().starts_with('#') || line == "----" {
                continue;
            }
            // A directive line. What follows it, up to `----` or a blank line, is the statement.
            let at = i;
            let mut statement = Vec::new();
            while i < lines.len() && lines[i] != "----" && !lines[i].trim().is_empty() {
                statement.push(lines[i]);
                i += 1;
            }
            // Then its expected block, which is output and not a statement.
            if i < lines.len() && lines[i] == "----" {
                i += 1;
                while i < lines.len() && !lines[i].trim().is_empty() {
                    i += 1;
                }
            }
            if !statement.is_empty() {
                out.push((path.clone(), at, statement.join("\n")));
            }
        }
    }
    out
}

/// **The gate that keeps the refusal list from being prose nobody reads.**
///
/// Every refusal carries a sentence saying what exists instead, and that sentence is the whole
/// reason the list is enumerated rather than left as free text. A refusal no statement reaches
/// is a sentence that was written once and has not been read since - so each has to be reached
/// by a case in the corpus.
///
/// The exceptions are listed with their reasons rather than the gate being softened, because a
/// gate with a silent hole in it is worth less than no gate.
#[test]
fn every_refusal_is_reached_by_a_statement_in_the_corpus() {
    /// Refusals no case can reach, and why.
    ///
    /// One, and it is a bound on the file rather than on the surface: `sql_insert_too_large`
    /// needs more than ten thousand tuples in one statement, which is a file nobody would read.
    /// It is checked in `tests/translate/writes.rs`, where the rows can be generated.
    ///
    /// The list was two. `sql_no_time_window` was not hard to write - nothing constructed it,
    /// because it was the refusal a window earned before the planner had a field class that
    /// could tell a set from a time quantum, and the window it refused is now answered. This
    /// gate found that, and the variant is gone.
    const EXCUSED: [(&str, &str); 4] = [
        ("sql_insert_too_large", "needs 10,001 tuples; covered in tests/translate/writes.rs"),
        // The two refusals a view is expanded into. `translate` holds no catalog - which is
        // what makes every test in this crate a parser test - so no statement here can reach a
        // refusal that needs a stored `SELECT` to decide. They are raised in `big-api::views`
        // and covered where the catalog is.
        ("sql_view_column", "needs a stored view; covered in big-api/tests/views.rs"),
        ("sql_view_depth", "needs a stored view; covered in big-api/tests/views.rs"),
        // The refusal a surface raises about itself rather than about the text. `translate`
        // accepts every `EXPLAIN` the dialect has - that is the whole point of the wrapper - so
        // no statement here can reach the one refusal that is about which surface was asked.
        ("sql_explain_rows", "raised in big-api::plan_sql_in; covered in big-api/tests/sql.rs"),
    ];

    let reached: BTreeSet<&str> = statements()
        .iter()
        .filter_map(|(_, _, sql)| big_sql::translate(sql).err())
        .map(|e| e.code())
        .collect();

    let mut missing = Vec::new();
    for refused in Refused::ALL {
        let code = refused.code();
        if reached.contains(code) || EXCUSED.iter().any(|(c, _)| *c == code) {
            continue;
        }
        missing.push(format!("  {refused:?} ({code})"));
    }
    assert!(
        missing.is_empty(),
        "\n{} refusal(s) no statement in tests/testdata reaches:\n{}\n\n\
         Add a case to tests/testdata/refusals.test, or excuse it here with the reason.\n",
        missing.len(),
        missing.join("\n")
    );

    // The exceptions have to stay exceptions. One that becomes reachable should be moved into
    // the corpus, and this is what says so.
    for (code, why) in EXCUSED {
        assert!(
            !reached.contains(code),
            "`{code}` is excused as `{why}`, but the corpus now reaches it - delete the excuse"
        );
    }
}

/// **`EXPLAIN X` is `X`, wrapped and otherwise untouched.**
///
/// Held over the whole corpus rather than over a handful of statements, because that is the size
/// of the claim: `EXPLAIN` was added in front of a parser with four entry points, and a clause it
/// broke would have to be one nobody wrote a case for. It costs nothing to keep - the statements
/// are already there, under whatever directive they were written for - and it covers the `WITH`
/// bindings, `UNION ALL`, every join, every type name and every refusal at once.
///
/// The refusal half is the load-bearing one. A statement this dialect will not answer is refused
/// under `EXPLAIN` with the *same code*: the refusal is about what was written, and `EXPLAIN` did
/// not write it. That is what makes the wrapper safe to have added - it inherits the whole list
/// rather than growing a second one that could disagree.
#[test]
fn explain_wraps_every_statement_unchanged() {
    for (path, line, sql) in statements() {
        // The corpus holds statements that are already an `EXPLAIN`; wrapping one again is the
        // nesting the parser refuses, and `explain.test` covers that on purpose.
        if sql.trim_start().get(..7).is_some_and(|w| w.eq_ignore_ascii_case("EXPLAIN")) {
            continue;
        }
        let at = format!("{}:{line}", path.display());
        match (big_sql::translate(&sql), big_sql::translate(&format!("EXPLAIN {sql}"))) {
            (Ok(inner), Ok(Sql::Explain { mode, inner: wrapped })) => {
                assert_eq!(mode, big_sql::ExplainMode::All, "{at}: bare EXPLAIN named a half");
                assert_eq!(*wrapped, inner, "{at}: EXPLAIN changed the statement under it");
            }
            (Err(bare), Err(explained)) => {
                assert_eq!(
                    bare.code(),
                    explained.code(),
                    "{at}: refused differently under EXPLAIN"
                );
            }
            (bare, explained) => {
                panic!("{at}: explained differently\n  bare: {bare:?}\n  explained: {explained:?}")
            }
        }
    }
}

/// **What a statement costs agrees with the keyword it opens with.**
///
/// Checked against the leading word *on purpose*, which is the one derivation
/// [`Sql::authority`] refuses to use: it reads the parse tree, this reads the text, and a test
/// that re-implemented the match it is checking would pass whatever that match said. Held over
/// the corpus so a statement added to a file is a statement this covers.
#[test]
fn what_a_statement_costs_agrees_with_the_word_it_opens_with() {
    for (path, line, sql) in statements() {
        let Ok(parsed) = big_sql::translate(&sql) else { continue };
        let mut words = sql.split_whitespace();
        // `EXPLAIN` and the half it may name are skipped rather than judged: what an explanation
        // costs is the *next* word's business, which is the claim the test below is about.
        let mut word = words.next().unwrap_or_default().to_uppercase();
        while matches!(word.as_str(), "EXPLAIN" | "PLAN" | "SHAPE") {
            word = words.next().unwrap_or_default().to_uppercase();
        }
        let expected = match word.as_str() {
            "CREATE" | "ALTER" | "DROP" => Authority::Admin,
            "INSERT" => Authority::Write,
            // `WITH` binds constants for a `SELECT`; the rest read what is already there.
            "SELECT" | "WITH" | "DESCRIBE" | "DESC" | "SHOW" => Authority::Read,
            other => panic!("{}:{line}: no expected authority for `{other}`", path.display()),
        };
        assert_eq!(
            parsed.authority(),
            expected,
            "{}:{line}: `{word}` statement demands the wrong authority\n  {sql}",
            path.display()
        );
    }
}

/// **`EXPLAIN X` costs exactly what `X` costs.**
///
/// The load-bearing half of [`Sql::authority`], and the reason it looks inside the wrapper: an
/// explanation that read as a plain read would let a `read` token name a schema change, which is
/// the class of statement its credential says it may not name. ClickHouse decides it the same
/// way - its `EXPLAIN` checks the access the explained query would have needed.
///
/// Held over the whole corpus rather than over a handful of statements, so the rule covers every
/// kind of statement anybody ever writes a case for, including ones added after this.
#[test]
fn explaining_a_statement_costs_what_the_statement_costs() {
    for (path, line, sql) in statements() {
        // Already an `EXPLAIN`; wrapping one again is the nesting the parser refuses.
        if sql.trim_start().get(..7).is_some_and(|w| w.eq_ignore_ascii_case("EXPLAIN")) {
            continue;
        }
        let (Ok(bare), Ok(explained)) =
            (big_sql::translate(&sql), big_sql::translate(&format!("EXPLAIN {sql}")))
        else {
            continue;
        };
        assert_eq!(
            explained.authority(),
            bare.authority(),
            "{}:{line}: EXPLAIN changed what the statement costs\n  {sql}",
            path.display()
        );
    }
}

/// **The gate on the inverse.**
///
/// `parse::column_type` decides what a type name means and `render::create_table` decides how a
/// field is written back; they are the two directions of one table, and an inverse kept honest
/// by nothing drifts. Every `CREATE TABLE` in the corpus is written back out and read again, and
/// has to come back as the same columns.
///
/// Over the corpus rather than over a list of type names, so a type added to the dialect is
/// covered the moment somebody writes a case for it.
#[test]
fn every_declared_table_survives_being_written_back_out() {
    let mut checked = 0;
    for (path, line, sql) in statements() {
        let Ok(Sql::Ddl(Ddl::CreateTable { database, table, engine, columns, .. })) =
            big_sql::translate(&sql)
        else {
            continue;
        };
        // Qualified when the statement was, because that is what `SHOW CREATE TABLE` answers
        // with - and a gate that rendered the bare name would be checking a statement the
        // server never produces.
        let name = match &database {
            Some(d) => format!("{d}.{table}"),
            None => table.clone(),
        };
        let written = big_sql::render::create_table(&name, engine.as_deref(), &columns);
        let back = match big_sql::translate(&written) {
            // The database has to survive the round trip too: a `sales.orders` that read back
            // as `orders` would recreate the table in whichever database asked.
            Ok(Sql::Ddl(Ddl::CreateTable { database: read_back, columns, .. }))
                if read_back == database =>
            {
                columns
            }
            other => panic!(
                "{}:{line} was written back as something else:\n{written}\n{other:?}",
                path.display()
            ),
        };
        assert_eq!(
            back,
            columns,
            "\n{}:{line} did not survive the round trip.\n  from: {sql}\n  written: {written}\n",
            path.display()
        );
        checked += 1;
    }
    // A gate that checked nothing would pass, which is the one way this test could lie.
    assert!(checked >= 10, "only {checked} CREATE TABLE statements in the corpus");
}

/// **The gate on the parser itself: no input is a crash.**
///
/// Every statement in the corpus, and every prefix and mutation of one, goes through
/// `translate`. What is checked is not what it answers - a truncated statement has no right
/// answer - but that it answers at all, with a refusal rather than a panic or a hang.
///
/// The corpus is the seed because it is the only collection of realistic statements this
/// workspace has, and a mutation of a realistic statement reaches further into the parser than
/// a random string does. `fuzz/fuzz_targets/parse_sql.rs` runs the same body without a bound on
/// the inputs; this one runs in the ordinary test suite so a regression is caught before
/// anybody thinks to run a fuzzer.
#[test]
fn no_statement_the_corpus_can_be_cut_into_makes_the_parser_panic() {
    let mut tried = 0;
    for (_, _, sql) in statements() {
        // Every prefix: the parser has to be as happy stopping early as finishing.
        for end in 0..=sql.len() {
            if !sql.is_char_boundary(end) {
                continue;
            }
            let _ = big_sql::translate(&sql[..end]);
            tried += 1;
        }
        // And a byte deleted from the middle, which is the mutation that turns a balanced
        // statement into an unbalanced one without shortening it.
        for cut in 0..sql.len() {
            if !sql.is_char_boundary(cut) || !sql.is_char_boundary(cut + 1) {
                continue;
            }
            let mutated = format!("{}{}", &sql[..cut], &sql[cut + 1..]);
            let _ = big_sql::translate(&mutated);
            tried += 1;
        }
    }
    assert!(tried > 10_000, "only {tried} inputs tried");
}
