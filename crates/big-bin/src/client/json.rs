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

//! Reading the JSON `big serve` writes, and no other JSON.
//!
//! Two halves, and the split is deliberate. [`parse`] is an ordinary reader for the subset of
//! JSON the server emits. [`Answer::read`] is the strict half: it matches the handful of shapes
//! `big serve` actually produces and **errors on anything else**, rather than falling back to
//! something plausible. There is exactly one producer for this reader, so a shape it does not
//! recognise is a version skew or a proxy in the way, and both are worth being told about.
//!
//! **Numbers stay as text.** A `Sum` over a wide field is a `u128` and a record id is a `u64`;
//! putting either through an `f64` would silently round an answer that was exact when it left
//! the server. Nothing here does arithmetic, so nothing here needs them as numbers.

use std::fmt::Write as _;

/// A JSON value, as far as this client needs one.
#[derive(Clone, PartialEq, Debug)]
pub enum Value {
    Null,
    Bool(bool),
    /// Kept exactly as written. See the module header.
    Num(String),
    Str(String),
    Arr(Vec<Value>),
    /// An object, in the order its keys were written.
    Obj(Vec<(String, Value)>),
}

impl Value {
    /// The value under `key`, for an object.
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Self::Obj(fields) => fields.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    /// One cell of a table: a scalar rendered as text, `null` as the empty string.
    ///
    /// The empty string rather than the word `null` because a table is read by a person and a
    /// pipe is read by a script, and both take an empty field to mean absent. `--format json`
    /// is there for anyone who needs to tell an absent value from an empty one.
    pub fn cell(&self) -> Option<String> {
        Some(match self {
            Self::Null => String::new(),
            Self::Bool(b) => b.to_string(),
            Self::Num(n) => n.clone(),
            Self::Str(s) => s.clone(),
            Self::Arr(items) => {
                let parts: Vec<String> = items.iter().map(|i| i.cell()).collect::<Option<_>>()?;
                parts.join(",")
            }
            // An object is not a cell. Its own arm rather than a `_` so that a nested shape
            // reaches `Answer::read`'s error instead of being flattened into something that
            // looks like data.
            Self::Obj(_) => return None,
        })
    }

    /// A single-valued object rendered as its one value: `{"count":4}` is `4`.
    ///
    /// What a group's aggregate is. Anything with more than one key would be ambiguous and is
    /// refused rather than picked from.
    fn scalar_of_one(&self) -> Option<String> {
        match self {
            Self::Obj(fields) => match fields.as_slice() {
                [(_, v)] => v.cell(),
                _ => None,
            },
            other => other.cell(),
        }
    }
}

/// Reads one JSON document, which must be the whole input.
pub fn parse(text: &str) -> Result<Value, String> {
    let bytes = text.as_bytes();
    let mut p = Parser { s: bytes, i: 0 };
    p.space();
    let value = p.value()?;
    p.space();
    if p.i < bytes.len() {
        return Err(format!("trailing input at byte {}", p.i));
    }
    Ok(value)
}

struct Parser<'a> {
    s: &'a [u8],
    i: usize,
}

impl Parser<'_> {
    fn space(&mut self) {
        while self.i < self.s.len() && self.s[self.i].is_ascii_whitespace() {
            self.i += 1;
        }
    }

    fn eat(&mut self, c: u8) -> bool {
        if self.s.get(self.i) == Some(&c) {
            self.i += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, c: u8) -> Result<(), String> {
        if self.eat(c) {
            Ok(())
        } else {
            Err(format!("expected `{}` at byte {}", c as char, self.i))
        }
    }

    fn literal(&mut self, word: &str) -> bool {
        if self.s[self.i..].starts_with(word.as_bytes()) {
            self.i += word.len();
            true
        } else {
            false
        }
    }

    fn value(&mut self) -> Result<Value, String> {
        self.space();
        match self.s.get(self.i) {
            None => Err("the document ended early".to_string()),
            Some(b'{') => self.object(),
            Some(b'[') => self.array(),
            Some(b'"') => self.string().map(Value::Str),
            Some(b't') if self.literal("true") => Ok(Value::Bool(true)),
            Some(b'f') if self.literal("false") => Ok(Value::Bool(false)),
            Some(b'n') if self.literal("null") => Ok(Value::Null),
            Some(c) if c.is_ascii_digit() || *c == b'-' => self.number(),
            Some(c) => Err(format!("unexpected `{}` at byte {}", *c as char, self.i)),
        }
    }

    fn object(&mut self) -> Result<Value, String> {
        self.expect(b'{')?;
        let mut out = Vec::new();
        self.space();
        if self.eat(b'}') {
            return Ok(Value::Obj(out));
        }
        loop {
            self.space();
            let key = self.string()?;
            self.space();
            self.expect(b':')?;
            out.push((key, self.value()?));
            self.space();
            if self.eat(b',') {
                continue;
            }
            self.expect(b'}')?;
            return Ok(Value::Obj(out));
        }
    }

    fn array(&mut self) -> Result<Value, String> {
        self.expect(b'[')?;
        let mut out = Vec::new();
        self.space();
        if self.eat(b']') {
            return Ok(Value::Arr(out));
        }
        loop {
            out.push(self.value()?);
            self.space();
            if self.eat(b',') {
                continue;
            }
            self.expect(b']')?;
            return Ok(Value::Arr(out));
        }
    }

    fn number(&mut self) -> Result<Value, String> {
        let start = self.i;
        self.eat(b'-');
        while self.s.get(self.i).is_some_and(|c| c.is_ascii_digit() || b".eE+-".contains(c)) {
            self.i += 1;
        }
        if self.i == start {
            return Err(format!("expected a number at byte {start}"));
        }
        Ok(Value::Num(String::from_utf8_lossy(&self.s[start..self.i]).into_owned()))
    }

    fn string(&mut self) -> Result<String, String> {
        self.expect(b'"')?;
        let mut out = String::new();
        loop {
            let Some(&c) = self.s.get(self.i) else {
                return Err("unterminated string".to_string());
            };
            self.i += 1;
            match c {
                b'"' => return Ok(out),
                b'\\' => {
                    let Some(&e) = self.s.get(self.i) else {
                        return Err("unterminated escape".to_string());
                    };
                    self.i += 1;
                    match e {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'u' => {
                            let hex = self
                                .s
                                .get(self.i..self.i + 4)
                                .ok_or_else(|| "truncated \\u escape".to_string())?;
                            let n = u32::from_str_radix(&String::from_utf8_lossy(hex), 16)
                                .map_err(|_| "bad \\u escape".to_string())?;
                            self.i += 4;
                            // The server only ever escapes control characters this way, so a
                            // surrogate pair cannot arrive; one is refused rather than
                            // half-decoded into a replacement character.
                            out.push(
                                char::from_u32(n)
                                    .ok_or_else(|| format!("\\u{n:04x} is not a character"))?,
                            );
                        }
                        other => return Err(format!("unknown escape \\{}", other as char)),
                    }
                }
                _ => {
                    // Copied byte-wise; the loop only compares against ASCII, so a multi-byte
                    // character passes through in pieces and reassembles correctly.
                    let start = self.i - 1;
                    while self.s.get(self.i).is_some_and(|&c| c != b'"' && c != b'\\') {
                        self.i += 1;
                    }
                    out.push_str(&String::from_utf8_lossy(&self.s[start..self.i]));
                }
            }
        }
    }
}

// -------------------------------------------------------------------------------------------
// The strict half
// -------------------------------------------------------------------------------------------

/// One answer, flattened into the columns and rows a terminal can show.
pub struct Answer {
    /// Column names, in order.
    pub columns: Vec<String>,
    /// One row per line, each the same length as `columns`.
    pub rows: Vec<Vec<String>>,
    /// Anything that is about the answer rather than in it - a cursor to continue with, a copy
    /// a write did not reach. Written to stderr, so a pipe carries only the table.
    pub notes: Vec<String>,
}

/// What the server said when it refused.
pub struct Failure {
    /// The stable identifier, chosen by the server and passed through untouched.
    pub code: String,
    /// The server's own sentence. Never reworded here: `sql_no_joins` has to mean the same
    /// thing on the command line as it does over HTTP, and a client that paraphrased would be
    /// the place the two drifted apart.
    pub message: String,
}

impl Failure {
    /// Reads an error body, falling back to the raw text when it is not one.
    ///
    /// A 5xx from a reverse proxy is HTML, not `big serve`'s JSON, and printing it is more useful
    /// than reporting that it could not be parsed.
    pub fn read(body: &str) -> Self {
        match parse(body) {
            Ok(v) => {
                let code = v.get("code").and_then(Value::cell);
                let message = v.get("error").and_then(Value::cell);
                match (code, message) {
                    (Some(code), Some(message)) => Self { code, message },
                    _ => Self { code: "unknown".to_string(), message: body.trim().to_string() },
                }
            }
            Err(_) => Self { code: "unknown".to_string(), message: body.trim().to_string() },
        }
    }
}

impl Answer {
    /// Recognises one of the shapes `big serve` writes, or says it does not.
    pub fn read(body: &str) -> Result<Self, String> {
        let value = parse(body)?;
        let Value::Obj(fields) = &value else {
            return Err("the server sent a document that is not an object".to_string());
        };
        let keys: Vec<&str> = fields.iter().map(|(k, _)| k.as_str()).collect();

        match keys.as_slice() {
            // `POST /sql`: already a result set.
            ["columns", "rows"] => result_set(&value),
            // `POST /table/{t}/query`, in each of its shapes.
            ["records", "next"] => records(&value),
            ["groups"] => groups(&value),
            // `GET /schema`.
            ["tables"] => schema(&value),
            // `GET /verify`.
            ["agree", "ranges"] => verify(&value),
            // `POST /repair`.
            ["repaired"] => repaired(&value),
            // Everything else the server writes is a flat object of scalars: a count, a sum, a
            // value, an id, a probe. Rendered as one row of its own keys, which is the reading
            // that needs no per-route knowledge and cannot be wrong about a shape it has not
            // been told about.
            _ => flat(fields),
        }
    }
}

fn result_set(v: &Value) -> Result<Answer, String> {
    let (Some(Value::Arr(columns)), Some(Value::Arr(rows))) = (v.get("columns"), v.get("rows"))
    else {
        return Err("`columns` and `rows` must both be arrays".to_string());
    };
    let columns: Vec<String> = columns
        .iter()
        .map(|c| c.cell().ok_or_else(|| "a column name is not a string".to_string()))
        .collect::<Result<_, _>>()?;

    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let Value::Arr(cells) = row else {
            return Err("a row is not an array".to_string());
        };
        if cells.len() != columns.len() {
            return Err(format!(
                "a row has {} cells and there are {} columns",
                cells.len(),
                columns.len()
            ));
        }
        out.push(
            cells
                .iter()
                .map(|c| c.cell().ok_or_else(|| "a cell is an object".to_string()))
                .collect::<Result<_, _>>()?,
        );
    }
    Ok(Answer { columns, rows: out, notes: Vec::new() })
}

fn records(v: &Value) -> Result<Answer, String> {
    let Some(Value::Arr(ids)) = v.get("records") else {
        return Err("`records` is not an array".to_string());
    };
    let rows = ids
        .iter()
        .map(|i| i.cell().map(|c| vec![c]).ok_or_else(|| "a record id is an object".to_string()))
        .collect::<Result<_, _>>()?;

    // The cursor is a note rather than a column: a script reading TSV wants ids and nothing
    // else, and a trailing row that is not a record is exactly the thing that breaks one.
    let notes = match v.get("next") {
        Some(Value::Null) | None => Vec::new(),
        Some(next) => vec![format!(
            "more records follow; continue with --after {}",
            next.cell().unwrap_or_default()
        )],
    };
    Ok(Answer { columns: vec!["id".to_string()], rows, notes })
}

fn groups(v: &Value) -> Result<Answer, String> {
    let Some(Value::Arr(groups)) = v.get("groups") else {
        return Err("`groups` is not an array".to_string());
    };
    let mut rows = Vec::with_capacity(groups.len());
    for g in groups {
        let key = g.get("key").and_then(Value::cell).unwrap_or_default();
        let row = g.get("row").and_then(Value::cell).unwrap_or_default();
        let value = g
            .get("value")
            .ok_or_else(|| "a group has no value".to_string())?
            .scalar_of_one()
            .ok_or_else(|| "a group's value is not a single measurement".to_string())?;
        rows.push(vec![key, row, value]);
    }
    Ok(Answer {
        columns: vec!["key".to_string(), "row".to_string(), "value".to_string()],
        rows,
        notes: Vec::new(),
    })
}

/// One row per field, so the whole schema is a table rather than a tree a terminal cannot show.
fn schema(v: &Value) -> Result<Answer, String> {
    let Some(Value::Arr(tables)) = v.get("tables") else {
        return Err("`tables` is not an array".to_string());
    };
    let mut rows = Vec::new();
    for t in tables {
        let name = t.get("name").and_then(Value::cell).unwrap_or_default();
        let Some(Value::Arr(fields)) = t.get("fields") else {
            return Err("a table's `fields` is not an array".to_string());
        };
        if fields.is_empty() {
            // A table with no fields is a real state and would otherwise vanish from the
            // listing entirely, which reads as "no such table".
            rows.push(vec![name.clone(), String::new(), String::new(), String::new()]);
        }
        for f in fields {
            let mut kind = f.get("kind").and_then(Value::cell).unwrap_or_default();
            // The two qualifiers that change what a kind means, folded into it rather than
            // given columns that are empty for every other kind.
            if let Some(scale) = f.get("scale").and_then(Value::cell) {
                let _ = write!(kind, "({scale})");
            }
            if let Some(g) = f.get("granularity").and_then(Value::cell) {
                let _ = write!(kind, "[{g}]");
            }
            rows.push(vec![
                name.clone(),
                f.get("name").and_then(Value::cell).unwrap_or_default(),
                kind,
                f.get("bit_depth").and_then(Value::cell).unwrap_or_default(),
            ]);
        }
    }
    Ok(Answer {
        columns: ["table", "field", "kind", "bit_depth"].map(str::to_string).to_vec(),
        rows,
        notes: Vec::new(),
    })
}

/// One row per copy of a range, because a copy is what a repair acts on.
fn verify(v: &Value) -> Result<Answer, String> {
    let Some(Value::Arr(ranges)) = v.get("ranges") else {
        return Err("`ranges` is not an array".to_string());
    };
    let mut rows = Vec::new();
    for r in ranges {
        let shards = r.get("shards").and_then(Value::cell).unwrap_or_default();
        let agree = r.get("agree").and_then(Value::cell).unwrap_or_default();
        let Some(Value::Arr(copies)) = r.get("copies") else {
            return Err("a range's `copies` is not an array".to_string());
        };
        for c in copies {
            rows.push(vec![
                shards.clone(),
                agree.clone(),
                c.get("node").and_then(Value::cell).unwrap_or_default(),
                c.get("digest").and_then(Value::cell).unwrap_or_default(),
                c.get("why").and_then(Value::cell).unwrap_or_default(),
            ]);
        }
    }
    let notes = match v.get("agree") {
        Some(Value::Bool(false)) => {
            vec!["the copies of at least one range disagree; see `bigctl repair`".to_string()]
        }
        _ => Vec::new(),
    };
    Ok(Answer {
        columns: ["shards", "agree", "node", "digest", "why"].map(str::to_string).to_vec(),
        rows,
        notes,
    })
}

fn repaired(v: &Value) -> Result<Answer, String> {
    let Some(Value::Arr(reports)) = v.get("repaired") else {
        return Err("`repaired` is not an array".to_string());
    };
    let rows = reports
        .iter()
        .map(|r| {
            vec![
                r.get("node").and_then(Value::cell).unwrap_or_default(),
                r.get("fragments").and_then(Value::cell).unwrap_or_default(),
                r.get("outcome").and_then(Value::cell).unwrap_or_default(),
            ]
        })
        .collect();
    Ok(Answer {
        columns: ["node", "fragments", "outcome"].map(str::to_string).to_vec(),
        rows,
        notes: Vec::new(),
    })
}

/// A flat object as one row of its own keys.
fn flat(fields: &[(String, Value)]) -> Result<Answer, String> {
    let mut columns = Vec::new();
    let mut row = Vec::new();
    let mut notes = Vec::new();
    for (key, value) in fields {
        // `missed` is a write that did not reach every copy. It belongs beside the count, not
        // inside it, and a person needs to see it whether or not they are looking at a table.
        if key == "missed" {
            if let Some(list) = value.cell() {
                if !list.is_empty() {
                    notes.push(format!("this write did not reach: {list}"));
                }
            }
            continue;
        }
        let Some(cell) = value.cell() else {
            return Err(format!("`{key}` is a shape this client does not know how to show"));
        };
        columns.push(key.clone());
        row.push(cell);
    }
    Ok(Answer { columns, rows: vec![row], notes })
}
