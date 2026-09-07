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

//! One key out of a JSON document held in a keyed column.
//!
//! # Why this is written here rather than reached for
//!
//! It is a few hundred bytes of scanning, and what it replaces is a dependency. The argument is
//! the one `sql_no_regex` makes about a regex engine: the work happens inside the render loop,
//! once per row per call, and a general parser brings a compile step, a class of pathological
//! input, and a version to track - to answer a question that is "find this key at the top level".
//!
//! # What it does not do
//!
//! **One level, and the key is a name rather than a path.** A nested value comes back from
//! `JSONExtractRaw` as the text it was written as, which the next call reads a key out of in
//! turn; that is the composition a path syntax would otherwise buy, without a second grammar to
//! keep in step with somebody else's.
//!
//! **Malformed input is absent, not an error.** A column of documents is a column somebody else
//! filled, and one bad row is not a reason to fail the statement it appears in - the same
//! decision `Datum::Null` already represents everywhere else here. There is nothing this can
//! return for "the eleventh row was truncated" that a client could act on differently.

/// The raw text of `key`'s value in `doc`, or `None`.
///
/// Whitespace-tolerant, escape-aware inside strings, and brace/bracket-balanced so a nested
/// object's own keys are skipped rather than matched. It scans the top-level object once and
/// stops at the first match: a document with a key twice answers with the first, which is what
/// every reader that streams does.
pub fn raw<'a>(doc: &'a str, key: &str) -> Option<&'a str> {
    let b = doc.as_bytes();
    let mut i = skip_ws(b, 0);
    if b.get(i) != Some(&b'{') {
        return None;
    }
    i = skip_ws(b, i + 1);
    while i < b.len() && b[i] != b'}' {
        // A member is a quoted name, a colon, then a value. Anything else and the document is
        // not one this can read, which is the same answer as a key that is not there.
        let (name, next) = string_at(b, i)?;
        i = skip_ws(b, next);
        if b.get(i) != Some(&b':') {
            return None;
        }
        i = skip_ws(b, i + 1);
        let end = value_end(b, i)?;
        // A member whose value is empty - `{"a":` truncated, or `{"a": , ...}` - is where the
        // document stopped making sense, so the search stops with it rather than handing back
        // the empty string. An empty string is a value somebody could have written, and telling
        // it apart from this afterwards is not possible.
        let value = doc[i..end].trim();
        if value.is_empty() {
            return None;
        }
        if name == key {
            return Some(value);
        }
        i = skip_ws(b, end);
        // A comma continues the object; a brace ends it. A document that does neither has run
        // out of shape, so there is nothing left to search.
        match b.get(i) {
            Some(b',') => i = skip_ws(b, i + 1),
            Some(b'}') => return None,
            _ => return None,
        }
    }
    None
}

/// The same value with a string's quotes and escapes taken off, for the text-shaped call.
///
/// A non-string value comes back as it was written - `JSONExtractString(d, 'n')` over `{"n":5}`
/// is `"5"` - because the alternative is absence, and a number a caller asked to see as text is
/// a number they can see.
pub fn text(doc: &str, key: &str) -> Option<String> {
    let v = raw(doc, key)?;
    if v.as_bytes().first() == Some(&b'"') {
        let (s, _) = string_at(v.as_bytes(), 0)?;
        return Some(s);
    }
    Some(v.to_string())
}

/// The first index at or after `i` that is not JSON whitespace.
fn skip_ws(b: &[u8], mut i: usize) -> usize {
    while i < b.len() && matches!(b[i], b' ' | b'\t' | b'\n' | b'\r') {
        i += 1;
    }
    i
}

/// The string starting at `i`, unescaped, and the index just past its closing quote.
fn string_at(b: &[u8], i: usize) -> Option<(String, usize)> {
    if b.get(i) != Some(&b'"') {
        return None;
    }
    let mut out = String::new();
    let mut i = i + 1;
    while i < b.len() {
        match b[i] {
            b'"' => return Some((out, i + 1)),
            b'\\' => {
                let c = *b.get(i + 1)?;
                match c {
                    b'n' => out.push('\n'),
                    b't' => out.push('\t'),
                    b'r' => out.push('\r'),
                    b'b' => out.push('\u{8}'),
                    b'f' => out.push('\u{c}'),
                    // `\uXXXX`, including the surrogate pair a character past the basic plane
                    // is written as. A lone or malformed one leaves the document unreadable
                    // rather than producing a replacement character, because a replacement
                    // character is a value somebody would then have to tell apart from a real
                    // one.
                    b'u' => {
                        let (ch, used) = unicode_at(b, i + 2)?;
                        out.push(ch);
                        i += used;
                    }
                    // `\"`, `\\`, `\/` and anything else stand for themselves.
                    other => out.push(char::from(other)),
                }
                i += 2;
            }
            _ => {
                // Multi-byte UTF-8 is copied whole rather than byte by byte, so a character is
                // never split across two pushes.
                let rest = core::str::from_utf8(&b[i..]).ok()?;
                let ch = rest.chars().next()?;
                out.push(ch);
                i += ch.len_utf8();
            }
        }
    }
    None
}

/// The character `\uXXXX` at `i` spells, and how many bytes past `i` it used.
fn unicode_at(b: &[u8], i: usize) -> Option<(char, usize)> {
    let hex = |at: usize| -> Option<u32> {
        let s = core::str::from_utf8(b.get(at..at + 4)?).ok()?;
        u32::from_str_radix(s, 16).ok()
    };
    let first = hex(i)?;
    // A high surrogate is only half a character, and the other half is the next escape.
    if (0xD800..0xDC00).contains(&first) {
        if b.get(i + 4) != Some(&b'\\') || b.get(i + 5) != Some(&b'u') {
            return None;
        }
        let second = hex(i + 6)?;
        if !(0xDC00..0xE000).contains(&second) {
            return None;
        }
        let c = 0x1_0000 + ((first - 0xD800) << 10) + (second - 0xDC00);
        return Some((char::from_u32(c)?, 10));
    }
    Some((char::from_u32(first)?, 4))
}

/// The index just past the value starting at `i`.
///
/// Nesting is counted rather than parsed: what this needs is where the value ends, and a scan
/// that tracks depth and knows not to count a brace inside a string gets that without deciding
/// whether what it skipped was well formed.
fn value_end(b: &[u8], i: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut i = i;
    while i < b.len() {
        match b[i] {
            b'"' => {
                let (_, next) = string_at(b, i)?;
                i = next;
                if depth == 0 {
                    return Some(i);
                }
                continue;
            }
            b'{' | b'[' => depth += 1,
            b'}' | b']' => {
                // A closer at depth zero belongs to the object this member is in, so the value
                // ended before it - which is the scalar case, handled below.
                if depth == 0 {
                    return Some(i);
                }
                depth -= 1;
                if depth == 0 {
                    return Some(i + 1);
                }
            }
            b',' if depth == 0 => return Some(i),
            _ => {}
        }
        i += 1;
    }
    (depth == 0).then_some(b.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_top_level_key_comes_back_as_written_and_as_text() {
        let d = r#"{"a": "x", "n": 5, "f": 1.5, "t": true}"#;
        assert_eq!(raw(d, "a"), Some(r#""x""#));
        assert_eq!(text(d, "a").as_deref(), Some("x"));
        assert_eq!(raw(d, "n"), Some("5"));
        assert_eq!(raw(d, "f"), Some("1.5"));
        assert_eq!(raw(d, "t"), Some("true"));
        assert_eq!(raw(d, "nope"), None);
    }

    /// A nested value comes back whole, which is what makes one level compose into two.
    #[test]
    fn a_nested_value_is_the_text_it_was_written_as() {
        let d = r#"{"o": {"b": 2}, "l": [1, {"x": 3}], "after": 9}"#;
        assert_eq!(raw(d, "o"), Some(r#"{"b": 2}"#));
        assert_eq!(raw(d, "l"), Some(r#"[1, {"x": 3}]"#));
        // The key after a nested value is still found, which is the thing depth counting buys.
        assert_eq!(raw(d, "after"), Some("9"));
        // Two calls are a path.
        assert_eq!(raw(raw(d, "o").unwrap(), "b"), Some("2"));
    }

    /// A brace or a colon inside a string is not structure.
    #[test]
    fn punctuation_inside_a_string_is_not_structure() {
        let d = r#"{"a": "}{,:", "b": 1}"#;
        assert_eq!(text(d, "a").as_deref(), Some("}{,:"));
        assert_eq!(raw(d, "b"), Some("1"));
        // A key whose name contains the one being looked for is not a match.
        assert_eq!(raw(r#"{"abc": 1}"#, "a"), None);
    }

    #[test]
    fn escapes_come_off_for_text_and_stay_on_for_raw() {
        let d = r#"{"a": "line\nbreak \"q\" é 😀"}"#;
        assert_eq!(text(d, "a").as_deref(), Some("line\nbreak \"q\" é 😀"));
        assert!(raw(d, "a").unwrap().contains("\\n"));
    }

    /// Malformed input is absent rather than an error, and never a panic - which is the property
    /// that matters when the documents were written by somebody else.
    #[test]
    fn a_broken_document_is_absent_and_not_a_panic() {
        for d in [
            "",
            "{",
            "{\"a\"",
            "{\"a\":",
            r#"{"a": "unterminated"#,
            r#"{"a" 1}"#,
            "[1,2,3]",
            "null",
            r#"{"a": "\u00"#,
            r#"{"a": "\ud83d"}"#,
        ] {
            assert_eq!(raw(d, "a"), None, "{d}");
            assert_eq!(text(d, "a"), None, "{d}");
        }
    }

    /// **A member that was complete before the document was cut off still answers.**
    ///
    /// The scan stops at the key it was asked for, so it never reaches the missing brace - and
    /// making it reach one would turn every lookup of a first key into a walk of the whole
    /// document to prove something the caller did not ask about. What it will not do is invent
    /// a value for a member that was itself cut off, which is the case above.
    #[test]
    fn a_member_completed_before_a_truncation_is_still_read() {
        assert_eq!(raw(r#"{"a": {"b": 1}"#, "a"), Some(r#"{"b": 1}"#));
        assert_eq!(raw(r#"{"a": 1, "b": "#, "a"), Some("1"));
        // But the member that was cut off is absent, not empty.
        assert_eq!(raw(r#"{"a": 1, "b": "#, "b"), None);
    }

    /// Every prefix of a good document, which is the shape a truncated row arrives in.
    #[test]
    fn no_prefix_of_a_document_panics() {
        let d = r#"{"a": "x", "o": {"b": [1, 2]}, "n": -1.5e3}"#;
        for i in 0..d.len() {
            let _ = raw(&d[..i], "a");
            let _ = text(&d[..i], "o");
        }
    }
}
