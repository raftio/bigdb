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
