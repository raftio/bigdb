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

//! `CREATE TABLE`, and the column types a field kind is spelled with.

use super::Parser;
use crate::ddl::{Column, ColumnKind};
use crate::error::{Refused, Result};
use crate::lex::Tok;
use crate::show::SYSTEM_DATABASE;

impl Parser<'_> {
    /// `CREATE TABLE [IF NOT EXISTS] <name> [(<column>, ...)] [ENGINE = <engine>]`, with
    /// `CREATE` consumed.
    ///
    /// ```text
    /// column := ident type
    /// type   := SET | MUTEX | BOOL | BOOLEAN | TIMEQUANTUM | TIMESTAMP | DATETIME
    ///         | TEXT | VARCHAR | CHAR | STRING
    ///         | TINYINT | SMALLINT | INT | INTEGER | BIGINT   [UNSIGNED | SIGNED]
    ///         | UINT '(' bits ')' | SIGNED [ '(' bits ')' ]
    ///         | (DECIMAL | NUMERIC) '(' precision ',' scale ')'
    /// ```
    ///
    /// The list is optional because a table with no fields is a real thing here - it is what
    /// `POST /table/{t}` creates - and because the field routes are still a way to add one to a
    /// table that already exists. A column list is the way to say the whole shape in one
    /// statement.
    pub(super) fn create_table(&mut self) -> Result<crate::ddl::Ddl> {
        // `DATASET` is BigQuery's word for the same thing and `SCHEMA` is the standard's;
        // `DATABASE` is what ClickHouse, Doris and StarRocks call it, and what every JDBC
        // driver introspects with. One meaning, so one statement.
        if self.word_is("DATABASE") || self.word_is("SCHEMA") || self.word_is("DATASET") {
            self.i += 1;
            let if_not_exists = self.if_exists(true)?;
            let at = self.at();
            let name = self.bare_ident("a database name")?;
            // `system` is where this build's own views live, so a database of that name is one
            // nobody could ever query: `SELECT * FROM system.t` is read as a system view before
            // it is read as a table. Refused at the name rather than created and shadowed -
            // a table you can write to and never read from is the worst of the two answers.
            if name.eq_ignore_ascii_case(SYSTEM_DATABASE) {
                return Err(self.refuse_at(Refused::SystemTable, at));
            }
            if self.peek().is_some() {
                return Err(self.syntax("the end of the statement"));
            }
            return Ok(crate::ddl::Ddl::CreateDatabase { name, if_not_exists });
        }
        // `OR REPLACE` comes before the object word, which is where every dialect puts it.
        let or_replace = if self.word_is("OR") {
            self.i += 1;
            self.expect_word("REPLACE", "REPLACE after OR")?;
            true
        } else {
            false
        };
        if self.word_is("VIEW") {
            self.i += 1;
            return self.create_view(or_replace);
        }
        if !self.word_is("TABLE") {
            // A materialised view is the one other `CREATE` with a sentence of its own: what it
            // asks for is a table plus a write path, not a name for a statement.
            if self.word_is("MATERIALIZED") {
                return Err(self.refuse(Refused::MaterializedView));
            }
            return Err(self.refuse(Refused::Write));
        }
        if or_replace {
            // `CREATE OR REPLACE TABLE` would drop and recreate, which is `DROP TABLE` wearing a
            // gentler word. It is refused where every other unbuilt write is.
            return Err(self.refuse(Refused::Write));
        }
        self.i += 1;
        let if_not_exists = self.if_exists(true)?;
        let (database, table) = self.table_ref("a table name")?;

        let columns = if self.eat(&Tok::LParen) { self.column_list()? } else { Vec::new() };

        // After the list, not before it: `CREATE TABLE t (a SET) ENGINE = columnar` is the
        // order every other dialect puts these in, and the order somebody writes them without
        // being told.
        let engine = if self.word_is("ENGINE") {
            self.i += 1;
            if !self.eat(&Tok::Op("=")) {
                return Err(self.syntax("= after ENGINE"));
            }
            Some(self.engine_name()?)
        } else {
            None
        };

        // Refused at the word, with the statement that does it named. A `TTL` accepted and
        // stored would be a promise nothing in this process keeps.
        if self.word_is("TTL") {
            return Err(self.refuse(Refused::DeclarativeTtl));
        }
        // The layout clauses, each refused at the word that writes it rather than as "expected
        // the end of the statement". Every one of them is a decision this engine makes for
        // itself: a shard is a function of the record id, an order is the record id, and a
        // sample is not something a column can define here.
        if self.word_is("PARTITION") || self.word_is("DISTRIBUTED") || self.word_is("CLUSTER") {
            return Err(self.refuse(Refused::Distribution));
        }
        if self.word_is("SAMPLE") {
            return Err(self.refuse(Refused::Sample));
        }
        if self.peek().is_some() {
            return Err(self.syntax("the end of the statement"));
        }
        Ok(crate::ddl::Ddl::CreateTable { database, table, engine, columns, if_not_exists })
    }

    /// `CREATE [OR REPLACE] VIEW [IF NOT EXISTS] <name> AS <select>`, with `VIEW` consumed.
    ///
    /// The body is **parsed to validate it and then thrown away**: what is stored is the source
    /// slice. Parsing here is what makes a malformed or unsupported body fail at `CREATE` rather
    /// than at the first read, which is the whole difference between a view that was never
    /// created and one that is a trap for whoever reads it next.
    fn create_view(&mut self, or_replace: bool) -> Result<crate::ddl::Ddl> {
        let if_not_exists = self.if_exists(true)?;
        // Both at once is two opposite answers to one situation - replace what is there, or
        // leave what is there - so it is a syntax error rather than a precedence rule nobody
        // would remember.
        if or_replace && if_not_exists {
            return Err(self.syntax("only one of OR REPLACE and IF NOT EXISTS"));
        }
        let (database, name) = self.table_ref("a view name")?;
        self.expect_word("AS", "AS before the view's SELECT")?;
        // Where the body starts, in the original text. Taken before anything is consumed, so it
        // points at the `SELECT` and not past it.
        let from = self.at();
        if !self.word_is("SELECT") {
            return Err(self.syntax("SELECT after AS"));
        }
        self.i += 1;
        let select = self.select()?;
        self.check_view_body(&select)?;
        if self.peek().is_some() {
            // A `UNION` lands here, and so does anything else after the body. Both are the same
            // answer: a view is one `SELECT`.
            return Err(self.refuse(Refused::ViewBody));
        }
        let body = self.src[from..].trim().to_string();
        Ok(crate::ddl::Ddl::CreateView { database, name, body, or_replace, if_not_exists })
    }

    /// Refuses a view body that is not a filter and a projection over one table.
    ///
    /// Every clause here is refused for one reason, stated once: **a view is inlined into the
    /// statement that reads it.** The base table replaces the view's name, the two `WHERE`s are
    /// ANDed, and the reader's columns are renamed through this select list. A clause that
    /// cannot survive that substitution is a clause whose answer would have to exist before the
    /// outer statement ran - which is a materialised view, and a different feature.
    fn check_view_body(&self, select: &crate::ast::Select) -> Result<()> {
        use crate::ast::Proj;
        // Ordered so the first thing wrong is the thing reported, outermost clause first.
        let bad = !select.joins.is_empty()
            || !select.group_by.is_empty()
            || select.having.is_some()
            || select.order_by.is_some()
            || select.limit.is_some()
            || select.offset.is_some()
            || select.format != crate::shape::Format::default()
            || select.items.is_empty()
            || select.items.iter().any(|item| {
                // A per-entry `FILTER` narrows one aggregate, and there are no aggregates here.
                item.filter.is_some()
                    // `*` is every column the table has *at the time it is read*, and a view is
                    // stored as text and re-planned at every read - so a body written with one
                    // would silently start exposing a column added to the table years later. A
                    // view names what it exposes.
                    || !matches!(&item.proj, Proj::Column(name) if name.qualifier.is_none())
            });
        if bad {
            return Err(self.refuse_at(Refused::ViewBody, self.view_body_at(select)));
        }
        Ok(())
    }

    /// Byte offset to point a [`Refused::ViewBody`] at: the offending select-list entry when it
    /// is one, and the start of the body otherwise.
    fn view_body_at(&self, select: &crate::ast::Select) -> usize {
        use crate::ast::Proj;
        select
            .items
            .iter()
            .find(|item| {
                item.filter.is_some()
                    || !matches!(&item.proj, Proj::Column(name) if name.qualifier.is_none())
            })
            .map_or_else(|| self.at(), |item| item.at)
    }

    /// `IF [NOT] EXISTS`, or nothing.
    ///
    /// One function for both spellings because the only difference is the word in the middle,
    /// and because half a clause - an `IF` with nothing after it - has to be a syntax error in
    /// both. Reading it as a table named `IF` would create one.
    pub(super) fn if_exists(&mut self, negated: bool) -> Result<bool> {
        if !self.eat_word("IF") {
            return Ok(false);
        }
        if negated {
            self.expect_word("NOT", "NOT EXISTS after IF")?;
        }
        self.expect_word("EXISTS", "EXISTS")?;
        Ok(true)
    }

    /// The columns, with `(` consumed and `)` eaten here.
    ///
    /// A name may repeat as far as this crate can tell - it holds no schema and would be
    /// guessing at what a second `country SET` means. It means the same refusal a second
    /// `POST /table/t/field/country` gets, raised where the field is created.
    fn column_list(&mut self) -> Result<Vec<Column>> {
        let mut columns = Vec::new();
        // `CREATE TABLE t ()` is a table with no fields written the long way, and refusing it
        // would be refusing a statement whose meaning is not in doubt.
        if self.eat(&Tok::RParen) {
            return Ok(columns);
        }
        loop {
            let at = self.at();
            let name = self.bare_ident("a column name")?;
            // Refused where it is declared rather than where it fails to be written. A field
            // called `id` is one an `INSERT` can never fill - it would read the value as the
            // record to write about - so it would sit empty while `SELECT *` showed the very
            // records it was meant to hold.
            if name.eq_ignore_ascii_case(crate::insert::RECORD_COLUMN) {
                return Err(self.refuse_at(Refused::IdColumn, at));
            }
            columns.push(self.column_type(name)?);
            if self.eat(&Tok::Comma) {
                continue;
            }
            self.expect(&Tok::RParen, ", or ) after the columns")?;
            return Ok(columns);
        }
    }

    /// One type name, and whatever it takes in brackets.
    ///
    /// **This is the only place in the crate that decides what a word means.** Everywhere else
    /// a name is passed on for a layer that holds a schema to resolve; here there is nothing to
    /// resolve against, because no field kind is spelled `TEXT` anywhere below. So the mapping
    /// is written out rather than inferred, and a name not on it is refused with the list.
    ///
    /// [`crate::render::create_table`] is the way back, and is the reason a type name that came
    /// from here round-trips: the two are inverses of one table, so they have to be read
    /// together when either changes.
    pub(super) fn column_type(&mut self, name: String) -> Result<Column> {
        use ColumnKind::*;

        let at = self.at();
        let Some(word) = self.word().map(str::to_ascii_uppercase) else {
            return Err(self.syntax("a column type"));
        };
        self.i += 1;

        // Widths are the ones the names have always meant, so `BIGINT` costs 64 bitmaps and
        // `SMALLINT` costs 16 - which is the whole of what a bit depth decides here. A display
        // width in brackets is refused below rather than read as a depth: `INT(11)` is eleven
        // digits where MySQL wrote it and eleven bits here, and eleven bits stop at 2047.
        let column = match word.as_str() {
            "SET" => Column { name, kind: Set, bit_depth: KEYLESS_DEPTH, scale: None },
            "MUTEX" => Column { name, kind: Mutex, bit_depth: KEYLESS_DEPTH, scale: None },
            "BOOL" | "BOOLEAN" => {
                Column { name, kind: Bool, bit_depth: KEYLESS_DEPTH, scale: None }
            }
            // A time quantum is keyed by a moment and viewed by day, and it keeps its own
            // spelling. `DATE` and `DATETIME` used to be spellings of it too, and stopped being
            // when they got scalar kinds: a keyed field answers `f = 'k' AND f BETWEEN lo AND
            // hi` and nothing else, so `WHERE d >= '2024-01-01'`, `ORDER BY d` and `max(d)` -
            // which is most of what anyone writes a date column for - had no way to be asked.
            // The two are different things and now have different names.
            "TIMEQUANTUM" => {
                Column { name, kind: TimeQuantum, bit_depth: KEYLESS_DEPTH, scale: None }
            }
            // A day count and a second count, both from 1970 and both signed, because dates
            // before it are ordinary values. The depths are what those counts need: 32 planes of
            // days is ±5.8 million years, and seconds want the full width.
            //
            // A bracket on any of these four is refused rather than read. `FLOAT(10, 2)` is a
            // MySQL decimal wearing a float's name, and `DATETIME(3)` asks for a sub-second
            // precision this does not keep; reading either would answer a question next to the
            // one that was asked. The type that keeps those digits exactly is `DECIMAL`, and it
            // is still here.
            "DATE" | "DATETIME" | "TIMESTAMP" | "FLOAT" | "FLOAT32" | "REAL" | "DOUBLE"
            | "FLOAT64" => {
                if self.peek() == Some(&Tok::LParen) {
                    return Err(self.refuse_at(Refused::ColumnType, at));
                }
                // `DOUBLE PRECISION` is the standard's spelling of the same type, read for the
                // reason `INT UNSIGNED` is: it is a word that says what was already meant.
                if word == "DOUBLE" {
                    self.eat_word("PRECISION");
                }
                let (kind, bit_depth) = match word.as_str() {
                    "DATE" => (Date, 32),
                    "DATETIME" | "TIMESTAMP" => (DateTime, 64),
                    "FLOAT" | "FLOAT32" | "REAL" => (Float32, 32),
                    _ => (Float64, 64),
                };
                Column { name, kind, bit_depth, scale: None }
            }
            // A length on a key is a bound on nothing: keys are stored whole, and there is no
            // truncation or padding for a `VARCHAR(255)` to describe.
            "TEXT" | "VARCHAR" | "CHAR" | "STRING" => {
                if self.peek() == Some(&Tok::LParen) {
                    return Err(self.refuse(Refused::Constraint));
                }
                Column { name, kind: Set, bit_depth: KEYLESS_DEPTH, scale: None }
            }
            // **The one type name that describes an encoding rather than a domain**, and the
            // encoding it describes is the only one a keyed field has: a `SET` interns each
            // distinct value once and stores a bitmap per key, which is what dictionary
            // encoding means where ClickHouse writes this. So it is read as the thing it
            // already is rather than refused as something this engine has no storage for, and
            // a statement pasted from ClickHouse keeps the column it meant.
            //
            // The inner name has to be a string. `LowCardinality(Int64)` asks for a dictionary
            // over a number, and a number here is bit planes with no dictionary to be low
            // cardinality *of* - so it is refused with the list rather than quietly widened
            // into a keyed field whose keys are digits.
            "LOWCARDINALITY" => {
                let inner = self.wrapped_type()?;
                if !matches!(inner.as_str(), "STRING" | "TEXT" | "VARCHAR" | "CHAR") {
                    return Err(self.refuse_at(Refused::ColumnType, at));
                }
                Column { name, kind: Set, bit_depth: KEYLESS_DEPTH, scale: None }
            }
            // The two wrappers this engine recognises and declines, refused at the word that
            // names them rather than after the bracket is read. Nothing is consumed first
            // because nothing needs to be: the statement stops here, so there is no second
            // error for a swallowed bracket to prevent - and refusing at the token means
            // `Nullable(Decimal(10, 2))`, which `wrapped_type` could not read, says the same
            // sentence as `Nullable(Int64)` rather than a syntax error about a nested `(`.
            "NULLABLE" => return Err(self.refuse_at(Refused::NullableType, at)),
            // The composite types, refused at the word rather than after the bracket, for the
            // reason `NULLABLE` is: `Array(Decimal(10, 2))` should say the sentence about
            // arrays, not a syntax error about the bracket inside one.
            "ARRAY" | "MAP" | "TUPLE" | "NESTED" => {
                return Err(self.refuse_at(Refused::CompositeType, at))
            }
            // Its own sentence rather than the composite one, because what it needs is a
            // different rewrite: `MUTEX` is already the storage an enum wants.
            "ENUM" | "ENUM8" | "ENUM16" => return Err(self.refuse_at(Refused::EnumType, at)),
            "BITMAP" | "AGGREGATEFUNCTION" => return Err(self.refuse_at(Refused::BitmapType, at)),
            // The sketch types, which are a different refusal from `BITMAP` even though both
            // decline a column: a bitmap is refused because every column here already is one,
            // and a sketch because there is nothing here for one to approximate.
            "HLL" | "QUANTILE_STATE" | "QUANTILESTATE" | "SIMPLEAGGREGATEFUNCTION" => {
                return Err(self.refuse_at(Refused::Sketch, at))
            }
            "TINYINT" | "SMALLINT" | "INT" | "INTEGER" | "BIGINT" => {
                if self.peek() == Some(&Tok::LParen) {
                    return Err(self.refuse(Refused::ColumnType));
                }
                let bit_depth = match word.as_str() {
                    "TINYINT" => 8,
                    "SMALLINT" => 16,
                    "BIGINT" => 64,
                    _ => 32,
                };
                // `INT UNSIGNED` is what every integer here already is, and `BIGINT SIGNED` is
                // the sign convention asked for by the name it has in SQL - so both are read
                // rather than refused over a word that says what was meant.
                let kind = if self.eat_word("SIGNED") {
                    Signed
                } else {
                    self.eat_word("UNSIGNED");
                    Int
                };
                Column { name, kind, bit_depth, scale: None }
            }
            // The two native spellings, which exist because the SQL names carry a fixed width
            // and a bit-sliced field is cheaper the narrower it is: every plane is one more
            // bitmap a range query intersects.
            "UINT" => Column { name, kind: Int, bit_depth: self.bit_depth()?, scale: None },
            "SIGNED" => {
                let bit_depth =
                    if self.peek() == Some(&Tok::LParen) { self.bit_depth()? } else { 32 };
                Column { name, kind: Signed, bit_depth, scale: None }
            }
            "DECIMAL" | "NUMERIC" => self.decimal(name)?,
            _ => return Err(self.refuse_at(Refused::ColumnType, at)),
        };

        // Named before the constraints, because these two are not constraints: they say where
        // the column's value comes from rather than what it may be, and the answer to each is a
        // different statement.
        if self.word_is("MATERIALIZED") || self.word_is("ALIAS") {
            return Err(self.refuse(Refused::ComputedColumn));
        }
        // Constraints are refused after the type rather than at the type, so that the refusal
        // points at `NOT NULL` and says what is wrong with it - and not at a `DECIMAL` that was
        // written perfectly well.
        if self.at_constraint() {
            return Err(self.refuse(Refused::Constraint));
        }
        Ok(column)
    }

    /// `DECIMAL(precision, scale)`, with the type name consumed.
    ///
    /// Both numbers, always. `DECIMAL(10)` is SQL for ten digits and no fraction, and reading
    /// it here as a scale of ten would turn a pasted definition into a field where `price > 5`
    /// asks about five ten-billionths - so the one-argument form is refused rather than given a
    /// meaning it does not have anywhere else.
    ///
    /// The precision buys the bit depth: enough bits to hold every number of that many digits,
    /// which is what the writer said the column holds. Deriving it is not a guess - 10^p - 1
    /// needs `ceil(p * log2(10))` bits and no fewer - and it means nobody has to translate
    /// digits into bitmaps by hand.
    fn decimal(&mut self, name: String) -> Result<Column> {
        if self.peek() != Some(&Tok::LParen) {
            return Err(self.refuse(Refused::DecimalScale));
        }
        self.i += 1;
        let precision = self.small_number("the precision, in digits")?;
        if !self.eat(&Tok::Comma) {
            return Err(self.refuse(Refused::DecimalScale));
        }
        let scale = self.small_number("the scale, in digits after the point")?;
        self.expect(&Tok::RParen, ") after the scale")?;

        // A scale wider than the number itself describes a value with no digits before the
        // point that the precision leaves room for, which is a definition nobody meant to
        // write. And `i8` is what the catalog stores a scale in.
        if precision == 0 || scale > precision || scale > i8::MAX as u64 {
            return Err(self.refuse(Refused::DecimalScale));
        }
        let bits = decimal_bits(precision);
        if !(1..=64).contains(&bits) {
            return Err(self.refuse(Refused::BitDepth));
        }
        Ok(Column {
            name,
            kind: ColumnKind::Decimal,
            bit_depth: bits as u32,
            scale: Some(scale as i8),
        })
    }

    /// `( name )`, for a type whose argument is another type's name.
    ///
    /// Uppercased on the way out, like every other type name here, so a caller compares against
    /// one spelling rather than remembering to fold. Not [`Self::bit_depth`] with a different
    /// body: that one reads a number, and the two would have to be told apart by their return
    /// types at every call site if they were one function.
    fn wrapped_type(&mut self) -> Result<String> {
        self.expect(&Tok::LParen, "( and a type name")?;
        let Some(inner) = self.word().map(str::to_ascii_uppercase) else {
            return Err(self.syntax("a type name"));
        };
        self.i += 1;
        self.expect(&Tok::RParen, ") after the type name")?;
        Ok(inner)
    }

    /// `( bits )`, bounded where a bit-sliced value is bounded.
    fn bit_depth(&mut self) -> Result<u32> {
        self.expect(&Tok::LParen, "( and a number of bits")?;
        let bits = self.small_number("a number of bits")?;
        self.expect(&Tok::RParen, ") after the number of bits")?;
        if !(1..=64).contains(&bits) {
            return Err(self.refuse(Refused::BitDepth));
        }
        Ok(bits as u32)
    }

    /// Whether what follows a type is a constraint rather than the end of the column.
    ///
    /// Named one by one rather than "anything that is not a comma": a word this list does not
    /// have is a typo in the type, and `syntax` pointing at it is more use than a refusal
    /// telling somebody there are no constraints when they did not write one.
    pub(super) fn at_constraint(&self) -> bool {
        const CONSTRAINTS: [&str; 10] = [
            "NOT",
            "NULL",
            "PRIMARY",
            "UNIQUE",
            "DEFAULT",
            "REFERENCES",
            "CHECK",
            "KEY",
            "AUTO_INCREMENT",
            "COMMENT",
        ];
        CONSTRAINTS.iter().any(|c| self.word_is(c))
    }

    /// An engine name, bare or quoted.
    ///
    /// Quoted is not decoration: `bitmap+columnar` contains a character the lexer has no token
    /// for, so that one has to be written `'bitmap+columnar'`. The bare form exists because
    /// `ENGINE = columnar` is what anyone coming from another column store will type, and
    /// refusing it over punctuation nobody can see would be a poor first impression.
    ///
    /// Which names are real is not decided here. This crate links no storage crate, and a
    /// second copy of the engine list would be a second copy to drift.
    fn engine_name(&mut self) -> Result<String> {
        match self.peek() {
            Some(Tok::Str(s)) | Some(Tok::Word(s)) | Some(Tok::Quoted(s)) => {
                let s = s.clone();
                self.i += 1;
                Ok(s)
            }
            _ => Err(self.syntax("an engine name, like columnar or 'bitmap+columnar'")),
        }
    }
}

/// What a field that stores no number carries as its depth.
///
/// Nothing reads it - a set has no bit planes - but the field route defaults to it, and a set
/// created in a column list should be the same row in `/schema` as a set created over
/// `POST /table/{t}/field/{f}?kind=set`. Two spellings of one field that differ in a number
/// nobody uses is a difference somebody will eventually have to explain.
pub(crate) const KEYLESS_DEPTH: u32 = 32;

/// How many bits a decimal of `precision` digits needs.
///
/// `f64::log2(10)` rounded up, kept as an integer ratio so this is exact for every precision a
/// `u64` could hold: 10^p needs at most ceil(p * 10 / 3) bits. A function rather than a line
/// because [`crate::render`] has to invert it, and an inverse of a formula written twice is an
/// inverse of neither.
pub(crate) fn decimal_bits(precision: u64) -> u64 {
    precision.saturating_mul(10).div_ceil(3)
}
