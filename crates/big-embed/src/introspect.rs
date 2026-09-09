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
use crate::views::ViewInfo;
use big_db::catalog::FieldKind;
use big_db::DbError;
use big_sql::{Column as SqlColumn, ColumnKind as SqlColumnKind};

/// The field kind a column list's type means.
pub fn kind_of(kind: SqlColumnKind) -> FieldKind {
    match kind {
        // Stored as the mutex it is. The member list travels beside the kind rather than inside
        // it - see `big_sql::Column::members` - which is what keeps an enum free of any engine
        // change at all.
        SqlColumnKind::Enum => FieldKind::Mutex,
        SqlColumnKind::Set => FieldKind::Set,
        SqlColumnKind::Mutex => FieldKind::Mutex,
        SqlColumnKind::Bool => FieldKind::Bool,
        SqlColumnKind::Int => FieldKind::Int,
        SqlColumnKind::Signed => FieldKind::SignedInt,
        SqlColumnKind::Decimal => FieldKind::Decimal,
        SqlColumnKind::TimeQuantum => FieldKind::TimeQuantum,
        SqlColumnKind::Float32 => FieldKind::Float32,
        SqlColumnKind::Float64 => FieldKind::Float64,
        SqlColumnKind::Date => FieldKind::Date,
        SqlColumnKind::DateTime => FieldKind::DateTime,
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
        FieldKind::Float32 => SqlColumnKind::Float32,
        FieldKind::Float64 => SqlColumnKind::Float64,
        FieldKind::Date => SqlColumnKind::Date,
        FieldKind::DateTime => SqlColumnKind::DateTime,
    }
}

/// `DESCRIBE t`: one row per field.
///
/// The columns are the ones `GET /schema` writes for a field, in the same order and with the
/// same rule about what is absent: a `scale` on a field that stores no decimal and a
/// `granularity` on a field with no views by time are [`Datum::Null`] rather than zero, so that
/// a reader need not know which kinds to ignore them for.
/// A view answers with **the columns it exposes**, under the names it exposes them by, each
/// carrying the kind of the column underneath. A `DESCRIBE` that listed the base table's fields
/// would undo the view in one statement.
pub fn describe(tables: &[TableInfo], views: &[ViewInfo], table: &str) -> Result<ResultSet> {
    let columns =
        ["name", "kind", "bit_depth", "scale", "granularity"].map(str::to_string).to_vec();
    if let Some(view) = find_view(views, table) {
        let (base, exposed) = view.shape()?;
        let under = find(tables, &base)?;
        let rows = exposed
            .iter()
            .filter_map(|(shown, column)| {
                // A column the base table no longer has: the view dangles, which is what a
                // `DROP COLUMN` under a view leaves behind. Omitted rather than reported as a
                // field of unknown kind - `SHOW CREATE VIEW` still says what it names.
                let f = under.fields.iter().find(|f| f.name == *column)?;
                let mut row = field_row(f);
                row[0] = Datum::Text(shown.clone());
                Some(row)
            })
            .collect();
        return Ok(ResultSet { columns, rows });
    }
    let info = find(tables, table)?;
    Ok(ResultSet { columns, rows: info.fields.iter().map(field_row).collect() })
}

/// `SHOW TABLES`: one row per table **and one per view**.
///
/// The engine is a column of its own because it is not derivable from anything else a client
/// can see - the same argument `GET /schema` makes for reporting it - and the field count
/// because the question after "which tables are there" is always "how big is this one".
///
/// **Views are in the listing, under a `type` column.** That is what a JDBC driver asks for and
/// what it expects back; a listing that hid them would leave a view queryable and invisible,
/// which is the worst of the two answers. A view has no engine - it stores nothing - so that
/// cell is [`Datum::Null`] rather than a made-up name, and its `fields` count is the columns it
/// exposes.
pub fn show_tables(tables: &[TableInfo], views: &[ViewInfo], database: Option<&str>) -> ResultSet {
    // `SHOW TABLES` with no `FROM` lists the request's own database, which the caller has
    // already resolved into `database`. Listing every database's tables under bare names would
    // answer with names that do not resolve from where they were asked.
    let database = database.unwrap_or(big_db::DEFAULT_DATABASE_NAME);
    let mut rows: Vec<Vec<Datum>> = tables
        .iter()
        .filter(|t| t.database == database)
        .map(|t| {
            vec![
                Datum::Text(t.name.clone()),
                Datum::Text("BASE TABLE".to_string()),
                Datum::Text(t.engine.as_str().to_string()),
                Datum::Int(t.fields.len() as i128),
            ]
        })
        .collect();
    rows.extend(views.iter().filter(|v| v.database == database).map(|v| {
        // A body that no longer parses counts no columns rather than failing the listing: a
        // listing is how somebody finds the broken view, so it must not be what the broken view
        // breaks.
        let exposed = v.shape().map_or(0, |(_, c)| c.len());
        vec![
            Datum::Text(v.name.clone()),
            Datum::Text("VIEW".to_string()),
            Datum::Null,
            Datum::Int(exposed as i128),
        ]
    }));
    // One namespace, so one sorted listing: a client drawing a tree gets tables and views
    // interleaved by name, which is where somebody looking for a name expects to find it.
    rows.sort_by(|a, b| match (&a[0], &b[0]) {
        (Datum::Text(x), Datum::Text(y)) => x.cmp(y),
        _ => core::cmp::Ordering::Equal,
    });
    ResultSet { columns: ["name", "type", "engine", "fields"].map(str::to_string).to_vec(), rows }
}

/// `SHOW VIEWS`: one row per view, with the statement it holds.
///
/// Not redundant with `SHOW TABLES`, which says a view exists but not what it means. This is the
/// listing an operator reads to find the view that a `DROP COLUMN` is about to break.
pub fn show_views(views: &[ViewInfo], database: Option<&str>) -> ResultSet {
    let database = database.unwrap_or(big_db::DEFAULT_DATABASE_NAME);
    ResultSet {
        columns: ["name", "statement"].map(str::to_string).to_vec(),
        rows: views
            .iter()
            .filter(|v| v.database == database)
            .map(|v| vec![Datum::Text(v.name.clone()), Datum::Text(v.text.clone())])
            .collect(),
    }
}

/// `SHOW DATABASES`: one row per database, with how many tables it holds.
///
/// The question every JDBC driver and BI tool opens with. The default database is always in
/// the answer, whether or not it holds anything: it is the one a request lands in when nothing
/// says otherwise, so a client that could not see it could not explain where its tables went.
///
/// `names` is the catalog's own list and is what decides which rows exist; `tables` only
/// decides the counts. Deriving the list from the tables instead would hide a database that
/// holds nothing yet - which is every database between its `CREATE` and its first table, and
/// the one state where an operator most needs the listing to confirm the `CREATE` landed.
pub fn show_databases(names: &[String], tables: &[TableInfo]) -> ResultSet {
    let mut counts: std::collections::BTreeMap<&str, usize> =
        [(big_db::DEFAULT_DATABASE_NAME, 0)].into_iter().collect();
    for name in names {
        counts.entry(name.as_str()).or_default();
    }
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

/// `SELECT * FROM numbers(n)`: the integers `0 .. n`, under one column.
///
/// **The one answer in this module that reads nothing at all** - not the catalog, not a fragment,
/// not a grant. It is here rather than in the query path because it is the same kind of thing
/// every other function here is: rows built at the coordinator, from something already in hand,
/// with no plan behind them and nothing to merge.
///
/// Bounded before it arrives: `big_sql::MAX_NUMBERS` is checked in the parser, so `n` here is
/// already a count this is willing to allocate.
pub fn numbers(n: u64) -> ResultSet {
    ResultSet {
        columns: vec!["number".to_string()],
        rows: (0..n).map(|i| vec![Datum::Int(i128::from(i))]).collect(),
    }
}

/// `SHOW ROLES`: one row per role.
///
/// The reserved role is prepended rather than stored, exactly as `default` appears in
/// `show_databases` above without a record behind it - see [`big_rbac::SUPERUSER`].
pub fn show_roles(roles: &[String]) -> ResultSet {
    ResultSet {
        columns: vec!["name".to_string()],
        rows: roles.iter().map(|r| vec![Datum::Text(r.clone())]).collect(),
    }
}

/// `SHOW GRANTS [FOR r]`: one row per object the role has been granted anything on.
///
/// An empty answer is the honest one for a role that holds nothing, and for a role that does not
/// exist. Telling those apart would make this statement a way to ask whether a role exists, which
/// is a question somebody who cannot administer roles has no business getting an answer to.
pub fn show_grants(grants: &[(Option<String>, Option<String>, Vec<&'static str>)]) -> ResultSet {
    ResultSet {
        columns: ["object", "privileges"].map(str::to_string).to_vec(),
        rows: grants
            .iter()
            .map(|(database, table, privileges)| {
                let object = match (database, table) {
                    (None, _) => "*.*".to_string(),
                    (Some(d), None) => format!("{d}.*"),
                    (Some(d), Some(t)) => format!("{d}.{t}"),
                };
                vec![Datum::Text(object), Datum::Text(privileges.join(", "))]
            })
            .collect(),
    }
}

/// `SHOW CREATE TABLE t`: one row holding the statement that would recreate it.
///
/// Rendered by `big-sql`, which owns the mapping between a type name and a field kind in both
/// directions - so what comes out is a statement its own parser reads back as this same table.
/// A view answers with the `CREATE VIEW` that would recreate it, built around the statement it
/// stored - which round-trips exactly, because it is the text this crate was handed.
///
/// `view` is `SHOW CREATE VIEW`, where a table under that name is the wrong object rather than
/// the answer. Unset, the name is looked up as either, which is what `SHOW CREATE x` means.
pub fn show_create(
    tables: &[TableInfo],
    views: &[ViewInfo],
    table: &str,
    view: bool,
) -> Result<ResultSet> {
    if let Some(v) = find_view(views, table) {
        // Qualified whenever it is not in the default database, for the reason the table branch
        // states below: the statement has to recreate the view *where it is*.
        let statement = format!("CREATE VIEW {} AS {}", v.qualified(), v.text);
        return Ok(ResultSet {
            columns: vec!["statement".to_string()],
            rows: vec![vec![Datum::Text(statement)]],
        });
    }
    if view {
        return Err(ApiError::Db(DbError::UnknownView(table.to_string())));
    }
    let info = find(tables, table)?;
    let columns: Vec<SqlColumn> = info
        .fields
        .iter()
        .map(|f| SqlColumn {
            name: f.name.clone(),
            // **The declared type, which the storage kind alone cannot say.** A mutex and an
            // enum are one `FieldKind`; what tells them apart is whether a member list was
            // declared, so that is what decides here rather than the kind.
            kind: match f.members.is_empty() {
                true => sql_kind_of(f.kind),
                false => SqlColumnKind::Enum,
            },
            bit_depth: f.bit_depth,
            scale: (f.kind == FieldKind::Decimal).then_some(f.scale),
            members: f.members.clone(),
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

/// The view of that name, if the name is a view.
///
/// `Option` rather than `Result` because every caller has a table branch to fall through to: a
/// name is a view, or a table, or neither - and only the last of those is an error, said once
/// by whichever branch runs out of places to look.
fn find_view<'a>(views: &'a [ViewInfo], name: &str) -> Option<&'a ViewInfo> {
    let r = big_db::TableRef::parse(name);
    views.iter().find(|v| v.name == r.table && v.database == r.database)
}

// ------------------------------------------------------------------------------------------
// `system.*`
// ------------------------------------------------------------------------------------------

/// Keeps only the columns a select list named, in the order it named them.
///
/// **A name the view does not have is dropped rather than refused**, which is the one place this
/// module is lenient and it is deliberate: these views are a debugging surface, they gain columns
/// between builds, and a script that names one this build has not got should come back with the
/// rest rather than with nothing. `SELECT *` is the answer to "what is there", and it is one
/// keystroke away.
fn only(set: ResultSet, columns: Option<&[String]>) -> ResultSet {
    let Some(wanted) = columns else { return set };
    let keep: Vec<usize> = wanted
        .iter()
        .filter_map(|w| set.columns.iter().position(|c| c.eq_ignore_ascii_case(w)))
        .collect();
    ResultSet {
        columns: keep.iter().map(|i| set.columns[*i].clone()).collect(),
        rows: set
            .rows
            .into_iter()
            .map(|row| keep.iter().map(|i| row[*i].clone()).collect())
            .collect(),
    }
}

/// `system.tables`: every table and view, in **every** database.
///
/// The difference from [`show_tables`] is the whole reason this is a separate function and a
/// separate `Shown` variant: that one answers about the request's database, and this one answers
/// about the server. A `database` column is therefore not decoration - without it the rows of two
/// databases would be indistinguishable.
pub fn system_tables(
    tables: &[TableInfo],
    views: &[ViewInfo],
    database: Option<&str>,
    columns: Option<&[String]>,
) -> ResultSet {
    let cols = ["database", "name", "type", "engine", "columns"].map(str::to_string).to_vec();
    let mut rows: Vec<Vec<Datum>> = tables
        .iter()
        .filter(|t| database.is_none_or(|d| t.database == d))
        .map(|t| {
            vec![
                Datum::Text(t.database.clone()),
                Datum::Text(t.name.clone()),
                Datum::Text("BASE TABLE".to_string()),
                Datum::Text(t.engine.as_str().to_string()),
                Datum::Int(t.fields.len() as i128),
            ]
        })
        .collect();
    rows.extend(views.iter().filter(|v| database.is_none_or(|d| v.database == d)).map(|v| {
        // A body that no longer parses counts no columns rather than failing the listing, for
        // the reason `show_tables` gives: a listing is how somebody finds the broken view.
        let exposed = v.shape().map_or(0, |(_, c)| c.len());
        vec![
            Datum::Text(v.database.clone()),
            Datum::Text(v.name.clone()),
            Datum::Text("VIEW".to_string()),
            Datum::Null,
            Datum::Int(exposed as i128),
        ]
    }));
    only(ResultSet { columns: cols, rows }, columns)
}

/// `system.columns`: every column of every table.
///
/// Views are deliberately absent. A view's columns are the base table's under other names, so
/// listing them here would double-count every column a view exposes - and this is the view an
/// operator counts with. `DESCRIBE v` still answers for one view.
pub fn system_columns(
    tables: &[TableInfo],
    database: Option<&str>,
    table: Option<&str>,
    columns: Option<&[String]>,
) -> ResultSet {
    let cols = ["database", "table", "name", "kind", "bit_depth", "scale", "granularity"]
        .map(str::to_string)
        .to_vec();
    let rows = tables
        .iter()
        .filter(|t| database.is_none_or(|d| t.database == d))
        .filter(|t| table.is_none_or(|n| t.name == n))
        .flat_map(|t| {
            t.fields.iter().map(|f| {
                let mut row = vec![Datum::Text(t.database.clone()), Datum::Text(t.name.clone())];
                row.extend(field_row(f));
                row
            })
        })
        .collect();
    only(ResultSet { columns: cols, rows }, columns)
}

/// `system.databases`: every database, with how many tables it holds.
pub fn system_databases(
    names: &[String],
    tables: &[TableInfo],
    database: Option<&str>,
    columns: Option<&[String]>,
) -> ResultSet {
    let set = show_databases(names, tables);
    let set = match database {
        None => set,
        // Filtered on the answer rather than inside `show_databases`, because the column it
        // filters on is the first one and this is one comparison - not because filtering rows is
        // something this module does in general.
        Some(d) => ResultSet {
            columns: set.columns,
            rows: set
                .rows
                .into_iter()
                .filter(|r| matches!(&r[0], Datum::Text(n) if n == d))
                .collect(),
        },
    };
    only(set, columns)
}

/// `system.parts`: every fragment this node holds.
///
/// **The one view here that repeats nothing.** `system.tables` says what `SHOW TABLES` says and
/// `system.columns` what `DESCRIBE` says; a fragment - `(table, field, view, shard)` and how many
/// bits are in it - has no other spelling on this surface at all. It is what turns "the query is
/// slow" into a question with an answer: which shards a table actually occupies, whether a
/// time-quantum field has the day views somebody expected, and where the data is not.
///
/// **This node's own**, and the `node` column says so. A coordinator holds the ranges it owns,
/// so the honest answer is the local one under a name that admits it rather than a total that
/// would be wrong on every node in a cluster.
pub fn system_parts(
    node: &str,
    parts: &[(String, big_db::FragmentAddr, u64)],
    database: Option<&str>,
    table: Option<&str>,
    columns: Option<&[String]>,
) -> ResultSet {
    let cols = ["node", "database", "table", "field", "view", "shard", "count"]
        .map(str::to_string)
        .to_vec();
    let rows = parts
        .iter()
        .filter_map(|(qualified, addr, count)| {
            let r = big_db::TableRef::parse(qualified);
            if !database.is_none_or(|d| r.database == d) || !table.is_none_or(|n| r.table == n) {
                return None;
            }
            Some(vec![
                Datum::Text(node.to_string()),
                Datum::Text(r.database.to_string()),
                Datum::Text(r.table.to_string()),
                // The reserved existence field has no name anybody wrote, and neither has the
                // standard view. Reported as what they are rather than as an absence, because an
                // operator counting fragments needs them counted.
                addr.field.clone().map_or(Datum::Text("_exists".to_string()), Datum::Text),
                Datum::Text(view_name(addr)),
                Datum::Int(i128::from(addr.shard)),
                Datum::Int(i128::from(*count)),
            ])
        })
        .collect();
    only(ResultSet { columns: cols, rows }, columns)
}

/// What to call the view a fragment is in.
///
/// **The reserved views have no name, and reporting them all as one would be worse than
/// reporting none.** `FragmentAddr::view` is `None` for three different things - the standard
/// view, the column segment and the mutex shadow - and which one it is lives in `view_id`. A
/// listing that read only the `Option` would print two rows per field of a `bitmap+columnar`
/// table under one name, which reads as a duplicate rather than as the two places the fact
/// actually is. On a view whose whole purpose is finding out where the data is, that is the one
/// mistake worth guarding by name.
fn view_name(addr: &big_db::FragmentAddr) -> String {
    if let Some(named) = &addr.view {
        return named.clone();
    }
    match addr.view_id {
        big_db::COLUMN_VIEW => "_column".to_string(),
        big_db::MUTEX_SHADOW_VIEW => "_mutex_shadow".to_string(),
        big_db::catalog::STANDARD_VIEW => "_standard".to_string(),
        // A reserved id this build has no name for. Printed as the number rather than guessed
        // at, so a fragment is never reported as being somewhere it is not.
        other => format!("_view{other}"),
    }
}
