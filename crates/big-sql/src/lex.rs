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

//! Text to tokens.
//!
//! Whole-input rather than streaming: a statement is a line or two, and a `Vec<Token>` lets the
//! parser look ahead by two without threading a peek buffer through every rule — which it needs
//! for `IS NOT NULL`, `NOT IN`, and telling `count(*)` from `count(DISTINCT x)`.
//!
//! Keywords are not lexed. They arrive as [`Tok::Word`] and the parser compares them without
//! regard to case, so `country` may be a column even though `COUNT` is a function: a reserved
//! word list is a thing to keep in sync with a language, and this one has no need of it.

use crate::error::{Result, SqlError};
use big_plan::Literal;

/// One token, and where it started.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Token {
    /// What it is.
    pub tok: Tok,
    /// Byte offset of its first character, which every error message quotes.
    pub at: usize,
}

/// What a token is.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Tok {
    /// A bare word: an identifier, or a keyword, told apart by the parser.
    Word(String),
    /// A double-quoted identifier, which is never a keyword.
    Quoted(String),
    /// A single-quoted string. SQL's `''` is the escape for one quote.
    Str(String),
    /// A number, already in the shape the planner takes.
    Num(Literal),
    /// A comparison. Normalised: `==` arrives as `=` and `<>` as `!=`, so the lowering has one
    /// spelling of each to translate rather than two of some.
    Op(&'static str),
    /// `(`
    LParen,
    /// `)`
    RParen,
    /// `,`
    Comma,
    /// `*`
    Star,
    /// `.`, which only ever separates a table qualifier from a column.
    Dot,
}

/// Splits a statement into tokens, or says where it stopped making sense.
pub fn lex(input: &str) -> Result<Vec<Token>> {
    let s = input.as_bytes();
    let mut i = 0;
    let mut out = Vec::new();

    while i < s.len() {
        let c = s[i];
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        // `--` to end of line. The only comment form, because it is the only one that cannot be
        // confused with an operator here: `/*` would have to be told from a `*` in a select list.
        if c == b'-' && s.get(i + 1) == Some(&b'-') {
            while i < s.len() && s[i] != b'\n' {
                i += 1;
            }
            continue;
        }

        let at = i;
        let tok = match c {
            b'(' => {
                i += 1;
                Tok::LParen
            }
            b')' => {
                i += 1;
                Tok::RParen
            }
            b',' => {
                i += 1;
                Tok::Comma
            }
            b'*' => {
                i += 1;
                Tok::Star
            }
            // Only ever a qualifier separator: a number's decimal point is consumed by the
            // number scanner below, which is reached first because a digit cannot start here.
            b'.' => {
                i += 1;
                Tok::Dot
            }
            b'=' => {
                i += 1;
                // `==` is not SQL, and accepting it costs nothing: PQL takes it, and somebody
                // moving a predicate between the two languages should not be stopped by it.
                if s.get(i) == Some(&b'=') {
                    i += 1;
                }
                Tok::Op("=")
            }
            b'!' if s.get(i + 1) == Some(&b'=') => {
                i += 2;
                Tok::Op("!=")
            }
            b'<' => {
                i += 1;
                match s.get(i) {
                    Some(b'=') => {
                        i += 1;
                        Tok::Op("<=")
                    }
                    Some(b'>') => {
                        i += 1;
                        Tok::Op("!=")
                    }
                    _ => Tok::Op("<"),
                }
            }
            b'>' => {
                i += 1;
                if s.get(i) == Some(&b'=') {
                    i += 1;
                    Tok::Op(">=")
                } else {
                    Tok::Op(">")
                }
            }
            b'\'' => string(s, &mut i, b'\'').map(Tok::Str)?,
            b'"' => string(s, &mut i, b'"').map(Tok::Quoted)?,
            b'-' | b'0'..=b'9' => number(s, &mut i)?,
            c if c.is_ascii_alphabetic() || c == b'_' => {
                let start = i;
                while i < s.len() && (s[i].is_ascii_alphanumeric() || s[i] == b'_') {
                    i += 1;
                }
                Tok::Word(String::from_utf8_lossy(&s[start..i]).into_owned())
            }
            other => {
                return Err(SqlError::Syntax {
                    at,
                    found: (other as char).to_string(),
                    want: "a word, a number, a string, or an operator",
                })
            }
        };
        out.push(Token { tok, at });
    }
    Ok(out)
}

/// A quoted run, with the quote character doubled to mean itself.
fn string(s: &[u8], i: &mut usize, quote: u8) -> Result<String> {
    let open = *i;
    *i += 1;
    let mut out = String::new();
    loop {
        match s.get(*i) {
            None => return Err(SqlError::UnterminatedString { at: open }),
            Some(&c) if c == quote => {
                // `''` inside a string is one quote, not the end of it. Checked before the exit
                // so that `'it''s'` is one token rather than two and a syntax error.
                if s.get(*i + 1) == Some(&quote) {
                    out.push(quote as char);
                    *i += 2;
                } else {
                    *i += 1;
                    return Ok(out);
                }
            }
            Some(_) => {
                // Byte-wise so that a multi-byte character survives: the loop only ever compares
                // against ASCII quotes, and every other byte is copied through.
                let start = *i;
                while *i < s.len() && s[*i] != quote {
                    *i += 1;
                }
                out.push_str(&String::from_utf8_lossy(&s[start..*i]));
            }
        }
    }
}

/// An integer, a decimal, or either with a leading `-`.
///
/// `-` is only ever a sign here: there is no arithmetic in this dialect, so a `-` that is not
/// followed by a digit is not a token at all.
fn number(s: &[u8], i: &mut usize) -> Result<Tok> {
    let at = *i;
    let negative = s[*i] == b'-';
    if negative {
        *i += 1;
        if !s.get(*i).is_some_and(u8::is_ascii_digit) {
            return Err(SqlError::Syntax {
                at,
                found: "-".to_string(),
                want: "a number after the sign",
            });
        }
    }

    let start = *i;
    while s.get(*i).is_some_and(u8::is_ascii_digit) {
        *i += 1;
    }
    let whole = &s[start..*i];

    // A `.` is part of the number only when a digit follows it, so `t.` would be a syntax error
    // rather than a decimal point with nothing after it.
    let mut frac: &[u8] = &[];
    if s.get(*i) == Some(&b'.') && s.get(*i + 1).is_some_and(u8::is_ascii_digit) {
        *i += 1;
        let f = *i;
        while s.get(*i).is_some_and(u8::is_ascii_digit) {
            *i += 1;
        }
        frac = &s[f..*i];
    }

    let digits = [whole, frac].concat();
    let units: u64 =
        String::from_utf8_lossy(&digits).parse().map_err(|_| SqlError::NumberTooLarge { at })?;
    let scale = u8::try_from(frac.len()).map_err(|_| SqlError::NumberTooLarge { at })?;

    Ok(Tok::Num(match (negative, scale) {
        // A negative fractional number used to be refused here, because a decimal field is
        // unsigned and there was nowhere else for one to go. A float field holds one perfectly
        // well, and a lexer cannot see which kind of field a value is headed for - so the shape
        // is read and the refusal moved to `big_plan::to_units`, which knows the field.
        (true, scale) if scale > 0 => {
            let units = i64::try_from(units).map_err(|_| SqlError::NumberTooLarge { at })?;
            Literal::Sdec { units: -units, scale }
        }
        (true, _) => {
            let v = i64::try_from(units).map_err(|_| SqlError::NumberTooLarge { at })?;
            Literal::Sint(-v)
        }
        (false, 0) => Literal::Int(units),
        (false, scale) => Literal::Dec { units, scale },
    }))
}
