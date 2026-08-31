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

//! An [`Answer`] as text.
//!
//! **The default follows the destination.** A terminal gets aligned columns; a pipe gets
//! tab-separated fields. That is not a convenience: a column-aligned table is a format nobody
//! can parse and everybody tries to, and the moment one is on the far end of a pipe somebody
//! writes an `awk` script against a column width. `--format` overrides both, and
//! `--format json` prints the server's body untouched for anything this file renders badly.
//!
//! Only `tsv` is promised to stay put. Table rendering is for a person looking at a screen, and
//! a column width is not an interface.

use crate::args::Format;
use crate::json::Answer;

/// Which format to use when the caller did not say.
pub fn default_for(tty: bool) -> Format {
    if tty {
        Format::Table
    } else {
        Format::Tsv
    }
}

/// The answer, as the caller asked to see it.
pub fn answer(a: &Answer, format: Format) -> String {
    match format {
        Format::Tsv => tsv(a),
        Format::Table => table(a),
        // Handled by the caller, which has the untouched body; reaching here would mean the
        // body was parsed and re-emitted, which is exactly what `json` promises not to do.
        Format::Json => unreachable!("the raw body is printed without being rendered"),
    }
}

/// Tab-separated, header first, one row per line.
///
/// A tab inside a cell would break the format, and a row key is user data that can contain
/// anything - so tabs, newlines and carriage returns are escaped rather than emitted. That
/// makes every line one record, which is the only property a script can rely on.
fn tsv(a: &Answer) -> String {
    let mut out = String::new();
    out.push_str(&a.columns.join("\t"));
    out.push('\n');
    for row in &a.rows {
        let cells: Vec<String> = row.iter().map(|c| escape(c)).collect();
        out.push_str(&cells.join("\t"));
        out.push('\n');
    }
    out
}

fn escape(cell: &str) -> String {
    if cell.contains(['\t', '\n', '\r']) {
        cell.replace('\\', "\\\\").replace('\t', "\\t").replace('\n', "\\n").replace('\r', "\\r")
    } else {
        cell.to_string()
    }
}

/// Aligned columns, with a rule under the header.
///
/// Width is counted in `char`s rather than bytes, so a key with a non-ASCII character lines up.
/// It is still wrong for a double-width glyph, and deliberately so: getting that right means a
/// Unicode width table, which is a dependency, and the answer to "my columns are crooked" is
/// `--format tsv`.
fn table(a: &Answer) -> String {
    let mut widths: Vec<usize> = a.columns.iter().map(|c| c.chars().count()).collect();
    for row in &a.rows {
        for (i, cell) in row.iter().enumerate() {
            if let Some(w) = widths.get_mut(i) {
                *w = (*w).max(cell.chars().count());
            }
        }
    }

    let mut out = String::new();
    write_row(&mut out, &a.columns, &widths);
    let rule: Vec<String> = widths.iter().map(|w| "-".repeat(*w)).collect();
    write_row(&mut out, &rule, &widths);
    for row in &a.rows {
        write_row(&mut out, row, &widths);
    }
    if a.rows.is_empty() {
        out.push_str("(no rows)\n");
    }
    out
}

fn write_row(out: &mut String, cells: &[String], widths: &[usize]) {
    let padded: Vec<String> = cells
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let width = widths.get(i).copied().unwrap_or(0);
            let pad = width.saturating_sub(c.chars().count());
            format!("{c}{}", " ".repeat(pad))
        })
        .collect();
    // Trailing spaces on the last column would be invisible padding a reader might copy.
    out.push_str(padded.join("  ").trim_end());
    out.push('\n');
}
