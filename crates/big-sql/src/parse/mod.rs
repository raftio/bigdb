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

//! Tokens to [`Select`]. Recursive descent, with the refusals placed where the construct is.
//!
//! ```text
//! statement := explain | create | alter | drop | insert | show
//!            | [WITH literal AS ident (',' literal AS ident)*] select
//! explain   := EXPLAIN [PLAN | SHAPE] statement   -- the inner one is not an EXPLAIN
//! create    := CREATE TABLE [IF NOT EXISTS] ident
//!              ['(' column (',' column)* ')'] [ENGINE '=' engine]
//!            | CREATE [OR REPLACE] VIEW [IF NOT EXISTS] ident AS body
//! body      := SELECT bare-item (',' bare-item)* FROM source [WHERE cond]
//! bare-item := ident [AS ident]   -- a view is a filter and a projection; see `check_view_body`
//! alter     := ALTER TABLE ident change (',' change)*
//! change    := ADD [COLUMN] column | DROP [COLUMN] ident
//! drop      := DROP TABLE [IF EXISTS] ident | DROP VIEW [IF EXISTS] ident
//! column    := ident type      -- see `Parser::column_type` for the type names
//! insert    := INSERT [INTO] ident '(' ident (',' ident)* ')' VALUES tuple (',' tuple)*
//! tuple     := '(' literal (',' literal)* ')'
//! show      := (DESCRIBE | DESC) [TABLE] ident | SHOW COLUMNS FROM ident
//!            | SHOW TABLES | SHOW VIEWS | SHOW CREATE [TABLE | VIEW] ident   [FORMAT ident]
//! select    := SELECT list FROM source [JOIN source ON name '=' name]
//!              [PREWHERE cond] [WHERE cond] [GROUP BY name] [HAVING having]
//!              [ORDER BY order] [LIMIT n [WITH TIES]] [OFFSET n] [FORMAT ident]
//! source    := ident [[AS] ident]
//! name      := [ident '.'] ident
//! list      := item (',' item)*
//! item      := ('*' | ident | agg [FILTER '(' WHERE cond ')'])  [AS ident]
//! agg       := COUNT '(' ('*' | DISTINCT name) ')' | (SUM|MIN|MAX|AVG) '(' name ')'
//!            | UNIQ '(' name ')' | TOPK ['(' n ')'] '(' name ')'
//!            | agg-name 'If' '(' [name ','] cond ')'
//! having    := agg op literal
//! order     := (agg | ident) [ASC|DESC]
//! cond      := disj
//! disj      := conj (OR conj)*
//! conj      := neg (AND neg)*
//! neg       := NOT neg | '(' cond ')' | predicate
//! predicate := name op literal
//!            | name [NOT] IN '(' literal (',' literal)* ')'
//!            | name [NOT] BETWEEN literal AND literal
//!            | name                                    -- a boolean column, `= TRUE`
//! ```
//!
//! **A refused construct is caught where it is written, not by falling off the end of the
//! grammar.** `JOIN` is refused at the keyword, not as "expected `WHERE`, found `join`" — the
//! second tells a user their SQL is malformed when it is perfectly good SQL that this engine
//! will not answer, and those call for entirely different reactions.

use crate::ast::{ExplainMode, Name, Query, Select};
use crate::error::{Refused, Result, SqlError};
use crate::insert::Insert;
use crate::lex::{lex, Tok, Token};
use crate::show::Show;
use big_plan::Literal;

/// How deep a `WHERE` clause may nest before it is refused.
///
/// **A bound on the stack, not a taste in conditions.** The reasoning is
/// [`big_plan::parse::MAX_DEPTH`]'s, unchanged and for the same reason: this is a recursive
/// descent, nesting is call depth, a stack overflow aborts the process rather than unwinding,
/// and `big serve` runs queries on pool threads whose stacks are smaller than the main thread's. The
/// limit is the same number so that neither language is the one that overflows first.
pub const MAX_DEPTH: usize = big_plan::parse::MAX_DEPTH;

/// One statement, whichever kind it turned out to be.
///
/// The leading keyword decides, and only here: everything downstream is handed one shape or the
/// other and never has to ask again.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Parsed {
    Query(Query),
    Insert(Insert),
    Show(Show),
    Ddl(crate::ddl::Ddl),
    /// `EXPLAIN <statement>`: describe what the statement would do, and do none of it.
    ///
    /// **One variant that wraps every kind, rather than a flag on each.** A statement kind added
    /// after this one is explainable the day it parses, with no arm here to remember - which is
    /// the property a per-kind flag would not have. The box is what a recursive variant costs;
    /// it is paid once per `EXPLAIN` and by nothing else.
    Explain {
        /// Which half was asked for. [`ExplainMode::All`] unless the statement named one.
        mode: ExplainMode,
        /// The statement being described, which is never itself an `EXPLAIN`.
        inner: Box<Parsed>,
    },
}

/// The wall clock, in seconds since the epoch.
///
/// Saturating rather than panicking on a clock set before 1970: a machine whose clock is wrong
/// should answer a query with a strange timestamp in it, not refuse to answer at all.
fn now_seconds() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_secs()).unwrap_or(i64::MAX),
        Err(e) => -i64::try_from(e.duration().as_secs()).unwrap_or(i64::MAX),
    }
}

/// Parses one statement, refusing anything this engine does not answer.
pub fn parse(input: &str) -> Result<Parsed> {
    let tokens = lex(input)?;
    let mut p = Parser {
        t: &tokens,
        i: 0,
        src: input,
        end: input.len(),
        depth: 0,
        bound: Default::default(),
        now: now_seconds(),
    };
    statement(&mut p, true)
}

/// The leading keyword, and the statement it turned out to introduce.
///
/// Split out of [`parse`] for one caller: `EXPLAIN` wraps a statement, so the dispatch has to be
/// able to run itself again. `explainable` is how it says the inner one may not be another
/// `EXPLAIN` - a bool rather than a depth, because there is no reading of `EXPLAIN EXPLAIN` that
/// means more than `EXPLAIN` does.
fn statement(p: &mut Parser<'_>, explainable: bool) -> Result<Parsed> {
    // The leading keyword decides which of four things happened, and only one of them is a
    // syntax error. A `DELETE` is a statement this surface refuses; a `foo` is not a statement.
    let Some(word) = p.word() else {
        return Err(SqlError::Syntax { at: p.at(), found: p.here(), want: "a SELECT statement" });
    };
    // `EXPLAIN <statement>` answers with what the statement *would* do, having done none of it.
    //
    // Checked before every other keyword and nowhere else, which is what keeps `EXPLAIN` from
    // becoming a reserved word: the lexer has no keyword list, so a table or a column may still
    // be called `explain` - only this one token position is special.
    if word.eq_ignore_ascii_case("EXPLAIN") {
        // A syntax error rather than a refusal, which is the split this crate's errors are
        // built on: a refusal says the text is a statement this engine will not answer, and
        // `EXPLAIN EXPLAIN` is not a statement in any dialect - there is no engine it means
        // something to. Nothing exists instead of it, so there is no sentence to write.
        if !explainable {
            return Err(p.syntax("a statement to explain"));
        }
        p.i += 1;
        let mode = p.explain_mode();
        let at = p.at();
        let inner = statement(p, false)?;
        // A half only a query has, named over a statement that has one printer. Refused here
        // rather than ignored: a client that asked for the plans of a `CREATE TABLE` has a
        // misunderstanding, and printing the schema change anyway would leave them with it.
        if mode != ExplainMode::All && !matches!(inner, Parsed::Query(_)) {
            return Err(p.refuse_at(Refused::ExplainHalf, at));
        }
        return Ok(Parsed::Explain { mode, inner: Box::new(inner) });
    }
    // `WITH 500 AS threshold SELECT ...` binds a constant for the statement to use by name.
    // **Constants only.** A CTE whose body is a select is a subquery, which this engine has no
    // set operation behind - `bindings` refuses one at the bracket that opens it.
    if word.eq_ignore_ascii_case("WITH") {
        p.i += 1;
        p.bindings()?;
        if !p.word_is("SELECT") {
            return Err(p.syntax("SELECT after the WITH bindings"));
        }
        p.i += 1;
        let select = p.select()?;
        return p.union(select).map(Parsed::Query);
    }

    match word.to_ascii_uppercase().as_str() {
        "SELECT" => {}
        // Each of these rules on its second word, so that `CREATE DATABASE` and `CREATE VIEW`
        // get the sentence that is about them rather than the one about writes in general: a
        // surface that grew `CREATE INDEX` by accident would be a surface nobody chose.
        "CREATE" => {
            p.i += 1;
            return p.create_table().map(Parsed::Ddl);
        }
        // `ALTER TABLE` adds and drops fields, which is the whole of what the engine below can
        // do to one.
        "ALTER" => {
            p.i += 1;
            return p.alter_table().map(Parsed::Ddl);
        }
        "DROP" => {
            p.i += 1;
            return p.drop_table().map(Parsed::Ddl);
        }
        "INSERT" => {
            p.i += 1;
            return p.insert().map(Parsed::Insert);
        }
        "DESCRIBE" | "DESC" | "SHOW" => return p.show().map(Parsed::Show),
        // `DELETE FROM` has its own sentence: what it asks for exists, as record ids sent to
        // `POST /table/{t}/delete`, and the reason it is not a statement here is that a record
        // is not a row.
        "DELETE" => return Err(p.refuse(Refused::DeleteRows)),
        "USE" => return Err(p.refuse(Refused::SessionUse)),
        "UPDATE" | "TRUNCATE" | "REPLACE" | "MERGE" | "UPSERT" => {
            return Err(p.refuse(Refused::Write))
        }
        _ => {
            return Err(SqlError::Syntax {
                at: p.at(),
                found: p.here(),
                want: "a SELECT statement",
            })
        }
    }
    p.i += 1;

    let select = p.select()?;
    p.union(select).map(Parsed::Query)
}

struct Parser<'a> {
    t: &'a [Token],
    i: usize,
    /// The statement as it was written.
    ///
    /// Held for exactly one clause: a `CREATE VIEW` stores its body as **text**, and the text is
    /// this sliced from the token after `AS` to the end. Every `Token` already carries its byte
    /// offset, so the slice costs nothing to find - and re-serialising the parsed body instead
    /// would mean a `SELECT` renderer, which is a second dialect to keep in step with this one.
    src: &'a str,
    /// Byte length of the input, which is where "end of statement" points.
    end: usize,
    depth: usize,
    /// Constants a `WITH` clause bound, by the name it gave them.
    bound: std::collections::BTreeMap<String, Literal>,
    /// The instant this statement was read, in seconds since the epoch.
    ///
    /// **Read once, here, and shared by every `now()` in the statement.** A clock read per call
    /// site would let `SELECT now() FROM t WHERE ts < now()` compare against two different
    /// moments, and one read per node would let two shards disagree about which records match a
    /// window. Taking it where the statement is parsed makes it the coordinator's single
    /// answer, which is the only one that is the same everywhere the statement goes.
    now: i64,
}

impl Parser<'_> {
    // ------------------------------------------------------------------------------------
    // Token handling
    // ------------------------------------------------------------------------------------

    pub(super) fn peek(&self) -> Option<&Tok> {
        self.t.get(self.i).map(|t| &t.tok)
    }

    /// Byte offset of the current token, or the end of the input when there is none.
    pub(super) fn at(&self) -> usize {
        self.t.get(self.i).map_or(self.end, |t| t.at)
    }

    pub(super) fn here(&self) -> String {
        match self.peek() {
            None => "the end of the statement".to_string(),
            Some(Tok::Word(w)) => w.clone(),
            Some(Tok::Quoted(w)) => format!("\"{w}\""),
            Some(Tok::Str(s)) => format!("'{s}'"),
            Some(Tok::Num(_)) => "a number".to_string(),
            Some(Tok::Op(o)) | Some(Tok::Arith(o)) => (*o).to_string(),
            Some(Tok::LParen) => "(".to_string(),
            Some(Tok::RParen) => ")".to_string(),
            Some(Tok::Comma) => ",".to_string(),
            Some(Tok::Star) => "*".to_string(),
            Some(Tok::Dot) => ".".to_string(),
        }
    }

    /// The branches after the first, and then whatever is left - which should be nothing.
    ///
    /// `UNION ALL` stacks two answers: same number of columns, named by the first branch, rows
    /// one after the other. Nothing here is a set operation over records - each branch is a
    /// whole statement with its own plans - which is why it costs no engine change and why a
    /// plain `UNION` is refused instead: removing duplicates would mean comparing two rendered
    /// answers, and rows are not what this index stores.
    fn union(&mut self, first: Select) -> Result<Query> {
        let mut branches = vec![first];
        while self.word_is("UNION") {
            self.i += 1;
            if !self.eat_word("ALL") {
                self.eat_word("DISTINCT");
                return Err(self.refuse(Refused::Union));
            }
            self.expect_word("SELECT", "SELECT after UNION ALL")?;
            branches.push(self.select()?);
        }
        if self.i < self.t.len() {
            if self.word_is("INTERSECT") || self.word_is("EXCEPT") {
                return Err(self.refuse(Refused::Subquery));
            }
            return Err(self.syntax("the end of the statement"));
        }
        Ok(Query { branches })
    }

    /// `WITH <literal> AS <name> [, ...]`, collected for the statement to use by name.
    ///
    /// Substituted wherever a literal is expected, which is the only place a constant could
    /// mean anything: this dialect has no expressions, so there is nowhere else for one to
    /// appear. A binding whose body is a select is a real CTE, which is a subquery.
    fn bindings(&mut self) -> Result<()> {
        loop {
            if self.word_is("SELECT") {
                return Ok(());
            }
            // `x AS (SELECT ...)` and `(SELECT ...) AS x` are both a subquery under a name.
            if self.peek() == Some(&Tok::LParen) || (self.word_at_is(1, "AS") && self.at_lparen(2))
            {
                return Err(self.refuse(Refused::Subquery));
            }
            let value = self.literal("a constant to bind")?;
            self.expect_word("AS", "AS after the constant")?;
            let name = self.bare_ident("a name for the constant")?;
            self.bound.insert(name, value);
            if !self.eat(&Tok::Comma) {
                return Ok(());
            }
        }
    }

    fn at_lparen(&self, n: usize) -> bool {
        matches!(self.t.get(self.i + n).map(|t| &t.tok), Some(Tok::LParen))
    }

    pub(super) fn refuse(&self, what: Refused) -> SqlError {
        SqlError::Refused { what, at: self.at() }
    }

    /// The same, pointed at a token the parser has already moved past.
    ///
    /// A refusal is only useful if it points at the construct that caused it, and `DISTINCT` is
    /// judged after the select list has been read - by which time `at()` is somewhere else.
    pub(super) fn refuse_at(&self, what: Refused, at: usize) -> SqlError {
        SqlError::Refused { what, at }
    }
    pub(super) fn syntax(&self, want: &'static str) -> SqlError {
        SqlError::Syntax { at: self.at(), found: self.here(), want }
    }

    /// The current token as a bare word, whatever its case.
    pub(super) fn word(&self) -> Option<&str> {
        match self.peek() {
            Some(Tok::Word(w)) => Some(w.as_str()),
            _ => None,
        }
    }

    pub(super) fn word_is(&self, kw: &str) -> bool {
        self.word().is_some_and(|w| w.eq_ignore_ascii_case(kw))
    }

    /// The word `n` tokens ahead, for the two-token keywords: `GROUP BY`, `NOT IN`, `IS NULL`.
    pub(super) fn word_at_is(&self, n: usize, kw: &str) -> bool {
        matches!(self.t.get(self.i + n).map(|t| &t.tok), Some(Tok::Word(w)) if w.eq_ignore_ascii_case(kw))
    }

    /// The half named right after `EXPLAIN`, if one was.
    ///
    /// Peeked in this one position and nowhere else, so `PLAN` and `SHAPE` stay ordinary words
    /// everywhere a table or a column may be called either.
    pub(super) fn explain_mode(&mut self) -> ExplainMode {
        if self.eat_word("PLAN") {
            ExplainMode::Plan
        } else if self.eat_word("SHAPE") {
            ExplainMode::Shape
        } else {
            ExplainMode::All
        }
    }

    pub(super) fn eat_word(&mut self, kw: &str) -> bool {
        if self.word_is(kw) {
            self.i += 1;
            true
        } else {
            false
        }
    }

    pub(super) fn expect_word(&mut self, kw: &'static str, want: &'static str) -> Result<()> {
        if self.eat_word(kw) {
            Ok(())
        } else {
            Err(self.syntax(want))
        }
    }

    pub(super) fn eat(&mut self, tok: &Tok) -> bool {
        if self.peek() == Some(tok) {
            self.i += 1;
            true
        } else {
            false
        }
    }

    pub(super) fn expect(&mut self, tok: &Tok, want: &'static str) -> Result<()> {
        if self.eat(tok) {
            Ok(())
        } else {
            Err(self.syntax(want))
        }
    }

    /// A column name, with the table qualifier it was written with.
    ///
    /// **The qualifier is kept, not dropped.** With a join in the statement it is the only
    /// thing that says which table a column belongs to, and the select list is parsed before
    /// `FROM` - so which table it names cannot be decided here. The lowering decides, where
    /// both sources are known, and a qualifier that names neither is refused there.
    pub(super) fn name(&mut self, want: &'static str) -> Result<Name> {
        let at = self.at();
        let first = self.bare_ident(want)?;
        if self.eat(&Tok::Dot) {
            let column = self.bare_ident("a column name after the qualifier")?;
            // `sales.orders.amount`. Refused by name rather than as "expected a column": the
            // one qualifier a column takes is an alias, and what the writer needs to be told
            // is which alias to give the table. See [`Refused::ThreePartName`].
            if self.peek() == Some(&Tok::Dot) {
                return Err(self.refuse_at(Refused::ThreePartName, at));
            }
            return Ok(Name { qualifier: Some(first), column });
        }
        Ok(Name::bare(first))
    }

    /// A non-negative whole number written literally.
    ///
    /// Not [`Parser::literal`]: a `WITH` binding has no business standing in for a bit depth,
    /// and a negative scale is not a thing the catalog stores.
    pub(super) fn small_number(&mut self, want: &'static str) -> Result<u64> {
        match self.peek() {
            Some(Tok::Num(Literal::Int(n))) => {
                let n = *n;
                self.i += 1;
                Ok(n)
            }
            _ => Err(self.syntax(want)),
        }
    }

    /// `[<database> '.'] <table>`, the way every statement names a table.
    ///
    /// Two parts and never three. A column keeps its one-level qualifier, which always means an
    /// alias - see [`crate::ast::Name`] - because the select list is parsed before `FROM` and a
    /// three-part name would make `a.b` ambiguous between "alias `a`, column `b`" and
    /// "database `a`, table `b`". `sales.orders.amount` is refused with the alias to write
    /// instead, rather than resolved by guessing.
    pub(super) fn table_ref(&mut self, want: &'static str) -> Result<(Option<String>, String)> {
        let at = self.at();
        let first = self.bare_ident(want)?;
        if !self.eat(&Tok::Dot) {
            return Ok((None, first));
        }
        let table = self.bare_ident("a table name after the database")?;
        if self.peek() == Some(&Tok::Dot) {
            return Err(self.refuse_at(Refused::ThreePartName, at));
        }
        Ok((Some(first), table))
    }

    pub(super) fn bare_ident(&mut self, want: &'static str) -> Result<String> {
        match self.peek() {
            Some(Tok::Word(w)) => {
                let w = w.clone();
                self.i += 1;
                Ok(w)
            }
            Some(Tok::Quoted(w)) => {
                let w = w.clone();
                self.i += 1;
                Ok(w)
            }
            _ => Err(self.syntax(want)),
        }
    }

    pub(super) fn literal(&mut self, want: &'static str) -> Result<Literal> {
        // A bare word that `WITH` bound is the value it was bound to. Checked first so a
        // binding is usable wherever a literal is, which is the only place it could mean
        // anything: this dialect has no expressions for one to appear in.
        if let Some(Tok::Word(w)) = self.peek() {
            if let Some(v) = self.bound.get(w.as_str()) {
                let v = v.clone();
                self.i += 1;
                return Ok(v);
            }
        }
        // `now()` is a value, so it belongs here rather than in each clause that takes one:
        // this is the single door a written value comes through, and putting it here is what
        // makes `WHERE ts < now()`, `VALUES (now())` and `WITH t AS now()` one feature rather
        // than three. It becomes the text a date is written in, so everything downstream reads
        // it through the same conversion a hand-written `'2024-01-15 10:30:00'` gets.
        if self.word_is("now")
            && matches!(self.t.get(self.i + 1).map(|t| &t.tok), Some(Tok::LParen))
        {
            self.i += 1;
            self.expect(&Tok::LParen, "( after now")?;
            self.expect(&Tok::RParen, ") after now(")?;
            return Ok(Literal::Str(big_civil::format_datetime(self.now)));
        }
        // `NULL` is refused where a value would go rather than parsed into one, because the
        // whole point is that this engine has no value it could become.
        if self.word_is("NULL") {
            return Err(self.refuse(Refused::Null));
        }
        if self.eat_word("TRUE") {
            return Ok(Literal::Bool(true));
        }
        if self.eat_word("FALSE") {
            return Ok(Literal::Bool(false));
        }
        // A leading `-` is a token of its own since this dialect gained arithmetic - see
        // [`crate::lex::Tok::Arith`] - so the sign is put back here, where what is being read is
        // known to be a value. A `-` in front of anything but a number is a syntax error rather
        // than a negation, which is what `negate` says.
        if matches!(self.peek(), Some(Tok::Arith("-"))) {
            let at = self.at();
            self.i += 1;
            let Some(Tok::Num(n)) = self.peek() else { return Err(self.syntax(want)) };
            let n = crate::lex::negate(n, at)?;
            self.i += 1;
            return Ok(n);
        }
        match self.peek() {
            Some(Tok::Num(n)) => {
                let n = n.clone();
                self.i += 1;
                Ok(n)
            }
            Some(Tok::Str(s)) => {
                let s = s.clone();
                self.i += 1;
                Ok(Literal::Str(s))
            }
            _ => Err(self.syntax(want)),
        }
    }
}

mod alter;
mod cond;
mod create;
/// The forward half of the decimal depth rule, for [`crate::render`] to invert.
pub(crate) use create::decimal_bits;
mod insert;
mod item;
mod scalar;
mod select;
mod show;
