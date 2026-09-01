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

#![no_main]

//! The SQL surface, which is the second entry point a stranger reaches directly.
//!
//! `POST /sql` hands `translate` whatever arrived in the body, so the contract is the query
//! parser's: return `Result`, never panic. Two things make this a larger surface than
//! `parse_pql`.
//!
//! **It is recursive descent over a much bigger grammar.** `WHERE ((((((...` is a stack overflow
//! rather than an error, which a `Result` cannot express, so the depth bound is part of what is
//! being fuzzed - if it is ever removed this target stops returning and starts crashing, which
//! is the point.
//!
//! **A statement is four different things.** A query, an insert, a listing and a schema change
//! leave by four different doors, and each carries structure the other three do not. So a
//! successful translation is walked afterwards: printing it traverses the same tree every reader
//! downstream will, and a value the parser can build but nothing else can look at fails here
//! rather than in a request.
//!
//! `crates/big-sql/tests/gates.rs` runs the same contract over every prefix and one-byte cut of
//! the test corpus, in the ordinary test suite. This one has no bound on its inputs.

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Invalid UTF-8 never reaches the translation: the HTTP layer decodes the body first, so
    // feeding it here would spend the fuzzer's budget on a case production cannot produce.
    let Ok(text) = core::str::from_utf8(data) else { return };

    let Ok(statement) = big_sql::translate(text) else { return };

    let printed = format!("{statement:?}");
    assert!(!printed.is_empty());

    match statement {
        // A query's shape names its plans by index, and a shape naming a plan the statement did
        // not make is a panic waiting for a caller that reads the answers. Checked here because
        // it is a property of the translation, and the caller that would find it is a long way
        // from the text that caused it.
        big_sql::Sql::Query(s) => {
            // `Shape::plans` rather than the cells' own, because a shape names plans no cell
            // does: the groups that drive its rows, and the sides a join's cells are scaled
            // against by position.
            for plan in s.answer.shape.plans() {
                assert!(
                    plan < s.calls.len(),
                    "a shape reads plan #{plan} of {} in `{text}`",
                    s.calls.len()
                );
            }
            for cell in s.answer.shape.cells() {
                // A join's cell names its side by position in the join's keys, so a shape whose
                // cell points past them would read a side that is not there.
                if let big_sql::Of::Paired { side, .. } = cell.of {
                    let sides = match &s.answer.shape {
                        big_sql::Shape::Join { sides, .. } => sides.len(),
                        _ => 0,
                    };
                    assert!(side < sides, "a cell reads side #{side} of {sides} in `{text}`");
                }
                // A search is indexed into its own list, which is why `Of::plans` does not
                // return it and why the bound here is a different number.
                if let big_sql::Of::Probe { probe } = cell.of {
                    assert!(
                        probe < s.probes.len(),
                        "a cell reads probe #{probe} of {} in `{text}`",
                        s.probes.len()
                    );
                }
            }
            // The tree the plan printer walks, over the same statement.
            assert!(!big_sql::explain::answer(&s.answer).is_empty());
        }
        // Every tuple as wide as the column list, and the record column - when there is one -
        // inside it. Both are the parser's to guarantee, and `Insert::record` is written on the
        // assumption that it did.
        big_sql::Sql::Insert(i) => {
            assert!(i.id_at.is_none_or(|at| at < i.columns.len()));
            for row in &i.rows {
                assert_eq!(row.len(), i.columns.len(), "a ragged tuple in `{text}`");
                let _ = i.record(row);
            }
            assert!(!big_sql::explain::insert(&i).is_empty());
        }
        big_sql::Sql::Show(s) => assert!(!big_sql::explain::show(&s).is_empty()),
        big_sql::Sql::Ddl(d) => {
            // A schema change is written back out and read again, which is the round trip
            // `render` and `parse::column_type` are two halves of.
            if let big_sql::Ddl::CreateTable { table, engine, columns, .. } = &d {
                let written = big_sql::render::create_table(table, engine.as_deref(), columns);
                match big_sql::translate(&written) {
                    Ok(big_sql::Sql::Ddl(big_sql::Ddl::CreateTable { columns: back, .. })) => {
                        assert_eq!(&back, columns, "`{text}` did not survive `{written}`");
                    }
                    other => panic!("`{written}` came back as {other:?}"),
                }
            }
            assert!(!big_sql::explain::ddl(&d).is_empty());
        }
    }
});
