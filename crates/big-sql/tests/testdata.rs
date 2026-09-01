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

//! The SQL surface, statement by statement, in files.
//!
//! # What this is for, next to `tests/translate`
//!
//! `tests/translate` states the claims: the two surfaces resolve to the same plan, a refusal
//! says what exists instead. This is the breadth those claims are held over - every clause,
//! every type name, every refusal, each written as two lines and an expected answer.
//!
//! Nothing here runs a database. `translate` needs no schema and the [`Stub`] below is a trait
//! impl of thirty lines, so a case in this corpus costs what a parser test costs. The corpus
//! that runs against real data is `big-api`'s.
//!
//! # The directives
//!
//! | directive | what it prints |
//! |---|---|
//! | `plan` | one tree per call the statement makes, and per search |
//! | `shape` | the answer: its columns, `HAVING`, `ORDER BY` and cut |
//! | `same <pql>` | the plan both surfaces resolve to, having checked that they agree |
//! | `error` | the stable code the refusal carries |
//! | `ddl` | the schema change |
//! | `insert` | the write |
//! | `show` | the question about the catalog |
//! | `render` | a `CREATE TABLE` written back out of the columns it declared |
//!
//! `BIG_REWRITE=1 cargo test -p big-sql --test testdata` regenerates every expected block.

use big_plan::{FieldClass, Keyed, Plan, Schema};
use big_sql::{explain, Sql};
use big_testfile::Case;

/// Two tables with the same columns, which is what makes `t.category = u.category` a join
/// rather than a type error.
///
/// Every class the planner distinguishes appears once, because a corpus of type-dependent
/// behaviour needs one column of each to be about: a scale that converts a written value, a
/// signed field where `-1` is a legal bound, a time quantum field a window has views to read,
/// and a plain set where it has none.
struct Stub;

impl Schema for Stub {
    // `u` and `v` are the further tables a join resolves against: two for the ordinary join,
    // three for the star.
    //
    // Each of them also exists in `sales` and in `ops`, which is what lets a corpus case be
    // about a qualified name at all - and what makes the cross-database join in
    // `database.test` a case rather than an assertion about an error message.
    fn has_table(&self, table: &str) -> bool {
        let (database, table) = match table.split_once('.') {
            Some((d, t)) => (d, t),
            None => ("default", table),
        };
        matches!(database, "default" | "sales" | "ops") && matches!(table, "t" | "u" | "v")
    }

    fn field_class(&self, table: &str, field: &str) -> Option<FieldClass> {
        if !self.has_table(table) {
            return None;
        }
        // Every copy of a table has the same columns, whichever database it is in: what a
        // database changes is which table a name reaches, not what the table holds.
        Some(match field {
            "amount" => FieldClass::Integer { scale: 0 },
            "price" => FieldClass::Integer { scale: 2 },
            "balance" => FieldClass::Signed,
            "category" | "country" => FieldClass::Keyed(Keyed::Set),
            "device" => FieldClass::Keyed(Keyed::Mutex),
            "visit" => FieldClass::Keyed(Keyed::Time),
            "active" => FieldClass::Boolean,
            _ => return None,
        })
    }

    /// The order a column list would have declared them in, which is the order `SELECT *`
    /// answers in. Written out rather than derived from the match above, because that match is
    /// a lookup and this is an ordering - and the corpus asserts the ordering.
    fn fields(&self, table: &str) -> Vec<String> {
        if !self.has_table(table) {
            return Vec::new();
        }
        ["amount", "price", "balance", "category", "country", "device", "visit", "active"]
            .iter()
            .filter(|f| self.field_class(table, f).is_some())
            .map(|f| (*f).to_string())
            .collect()
    }

    /// `t` keeps its values and `u` does not, so that the refusal a projection meets over an
    /// index-only table is reachable from this corpus without a second schema.
    fn stores_values(&self, table: &str) -> bool {
        table.rsplit('.').next() == Some("t")
    }
}

#[test]
fn the_corpus() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/testdata");
    big_testfile::run(dir, dispatch);
}

fn dispatch(case: &Case) -> String {
    let sql = case.input.trim();
    match case.directive.as_str() {
        "plan" => with_query(sql, plans),
        "shape" => with_query(sql, |s| explain::answer(&resolved_answer(s))),
        "same" => same(sql, &case.args),
        "error" => match big_sql::translate(sql) {
            // Not a panic: a statement that stopped being refused is a change to the surface,
            // and it should read as one failing case rather than as a crashed corpus.
            Ok(_) => "accepted".to_string(),
            Err(e) => e.code().to_string(),
        },
        "ddl" => with(sql, |s| match s {
            Sql::Ddl(d) => explain::ddl(&d),
            other => not(&other, "a schema change"),
        }),
        "insert" => with(sql, |s| match s {
            Sql::Insert(i) => explain::insert(&i),
            other => not(&other, "a write"),
        }),
        "show" => with(sql, |s| match s {
            Sql::Show(s) => explain::show(&s),
            other => not(&other, "a question about the catalog"),
        }),
        "render" => with(sql, |s| match s {
            Sql::Ddl(big_sql::Ddl::CreateTable {
                database: None, table, engine, columns, ..
            }) => big_sql::render::create_table(&table, engine.as_deref(), &columns),
            other => not(&other, "a CREATE TABLE"),
        }),
        other => format!("unknown directive `{other}`"),
    }
}

/// Translates, and hands the result to `f` - or answers with the refusal instead.
///
/// Every directive goes through here so that a refused statement reads the same wherever it
/// turns up: one line saying which code, which diffs cleanly against the tree that was
/// expected.
fn with(sql: &str, f: impl FnOnce(Sql) -> String) -> String {
    match big_sql::translate(sql) {
        Ok(s) => f(s),
        Err(e) => format!("error: {}", e.code()),
    }
}

fn with_query(sql: &str, f: impl FnOnce(big_sql::Statement) -> String) -> String {
    with(sql, |s| match s {
        Sql::Query(q) => f(q),
        other => not(&other, "a query"),
    })
}

/// What a statement turned out to be, when a directive wanted something else.
fn not(sql: &Sql, wanted: &str) -> String {
    let what = match sql {
        Sql::Query(_) => "a query",
        Sql::Insert(_) => "a write",
        Sql::Show(_) => "a question about the catalog",
        Sql::Ddl(_) => "a schema change",
    };
    format!("not {wanted}: {what}")
}

/// One tree per call, then one per search, in the order a caller would run them.
///
/// Consecutive trees need no separator: every root line starts at column zero and every other
/// line is indented, so where one ends and the next begins is unambiguous.
fn plans(statement: big_sql::Statement) -> String {
    let mut out: Vec<String> = Vec::new();
    for ask in &statement.calls {
        out.push(match big_plan::plan(&ask.table, &ask.call, &Stub) {
            Ok(plan) => big_plan::explain(&plan),
            Err(e) => format!("did not resolve: {e}"),
        });
    }
    for probe in &statement.probes {
        // A search is not a plan, and printing it as one would hide what it costs: it is the
        // same `Count` asked with moving bounds until it converges, about a round trip per bit
        // of the field's depth.
        let head = format!("Probe {}.{} per_mille={}", probe.table, probe.field, probe.per_mille);
        out.push(match big_plan::plan(&probe.table, &probe.rows, &Stub) {
            Ok(rows) => {
                let printed = big_plan::explain(&rows);
                let under = printed.split_once('\n').map(|(_, rest)| rest).unwrap_or("└── all");
                format!("{head}\n{under}")
            }
            Err(e) => format!("{head}\ndid not resolve: {e}"),
        });
    }
    out.join("\n")
}

/// The shape, with every written threshold and unit put against the schema.
///
/// Unresolved is not the interesting state: it is what a shape is between the lowering and the
/// planning, and a corpus of it would be a corpus of a value nothing ever reads. The printer
/// still marks the unresolved forms, so a shape that reached here without being resolved says
/// so rather than printing as a correct one.
fn resolved_answer(statement: big_sql::Statement) -> big_sql::Answer {
    let big_sql::Answer { shape, format, calls } = statement.answer;
    // A shape that will not resolve is printed unresolved rather than swallowed: the printer
    // marks those forms, so the case shows what happened instead of quietly passing.
    let shape = shape.clone().resolve(&Stub).unwrap_or(shape);
    big_sql::Answer { shape, format, calls }
}

/// The claim `tests/translate` is built on, held at one more point: this statement and this
/// query-language call resolve to the same plan.
///
/// The PQL is asked of the table the statement names, rather than of a table written into the
/// case - the claim is about what the statement means, and the statement says which table.
fn same(sql: &str, pql: &str) -> String {
    let statement = match big_sql::translate(sql) {
        Ok(Sql::Query(q)) => q,
        Ok(other) => return not(&other, "a query"),
        Err(e) => return format!("error: {}", e.code()),
    };
    let [ask] = statement.calls.as_slice() else {
        return format!("made {} plans, and this comparison takes one", statement.calls.len());
    };
    let from_sql = match big_plan::plan(&ask.table, &ask.call, &Stub) {
        Ok(p) => p,
        Err(e) => return format!("the statement did not resolve: {e}"),
    };
    let call = match big_plan::parse(pql) {
        Ok(c) => c,
        Err(e) => return format!("`{pql}` did not parse: {e}"),
    };
    let from_pql = match big_plan::plan(&ask.table, &call, &Stub) {
        Ok(p) => p,
        Err(e) => return format!("`{pql}` did not resolve: {e}"),
    };
    if from_sql == from_pql {
        // The shared plan, which is what the case is claiming. Printing it rather than "ok"
        // keeps the file readable as documentation of the translation, and keeps a rewrite from
        // being able to hide a disagreement behind a word.
        big_plan::explain(&from_sql)
    } else {
        disagreement(&from_sql, &from_pql)
    }
}

fn disagreement(from_sql: &Plan, from_pql: &Plan) -> String {
    format!(
        "THE TWO SURFACES DISAGREE\nfrom sql:\n{}\nfrom pql:\n{}",
        big_plan::explain(from_sql),
        big_plan::explain(from_pql)
    )
}
