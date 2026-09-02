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

//! Values and names, written as the statement text the server reads back.
//!
//! # The one place a caller's bytes become SQL
//!
//! Every string that reaches a statement passes through [`push_text`] or [`push_ident`], and
//! nothing else in this crate writes a quote. That is deliberate and it is the whole of the
//! injection argument: there is one function to read, it is thirty lines, it has no I/O, and
//! its tests run before anything in this crate opens a socket.
//!
//! # Why identifiers are always quoted
//!
//! `big_sql`'s `bare_ident` takes a `Tok::Word` or a `Tok::Quoted` and treats them alike, and
//! the lexer builds a `Quoted` from `"..."` with `""` meaning one `"` - the same doubling rule
//! `'...'` has. So quoting always is legal everywhere a name may appear, and it removes a class
//! of mistake rather than managing it: a column called `values` or `select` is a `Tok::Word`
//! that the parser would read as the keyword it spells. Quoting is not a fallback for awkward
//! names here; it is the only path.
//!
//! # Why numbers are checked against the lexer's own grammar
//!
//! `big_sql::lex::number` reads `[-]digits[.digits]` and nothing else - **there is no exponent
//! form** - and it builds `units / 10^scale` with `units` a `u64` and `scale` a `u8`. So a great
//! many `f64` values have no spelling in this dialect, and `format!("{:?}")` produces one of the
//! unspellable ones (`1e300`, `1e-7`) for perfectly ordinary inputs. Checking here means the
//! caller is told which message was wrong; leaving it to the server means one value refuses a
//! batch of eight thousand and the sentence names none of them.
//!
//! This is the one place this crate restates a rule that lives in `big-sql`, and the round-trip
//! tests in `tests/` are what keep the restatement honest.

use crate::error::Error;
use crate::value::Value;

/// The column that names a record id, which this crate refuses to write.
///
/// Matched the way the parser matches it - `eq_ignore_ascii_case`, see
/// `big_sql::parse::insert` - so that `_RECORD_ID` cannot slip past a check written against the
/// lower-case spelling and turn an allocating statement into one that names its own addresses.
pub(crate) const RECORD_COLUMN: &str = "_record_id";

/// The largest `scale` a literal can carry, because the lexer keeps it in a `u8`.
const MAX_SCALE: usize = u8::MAX as usize;

/// A column name this crate will write.
///
/// Two refusals, and both are about what the *caller* meant rather than about what would parse.
/// An empty name parses perfectly well as `""` and names nothing; `_record_id` parses perfectly
/// well and would quietly turn off the allocation this crate exists to rely on.
pub(crate) fn check_column(name: &str) -> Result<(), Error> {
    if name.is_empty() {
        return Err(Error::Value("a column with no name".to_string()));
    }
    if name.eq_ignore_ascii_case(RECORD_COLUMN) {
        return Err(Error::Value(format!(
            "{name} is the record id, which this crate does not write: leave the column out and \
             the server allocates one"
        )));
    }
    Ok(())
}

/// One name, double-quoted, with `"` doubled to mean itself.
pub(crate) fn push_ident(name: &str, out: &mut String) -> Result<(), Error> {
    if name.is_empty() {
        return Err(Error::Value("a name with no characters in it".to_string()));
    }
    out.push('"');
    for c in name.chars() {
        if c == '"' {
            out.push('"');
        }
        out.push(c);
    }
    out.push('"');
    Ok(())
}

/// A table, which may be written `database.table`.
///
/// Split on the **first** `.`, matching `big_db::TableRef::parse`, so a qualified name reaches
/// the server qualified. The cost is that a table whose own name contains a dot cannot be
/// addressed - the same cost every other client in this repository pays, for the same reason.
pub(crate) fn push_table(name: &str, out: &mut String) -> Result<(), Error> {
    match name.split_once('.') {
        Some((database, table)) => {
            push_ident(database, out)?;
            out.push('.');
            push_ident(table, out)
        }
        None => push_ident(name, out),
    }
}

/// One string literal, single-quoted, with `'` doubled to mean itself.
///
/// This is `big_sql::lex::string`'s rule read backwards, and it is total: there is no character
/// a caller can send that ends the literal early, because the only character that could is the
/// one being doubled.
pub(crate) fn push_text(s: &str, out: &mut String) {
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push('\'');
        }
        out.push(c);
    }
    out.push('\'');
}

/// One value, as the literal the server will read.
pub(crate) fn push_value(value: Value<'_>, out: &mut String) -> Result<(), Error> {
    match value {
        Value::Int(v) => {
            out.push_str(itoa(v).as_str());
            Ok(())
        }
        Value::Signed(v) => {
            out.push_str(&v.to_string());
            Ok(())
        }
        Value::Decimal(s) => push_number(s, out),
        Value::Float(v) => push_float(v, out),
        Value::Text(s) => {
            push_text(s, out);
            Ok(())
        }
        Value::Bool(v) => {
            // `TRUE` and `FALSE`, which `big_sql::parse` reads with `eat_word` and therefore
            // reads in any case. Upper because that is how the rest of the dialect is written.
            out.push_str(if v { "TRUE" } else { "FALSE" });
            Ok(())
        }
        Value::Keyed { key, at } => {
            // Built as one string rather than pushed in three pieces, because the quoting rule
            // has to apply to the whole of it: a key containing a `'` must still be doubled, and
            // a key containing an `@` is still safe because the server splits on the last one.
            let mut joined = String::with_capacity(key.len() + 21);
            joined.push_str(key);
            joined.push('@');
            joined.push_str(&at.to_string());
            push_text(&joined, out);
            Ok(())
        }
    }
}

/// A number written out, checked against the grammar and the ceilings the lexer has.
fn push_number(s: &str, out: &mut String) -> Result<(), Error> {
    readable(s)?;
    out.push_str(s);
    Ok(())
}

/// A float, in the one notation this dialect has.
///
/// `{:?}` is tried first because it is the shortest spelling that reads back as the same number.
/// When it comes out in exponent form - which it does for magnitudes an ordinary program still
/// produces - a fixed spelling is searched for instead, shortest first, and the search is over
/// the scales a literal can actually carry.
fn push_float(v: f64, out: &mut String) -> Result<(), Error> {
    if !v.is_finite() {
        return Err(Error::Value(format!(
            "{v} cannot be written as a number: this dialect has no spelling for it"
        )));
    }

    let short = format!("{v:?}");
    if !short.contains(['e', 'E']) {
        return push_number(&short, out);
    }

    for scale in 0..=MAX_SCALE {
        let fixed = format!("{v:.scale$}");
        // The first spelling that reads back as the same number is the shortest one that does,
        // because the scales are tried in order.
        if fixed.parse::<f64>() == Ok(v) {
            return push_number(&fixed, out);
        }
    }

    Err(Error::Value(format!(
        "{v} cannot be written as a number this dialect reads: it has no exponent form, and a \
         literal carries at most {MAX_SCALE} digits after the point"
    )))
}

/// Whether the server's lexer would read this text as one number.
///
/// `big_sql::lex::number`: an optional `-`, at least one digit, then optionally a `.` and at
/// least one more digit. The digits either side of the point are concatenated into `units`,
/// which is a `u64` - and an `i64` when the sign is there - and the digits after the point are
/// counted into `scale`, which is a `u8`.
fn readable(s: &str) -> Result<(), Error> {
    let refuse =
        |why: &str| Err(Error::Value(format!("{s} is not a number this dialect reads: {why}")));

    let (negative, digits) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s),
    };

    let (whole, frac) = match digits.split_once('.') {
        Some((w, f)) => (w, f),
        None => (digits, ""),
    };

    if whole.is_empty() || !whole.bytes().all(|b| b.is_ascii_digit()) {
        return refuse("it needs at least one digit before the point");
    }
    if digits.contains('.') && (frac.is_empty() || !frac.bytes().all(|b| b.is_ascii_digit())) {
        return refuse("it needs at least one digit after the point");
    }
    if frac.len() > MAX_SCALE {
        return refuse("a literal carries at most 255 digits after the point");
    }

    // Concatenated exactly as the lexer concatenates them, so a value that overflows here is
    // the value that would have come back as `NumberTooLarge`.
    let mut units = String::with_capacity(whole.len() + frac.len());
    units.push_str(whole);
    units.push_str(frac);
    let Ok(units) = units.parse::<u64>() else {
        return refuse("it has more digits than a literal holds");
    };
    if negative && i64::try_from(units).is_err() {
        return refuse("it has more digits than a negative literal holds");
    }
    Ok(())
}

/// `u64::to_string`, kept behind a name so the one allocation per integer is visible.
///
/// Not an optimisation yet - it is `to_string` - but it is the single place to put one if a
/// profile ever says integers are where a batch's time goes.
fn itoa(v: u64) -> String {
    v.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rendered(value: Value<'_>) -> Result<String, Error> {
        let mut out = String::new();
        push_value(value, &mut out)?;
        Ok(out)
    }

    #[test]
    fn a_quote_in_a_value_cannot_end_the_statement() {
        assert_eq!(rendered(Value::Text("O'Brien")).unwrap(), "'O''Brien'");
        assert_eq!(
            rendered(Value::Text("x'); DROP TABLE t; --")).unwrap(),
            "'x''); DROP TABLE t; --'"
        );
        // The empty string is a key like any other, and it is not the absence of one.
        assert_eq!(rendered(Value::Text("")).unwrap(), "''");
        // A run of quotes doubles every one of them, rather than the first.
        assert_eq!(rendered(Value::Text("'''")).unwrap(), "''''''''");
    }

    #[test]
    fn a_quote_in_a_name_cannot_end_the_identifier() {
        let mut out = String::new();
        push_ident("we\"ird", &mut out).unwrap();
        assert_eq!(out, "\"we\"\"ird\"");
    }

    #[test]
    fn a_name_that_spells_a_keyword_is_still_a_name() {
        // The reason every identifier is quoted: unquoted, this is `Tok::Word("values")` and
        // the parser reads it as the keyword.
        let mut out = String::new();
        push_ident("values", &mut out).unwrap();
        assert_eq!(out, "\"values\"");
    }

    #[test]
    fn a_qualified_table_is_quoted_in_two_pieces() {
        let mut out = String::new();
        push_table("sales.orders", &mut out).unwrap();
        assert_eq!(out, "\"sales\".\"orders\"");

        let mut out = String::new();
        push_table("orders", &mut out).unwrap();
        assert_eq!(out, "\"orders\"");
    }

    #[test]
    fn the_record_id_column_is_refused_however_it_is_spelled() {
        for spelling in ["_record_id", "_RECORD_ID", "_Record_Id"] {
            assert!(
                check_column(spelling).is_err(),
                "{spelling} names the record id and must not be writable"
            );
        }
        check_column("record_id").expect("an ordinary column that merely reads like it");
        check_column("id").expect("`id` belongs to whoever is writing the table");
    }

    #[test]
    fn a_float_that_is_not_a_number_is_refused_before_it_is_sent() {
        assert!(rendered(Value::Float(f64::NAN)).is_err());
        assert!(rendered(Value::Float(f64::INFINITY)).is_err());
        assert!(rendered(Value::Float(f64::NEG_INFINITY)).is_err());
    }

    #[test]
    fn a_float_is_written_in_the_only_notation_this_dialect_has() {
        // Ordinary magnitudes come straight out of `{:?}`.
        assert_eq!(rendered(Value::Float(2.75)).unwrap(), "2.75");
        assert_eq!(rendered(Value::Float(-0.5)).unwrap(), "-0.5");
        assert_eq!(rendered(Value::Float(0.0)).unwrap(), "0.0");

        // `{:?}` gives `1e-7`, which the lexer cannot read, so a fixed spelling is found.
        let small = rendered(Value::Float(1e-7)).unwrap();
        assert!(!small.contains('e'), "{small} still carries an exponent");
        assert_eq!(small.parse::<f64>().unwrap(), 1e-7);

        // Large but inside what a `u64` of units holds.
        let large = rendered(Value::Float(1e18)).unwrap();
        assert!(!large.contains('e'), "{large} still carries an exponent");
        assert_eq!(large.parse::<f64>().unwrap(), 1e18);
    }

    #[test]
    fn a_float_with_no_spelling_is_refused_rather_than_rounded() {
        // 1e300 written out is three hundred and one digits, and `units` is a `u64`. Refusing is
        // the honest answer; rounding it to something that fits would be writing a different
        // number than the caller sent.
        let err = rendered(Value::Float(1e300)).unwrap_err();
        assert!(matches!(err, Error::Value(_)), "{err}");
    }

    #[test]
    fn a_decimal_is_checked_against_the_grammar_the_lexer_has() {
        assert_eq!(rendered(Value::Decimal("12.50")).unwrap(), "12.50");
        assert_eq!(rendered(Value::Decimal("-12.50")).unwrap(), "-12.50");
        assert_eq!(rendered(Value::Decimal("0")).unwrap(), "0");

        for bad in ["", ".5", "5.", "1e3", "1,5", "12.5.0", "abc", "-", "- 1", "+1"] {
            assert!(rendered(Value::Decimal(bad)).is_err(), "{bad} must not reach a statement");
        }
    }

    #[test]
    fn a_number_with_more_digits_than_a_literal_holds_is_refused() {
        // One past `u64::MAX`, which is where `lex::number` answers `NumberTooLarge`.
        assert!(rendered(Value::Decimal("18446744073709551616")).is_err());
        // The same digits with a point in them are the same `units`, so the same refusal.
        assert!(rendered(Value::Decimal("1844674407370955161.6")).is_err());
        // And just inside it is fine.
        assert!(rendered(Value::Decimal("18446744073709551615")).is_ok());
    }

    #[test]
    fn a_keyed_value_carries_its_moment_and_is_still_quoted() {
        assert_eq!(
            rendered(Value::Keyed { key: "gb", at: 1_750_000_000 }).unwrap(),
            "'gb@1750000000'"
        );
        // The server splits on the last `@`, so a key holding one survives the round trip.
        assert_eq!(rendered(Value::Keyed { key: "a@b", at: 1 }).unwrap(), "'a@b@1'");
        // And a key holding a quote is doubled like any other string.
        assert_eq!(rendered(Value::Keyed { key: "o'b", at: 1 }).unwrap(), "'o''b@1'");
    }

    #[test]
    fn a_bool_is_written_the_way_the_parser_eats_it() {
        assert_eq!(rendered(Value::Bool(true)).unwrap(), "TRUE");
        assert_eq!(rendered(Value::Bool(false)).unwrap(), "FALSE");
    }

    #[test]
    fn a_signed_value_keeps_its_sign_and_an_unsigned_one_has_none() {
        assert_eq!(rendered(Value::Signed(-1)).unwrap(), "-1");
        assert_eq!(rendered(Value::Signed(i64::MIN)).unwrap(), i64::MIN.to_string());
        assert_eq!(rendered(Value::Int(u64::MAX)).unwrap(), u64::MAX.to_string());
    }
}
