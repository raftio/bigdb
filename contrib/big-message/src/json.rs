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

//! Enough JSON to read the two bodies this crate ever receives.
//!
//! `POST /sql` answers an `INSERT` with `{"columns":["inserted"],"rows":[[n]]}` and refuses with
//! `{"error":"...","code":"..."}` - `big_http::json::error`. Nothing else arrives here.
//!
//! A parser rather than a scan for the field names, because the one field this has to read out
//! of a refusal is a **sentence the server wrote**, which can contain a brace, a quote or a
//! colon. Matching on prose is what the `code` field exists to avoid; parsing prose out of a
//! document by looking for punctuation is the same mistake one layer down.

/// One JSON value, in the shapes the two bodies use.
#[derive(Clone, PartialEq, Debug)]
pub(crate) enum Value {
    Null,
    Bool(bool),
    /// Kept as written. Nothing here needs a float, and a number read into one and back out
    /// again is a number that can come back different.
    Num(String),
    Str(String),
    Arr(Vec<Value>),
    Obj(Vec<(String, Value)>),
}

impl Value {
    /// The value of a field, for an object.
    pub(crate) fn get(&self, name: &str) -> Option<&Value> {
        match self {
            Self::Obj(fields) => fields.iter().find(|(k, _)| k == name).map(|(_, v)| v),
            _ => None,
        }
    }

    /// The nth element, for an array.
    pub(crate) fn at(&self, i: usize) -> Option<&Value> {
        match self {
            Self::Arr(items) => items.get(i),
            _ => None,
        }
    }

    /// The text of a string, or the digits of a number.
    pub(crate) fn cell(&self) -> Option<&str> {
        match self {
            Self::Str(s) | Self::Num(s) => Some(s),
            _ => None,
        }
    }
}

/// Reads one document, or says where it stopped making sense.
pub(crate) fn parse(text: &str) -> Result<Value, String> {
    let bytes = text.as_bytes();
    let mut i = 0;
    let value = value(bytes, &mut i)?;
    space(bytes, &mut i);
    if i != bytes.len() {
        return Err(format!("trailing text after the document, at byte {i}"));
    }
    Ok(value)
}

fn value(s: &[u8], i: &mut usize) -> Result<Value, String> {
    space(s, i);
    match s.get(*i) {
        None => Err("the document ended early".to_string()),
        Some(b'{') => object(s, i),
        Some(b'[') => array(s, i),
        Some(b'"') => string(s, i).map(Value::Str),
        Some(b't') => word(s, i, "true").map(|()| Value::Bool(true)),
        Some(b'f') => word(s, i, "false").map(|()| Value::Bool(false)),
        Some(b'n') => word(s, i, "null").map(|()| Value::Null),
        Some(_) => number(s, i),
    }
}

fn object(s: &[u8], i: &mut usize) -> Result<Value, String> {
    *i += 1;
    let mut fields = Vec::new();
    space(s, i);
    if s.get(*i) == Some(&b'}') {
        *i += 1;
        return Ok(Value::Obj(fields));
    }
    loop {
        space(s, i);
        let key = string(s, i)?;
        space(s, i);
        if s.get(*i) != Some(&b':') {
            return Err(format!("expected a colon after a field name, at byte {i}"));
        }
        *i += 1;
        fields.push((key, value(s, i)?));
        space(s, i);
        match s.get(*i) {
            Some(b',') => *i += 1,
            Some(b'}') => {
                *i += 1;
                return Ok(Value::Obj(fields));
            }
            _ => return Err(format!("expected a comma or a closing brace, at byte {i}")),
        }
    }
}

fn array(s: &[u8], i: &mut usize) -> Result<Value, String> {
    *i += 1;
    let mut items = Vec::new();
    space(s, i);
    if s.get(*i) == Some(&b']') {
        *i += 1;
        return Ok(Value::Arr(items));
    }
    loop {
        items.push(value(s, i)?);
        space(s, i);
        match s.get(*i) {
            Some(b',') => *i += 1,
            Some(b']') => {
                *i += 1;
                return Ok(Value::Arr(items));
            }
            _ => return Err(format!("expected a comma or a closing bracket, at byte {i}")),
        }
    }
}

fn string(s: &[u8], i: &mut usize) -> Result<String, String> {
    if s.get(*i) != Some(&b'"') {
        return Err(format!("expected a string, at byte {i}"));
    }
    *i += 1;
    let mut out = String::new();
    loop {
        match s.get(*i) {
            None => return Err("a string ran to the end of the document".to_string()),
            Some(b'"') => {
                *i += 1;
                return Ok(out);
            }
            Some(b'\\') => {
                *i += 1;
                let escape = s.get(*i).ok_or("an escape ran to the end of the document")?;
                *i += 1;
                match escape {
                    b'"' => out.push('"'),
                    b'\\' => out.push('\\'),
                    b'/' => out.push('/'),
                    b'b' => out.push('\u{8}'),
                    b'f' => out.push('\u{c}'),
                    b'n' => out.push('\n'),
                    b'r' => out.push('\r'),
                    b't' => out.push('\t'),
                    b'u' => {
                        // The server writes these for control characters and nothing else, so a
                        // lone surrogate never arrives; one is replaced rather than refused,
                        // because a refusal here would lose the whole sentence over one byte.
                        let hex = s
                            .get(*i..*i + 4)
                            .and_then(|h| core::str::from_utf8(h).ok())
                            .and_then(|h| u32::from_str_radix(h, 16).ok())
                            .ok_or_else(|| format!("a broken \\u escape, at byte {i}"))?;
                        *i += 4;
                        out.push(char::from_u32(hex).unwrap_or('\u{fffd}'));
                    }
                    other => return Err(format!("an unknown escape: \\{}", *other as char)),
                }
            }
            Some(_) => {
                // Copied a character at a time so that multi-byte UTF-8 survives: the server's
                // sentences name tables and fields, which are whatever the schema called them.
                let rest = core::str::from_utf8(&s[*i..])
                    .map_err(|_| "the document is not UTF-8".to_string())?;
                let c = rest.chars().next().expect("the slice is not empty");
                *i += c.len_utf8();
                out.push(c);
            }
        }
    }
}

fn number(s: &[u8], i: &mut usize) -> Result<Value, String> {
    let start = *i;
    if s.get(*i) == Some(&b'-') {
        *i += 1;
    }
    while s
        .get(*i)
        .is_some_and(|b| b.is_ascii_digit() || matches!(b, b'.' | b'e' | b'E' | b'+' | b'-'))
    {
        *i += 1;
    }
    if *i == start {
        return Err(format!("expected a value, at byte {start}"));
    }
    Ok(Value::Num(String::from_utf8_lossy(&s[start..*i]).into_owned()))
}

fn word(s: &[u8], i: &mut usize, want: &str) -> Result<(), String> {
    if s.get(*i..*i + want.len()) == Some(want.as_bytes()) {
        *i += want.len();
        Ok(())
    } else {
        Err(format!("expected {want}, at byte {i}"))
    }
}

fn space(s: &[u8], i: &mut usize) {
    while s.get(*i).is_some_and(u8::is_ascii_whitespace) {
        *i += 1;
    }
}

/// How many rows an `INSERT` reported, from `{"columns":["inserted"],"rows":[[n]]}`.
///
/// `None` when the body is not that shape. The caller has a better answer than a guess in that
/// case - the server returns the statement's own row count, which the caller already knows.
pub(crate) fn inserted(body: &str) -> Option<u64> {
    parse(body).ok()?.get("rows")?.at(0)?.at(0)?.cell()?.parse().ok()
}

/// The code and the sentence out of a refusal.
///
/// Falls back to the raw text, because a 5xx from a reverse proxy is HTML rather than
/// `big_http::json::error`'s JSON, and showing it is more use than reporting that it would not
/// parse. The sentence is never reworded: `sql_no_joins` has to mean the same thing here as it
/// does on the command line, and a client that paraphrased would be where the two drift apart.
pub(crate) fn failure(body: &str) -> (String, String) {
    let unknown = || ("unknown".to_string(), body.trim().to_string());
    let Ok(value) = parse(body) else { return unknown() };
    match (value.get("code").and_then(Value::cell), value.get("error").and_then(Value::cell)) {
        (Some(code), Some(message)) => (code.to_string(), message.to_string()),
        _ => unknown(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_answer_to_an_insert_is_read_as_its_row_count() {
        assert_eq!(inserted(r#"{"columns":["inserted"],"rows":[[2]]}"#), Some(2));
        assert_eq!(inserted(r#"{"columns":["inserted"],"rows":[[8000]]}"#), Some(8000));
    }

    #[test]
    fn a_body_that_is_not_that_shape_is_not_a_zero() {
        // Zero would be a wrong answer that reads like a right one. `None` lets the caller fall
        // back to the count it already knows.
        assert_eq!(inserted("{}"), None);
        assert_eq!(inserted(r#"{"rows":[]}"#), None);
        assert_eq!(inserted("not json at all"), None);
    }

    #[test]
    fn a_refusal_keeps_the_servers_own_code_and_sentence() {
        let body = r#"{"error":"unknown field nope in table t","code":"unknown_field"}"#;
        let (code, message) = failure(body);
        assert_eq!(code, "unknown_field");
        assert_eq!(message, "unknown field nope in table t");
    }

    #[test]
    fn a_sentence_holding_punctuation_survives_being_read() {
        // The reason this file parses instead of scanning for `"error":"` and the next quote.
        let body = r#"{"error":"line 3: {\"a\": 1} is not a value","code":"malformed_line"}"#;
        let (code, message) = failure(body);
        assert_eq!(code, "malformed_line");
        assert_eq!(message, r#"line 3: {"a": 1} is not a value"#);
    }

    #[test]
    fn a_body_that_is_not_json_is_passed_through_rather_than_lost() {
        let (code, message) = failure("<html>502 Bad Gateway</html>");
        assert_eq!(code, "unknown");
        assert_eq!(message, "<html>502 Bad Gateway</html>");
    }

    #[test]
    fn a_sentence_naming_a_table_in_another_script_survives() {
        let body = r#"{"error":"unknown table 顧客","code":"unknown_table"}"#;
        assert_eq!(failure(body).1, "unknown table 顧客");
    }

    #[test]
    fn the_shapes_the_server_writes_all_parse() {
        for body in [
            r#"{"columns":["inserted"],"rows":[[2]]}"#,
            r#"{"imported":42}"#,
            r#"{"imported":2,"missed":["node-b: connection refused"]}"#,
            r#"{"records":[0,1],"next":null}"#,
            r#"{"columns":["a"],"rows":[["x"],[true],[null],[-1.5]]}"#,
        ] {
            parse(body).unwrap_or_else(|e| panic!("{body} did not parse: {e}"));
        }
    }
}
