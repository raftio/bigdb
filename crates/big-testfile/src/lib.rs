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

//! Tests written as files of statements and the answers they produce.
//!
//! # Why a file format rather than more `#[test]` functions
//!
//! A hand-written test states a claim, and the good ones here do: *these two surfaces resolve
//! to the same plan*, *this refusal says what exists instead*. That is worth four lines of Rust
//! and a paragraph of why.
//!
//! Breadth is a different job. Four hundred statements, each checked against the tree it
//! translates into, is not four hundred claims - it is one claim held at four hundred points,
//! and writing each point as a function buries the interesting tests among the routine ones and
//! makes the routine ones expensive enough that nobody adds the four hundred and first. So they
//! move into files, where a case costs two lines and the expected output is generated.
//!
//! Both halves stay. `tests/translate` still carries the claims; the corpora carry the surface.
//!
//! # The format
//!
//! ```text
//! # A comment. Blank lines separate cases.
//!
//! plan
//! SELECT count(*) FROM t WHERE amount >= 500
//! ----
//! Count t
//! └── amount >= 500
//!
//! error
//! SELECT * FROM a, b
//! ----
//! sql_no_joins
//! ```
//!
//! A case is a directive line, the statement under it, and - after `----` - what the directive
//! is expected to print. The expected block ends at a blank line, so nothing a directive prints
//! may contain one; every printer in this workspace draws trees, which never do.
//!
//! Words after the directive name are its arguments, available to the dispatch as
//! [`Case::args`]. That is where a second input goes - the PQL a `same` case compares against,
//! for instance - so that the block after `----` is always *output*, and therefore always safe
//! to regenerate.
//!
//! # Rewriting
//!
//! `BIG_REWRITE=1 cargo test` replaces every expected block with what the code actually printed
//! and reports the files it touched, rather than failing. Comments, blank lines and case order
//! survive it.
//!
//! **A rewrite is a diff to read, not a fix.** It is the cheap way to add a hundred cases and
//! the cheap way to accept a hundred regressions, and the only thing standing between the two
//! is that the change shows up in `git diff` at the level of the answers.

use std::path::{Path, PathBuf};

/// One case: a directive, the statement it applies to, and what it should print.
pub struct Case {
    /// The first word of the directive line - `plan`, `error`, `query`.
    pub directive: String,
    /// The rest of that line, trimmed. Empty when there was none.
    pub args: String,
    /// The lines between the directive and `----`, which is the statement.
    pub input: String,
    /// The block after `----`, without its trailing newline. Empty when there was no block.
    pub expected: String,
    /// The file this came from, for a failure that has to be found.
    pub file: PathBuf,
    /// The 1-based line the directive is on.
    pub line: usize,
}

impl Case {
    /// The directive's arguments split on whitespace, for a dispatch that takes more than one.
    ///
    /// Not what a `same` case wants - a PQL call has spaces inside it and is read whole from
    /// [`Case::args`] - which is why this is a method rather than the field.
    pub fn words(&self) -> Vec<&str> {
        self.args.split_whitespace().collect()
    }

    /// Where this case is, spelled the way an editor will jump to.
    pub fn at(&self) -> String {
        format!("{}:{}", self.file.display(), self.line)
    }
}

/// A file, kept in enough detail to be written back out unchanged.
///
/// The raw chunks are why: comments and blank lines are the part of a corpus a person wrote,
/// and a rewrite that dropped them would make the files worth less every time they were
/// regenerated.
struct TestFile {
    path: PathBuf,
    chunks: Vec<Chunk>,
}

enum Chunk {
    /// Comments and blank lines, verbatim, each still carrying its newline.
    Raw(String),
    Case(Case),
}

/// Runs every `.test` file in `dir` through `dispatch`, failing on any case that reports
/// something - and ignoring the expected blocks entirely.
///
/// **This is what a second configuration needs.** A corpus's expected blocks belong to whichever
/// run wrote them; another way of running the same files - over a socket, across two nodes -
/// cannot be checked against them without re-deriving one rendering from another, and what that
/// would then be checking is the re-derivation. So a configuration checks a *property* instead:
/// it runs each case and answers with what disagreed, or with nothing.
///
/// Files are never rewritten here, whatever `BIG_REWRITE` says. A configuration has no expected
/// output of its own, and letting it write into the corpus would let it overwrite the answers
/// the corpus exists to record.
pub fn check(dir: impl AsRef<Path>, mut dispatch: impl FnMut(&Case) -> String) {
    let mut failures = Vec::new();
    let mut cases = 0usize;
    for (path, file) in files(dir.as_ref()) {
        for chunk in &file.chunks {
            let Chunk::Case(case) = chunk else { continue };
            cases += 1;
            let said = caught(&mut dispatch, case);
            if !said.is_empty() {
                failures.push(report_said(case, &said));
            }
        }
        let _ = path;
    }
    assert!(
        failures.is_empty(),
        "\n{} of {cases} case(s) disagreed.\n{}\n",
        failures.len(),
        failures.join("")
    );
}

/// One disagreement, with no expected block to lay it out against.
fn report_said(case: &Case, said: &str) -> String {
    format!(
        "\n{} `{}{}`\n  {}\n{}\n",
        case.at(),
        case.directive,
        if case.args.is_empty() { String::new() } else { format!(" {}", case.args) },
        case.input.replace('\n', "\n  "),
        indent(said),
    )
}

/// Runs every `.test` file in `dir` through `dispatch`, comparing what it prints against what
/// each case expects.
///
/// Panics with the failures at the end rather than at the first one: a change to a printer
/// breaks every case that uses it, and being told the first of two hundred is being told
/// nothing.
///
/// `BIG_TEST_FILTER=<substring>` narrows to files whose name contains it, for iterating on one.
pub fn run(dir: impl AsRef<Path>, mut dispatch: impl FnMut(&Case) -> String) {
    let dir = dir.as_ref();
    let rewriting = std::env::var_os("BIG_REWRITE").is_some_and(|v| v != "0" && !v.is_empty());
    let filter = std::env::var("BIG_TEST_FILTER").unwrap_or_default();

    let _ = &filter;
    let mut failures: Vec<String> = Vec::new();
    let mut rewritten: Vec<PathBuf> = Vec::new();
    let mut cases = 0usize;

    for (path, mut file) in files(dir) {
        let mut changed = false;

        for chunk in &mut file.chunks {
            let Chunk::Case(case) = chunk else { continue };
            cases += 1;
            let actual = caught(&mut dispatch, case);
            if actual == case.expected {
                continue;
            }
            if rewriting {
                case.expected = actual;
                changed = true;
            } else {
                failures.push(report(case, &actual));
            }
        }

        if changed {
            std::fs::write(&path, render(&file))
                .unwrap_or_else(|e| panic!("could not rewrite {}: {e}", path.display()));
            rewritten.push(path);
        }
    }

    // A rewrite fails the run, and it is not squeamishness. `libtest` captures the output of a
    // test that passes, so a message about files having been changed would be invisible exactly
    // when it matters most; and a green `make test` under `BIG_REWRITE=1` is a green run that
    // proves nothing. Failing here makes the notice visible and makes the plain re-run - with no
    // environment variable and nothing left to regenerate - the thing that says it passed.
    assert!(
        rewritten.is_empty(),
        "\nBIG_REWRITE rewrote {} file(s):\n  {}\n\n\
         Read the diff, then re-run without BIG_REWRITE to confirm.\n",
        rewritten.len(),
        rewritten.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join("\n  ")
    );
    assert!(
        failures.is_empty(),
        "\n{} of {cases} case(s) failed.\n{}\n\
         Re-run with BIG_REWRITE=1 to accept these, then read the diff.\n",
        failures.len(),
        failures.join("")
    );
}

/// Runs one case, turning a panic inside the dispatch into a failure of that case.
///
/// Deliberately *not* returned as output: a panic is a bug in the dispatch or a case it cannot
/// express, and letting `BIG_REWRITE` bake `panicked at ...` into a file as the expected answer
/// would turn a crash into a passing test.
fn caught(dispatch: &mut impl FnMut(&Case) -> String, case: &Case) -> String {
    let hook = std::panic::take_hook();
    // The default hook prints a backtrace for a panic that is about to be reported with better
    // context anyway; the payload below carries the message.
    std::panic::set_hook(Box::new(|_| {}));
    let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| dispatch(case)));
    std::panic::set_hook(hook);
    match out {
        Ok(text) => text,
        Err(payload) => {
            let msg = payload
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "a panic carrying no message".to_string());
            panic!("{} `{}` panicked: {msg}", case.at(), case.directive);
        }
    }
}

/// One failure, with both blocks laid out so the difference is the thing that stands out.
fn report(case: &Case, actual: &str) -> String {
    format!(
        "\n{} `{}{}`\n  {}\n  --- expected ---\n{}\n  --- actual ---\n{}\n",
        case.at(),
        case.directive,
        if case.args.is_empty() { String::new() } else { format!(" {}", case.args) },
        case.input.replace('\n', "\n  "),
        indent(&case.expected),
        indent(actual),
    )
}

fn indent(block: &str) -> String {
    if block.is_empty() {
        return "  (nothing)".to_string();
    }
    block.lines().map(|l| format!("  {l}")).collect::<Vec<_>>().join("\n")
}

/// Every `.test` file under `dir`, parsed, in a fixed order - so a failure list reads the same
/// on every machine.
///
/// `BIG_TEST_FILTER=<substring>` narrows to files whose path contains it, for iterating on one.
fn files(dir: &Path) -> Vec<(PathBuf, TestFile)> {
    let filter = std::env::var("BIG_TEST_FILTER").unwrap_or_default();
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("no corpus at {}: {e}", dir.display()))
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.extension().is_some_and(|e| e == "test")
                && (filter.is_empty() || p.to_string_lossy().contains(&filter))
        })
        .collect();
    paths.sort();
    assert!(!paths.is_empty(), "no .test files under {} (filter: {filter:?})", dir.display());
    paths
        .into_iter()
        .map(|path| {
            let text = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("could not read {}: {e}", path.display()));
            let file = parse(&path, &text);
            (path, file)
        })
        .collect()
}

/// Splits a file into the cases and everything between them.
fn parse(path: &Path, text: &str) -> TestFile {
    let lines: Vec<&str> = text.lines().collect();
    let mut chunks = Vec::new();
    let mut raw = String::new();
    let mut i = 0;

    while i < lines.len() {
        let line = lines[i];
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            raw.push_str(line);
            raw.push('\n');
            i += 1;
            continue;
        }

        if !raw.is_empty() {
            chunks.push(Chunk::Raw(std::mem::take(&mut raw)));
        }

        let directive_line = i + 1;
        let (directive, args) = match line.split_once(char::is_whitespace) {
            Some((d, rest)) => (d.to_string(), rest.trim().to_string()),
            None => (line.to_string(), String::new()),
        };
        i += 1;

        let mut input: Vec<&str> = Vec::new();
        while i < lines.len() && lines[i] != "----" && !lines[i].trim().is_empty() {
            input.push(lines[i]);
            i += 1;
        }

        let mut expected: Vec<&str> = Vec::new();
        if i < lines.len() && lines[i] == "----" {
            i += 1;
            while i < lines.len() && !lines[i].trim().is_empty() {
                expected.push(lines[i]);
                i += 1;
            }
        }

        chunks.push(Chunk::Case(Case {
            directive,
            args,
            input: input.join("\n"),
            expected: expected.join("\n"),
            file: path.to_path_buf(),
            line: directive_line,
        }));
    }

    if !raw.is_empty() {
        chunks.push(Chunk::Raw(raw));
    }
    TestFile { path: path.to_path_buf(), chunks }
}

/// The file as text, which for an untouched file is the text it was read from.
fn render(file: &TestFile) -> String {
    let mut out = String::new();
    for chunk in &file.chunks {
        match chunk {
            Chunk::Raw(text) => out.push_str(text),
            Chunk::Case(case) => {
                out.push_str(&case.directive);
                if !case.args.is_empty() {
                    out.push(' ');
                    out.push_str(&case.args);
                }
                out.push('\n');
                if !case.input.is_empty() {
                    out.push_str(&case.input);
                    out.push('\n');
                }
                // No block for a directive that printed nothing, so a `statement ok` stays two
                // lines rather than growing a `----` that says the same thing.
                if !case.expected.is_empty() {
                    out.push_str("----\n");
                    out.push_str(&case.expected);
                    out.push('\n');
                }
            }
        }
    }
    if !out.ends_with('\n') {
        out.push('\n');
    }
    let _ = &file.path;
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cases(text: &str) -> Vec<Case> {
        parse(Path::new("x.test"), text)
            .chunks
            .into_iter()
            .filter_map(|c| match c {
                Chunk::Case(case) => Some(case),
                Chunk::Raw(_) => None,
            })
            .collect()
    }

    #[test]
    fn a_case_is_a_directive_a_statement_and_a_block() {
        let [case] = cases("plan\nSELECT 1\n----\nCount t\n").try_into().ok().unwrap();
        assert_eq!(case.directive, "plan");
        assert_eq!(case.args, "");
        assert_eq!(case.input, "SELECT 1");
        assert_eq!(case.expected, "Count t");
        assert_eq!(case.line, 1);
    }

    #[test]
    fn the_rest_of_the_directive_line_is_its_arguments_whole() {
        let [case] = cases("same Count(Row(a > 1))\nSELECT 1\n").try_into().ok().unwrap();
        assert_eq!(case.directive, "same");
        // Whole, not split: a PQL call has spaces in it.
        assert_eq!(case.args, "Count(Row(a > 1))");
        assert_eq!(case.expected, "");
    }

    #[test]
    fn a_statement_may_span_lines_and_the_line_number_is_the_directives() {
        let text = "# a note\n\nplan\nSELECT a\n  FROM t\n----\nRows t\n";
        let [case] = cases(text).try_into().ok().unwrap();
        assert_eq!(case.input, "SELECT a\n  FROM t");
        assert_eq!(case.line, 3);
    }

    /// The property a rewrite rests on: a file nothing changed comes back byte for byte, notes
    /// and blank lines included.
    #[test]
    fn a_file_round_trips_through_the_parser() {
        let text = "\
# What this file claims.

plan
SELECT count(*) FROM t
----
Count t
└── all

# Two in a row, one of which prints nothing.
error
SELECT * FROM a, b
----
sql_no_joins

statement
CREATE TABLE t (a UINT(8))
";
        assert_eq!(render(&parse(Path::new("x.test"), text)), text);
    }

    #[test]
    fn a_file_of_nothing_but_notes_has_no_cases() {
        assert!(cases("# just a note\n\n# and another\n").is_empty());
    }
}
