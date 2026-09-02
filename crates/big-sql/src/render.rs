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

//! A schema, written back as the statement that would create it.
//!
//! # Why this lives here
//!
//! `Parser::column_type` is the one place in this crate that decides what a type name means -
//! there is no field kind spelled `TEXT` anywhere below, so the mapping is a dialect and it is
//! written out. The mapping back is the inverse of that same table, and an inverse kept in
//! another crate is an inverse that drifts: the two are here together, and the round-trip test
//! in `tests/translate` fails when either moves without the other.

use crate::ddl::{Column, ColumnKind};

/// The `CREATE TABLE` that would create these columns, which this crate's own parser reads back
/// as the same columns.
///
/// The native spellings are used throughout - `SET`, `UINT(n)`, `SIGNED(n)`, `DECIMAL(p, s)` -
/// rather than the SQL ones that mean them. `TEXT` and `SET` create the same field, and
/// answering with the name of the thing that exists is more use to somebody reading the schema
/// than answering with the name they happened to type.
pub fn create_table(table: &str, engine: Option<&str>, columns: &[Column]) -> String {
    let mut out = format!("CREATE TABLE {table}");
    if !columns.is_empty() {
        out.push_str(" (\n");
        for (i, column) in columns.iter().enumerate() {
            out.push_str("    ");
            out.push_str(&column.name);
            out.push(' ');
            out.push_str(&column_type(column));
            if i + 1 < columns.len() {
                out.push(',');
            }
            out.push('\n');
        }
        out.push(')');
    }
    if let Some(engine) = engine {
        // Quoted whenever it is not a bare word, which is what `bitmap+columnar` is: the lexer
        // has no token for `+`, so an unquoted one would not read back.
        if engine.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
            out.push_str(&format!(" ENGINE = {engine}"));
        } else {
            out.push_str(&format!(" ENGINE = '{engine}'"));
        }
    }
    out
}

/// One column's type, in the spelling `Parser::column_type` reads back as this same column.
fn column_type(column: &Column) -> String {
    match column.kind {
        ColumnKind::Set => "SET".to_string(),
        ColumnKind::Mutex => "MUTEX".to_string(),
        ColumnKind::Bool => "BOOL".to_string(),
        ColumnKind::TimeQuantum => "TIMEQUANTUM".to_string(),
        // No width in brackets: the two float kinds carry theirs in their names, and a
        // `FLOAT32(32)` that read back would be a second spelling of one type.
        ColumnKind::Float32 => "FLOAT32".to_string(),
        ColumnKind::Float64 => "FLOAT64".to_string(),
        ColumnKind::Date => "DATE".to_string(),
        ColumnKind::DateTime => "DATETIME".to_string(),
        ColumnKind::Int => format!("UINT({})", column.bit_depth),
        ColumnKind::Signed => format!("SIGNED({})", column.bit_depth),
        ColumnKind::Decimal => {
            format!("DECIMAL({}, {})", precision_for(column.bit_depth), column.scale.unwrap_or(0))
        }
    }
}

/// How many digits a decimal of this bit depth was declared with.
///
/// **The one place this file is not an exact inverse, and it is not one because the forward
/// direction is not injective.** A column list says `DECIMAL(p, s)` and the parser derives
/// `ceil(p * log2(10))` bits from it; a field created over
/// `POST /table/{t}/field/{f}?kind=decimal&bit_depth=20` names bits directly, and there may be
/// no `p` that derives exactly 20. So this answers with the smallest precision whose depth
/// covers the field - which holds every value the field can - and a decimal that came from a
/// column list round-trips exactly, because its depth is one this formula produces.
fn precision_for(bit_depth: u32) -> u64 {
    (1..=19).find(|p| crate::parse::decimal_bits(*p) >= u64::from(bit_depth)).unwrap_or(19)
}
