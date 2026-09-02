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

//! SQL against a real database: statements that build a table, and the rows they answer with.
//!
//! # Why the corpus lives here and not in `big-api`
//!
//! This is the first layer at which *a whole statement* runs. `big-api` plans and executes a
//! query, but a `CREATE TABLE` becomes a schema change that goes to the leader and then
//! everywhere, and an `INSERT` becomes facts routed to the shards that own their records - so
//! the one function that takes any statement and answers with rows is `Cluster::sql`. A corpus
//! written in SQL needs that function, and `Cluster::solo` gives it over an in-memory database
//! with no socket anywhere.
//!
//! # Why the answers are checked twice
//!
//! A number written by hand can be wrong, and it can be wrong in a way that a test written from
//! the same misunderstanding will agree with. So the `same` cases ask each question twice -
//! once in SQL, once in the query language it translates into - and insist the two answers are
//! identical before either is written down. A disagreement between the two surfaces cannot be
//! anything but a bug in the translation.
//!
//! That is the same claim `big-sql`'s corpus makes about plans, held one layer lower: there, two
//! statements resolve to one plan; here, two surfaces produce one answer out of real data.
//!
//! # The directives
//!
//! | directive | what it does |
//! |---|---|
//! | `statement` | runs it, expecting success. Prints nothing, or the refusal |
//! | `exec` | the same, printing the one-cell answer a change comes back with |
//! | `query [rowsort]` | runs it and prints the rows |
//! | `same <pql>` | the same, having checked the query language answers identically |
//! | `error` | expects a refusal and prints its stable code |
//!
//! `BIG_REWRITE=1 cargo test -p big-cluster --test logic` regenerates every expected block.

use std::path::PathBuf;

use big_api::{Api, Datum, MemPager, QueryOptions, ResultSet};
use big_cluster::{Cluster, ClusterError};
use big_testfile::Case;

#[test]
fn the_corpus() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/logic");
    // One database per file, so a file is a story that starts from nothing and no file can be
    // made to pass by something another one created.
    let mut db: Option<(PathBuf, Cluster<MemPager>)> = None;
    big_testfile::run(dir, move |case| {
        let fresh = db.as_ref().is_none_or(|(path, _)| path != &case.file);
        if fresh {
            db = Some((case.file.clone(), Cluster::solo(Api::in_memory().unwrap())));
        }
        dispatch(&db.as_ref().unwrap().1, case)
    });
}

fn dispatch(db: &Cluster<MemPager>, case: &Case) -> String {
    let sql = case.input.trim();
    match case.directive.as_str() {
        "statement" => match db.sql(sql, &opts()) {
            Ok(_) => String::new(),
            Err(e) => refusal(&e),
        },
        "exec" => match db.sql(sql, &opts()) {
            Ok((set, _)) => table(&set, false),
            Err(e) => refusal(&e),
        },
        "query" => match db.sql(sql, &opts()) {
            Ok((set, _)) => table(&set, case.words().contains(&"rowsort")),
            Err(e) => refusal(&e),
        },
        "same" => same(db, sql, &case.args, case.words().contains(&"rowsort")),
        "error" => match db.sql(sql, &opts()) {
            // A statement that stopped being refused is a change to the surface, and reads here
            // as one failing case rather than as a crashed corpus.
            Ok(_) => "accepted".to_string(),
            Err(e) => refusal(&e),
        },
        other => format!("unknown directive `{other}`"),
    }
}

fn opts() -> QueryOptions {
    QueryOptions::default()
}

/// The stable code a failure carries, reaching past the cluster's own `internal`.
///
/// A statement refused for what it says fails locally, and the cluster reports every local
/// failure under one code because from its side that is all they are. The corpus is about the
/// statement, so it asks the engine's error what it was.
fn refusal(e: &ClusterError) -> String {
    match e {
        ClusterError::Local(e) => format!("error: {}", e.code()),
        other => format!("error: {}", other.code()),
    }
}

/// A result set as a block of aligned columns.
///
/// Aligned rather than tab-separated because these blocks are read in a diff, and a column that
/// moves when a number grows a digit is a column somebody has to count spaces in.
fn table(set: &ResultSet, sorted: bool) -> String {
    let mut rows: Vec<Vec<String>> =
        set.rows.iter().map(|r| r.iter().map(datum).collect()).collect();
    // `rowsort` is for the answers whose order is not part of the claim - a projection reads
    // records in whatever order the shards hand them over. An answer that *is* ordered says so
    // by not asking for this.
    if sorted {
        rows.sort();
    }
    let header: Vec<String> = set.columns.clone();
    let widths: Vec<usize> = (0..header.len())
        .map(|i| {
            rows.iter()
                .map(|r| r.get(i).map_or(0, |c| c.chars().count()))
                .chain(std::iter::once(header[i].chars().count()))
                .max()
                .unwrap_or(0)
        })
        .collect();
    let line = |cells: &[String]| {
        cells
            .iter()
            .enumerate()
            .map(|(i, c)| format!("{c:<width$}", width = widths.get(i).copied().unwrap_or(0)))
            .collect::<Vec<_>>()
            .join("  ")
            .trim_end()
            .to_string()
    };
    let mut out = vec![line(&header)];
    out.extend(rows.iter().map(|r| line(r)));
    out.join("\n")
}

/// One cell, spelled the way the answer means it rather than the way one format writes it.
fn datum(d: &Datum) -> String {
    match d {
        // Not zero, and the corpus has to be able to tell them apart: a `min` over no records
        // and a `sum` over none are different answers.
        Datum::Null => "NULL".to_string(),
        Datum::Int(v) => v.to_string(),
        Datum::Dec { units, scale } => big_api::fixed(*units, *scale),
        Datum::Real(v) => format!("{v}"),
        // Spelled the way every output format spells them, so a corpus answer and a client's
        // answer cannot come to disagree about what a date is.
        Datum::Date(d) => big_api::date_text(*d),
        Datum::Timestamp(t) => big_api::timestamp_text(*t),
        Datum::Text(s) => s.clone(),
        Datum::Keys(keys) => format!("[{}]", keys.join(", ")),
    }
}

/// Asks the same question in both surfaces and insists they agree before printing the answer.
///
/// The comparison is of the raw values each surface's plans produced, not of the rendered rows:
/// rendering is the shape's job and the query language has no shape, so comparing text would be
/// comparing one surface's answer against the other's answer *plus a renderer*.
///
/// One question per statement, which is what makes the comparison mean anything: the other
/// surface cannot ask two at once, so a statement making several plans would be compared against
/// the first of them and the case would say less than it looks like it says.
fn same(db: &Cluster<MemPager>, sql: &str, pql: &str, sorted: bool) -> String {
    let api = db.local();
    // Translated first, for the table alone. The case does not repeat it, so it cannot quietly
    // ask the other surface about a different one.
    let table_name = match api.translate(sql) {
        Ok(big_api::Sql::Query(statement)) => match statement.tables()[..] {
            [one] => one.to_string(),
            _ => return "reads more than one table, and this comparison takes one".to_string(),
        },
        Ok(_) => return "not a query".to_string(),
        Err(e) => return format!("error: {}", e.code()),
    };
    let (from_sql, answer) = match api.sql(sql, &opts()) {
        Ok(pair) => pair,
        Err(e) => return format!("error: {}", e.code()),
    };
    let [value] = from_sql.as_slice() else {
        return format!("made {} plans, and this comparison takes one", from_sql.len());
    };
    let from_pql = match api.query(&table_name, pql) {
        Ok(v) => v,
        Err(e) => return format!("`{pql}` did not run: {e}"),
    };
    if format!("{value:?}") != format!("{from_pql:?}") {
        return format!("THE TWO SURFACES DISAGREE\nfrom sql: {value:?}\nfrom pql: {from_pql:?}");
    }
    table(&big_api::result_set(&answer, &from_sql), sorted)
}
