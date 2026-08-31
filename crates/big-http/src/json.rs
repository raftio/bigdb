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

//! Writing JSON by hand.
//!
//! The whole output surface is four shapes, so a serialisation library would be a dependency
//! carried for one file. Escaping is the part worth getting right, and it is one function.

use big_api::{Answer, Datum, Format, ResultSet, TableInfo};
use big_cluster::{RangeVerdict, RepairReport, WriteOutcome};
use big_db::RecordId;
use big_exec::{Group, Value};

/// Escapes a string into a JSON string literal, including the quotes.
///
/// Control characters go out as `\u00XX` rather than raw, because a raw one makes the document
/// invalid and a row key is user data that can contain anything.
pub fn string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// An error body: a stable code and a sentence.
///
/// The code is what a client matches on and the message is what a person reads. Keeping both
/// in every error body is the whole reason the codes exist - a body with only prose forces
/// clients to match on prose.
pub fn error(code: &str, message: &str) -> String {
    format!("{{\"error\":{},\"code\":{}}}", string(message), string(code))
}

/// Which slice of a record list to return.
///
/// **There is no default limit.** A default would silently truncate every client that already
/// asks for a whole result and does not know a cursor exists, which is a far worse break than
/// the envelope change below: one is a shape they can see, the other is missing records they
/// cannot. Asking for a page is opting in.
#[derive(Clone, Copy, Default)]
pub struct Page {
    /// Resume strictly after this record id. `None` starts at the beginning.
    pub after: Option<RecordId>,
    /// How many ids to return. `None` returns all of them.
    pub limit: Option<usize>,
}

impl Page {
    /// Whether the client asked for a page at all. An unpaged response is the whole answer,
    /// with no `next`, so this is what decides which of the two shapes to encode.
    pub fn is_default(&self) -> bool {
        self.after.is_none() && self.limit.is_none()
    }

    /// The inclusive floor a cursor implies. `after` is exclusive because it is the last id the
    /// client already has, and saturating rather than wrapping because `u64::MAX` is a legal
    /// record id whose successor is not.
    fn floor(&self) -> RecordId {
        self.after.map_or(0, |a| a.saturating_add(1))
    }
}

/// A record list plus the cursor to continue it.
///
/// `next` is the id to send back as `after`, or `null` when this page is the last. Deciding
/// that needs one id beyond the page, which is why the iterator takes `limit + 1` and then
/// drops the extra: any other way of answering "is there more" either counts the whole result
/// or lies at the boundary.
pub fn rows(m: &big_db::Matches, page: Page) -> String {
    let limit = page.limit.unwrap_or(usize::MAX);
    let mut ids: Vec<RecordId> =
        m.records_from(page.floor()).take(limit.saturating_add(1)).collect();

    let next = if ids.len() > limit {
        ids.truncate(limit);
        ids.last().copied()
    } else {
        None
    };
    let list: Vec<String> = ids.iter().map(|r| r.to_string()).collect();
    let next = next.map_or("null".to_string(), |n| n.to_string());
    format!("{{\"records\":[{}],\"next\":{next}}}", list.join(","))
}

/// A page of ids that was already cut to size, plus its cursor.
///
/// The `next` rule differs from [`rows`] and has to: a listing asked the engine for `limit` ids
/// and cannot see whether a `limit + 1`-th exists without another read. A full page therefore
/// reports a cursor even when it happens to be the last one, and the client learns that from
/// the empty page that follows. Over-reading by one to avoid that would cost a shard read on
/// every page to save one request at the end of a scan.
pub fn records(ids: &[RecordId], limit: usize) -> String {
    let next = if ids.len() == limit { ids.last().copied() } else { None };
    let list: Vec<String> = ids.iter().map(|r| r.to_string()).collect();
    let next = next.map_or("null".to_string(), |n| n.to_string());
    format!("{{\"records\":[{}],\"next\":{next}}}", list.join(","))
}

/// One query result as JSON, unpaged.
pub fn value(v: &Value) -> String {
    value_paged(v, Page::default())
}

/// One query result as JSON, with a cursor when the client asked for a page.
pub fn value_paged(v: &Value, page: Page) -> String {
    match v {
        Value::Count(n) => format!("{{\"count\":{n}}}"),
        Value::Sum(n) => format!("{{\"sum\":{n}}}"),
        // The same shape as an unsigned sum on purpose: JSON numbers are signed, so a client
        // reading `{"sum":...}` needs to know nothing about how the field was declared.
        Value::SignedSum(n) => format!("{{\"sum\":{n}}}"),
        // Absent is `null`, not zero: nothing matched is a different answer from a total of
        // nothing.
        Value::Extreme(x) => match x {
            Some(n) => format!("{{\"value\":{n}}}"),
            None => "{\"value\":null}".to_string(),
        },
        Value::SignedExtreme(x) => match x {
            Some(n) => format!("{{\"value\":{n}}}"),
            None => "{\"value\":null}".to_string(),
        },
        Value::Rows(m) => rows(m, page),
        // A pair grouping asked for in the query language. One object per pair, naming both
        // halves, because this route's answers name what they hold.
        Value::Pairs(pairs) => {
            let items: Vec<String> = pairs
                .iter()
                .map(|p| format!("{{\"left\":{},\"right\":{}}}", group(&p.left), group(&p.right)))
                .collect();
            format!("{{\"pairs\":[{}]}}", items.join(","))
        }
        // A projection asked for in the query language rather than in SQL. One object per
        // record rather than an array of cells, because this route's answers name what they
        // hold - the result-set shape belongs to `/sql`, where the client asked for a table.
        Value::Table(t) => {
            let items: Vec<String> = t
                .iter()
                .map(|p| {
                    let cells: Vec<String> = p.values.iter().map(projection).collect();
                    format!("{{\"record\":{},\"values\":[{}]}}", p.record, cells.join(","))
                })
                .collect();
            format!("{{\"rows\":[{}]}}", items.join(","))
        }
        Value::Groups(g) => {
            let items: Vec<String> = g.iter().map(group).collect();
            format!("{{\"groups\":[{}]}}", items.join(","))
        }
    }
}

/// One SQL answer as a result set: the columns, then a row per line of it.
///
/// **A different shape from [`value_paged`] on purpose.** The other query route answers
/// `{"count": 41}` because that is what its language asked for - a count, a sum, a list of
/// groups. A SQL client asked for a table and is written to read one, so it gets columns and
/// rows even when the table is one cell wide. Two surfaces, two shapes, and neither pretending
/// to be the other.
///
/// `values` holds one answer per plan the statement made, in the order the shape names them.
/// Assembling them into rows is the last thing that happens to an answer, and it happens here
/// because here is after the merge - see `big_sql::Shape`.
pub fn result_set(answer: &Answer, values: &[Value]) -> String {
    write_rows(answer.format, &big_api::result_set(answer, values))
}

/// A result set, spelled the way the statement's `FORMAT` asked for.
///
/// Every format renders from the same [`ResultSet`], which is what a cell *is* rather than what
/// one format spells it as. This used to build the JSON row first and take it apart again for
/// the separated formats; the cells are typed now, so each format writes them once.
fn write_rows(format: Format, set: &ResultSet) -> String {
    if format == Format::Json {
        let columns: Vec<String> = set.columns.iter().map(|c| string(c)).collect();
        let rows: Vec<String> = set.rows.iter().map(|r| json_row(r)).collect();
        return format!("{{\"columns\":[{}],\"rows\":[{}]}}", columns.join(","), rows.join(","));
    }
    let sep = match format {
        Format::Csv | Format::CsvWithNames => ',',
        _ => '\t',
    };
    let mut out = String::new();
    if matches!(format, Format::TsvWithNames | Format::CsvWithNames) {
        out.push_str(&set.columns.join(&sep.to_string()));
        out.push('\n');
    }
    for row in &set.rows {
        let cells: Vec<String> = row.iter().map(separated_cell).collect();
        out.push_str(&cells.join(&sep.to_string()));
        out.push('\n');
    }
    out
}

/// One row as a JSON array.
fn json_row(row: &[Datum]) -> String {
    let cells: Vec<String> = row.iter().map(json_cell).collect();
    format!("[{}]", cells.join(","))
}

/// One cell as JSON.
fn json_cell(d: &Datum) -> String {
    match d {
        Datum::Null => "null".to_string(),
        Datum::Int(v) => v.to_string(),
        Datum::Real(v) => real(*v),
        Datum::Text(s) => string(s),
        Datum::Keys(keys) => keys_cell(keys),
    }
}

/// One cell in a separated format, where there are no quotes around a string.
fn separated_cell(d: &Datum) -> String {
    match d {
        Datum::Null => "null".to_string(),
        Datum::Int(v) => v.to_string(),
        Datum::Real(v) => real(*v),
        Datum::Text(s) => bare(s),
        // A list has no separated spelling, so it keeps its JSON one. A client reading `topK`
        // out of a CSV is reading one JSON array per cell, which is what it was before typed
        // cells and what ClickHouse answers with too.
        Datum::Keys(keys) => keys_cell(keys),
    }
}

/// The list `topK` answers with, as a JSON array in every format.
fn keys_cell(keys: &[String]) -> String {
    let items: Vec<String> = keys.iter().map(|k| string(k)).collect();
    format!("[{}]", items.join(","))
}

/// A text cell with no quotes around it, for the formats that do not quote.
///
/// Escaped the way JSON escapes it *except* for the quote and the backslash, which have nothing
/// to close here. A control character still goes out as `\n` or `\u0009` rather than raw,
/// because a raw newline would end the row and a raw tab would end the cell, and neither format
/// has quoting to contain them.
fn bare(s: &str) -> String {
    let escaped = string(s);
    let inner = &escaped[1..escaped.len() - 1];
    inner.replace("\\\"", "\"").replace("\\\\", "\\")
}

/// A number with a fractional part, kept telling apart from one without.
///
/// Rust prints a whole float as `3` and JSON accepts it, but a column that is sometimes `3` and
/// sometimes `3.5` is a column a client has to sniff. A trailing `.0` keeps every average the
/// same kind of number.
fn real(v: f64) -> String {
    let s = v.to_string();
    if s.contains(['.', 'e', 'E']) {
        s
    } else {
        format!("{s}.0")
    }
}

/// One projected cell.
///
/// **Additive on purpose.** An integer column still renders as a JSON number and an absent cell
/// still as `null`, exactly as before segments existed - so no client that could read a
/// projection can be broken by one. A string and an array only appear for a keyed column, which
/// a projection used to refuse outright.
fn projection(p: &big_api::Projection) -> String {
    match p {
        big_api::Projection::Absent => "null".to_string(),
        big_api::Projection::Int(v) => v.to_string(),
        big_api::Projection::Text(s) => string(s),
        big_api::Projection::Texts(v) => {
            let items: Vec<String> = v.iter().map(|s| string(s)).collect();
            format!("[{}]", items.join(","))
        }
    }
}

fn group(g: &Group) -> String {
    let key = match &g.key {
        Some(k) => string(k),
        None => "null".to_string(),
    };
    format!("{{\"key\":{key},\"row\":{},\"value\":{}}}", g.row, value(&g.value))
}

/// The schema snapshot as JSON: tables, each with its fields, kinds and bit depths.
pub fn schema(tables: &[TableInfo]) -> String {
    let items: Vec<String> = tables
        .iter()
        .map(|t| {
            let fields: Vec<String> = t
                .fields
                .iter()
                .map(|f| {
                    // `scale` only for a decimal, `granularity` only for a time quantum: a
                    // field that has neither would carry two fields that mean nothing, and a
                    // reader would have to know which kinds to ignore them for.
                    let mut extra = String::new();
                    if f.kind == big_api::FieldKind::Decimal {
                        // A decimal without its scale is an integer wearing a different name:
                        // `price > 5` means `> 500` on a field with two of them, and a client
                        // that cannot see the scale cannot know that.
                        extra.push_str(&format!(",\"scale\":{}", f.scale));
                    }
                    if !f.granularity.is_empty() {
                        let views: Vec<String> = f
                            .granularity
                            .iter()
                            .map(|g| string(&g.as_char().to_string()))
                            .collect();
                        extra.push_str(&format!(",\"granularity\":[{}]", views.join(",")));
                    }
                    format!(
                        "{{\"name\":{},\"kind\":{},\"bit_depth\":{}{extra}}}",
                        string(&f.name),
                        string(&format!("{:?}", f.kind).to_lowercase()),
                        f.bit_depth
                    )
                })
                .collect();
            format!(
                "{{\"name\":{},\"engine\":{},\"fields\":[{}]}}",
                string(&t.name),
                string(t.engine.as_str()),
                fields.join(",")
            )
        })
        .collect();
    format!("{{\"tables\":[{}]}}", items.join(","))
}

/// Whether every copy of every range still holds the same facts.
///
/// `agree` appears twice on purpose: once per range, because that is where a repair happens,
/// and once at the top, because that is the answer to the question that was asked. A copy that
/// could not be reached carries `why` and no digest - unreachable is not agreement, and a
/// report that rolled the two together would be a report an operator learns to ignore.
pub fn verify(ranges: &[RangeVerdict]) -> String {
    let items: Vec<String> = ranges
        .iter()
        .map(|r| {
            let copies: Vec<String> = r
                .copies
                .iter()
                .map(|c| {
                    let digest = c.digest.map_or("null".to_string(), |d| d.to_string());
                    let why = c.why.as_deref().map_or("null".to_string(), string);
                    format!("{{\"node\":{},\"digest\":{digest},\"why\":{why}}}", string(&c.node))
                })
                .collect();
            format!(
                "{{\"shards\":{},\"primary\":{},\"agree\":{},\"copies\":[{}]}}",
                string(&r.shards),
                string(&r.primary),
                r.agree,
                copies.join(",")
            )
        })
        .collect();
    let agree = ranges.iter().all(|r| r.agree);
    format!("{{\"agree\":{agree},\"ranges\":[{}]}}", items.join(","))
}

/// What a write managed, and what it did not.
///
/// `missed` is absent when everything landed, which is the shape every existing client already
/// reads. A cluster that has chosen availability is the only one that can produce a non-empty
/// one, and a client that ignores the field gets exactly what it got before - which is the
/// point: the field is there for the caller who wants to know, not as a shape change for the
/// caller who does not.
pub fn wrote(name: &str, outcome: &WriteOutcome) -> String {
    if outcome.missed.is_empty() {
        return format!("{{\"{name}\":{}}}", outcome.count);
    }
    let missed: Vec<String> = outcome.missed.iter().map(|m| string(m)).collect();
    format!("{{\"{name}\":{},\"missed\":[{}]}}", outcome.count, missed.join(","))
}

/// What a repair managed, per copy.
pub fn repaired(reports: &[RepairReport]) -> String {
    let items: Vec<String> = reports
        .iter()
        .map(|r| {
            format!(
                "{{\"node\":{},\"fragments\":{},\"outcome\":{}}}",
                string(&r.node),
                r.fragments,
                string(&r.outcome)
            )
        })
        .collect();
    format!("{{\"repaired\":[{}]}}", items.join(","))
}
