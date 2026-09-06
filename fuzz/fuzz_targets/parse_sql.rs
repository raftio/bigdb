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
//! **A statement is nine different things.** A query, an insert, a delete, an update, a listing,
//! a schema change, a change to who may do what and a kill leave by eight different doors; an
//! `EXPLAIN` wraps whichever of them it was given and a `SETTINGS` clause wraps the ones that
//! spend something, and each carries structure the others do not. So a successful translation is
//! walked afterwards: printing it traverses the same tree every reader downstream will, and a
//! value the parser can build but nothing else can look at fails here rather than in a request.
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
            for row in i.values() {
                assert_eq!(row.len(), i.columns.len(), "a ragged tuple in `{text}`");
                let _ = i.record(row);
            }
            // The other source: values read by a query. The three rules the parser promises
            // about that form, none of which the type holds - as many selected columns as the
            // statement names, no id column, since the form always allocates, and a source
            // table that is not the target, which is the read that would feed its own write.
            if let Some(select) = i.select() {
                assert_eq!(select.items.len(), i.columns.len(), "a mismatched width in `{text}`");
                assert!(i.id_at.is_none(), "an id column read from a query in `{text}`");
                assert!(
                    select.from.table != i.table || select.from.database != i.database,
                    "a statement that reads what it writes in `{text}`"
                );
            }
            assert!(!big_sql::explain::insert(&i).is_empty());
        }
        big_sql::Sql::Show(s) => assert!(!big_sql::explain::show(&s).is_empty()),
        // **The three promises the parser makes about a delete that its type does not hold.**
        // The `WHERE` is required, so the call is never a bare `All()` that would clear the
        // table; the table name resolves to something; and the tables the filter reads are
        // collected for `demands`, so a name in that list that the statement never wrote would
        // be a privilege demanded on an object nobody asked about.
        big_sql::Sql::Delete(d) => {
            assert!(!d.qualified().is_empty(), "a delete with no table in `{text}`");
            assert!(d.rows.name != "All", "a delete with no filter in `{text}`");
            for read in &d.reads {
                assert!(!read.is_empty(), "a delete reading an unnamed table in `{text}`");
            }
        }
        // The same, and one more: the assignment list is never empty - the parser loops at
        // least once - and no assignment names the record column, which is refused where it is
        // written because a record id is an address rather than a value in a column.
        big_sql::Sql::Update(u) => {
            assert!(!u.qualified().is_empty(), "an update with no table in `{text}`");
            assert!(u.rows.name != "All", "an update with no filter in `{text}`");
            assert!(!u.assignments.is_empty(), "an update assigning nothing in `{text}`");
            for (column, _) in &u.assignments {
                assert_ne!(column, big_sql::RECORD_COLUMN, "an update of the id in `{text}`");
            }
        }
        // A kill is one string, and explaining it needs no schema - which is the whole of what
        // there is to walk.
        big_sql::Sql::Kill(id) => {
            assert!(!big_sql::explain::explained(
                big_sql::ExplainMode::All,
                &big_sql::explain::Explained::Kill(&id)
            )
            .is_empty());
        }
        // **The wrapper is never empty and never doubled**, neither of which the type holds:
        // the parser leaves it off entirely when no key was written, and reads the clause once
        // per statement. A `max_delete_records` outside a delete is refused in `finish`, so a
        // statement carrying one anywhere else got past a check that is supposed to be total.
        big_sql::Sql::Settings { settings, inner } => {
            assert!(!settings.is_empty(), "an empty SETTINGS wrapper in `{text}`");
            assert!(
                !matches!(*inner, big_sql::Sql::Settings { .. }),
                "`{text}` carried two SETTINGS clauses"
            );
            assert!(
                settings.max_delete_records.is_none()
                    || matches!(*inner, big_sql::Sql::Delete(_)),
                "`{text}` bounded a delete that is not one"
            );
        }
        // A grant is wholly in the parse tree, so the only thing left to walk is its printer.
        big_sql::Sql::Acl(a) => assert!(!big_sql::explain::acl(&a).is_empty()),
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
        // An `EXPLAIN` is the statement under it, so the properties worth fuzzing are the ones
        // already checked above - reached by feeding the inner statement back through this
        // target rather than by a second copy of every assertion.
        //
        // **The two rules the wrapper itself has to keep**, neither of which the type enforces:
        // the parser refuses a second `EXPLAIN`, so the inner statement is never one; and the
        // printer promises no blank line, because the corpora end an expected block at one and a
        // blank line would silently truncate a case to half an answer.
        big_sql::Sql::Explain { mode, inner } => {
            assert!(
                !matches!(*inner, big_sql::Sql::Explain { .. }),
                "`{text}` nested an EXPLAIN the parser is supposed to refuse"
            );
            // The statement's own printer, taken alongside the wrapper that labels it.
            //
            // **Asserted on the part, not on what `explained` returns.** That function filters
            // blank lines out itself, so a check downstream of the filter is a check of the
            // filter and passes whatever the printers do. The rule that can actually be broken
            // is this one - a printer that emitted a blank line would have it silently
            // swallowed here and would truncate a corpus case in the middle, which reads as a
            // passing test of half an answer.
            let (described, part) = match &*inner {
                big_sql::Sql::Ddl(d) => {
                    (big_sql::explain::Explained::Ddl(d), big_sql::explain::ddl(d))
                }
                big_sql::Sql::Insert(i) => {
                    (big_sql::explain::Explained::Insert(i), big_sql::explain::insert(i))
                }
                big_sql::Sql::Show(s) => {
                    (big_sql::explain::Explained::Show(s), big_sql::explain::show(s))
                }
                big_sql::Sql::Acl(a) => {
                    (big_sql::explain::Explained::Acl(a), big_sql::explain::acl(a))
                }
                big_sql::Sql::Kill(id) => {
                    (big_sql::explain::Explained::Kill(id), format!("kill {id}"))
                }
                // A query's half needs a schema to resolve, and this target links none. Its
                // plans and shape are fuzzed through the `Sql::Query` arm above instead - and a
                // delete's and an update's are resolved the same way, against a catalog, so they
                // leave by the same door.
                big_sql::Sql::Query(_)
                | big_sql::Sql::Delete(_)
                | big_sql::Sql::Update(_)
                | big_sql::Sql::Settings { .. }
                | big_sql::Sql::Explain { .. } => return,
            };
            assert!(!part.is_empty(), "`{text}` explained to nothing");
            assert!(
                !part.lines().any(|l| l.trim().is_empty()),
                "`{text}` explained with a blank line, which truncates a corpus case"
            );
            // ...and the wrapper adds the label and loses nothing, which is the other half of
            // what makes the filter above a no-op rather than an edit.
            let printed = big_sql::explain::explained(mode, &described);
            assert_eq!(
                printed.lines().count(),
                part.lines().count() + 1,
                "`{text}` explained to something other than its own printer's output under a \
                 label:\n{printed}"
            );
        }
    }
});
