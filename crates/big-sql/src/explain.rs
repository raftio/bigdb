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

//! A translated statement, written back as something somebody can read.
//!
//! # Why a shape needs its own printer
//!
//! `big_plan::explain` prints half of what a statement means. The other half is deliberately not
//! in the plan - see [`crate::shape`] - so a corpus that only checked plans would be blind to
//! every clause the shape carries: which columns, in which order, under which `HAVING`, cut
//! how. Those are exactly the clauses that were added last and are most likely to move.
//!
//! So there are two printers and two directives, and a case uses whichever half it is about.
//! Keeping them apart is worth more than one combined dump: a `WHERE` that changes should not
//! churn the expected output of a test about `ORDER BY`.
//!
//! # What the notation says
//!
//! A cell reads a plan by index, and the index is what the shape actually holds - so it prints
//! as `#0`, not as the plan's text. A shape that pointed at the wrong plan would print
//! differently from one that pointed at the right one, which is the property that makes this
//! usable as an expected answer.

use crate::ddl::{Alter, Column, Ddl};
use crate::insert::Insert;
use crate::shape::{
    Absent, Answer, Cell, Cut, Format, GroupOrder, Having, JoinSide, Of, OrderBy, Pairing,
    Selected, Shape, Threshold, Units,
};
use crate::show::{Show, Shown};
use big_plan::Literal;

/// The answer a statement produces: its shape, and how it is written out.
///
/// ```text
/// Groups keys=[0]
/// ├── country = key
/// ├── n = group #0
/// └── order: n desc
/// ```
pub fn answer(answer: &Answer) -> String {
    let mut out = shape(&answer.shape);
    // Only when it is not the default. Every case in the corpus would otherwise carry a line
    // saying `json`, which is the answer to a question none of them asked.
    if answer.format != Format::default() {
        let head = out.find('\n').unwrap_or(out.len());
        out.insert_str(head, &format!(" format={}", format_name(answer.format)));
    }
    out
}

/// The shape alone, as a tree of lines. No trailing newline.
pub fn shape(shape: &Shape) -> String {
    let mut out = String::new();
    write_shape(&mut out, shape, "");
    out
}

fn write_shape(out: &mut String, shape: &Shape, prefix: &str) {
    out.push_str(&shape_head(shape));
    let kids = shape_kids(shape);
    write_lines(out, &kids, prefix);
}

/// A shape's own line: what kind of answer it is, and the plans that drive its rows.
fn shape_head(shape: &Shape) -> String {
    match shape {
        Shape::Row { .. } => "Row".to_string(),
        Shape::Records { column, limit } => {
            format!("Records column={column}{}", opt(" limit=", limit.as_ref()))
        }
        Shape::Table { .. } => "Table".to_string(),
        Shape::Union { .. } => "Union".to_string(),
        Shape::Pairs { keys, .. } => format!("Pairs keys={}", plans(keys)),
        // Printed as `keys=`, which is what these are: the plans whose keys make the rows.
        // The word outlives the field name so that widening the sides churns no golden line.
        Shape::Join { sides: s, per_key, .. } => {
            format!("Join keys=({}){}", plans_of(s), if *per_key { " per_key" } else { "" })
        }
        Shape::Groups { keys, .. } => format!("Groups keys={}", plans(keys)),
    }
}

/// A line under a shape: a column, or one of the clauses applied after the merge.
enum Line<'a> {
    Text(String),
    /// Borrowed rather than cloned: a union's branches are the only nested shapes, and a shape
    /// is large enough that copying one per line would be a copy per branch of every union.
    Shape(&'a Shape),
}

fn shape_kids(shape: &Shape) -> Vec<Line<'_>> {
    match shape {
        Shape::Row { cells } => cells.iter().map(|c| Line::Text(cell(c, &[]))).collect(),
        Shape::Records { .. } => Vec::new(),
        Shape::Table { columns } => {
            columns.named().iter().map(|c| Line::Text(selected(c))).collect()
        }
        Shape::Union { branches } => branches.iter().map(Line::Shape).collect(),
        // A join's sides are named by position in its own keys, so its cells are the only ones
        // that need them to print. The other two pass nothing, which is what they hold.
        Shape::Join { sides, cells, having, order, cut, .. } => {
            clauses(sides, cells, having, order, cut)
        }
        Shape::Pairs { cells, having, order, cut, .. }
        | Shape::Groups { cells, having, order, cut, .. } => {
            clauses(&[], cells, having, order, cut)
        }
    }
}

/// The columns of a shape that has rows, and the clauses applied to them after the merge.
fn clauses<'a>(
    sides: &[JoinSide],
    cells: &[Cell],
    having: &Option<Having>,
    order: &Option<GroupOrder>,
    cut: &Cut,
) -> Vec<Line<'a>> {
    let mut out: Vec<Line> = cells.iter().map(|c| Line::Text(cell(c, sides))).collect();
    if let Some(h) = having {
        out.push(Line::Text(format!("having: {}", having_of(h, sides))));
    }
    if let Some(o) = order {
        out.push(Line::Text(format!("order: {}", order_of(o, sides))));
    }
    if let Some(text) = cut_of(cut) {
        out.push(Line::Text(text));
    }
    out
}

/// One column of an answer: its name, and the number that goes in it.
fn cell(cell: &Cell, sides: &[JoinSide]) -> String {
    format!("{} = {}{}", cell.column, of(cell.of, sides), units(&cell.units))
}

/// One column of a projection, which names no plan - the values are the plan's own answer.
fn selected(selected: &Selected) -> String {
    format!("{}{}", selected.column, units(&selected.units))
}

/// Where a cell's number comes from, with plans named by the index the shape holds.
///
/// `sides` is the enclosing join's, if it is a join: a paired cell names only its own plan and
/// the position it sits at, so the sides it is scaled against are resolved here rather than
/// carried in every cell.
fn of(of: Of, sides: &[JoinSide]) -> String {
    match of {
        Of::Value { plan } => format!("#{plan}"),
        Of::Groups { plan } => format!("groups(#{plan})"),
        Of::Ratio { plan, over } => format!("#{plan} / #{over}"),
        Of::Key => "key".to_string(),
        Of::RightKey => "right key".to_string(),
        Of::Probe { probe } => format!("probe #{probe}"),
        // The fallback matters: it is the whole of what a `FILTER` leaves behind, and two
        // shapes that differ only in it are two different answers for an emptied group.
        Of::Group { plan, absent } => format!(
            "group #{plan} else {}",
            match absent {
                Absent::Zero => "0",
                Absent::Null => "null",
            }
        ),
        // This side's plan first, then every other side it is scaled against, in the order the
        // join holds them. A `Paired` printed with no keys is a shape the lowering cannot
        // produce, and says so by naming only the plan it has.
        Of::Paired { plan, side, how } => format!(
            "{}(#{plan}{})",
            match how {
                Pairing::Product => "product",
                Pairing::Least => "least",
                Pairing::Greatest => "greatest",
            },
            sides
                .iter()
                .enumerate()
                .filter(|(i, _)| *i != side)
                .map(|(_, s)| format!(", #{}", s.keyed.plan()))
                .collect::<String>()
        ),
        Of::Keys { plan } => format!("keys(#{plan})"),
        Of::SharedKeys => format!("shared({})", plans_of(sides)),
        // Both halves scaled by the same sides, so the sides are printed once around the pair
        // rather than twice - which is also what says they cancel nowhere.
        Of::PairedRatio { top, bottom, side } => format!(
            "ratio(#{top} / #{bottom}{})",
            sides
                .iter()
                .enumerate()
                .filter(|(i, _)| *i != side)
                .map(|(_, s)| format!(", #{}", s.keyed.plan()))
                .collect::<String>()
        ),
    }
}

/// A join's sides, as the plans they are: `#0, #1`.
fn plans_of(sides: &[JoinSide]) -> String {
    sides.iter().map(|s| format!("#{}", s.keyed.plan())).collect::<Vec<_>>().join(", ")
}

/// What a number is measured in, printed only when it is not plain.
///
/// The unresolved form prints as the field it came from rather than as nothing, because a shape
/// that reached an answer unresolved is off by a factor of that field's scale - and a printer
/// that showed the two identically would let exactly that bug through.
fn units(units: &Units) -> String {
    match units {
        Units::Digits(0) => String::new(),
        Units::Digits(n) => format!(" scale={n}"),
        Units::Written { table, field } => format!(" units={table}.{field} (unresolved)"),
    }
}

fn having_of(having: &Having, sides: &[JoinSide]) -> String {
    let value = match &having.value {
        Threshold::Units(n) => n.to_string(),
        Threshold::Written { table, field, value } => {
            format!("{} as {table}.{field} (unresolved)", literal(value))
        }
    };
    format!("{} {} {value}", of(having.of, sides), having.op)
}

fn order_of(order: &GroupOrder, sides: &[JoinSide]) -> String {
    let by = match order.by {
        OrderBy::Key => "key".to_string(),
        OrderBy::Value { of: o } => of(o, sides),
    };
    format!("{by} {}", if order.desc { "desc" } else { "asc" })
}

/// The cut, when there is one. `None` for the default, so a shape that cuts nothing says
/// nothing.
fn cut_of(cut: &Cut) -> Option<String> {
    if cut == &Cut::default() {
        return None;
    }
    let mut parts = Vec::new();
    if let Some(n) = cut.limit {
        parts.push(format!("limit {n}"));
    }
    if let Some(n) = cut.offset {
        parts.push(format!("offset {n}"));
    }
    if cut.ties {
        parts.push("with ties".to_string());
    }
    Some(format!("cut: {}", parts.join(" ")))
}

/// A schema change, as one line and - for a column list - a line per column.
pub fn ddl(ddl: &Ddl) -> String {
    match ddl {
        Ddl::CreateDatabase { name, if_not_exists } => {
            format!("CreateDatabase {name}{}", if *if_not_exists { " if_not_exists" } else { "" })
        }
        Ddl::DropDatabase { name, if_exists, cascade } => format!(
            "DropDatabase {name}{}{}",
            if *if_exists { " if_exists" } else { "" },
            if *cascade { " cascade" } else { "" },
        ),
        Ddl::CreateTable { database, table, engine, columns, if_not_exists } => {
            let mut out = format!(
                "CreateTable {}{}{}",
                qualified(database, table),
                if *if_not_exists { " if_not_exists" } else { "" },
                opt(" engine=", engine.as_ref()),
            );
            let kids: Vec<Line> = columns.iter().map(|c| Line::Text(column(c))).collect();
            write_lines(&mut out, &kids, "");
            out
        }
        Ddl::AlterTable { database, table, changes } => {
            let mut out = format!("AlterTable {}", qualified(database, table));
            let kids: Vec<Line> = changes
                .iter()
                .map(|c| {
                    Line::Text(match c {
                        Alter::Add(col) => format!("add {}", column(col)),
                        Alter::Drop(name) => format!("drop {name}"),
                    })
                })
                .collect();
            write_lines(&mut out, &kids, "");
            out
        }
        Ddl::DropTable { database, table, if_exists } => {
            format!(
                "DropTable {}{}",
                qualified(database, table),
                if *if_exists { " if_exists" } else { "" }
            )
        }
        Ddl::CreateView { database, name, body, or_replace, if_not_exists } => {
            let mut out = format!(
                "CreateView {}{}{}",
                qualified(database, name),
                if *or_replace { " or_replace" } else { "" },
                if *if_not_exists { " if_not_exists" } else { "" },
            );
            // The body on its own line, as stored. A corpus case then shows the exact string
            // that goes to disk and comes back to be re-parsed, which is the thing worth
            // pinning: a change to how the slice is taken shows up here as a diff.
            write_lines(&mut out, &[Line::Text(body.clone())], "");
            out
        }
        Ddl::DropView { database, name, if_exists } => {
            format!(
                "DropView {}{}",
                qualified(database, name),
                if *if_exists { " if_exists" } else { "" }
            )
        }
    }
}

/// `database.table`, or just the table when the statement did not name one.
///
/// Printed rather than defaulted to `default`, so a corpus case shows what was *written*: a
/// statement that named no database and one that named the default one are different
/// statements, and only the first follows the request's `?database=`.
fn qualified(database: &Option<String>, table: &str) -> String {
    match database {
        Some(d) => format!("{d}.{table}"),
        None => table.to_string(),
    }
}

/// One declared column: what it stores, how deep, and - for a decimal - to how many digits.
///
/// The depth prints for every kind, including the ones it means nothing for. It is what the
/// field route is sent either way, so a corpus that hid it would not be showing what the
/// statement does.
fn column(column: &Column) -> String {
    format!(
        "{} {} depth={}{}",
        column.name,
        column.kind.as_str(),
        column.bit_depth,
        opt(" scale=", column.scale.as_ref())
    )
}

/// A write, as its columns and its rows.
pub fn insert(insert: &Insert) -> String {
    let mut out = format!(
        "Insert {} ({}){}",
        qualified(&insert.database, &insert.table),
        insert.columns.join(", "),
        // Which column is the record id is the whole difference between the two forms of this
        // statement - the other one leaves the layer above to allocate - so it is named.
        match insert.id_at {
            Some(i) => format!(" id_at={i}"),
            None => " id=allocated".to_string(),
        }
    );
    let kids: Vec<Line> = insert
        .rows
        .iter()
        .map(|row| Line::Text(row.iter().map(literal).collect::<Vec<_>>().join(", ")))
        .collect();
    write_lines(&mut out, &kids, "");
    out
}

/// A question about the catalog.
pub fn show(show: &Show) -> String {
    let what = match &show.what {
        Shown::Columns { database, table } => format!("Columns {}", qualified(database, table)),
        Shown::Tables { database } => match database {
            Some(d) => format!("Tables {d}"),
            None => "Tables".to_string(),
        },
        Shown::Views { database } => match database {
            Some(d) => format!("Views {d}"),
            None => "Views".to_string(),
        },
        Shown::Databases => "Databases".to_string(),
        Shown::Create { database, table, view } => {
            format!("Create {}{}", qualified(database, table), if *view { " view" } else { "" })
        }
    };
    match show.format {
        f if f == Format::default() => what,
        f => format!("{what} format={}", format_name(f)),
    }
}

/// A literal exactly as the statement wrote it, so that a decimal's written scale is visible -
/// `1.50` and `1.5` are two different statements to a field of scale two.
fn literal(literal: &Literal) -> String {
    match literal {
        Literal::Int(n) => n.to_string(),
        Literal::Sint(n) => n.to_string(),
        Literal::Dec { units, scale } => {
            let s = *scale as usize;
            let text = format!("{units:0>width$}", width = s + 1);
            let (whole, frac) = text.split_at(text.len() - s);
            if s == 0 {
                whole.to_string()
            } else {
                format!("{whole}.{frac}")
            }
        }
        Literal::Str(s) => format!("'{s}'"),
        Literal::Bool(b) => b.to_string(),
    }
}

fn format_name(format: Format) -> &'static str {
    match format {
        Format::Json => "json",
        Format::Tsv => "tsv",
        Format::TsvWithNames => "tsv_with_names",
        Format::Csv => "csv",
        Format::CsvWithNames => "csv_with_names",
    }
}

fn plans(keys: &[usize]) -> String {
    format!("[{}]", keys.iter().map(|k| format!("#{k}")).collect::<Vec<_>>().join(", "))
}

/// `label=value` when there is a value, and nothing at all when there is not.
fn opt<T: std::fmt::Display>(label: &str, value: Option<&T>) -> String {
    value.map(|v| format!("{label}{v}")).unwrap_or_default()
}

/// Draws `kids` under a head line already written, with the elbows each position calls for.
fn write_lines(out: &mut String, kids: &[Line], prefix: &str) {
    for (i, kid) in kids.iter().enumerate() {
        let last = i + 1 == kids.len();
        let (elbow, carry) = if last { ("└── ", "    ") } else { ("├── ", "│   ") };
        out.push('\n');
        out.push_str(prefix);
        out.push_str(elbow);
        match kid {
            Line::Text(text) => out.push_str(text),
            Line::Shape(shape) => write_shape(out, shape, &format!("{prefix}{carry}")),
        }
    }
}
