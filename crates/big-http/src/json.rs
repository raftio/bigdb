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

use big_cluster::{MoveReport, RangeVerdict, RepairReport, Topology, WriteOutcome};
use big_db::RecordId;
use big_embed::{Datum, Format, GroupAt, GroupKey, ResultSet, TableInfo, TimeUnit};
use big_exec::{Group, Value};

/// The JSON primitives, re-exported from [`big_wire`].
///
/// They moved out with the parser: escaping a string and shaping an error body are things a
/// proxy needs and a database is not required for. Everything below them here — rows, result
/// sets, schemas, topologies — is shaped by the engine's types and stayed.
pub use big_wire::json::{error, string};

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
        // Through `real` for the reason every other float in this file is: a column that is
        // sometimes `3` and sometimes `3.5` is a column a client has to sniff.
        Value::RealSum(n) => format!("{{\"sum\":{}}}", real(*n)),
        Value::RealExtreme(x) => match x {
            Some(n) => format!("{{\"value\":{}}}", real(*n)),
            None => "{\"value\":null}".to_string(),
        },
        Value::Rows(m) => rows(m, page),
        // A pair grouping asked for in the query language. One object per pair, naming both
        // halves, because this route's answers name what they hold.
        // A list of keys rather than named halves, because the arity is a number here:
        // `left`/`right` had nothing to call a third.
        Value::Tuples(tuples) => {
            let items: Vec<String> = tuples
                .iter()
                .map(|t| {
                    let keys: Vec<String> = t.keys.iter().map(group_key).collect();
                    format!("{{\"keys\":[{}],\"value\":{}}}", keys.join(","), value(&t.value))
                })
                .collect();
            format!("{{\"tuples\":[{}]}}", items.join(","))
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

/// A result set, spelled the way the statement's `FORMAT` asked for.
///
/// **A different shape from [`value_paged`] on purpose.** The other query route answers
/// `{"count": 41}` because that is what its language asked for - a count, a sum, a list of
/// groups. A SQL client asked for a table and is written to read one, so it gets columns and
/// rows even when the table is one cell wide. Two surfaces, two shapes, and neither pretending
/// to be the other.
///
/// Every format renders from the same [`ResultSet`], which is what a cell *is* rather than what
/// one format spells it as. This used to build the JSON row first and take it apart again for
/// the separated formats; the cells are typed now, so each format writes them once.
///
/// The rows arrive already assembled: a statement's answers become rows in the coordinator,
/// because only there is every owner's answer in - and because three of the four kinds of
/// statement have no plans to assemble at all. See `big_cluster::Cluster::sql`.
pub fn result_set(format: Format, set: &ResultSet) -> String {
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
        // A JSON number, not a string: `12.50` is what the value is, and quoting it would make
        // every client parse a decimal out of text.
        Datum::Dec { units, scale } => big_embed::fixed(*units, *scale),
        Datum::Real(v) => real(*v),
        // A JSON **string**, unlike every other number here. An ISO date is a string in every
        // schema anyone will point at this, and a bare count of days from 1970 is a number no
        // reader can read.
        Datum::Date(d) => string(&big_embed::date_text(*d)),
        Datum::Timestamp(t) => string(&big_embed::timestamp_text(*t)),
        Datum::Text(s) => string(s),
        Datum::Keys(keys) => keys_cell(keys),
    }
}

/// One cell in a separated format, where there are no quotes around a string.
fn separated_cell(d: &Datum) -> String {
    match d {
        Datum::Null => "null".to_string(),
        Datum::Int(v) => v.to_string(),
        Datum::Dec { units, scale } => big_embed::fixed(*units, *scale),
        Datum::Real(v) => real(*v),
        // Unquoted, which is what a spreadsheet and an `awk` script both want, and safe to leave
        // bare because a date has no separator or control character in it.
        Datum::Date(d) => big_embed::date_text(*d),
        Datum::Timestamp(t) => big_embed::timestamp_text(*t),
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
fn projection(p: &big_embed::Projection) -> String {
    match p {
        big_embed::Projection::Absent => "null".to_string(),
        big_embed::Projection::Int(v) => v.to_string(),
        big_embed::Projection::Real(v) => real(*v),
        big_embed::Projection::Text(s) => string(s),
        big_embed::Projection::Texts(v) => {
            let items: Vec<String> = v.iter().map(|s| string(s)).collect();
            format!("[{}]", items.join(","))
        }
    }
}

fn group_key(k: &GroupKey) -> String {
    let (key, row) = named_at(k.at, k.key.as_deref());
    format!("{{\"key\":{key},\"row\":{row}}}")
}

fn group(g: &Group) -> String {
    let (key, row) = named_at(g.at, g.key.as_deref());
    format!("{{\"key\":{key},\"row\":{row},\"value\":{}}}", value(&g.value))
}

/// What a group is called on this route, and the number it is addressed by.
///
/// **A bucket names itself.** A row id is meaningless without the dictionary that issued it, so a
/// keyed group carries the string beside it and this route hands back both. A calendar bucket has
/// no dictionary and needs none: the moment it starts *is* its name, so it is written out as the
/// date it stands for rather than as a number a reader would have to know the unit of to read.
fn named_at(at: GroupAt, key: Option<&str>) -> (String, String) {
    match at {
        GroupAt::Row(row) => (key.map_or_else(|| "null".to_string(), string), row.to_string()),
        GroupAt::Bucket { start, unit } => (
            string(&match unit {
                TimeUnit::Days => big_civil::format_date(start),
                TimeUnit::Seconds => big_civil::format_datetime(start),
            }),
            start.to_string(),
        ),
    }
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
                    if f.kind == big_embed::FieldKind::Decimal {
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

/// What an early answer says, and the one field that separates it from a durable one.
///
/// `"durable":false` is emitted **only** here, so a caller that never asks to be answered early
/// sees byte for byte what it saw before. A client that ignores the field gets the count it
/// always got; a client that reads it knows this number is a promise about what was accepted
/// rather than a report of what landed.
pub fn queued(name: &str, count: u64) -> String {
    format!("{{\"{name}\":{count},\"durable\":false}}")
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
                "{{\"node\":{},\"shards\":{},\"fragments\":{},\"outcome\":{}}}",
                string(&r.node),
                string(&r.shards),
                r.fragments,
                string(&r.outcome)
            )
        })
        .collect();
    format!("{{\"repaired\":[{}]}}", items.join(","))
}

/// What the cluster looks like right now.
///
/// **The one surface an autoscaler or a Kubernetes controller reads.** Everything a placement
/// decision needs is here - which ranges exist, who holds each, what is moving - so nothing
/// outside has to infer the shape from a config file it was not given.
pub fn topology(t: &Topology) -> String {
    let ranges: Vec<String> = t
        .ranges
        .iter()
        .map(|r| {
            let holders: Vec<String> = r.holders.iter().map(|h| string(h)).collect();
            let moving = match (&r.moving_to, r.moving_state) {
                (Some(to), Some(state)) => {
                    format!(",\"moving_to\":{},\"moving_state\":{}", string(to), string(state))
                }
                _ => String::new(),
            };
            format!(
                "{{\"id\":{},\"shards\":{},\"primary\":{},\"holders\":[{}]{}}}",
                r.id,
                string(&r.shards),
                string(&r.primary),
                holders.join(","),
                moving
            )
        })
        .collect();
    let members: Vec<String> = t
        .members
        .iter()
        .map(|m| {
            format!(
                "{{\"name\":{},\"addr\":{},\"state\":{}}}",
                string(&m.name),
                string(&m.addr),
                string(m.state)
            )
        })
        .collect();
    let behind: Vec<String> = t.behind.iter().map(|b| string(b)).collect();
    let leader = match &t.leader {
        Some(l) => string(l),
        None => "null".to_string(),
    };
    format!(
        "{{\"cluster_id\":{},\"epoch\":{},\"leader\":{},\"schema_leader\":{},\"members\":[{}],\
         \"ranges\":[{}],\"behind\":[{}]}}",
        string(&t.cluster_id),
        t.epoch,
        leader,
        string(&t.schema_leader),
        members.join(","),
        ranges.join(","),
        behind.join(",")
    )
}

/// What a move managed.
pub fn moved(r: &MoveReport) -> String {
    format!(
        "{{\"range\":{},\"shards\":{},\"from\":{},\"to\":{},\"fragments\":{},\
         \"dropped\":{},\"outcome\":{}}}",
        r.range,
        string(&r.shards),
        string(&r.from),
        string(&r.to),
        r.fragments,
        r.dropped,
        string(&r.outcome)
    )
}
