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
//! that runs against real data is `big-embed`'s.
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
//! | `delete` | the table, and the plan the filter selects by |
//! | `update` | the assignments, and the plan the filter selects by |
//! | `show` | the question about the catalog |
//! | `explain` | what `EXPLAIN` answers with, line for line |
//! | `render` | a `CREATE TABLE` written back out of the columns it declared |
//!
//! `BIG_REWRITE=1 cargo test -p big-sql --test testdata` regenerates every expected block.

use big_plan::{FieldClass, Keyed, Plan, Schema, TimeUnit};
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
            "rate" => FieldClass::Float { bits: 64 },
            "day" => FieldClass::Temporal { unit: TimeUnit::Days },
            "seen" => FieldClass::Temporal { unit: TimeUnit::Seconds },
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
        [
            "amount", "price", "balance", "category", "country", "device", "visit", "active",
            "rate", "day", "seen",
        ]
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
        "acl" => with(sql, |s| match s {
            Sql::Acl(a) => explain::acl(&a),
            other => not(&other, "a change to who may do what"),
        }),
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
        // Exactly what a client gets back from `EXPLAIN`, line for line.
        //
        // The rows it becomes are `big-embed`'s, but every character of the text is this crate's -
        // which is why the format is pinned here, at parser-test speed against `Stub`, rather
        // than only where a database is running.
        "explain" => with(sql, |s| match s {
            Sql::Explain { mode, inner } => explained(mode, *inner),
            other => not(&other, "an explanation"),
        }),
        // The call a delete selects by, so the corpus can pin that it is **the same tree** the
        // equivalent `SELECT *` produces. Two lowerings of one predicate is two sets waiting to
        // differ, and the one that differed would be the one doing the deleting.
        // The same, for an update: the assignments and the tree it selects by.
        "update" => with(sql, |s| match s {
            Sql::Update(u) => match big_plan::plan(&u.qualified(), &u.rows, &Stub) {
                Ok(plan) => format!(
                    "update {} set {}\n{}",
                    u.qualified(),
                    u.assignments
                        .iter()
                        .map(|(c, v)| format!("{c} = {}", big_sql::explain::literal_of(v)))
                        .collect::<Vec<_>>()
                        .join(", "),
                    big_plan::explain(&plan)
                ),
                Err(e) => format!("did not resolve: {e}"),
            },
            other => not(&other, "an update"),
        }),
        "delete" => with(sql, |s| match s {
            Sql::Delete(d) => match big_plan::plan(&d.qualified(), &d.rows, &Stub) {
                Ok(plan) => format!("delete {}\n{}", d.qualified(), big_plan::explain(&plan)),
                Err(e) => format!("did not resolve: {e}"),
            },
            other => not(&other, "a delete"),
        }),
        // The clause itself, so the corpus pins which keys are read and what each becomes.
        "settings" => with(sql, |s| match s {
            Sql::Settings { settings, .. } => format!("{settings:?}"),
            other => not(&other, "a bounded statement"),
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
        // **A budget is unwrapped and not printed here, which is the claim worth pinning.** A
        // statement with a `SETTINGS` clause lowers to exactly the plan and shape the same
        // statement without one lowers to - the clause changes what the executor gives it, not
        // what it asks. Any corpus case writing both spellings expects one block.
        Sql::Settings { inner, .. } => match *inner {
            Sql::Query(q) => f(q),
            other => not(&other, "a query"),
        },
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
        Sql::Acl(_) => "a change to who may do what",
        Sql::Explain { .. } => "an explanation",
        Sql::Delete(_) => "a delete",
        Sql::Update(_) => "an update",
        Sql::Kill(_) => "a kill",
        Sql::Settings { .. } => "a bounded statement",
    };
    format!("not {wanted}: {what}")
}

/// An explanation, resolved against [`Stub`] exactly as a node would resolve it against a real
/// schema - which is what makes the answer below the one a client would have read.
fn explained(mode: big_sql::ExplainMode, inner: Sql) -> String {
    use big_sql::explain::{Explained, Probed};
    match inner {
        Sql::Query(statement) => {
            // **Collected, never filtered**, which is what `Cluster::sql_explain` does with the
            // same two lists - and the difference is not tidiness. A call that does not resolve
            // is what an `EXPLAIN` reports; dropping it instead renumbers everything after it,
            // and the shape names its plans by index, so `plan #0` here would print the tree
            // the shape calls `#1`. A corpus that pinned that would be pinning two halves that
            // contradict each other, and `BIG_REWRITE=1` would write it in without a word.
            let plans = match statement
                .calls
                .iter()
                .map(|ask| big_plan::plan(&ask.table, &ask.call, &Stub))
                .collect::<core::result::Result<Vec<_>, _>>()
            {
                Ok(plans) => plans,
                Err(e) => return format!("did not resolve: {e}"),
            };
            let probes = match statement
                .probes
                .iter()
                .map(|p| {
                    big_plan::plan(&p.table, &p.rows, &Stub).map(|rows| Probed { probe: p, rows })
                })
                .collect::<core::result::Result<Vec<_>, _>>()
            {
                Ok(probes) => probes,
                Err(e) => return format!("did not resolve: {e}"),
            };
            let answer = resolved_answer(statement.clone());
            big_sql::explain::explained(
                mode,
                &Explained::Query { plans: &plans, probes: &probes, answer: &answer },
            )
        }
        Sql::Ddl(d) => big_sql::explain::explained(mode, &Explained::Ddl(&d)),
        Sql::Acl(a) => big_sql::explain::explained(mode, &Explained::Acl(&a)),
        Sql::Insert(i) => big_sql::explain::explained(mode, &Explained::Insert(&i)),
        Sql::Show(s) => big_sql::explain::explained(mode, &Explained::Show(&s)),
        // The parser refuses a second `EXPLAIN`, so no statement in the corpus reaches this.
        Sql::Explain { .. } => "explain of an explain".to_string(),
        // Nothing is resolved for a kill: it names a query id, not an object in the catalog.
        Sql::Kill(id) => big_sql::explain::explained(mode, &Explained::Kill(&id)),
        // Resolved against `Stub`, the same way a query's plans are - which is what makes
        // `EXPLAIN DELETE` a way to check a predicate before running it against records that do
        // not come back.
        Sql::Delete(d) => match big_plan::plan(&d.qualified(), &d.rows, &Stub) {
            Ok(plan) => big_sql::explain::explained(
                mode,
                &Explained::Delete { table: &d.qualified(), rows: &plan },
            ),
            Err(e) => format!("did not resolve: {e}"),
        },
        Sql::Update(u) => match big_plan::plan(&u.qualified(), &u.rows, &Stub) {
            Ok(plan) => big_sql::explain::explained(
                mode,
                &Explained::Update {
                    table: &u.qualified(),
                    assignments: &u.assignments,
                    rows: &plan,
                },
            ),
            Err(e) => format!("did not resolve: {e}"),
        },
        // **The budget is unwrapped and not printed, because this printer cannot know it.** The
        // `settings` line a client reads carries the *effective* numbers - the minimum of what
        // the statement asked for and what the operator configured - and the second of those is
        // a fact about a running server, which a parser test has none of. So the line is added
        // where the options are, in `Cluster::sql_explain`, and pinned in `big-http`'s tests;
        // what this corpus pins is that everything else about the explanation is unchanged.
        Sql::Settings { inner, .. } => explained(mode, *inner),
    }
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
