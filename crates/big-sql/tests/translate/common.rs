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

//! What SQL means here, stated as: the same plan the query language produces.
//!
//! **The equivalence tests are the load-bearing ones.** A test that a statement translates into
//! some plan says only that it translated; a test that it translates into *the plan the
//! hand-written query would have produced* is what makes "SQL is a translation" a checkable
//! claim rather than a description. They compare plans rather than ASTs on purpose: the two
//! surfaces spell an equality differently - `Row(country="GB")` parses as a named argument and
//! the lowering emits a comparison - and the planner resolves both to the same thing, which is
//! the level the claim is actually made at.

pub use big_plan::{FieldClass, Keyed, Literal, Plan, Schema};
pub use big_sql::{
    Absent, Cell, Cut, GroupOrder, Having, JoinSide, Keying, Of, OrderBy, Pairing, Refused, Shape,
    SqlError, Statement, Threshold,
};

/// Translates a statement that is a query, which is every statement in this suite.
///
/// The crate's own `translate` answers with either a query or a schema change, because those are
/// two different things downstream. Every claim in these files is about a query, so unwrapping
/// here keeps each of them one line - and a schema change reaching one of them is a test that
/// has quietly stopped testing what it says, so it panics rather than being skipped.
pub fn translate(text: &str) -> big_sql::Result<Statement> {
    match big_sql::translate(text)? {
        big_sql::Sql::Query(s) => Ok(s),
        other => panic!("`{text}` is not a query: {other:?}"),
    }
}

/// The schema the benchmark's analytical table has, and nothing else.
///
/// Thirty lines and no storage, which is the point of `big-plan` taking a trait here: an
/// equivalence test costs what a parser test costs.
pub struct Stub;

impl Schema for Stub {
    // `u` and `v` exist so that a join has further tables to resolve against. They carry the
    // same fields, which is what makes `t.category = u.category` a join and not a type error -
    // and `v` is what makes a three-table star one.
    fn has_table(&self, table: &str) -> bool {
        matches!(table, "t" | "u" | "v")
    }

    fn field_class(&self, table: &str, field: &str) -> Option<FieldClass> {
        if !self.has_table(table) {
            return None;
        }
        Some(match field {
            "amount" => FieldClass::Integer { scale: 0 },
            "price" => FieldClass::Integer { scale: 2 },
            "balance" => FieldClass::Signed,
            "category" | "country" => FieldClass::Keyed(Keyed::Set),
            // A time quantum field, so a window over one has somewhere to land.
            "visit" => FieldClass::Keyed(Keyed::Time),
            "active" => FieldClass::Boolean,
            _ => return None,
        })
    }
}

/// A cell whose number comes out of a field, which is what carries that field's scale out to
/// the answer. `table` is `t` for every statement in this suite but a join.
pub fn measured(column: &str, of: big_sql::Of, table: &str, field: &str) -> big_sql::Cell {
    big_sql::Cell {
        column: column.to_string(),
        of,
        units: big_sql::Units::Written { table: table.to_string(), field: field.to_string() },
    }
}

/// The plan behind a statement that makes exactly one, which every comparison against PQL
/// needs: the other language cannot ask two questions at once, so comparing against the first
/// of several would be a test that says less than it looks like it says.
pub fn plan_sql(text: &str) -> Plan {
    let s = translate(text).unwrap_or_else(|e| panic!("`{text}` was refused: {e}"));
    let [ask] = s.calls.as_slice() else {
        panic!("`{text}` made {} plans, and this comparison takes one", s.calls.len())
    };
    big_plan::plan(&ask.table, &ask.call, &Stub)
        .unwrap_or_else(|e| panic!("`{text}` did not resolve: {e}"))
}

pub fn plan_pql(text: &str) -> Plan {
    plan_pql_of("t", text)
}

/// The same, asked of a named table - which a join needs, because its two calls are asked of
/// two different ones.
pub fn plan_pql_of(table: &str, text: &str) -> Plan {
    let call = big_plan::parse(text).unwrap_or_else(|e| panic!("`{text}` did not parse: {e}"));
    big_plan::plan(table, &call, &Stub).unwrap_or_else(|e| panic!("`{text}` did not resolve: {e}"))
}

pub fn same(sql: &str, pql: &str) {
    assert_eq!(plan_sql(sql), plan_pql(pql), "\n  sql: {sql}\n  pql: {pql}\n");
}

/// One of a statement's calls, resolved. The level every claim in this file is made at: the
/// two surfaces spell an equality differently and the planner resolves both to one thing.
pub fn resolved(sql: &str, i: usize) -> Plan {
    let s = translate(sql).unwrap_or_else(|e| panic!("`{sql}` was refused: {e}"));
    let ask = &s.calls[i];
    big_plan::plan(&ask.table, &ask.call, &Stub)
        .unwrap_or_else(|e| panic!("`{sql}` call {i} did not resolve: {e}"))
}

/// The `ORDER BY` a statement leaves for the coordinator, or `None` when it leaves none.
pub fn order_of(sql: &str) -> Option<GroupOrder> {
    match translate(sql).unwrap_or_else(|e| panic!("`{sql}` was refused: {e}")).answer.shape {
        Shape::Groups { order, .. } => order,
        other => panic!("`{sql}` is not a grouped answer: {other:?}"),
    }
}

/// The stable code a refused statement carries.
///
/// Through the crate's own `translate` rather than the one above, because a refusal is the one
/// thing both kinds of statement can be: `CREATE TABLE t (a FLOAT)` is refused as surely as a
/// join is, and routing it through a helper that panics on a schema change would leave the
/// column list untestable from here.
pub fn code(sql: &str) -> &'static str {
    match big_sql::translate(sql) {
        Ok(big_sql::Sql::Query(s)) => panic!("`{sql}` was accepted, as {:?}", s.calls),
        Ok(other) => panic!("`{sql}` was accepted, as {other:?}"),
        Err(e) => e.code(),
    }
}

/// Translates a statement that is a schema change, which is every statement in `schema`.
pub fn ddl(sql: &str) -> big_sql::Ddl {
    match big_sql::translate(sql).unwrap_or_else(|e| panic!("`{sql}` was refused: {e}")) {
        big_sql::Sql::Ddl(d) => d,
        other => panic!("`{sql}` is not a schema change: {other:?}"),
    }
}

/// Translates a statement that writes rows, which is every statement in `writes`.
pub fn insert(sql: &str) -> big_sql::Insert {
    match big_sql::translate(sql).unwrap_or_else(|e| panic!("`{sql}` was refused: {e}")) {
        big_sql::Sql::Insert(i) => i,
        other => panic!("`{sql}` is not an INSERT: {other:?}"),
    }
}

/// Translates a statement that asks about the catalog, which is every statement in
/// `introspect`.
pub fn show(sql: &str) -> big_sql::Show {
    match big_sql::translate(sql).unwrap_or_else(|e| panic!("`{sql}` was refused: {e}")) {
        big_sql::Sql::Show(s) => s,
        other => panic!("`{sql}` does not ask about the catalog: {other:?}"),
    }
}

/// The columns a `CREATE TABLE` declared, as `(name, kind, bit_depth, scale)`.
///
/// A tuple rather than the struct: a column list test is a table of what each type name means,
/// and a table reads better than twenty struct literals.
pub fn columns(sql: &str) -> Vec<(String, &'static str, u32, Option<i8>)> {
    let big_sql::Ddl::CreateTable { columns, .. } = ddl(sql) else {
        panic!("`{sql}` is not a CREATE TABLE")
    };
    columns.into_iter().map(|c| (c.name, c.kind.as_str(), c.bit_depth, c.scale)).collect()
}
