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

//! The catalog, answered as rows.
//!
//! `DESCRIBE t` and `SHOW TABLES` ask what `GET /schema` answers, in the language the rest of
//! the statement was written in. There is no plan behind either: the answer is already sitting
//! in the snapshot every node holds, so these are functions over [`TableInfo`] rather than
//! anything that reaches storage - which is also what makes them testable without a pager.
//!
//! # Why the mapping between kinds lives here
//!
//! `big-sql` decides what a *type name* means, because no field kind is spelled `TEXT` anywhere
//! below. What a [`FieldKind`] is called in SQL is the other half of that, and it belongs on
//! this side of the line: `big-sql` links no storage crate and has never heard of `FieldKind`.

use crate::error::{ApiError, Result};
use crate::result::{Datum, ResultSet};
use crate::schema::{FieldInfo, TableInfo};
use big_db::catalog::FieldKind;
use big_db::DbError;
use big_sql::{Column as SqlColumn, ColumnKind as SqlColumnKind};

/// The field kind a column list's type means.
pub fn kind_of(kind: SqlColumnKind) -> FieldKind {
    match kind {
        SqlColumnKind::Set => FieldKind::Set,
        SqlColumnKind::Mutex => FieldKind::Mutex,
        SqlColumnKind::Bool => FieldKind::Bool,
        SqlColumnKind::Int => FieldKind::Int,
        SqlColumnKind::Signed => FieldKind::SignedInt,
        SqlColumnKind::Decimal => FieldKind::Decimal,
        SqlColumnKind::TimeQuantum => FieldKind::TimeQuantum,
    }
}

/// The inverse, for writing a schema back out as the statement that would create it.
///
/// Exhaustive on purpose: a field kind added below has to be given a spelling here, or this
/// stops compiling - which is the only thing that keeps `SHOW CREATE TABLE` from quietly
/// omitting a field it has no word for.
pub fn sql_kind_of(kind: FieldKind) -> SqlColumnKind {
    match kind {
        FieldKind::Set => SqlColumnKind::Set,
        FieldKind::Mutex => SqlColumnKind::Mutex,
        FieldKind::Bool => SqlColumnKind::Bool,
        FieldKind::Int => SqlColumnKind::Int,
        FieldKind::SignedInt => SqlColumnKind::Signed,
        FieldKind::Decimal => SqlColumnKind::Decimal,
        FieldKind::TimeQuantum => SqlColumnKind::TimeQuantum,
    }
}

/// `DESCRIBE t`: one row per field.
///
/// The columns are the ones `GET /schema` writes for a field, in the same order and with the
/// same rule about what is absent: a `scale` on a field that stores no decimal and a
/// `granularity` on a field with no views by time are [`Datum::Null`] rather than zero, so that
/// a reader need not know which kinds to ignore them for.
pub fn describe(tables: &[TableInfo], table: &str) -> Result<ResultSet> {
    let info = find(tables, table)?;
    Ok(ResultSet {
        columns: ["name", "kind", "bit_depth", "scale", "granularity"].map(str::to_string).to_vec(),
        rows: info.fields.iter().map(field_row).collect(),
    })
}

/// `SHOW TABLES`: one row per table.
///
/// The engine is a column of its own because it is not derivable from anything else a client
/// can see - the same argument `GET /schema` makes for reporting it - and the field count
/// because the question after "which tables are there" is always "how big is this one".
pub fn show_tables(tables: &[TableInfo], database: Option<&str>) -> ResultSet {
    // `SHOW TABLES` with no `FROM` lists the request's own database, which the caller has
    // already resolved into `database`. Listing every database's tables under bare names would
    // answer with names that do not resolve from where they were asked.
    let database = database.unwrap_or(big_db::DEFAULT_DATABASE_NAME);
    ResultSet {
        columns: ["name", "engine", "fields"].map(str::to_string).to_vec(),
        rows: tables
            .iter()
            .filter(|t| t.database == database)
            .map(|t| {
                vec![
                    Datum::Text(t.name.clone()),
                    Datum::Text(t.engine.as_str().to_string()),
                    Datum::Int(t.fields.len() as i128),
                ]
            })
            .collect(),
    }
}

/// `SHOW DATABASES`: one row per database, with how many tables it holds.
///
/// The question every JDBC driver and BI tool opens with. The default database is always in
/// the answer, whether or not it holds anything: it is the one a request lands in when nothing
/// says otherwise, so a client that could not see it could not explain where its tables went.
pub fn show_databases(tables: &[TableInfo]) -> ResultSet {
    let mut counts: std::collections::BTreeMap<&str, usize> =
        [(big_db::DEFAULT_DATABASE_NAME, 0)].into_iter().collect();
    for t in tables {
        *counts.entry(t.database.as_str()).or_default() += 1;
    }
    ResultSet {
        columns: ["name", "tables"].map(str::to_string).to_vec(),
        rows: counts
            .into_iter()
            .map(|(name, n)| vec![Datum::Text(name.to_string()), Datum::Int(n as i128)])
            .collect(),
    }
}

/// `SHOW CREATE TABLE t`: one row holding the statement that would recreate it.
///
/// Rendered by `big-sql`, which owns the mapping between a type name and a field kind in both
/// directions - so what comes out is a statement its own parser reads back as this same table.
pub fn show_create(tables: &[TableInfo], table: &str) -> Result<ResultSet> {
    let info = find(tables, table)?;
    let columns: Vec<SqlColumn> = info
        .fields
        .iter()
        .map(|f| SqlColumn {
            name: f.name.clone(),
            kind: sql_kind_of(f.kind),
            bit_depth: f.bit_depth,
            scale: (f.kind == FieldKind::Decimal).then_some(f.scale),
        })
        .collect();
    // Qualified whenever the table is not in the default database, so the statement this
    // answers with recreates the table *where it is*. An unqualified one would recreate it in
    // whichever database the next request happened to be against.
    let name = big_db::TableRef::new(&info.database, &info.name).to_string();
    let statement = big_sql::render::create_table(&name, Some(info.engine.as_str()), &columns);
    Ok(ResultSet {
        columns: vec!["statement".to_string()],
        rows: vec![vec![Datum::Text(statement)]],
    })
}

/// One field's row, in the shape `describe` names.
fn field_row(f: &FieldInfo) -> Vec<Datum> {
    vec![
        Datum::Text(f.name.clone()),
        // The spelling `GET /schema` uses, because it is the same field being described and
        // two names for one kind is a difference somebody would have to explain.
        Datum::Text(format!("{:?}", f.kind).to_lowercase()),
        Datum::Int(i128::from(f.bit_depth)),
        match f.kind {
            FieldKind::Decimal => Datum::Int(i128::from(f.scale)),
            _ => Datum::Null,
        },
        match f.granularity.is_empty() {
            true => Datum::Null,
            false => Datum::Keys(f.granularity.iter().map(|g| g.as_char().to_string()).collect()),
        },
    ]
}

/// The table, or the error naming it - which is the same error a query against it would give,
/// so that a client branches on one code rather than two.
fn find<'a>(tables: &'a [TableInfo], table: &str) -> Result<&'a TableInfo> {
    let r = big_db::TableRef::parse(table);
    tables
        .iter()
        .find(|t| t.name == r.table && t.database == r.database)
        .ok_or_else(|| ApiError::Db(DbError::UnknownTable(table.to_string())))
}
