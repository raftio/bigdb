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
//! statement := [WITH literal AS ident (',' literal AS ident)*] select
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

use crate::ast::{Name, Query, Select};
use crate::error::{Refused, Result, SqlError};
use crate::lex::{lex, Tok, Token};
use big_plan::Literal;

/// How deep a `WHERE` clause may nest before it is refused.
///
/// **A bound on the stack, not a taste in conditions.** The reasoning is
/// [`big_plan::parse::MAX_DEPTH`]'s, unchanged and for the same reason: this is a recursive
/// descent, nesting is call depth, a stack overflow aborts the process rather than unwinding,
/// and `bigd` runs queries on pool threads whose stacks are smaller than the main thread's. The
/// limit is the same number so that neither language is the one that overflows first.
pub const MAX_DEPTH: usize = big_plan::parse::MAX_DEPTH;

/// One statement, whichever kind it turned out to be.
///
/// The leading keyword decides, and only here: everything downstream is handed one shape or the
/// other and never has to ask again.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Parsed {
    Query(Query),
    Ddl(crate::ddl::Ddl),
}

/// Parses one statement, refusing anything this engine does not answer.
pub fn parse(input: &str) -> Result<Parsed> {
    let tokens = lex(input)?;
    let mut p = Parser { t: &tokens, i: 0, end: input.len(), depth: 0, bound: Default::default() };

    // The leading keyword decides which of three things happened, and only one of them is a
    // syntax error. A `DELETE` is a statement this surface refuses; a `foo` is not a statement.
    let Some(word) = p.word() else {
        return Err(SqlError::Syntax { at: p.at(), found: p.here(), want: "a SELECT statement" });
    };
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
        // `CREATE TABLE` is the one schema change this surface takes. Everything else that
        // writes is still refused by name, including the rest of `CREATE`: a surface that grew
        // `CREATE INDEX` by accident would be a surface nobody chose.
        "CREATE" => {
            p.i += 1;
            return p.create_table().map(Parsed::Ddl);
        }
        "INSERT" | "UPDATE" | "DELETE" | "DROP" | "ALTER" | "TRUNCATE" | "REPLACE" | "MERGE"
        | "UPSERT" => return Err(p.refuse(Refused::Write)),
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
    /// Byte length of the input, which is where "end of statement" points.
    end: usize,
    depth: usize,
    /// Constants a `WITH` clause bound, by the name it gave them.
    bound: std::collections::BTreeMap<String, Literal>,
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
            Some(Tok::Op(o)) => (*o).to_string(),
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
        let first = self.bare_ident(want)?;
        if self.eat(&Tok::Dot) {
            let column = self.bare_ident("a column name after the qualifier")?;
            return Ok(Name { qualifier: Some(first), column });
        }
        Ok(Name::bare(first))
    }

    // ------------------------------------------------------------------------------------
    // Schema
    // ------------------------------------------------------------------------------------

    /// `CREATE TABLE <name> [ENGINE = <engine>]`, with `CREATE` already consumed.
    ///
    /// No column list, and that is the engine's own shape rather than a shortcut: a table and
    /// its fields are two statements here because they are two requests, and a column syntax
    /// would need a mapping from SQL types onto field kinds that have no SQL analogue - a set,
    /// a mutex, a time quantum. Inventing one silently would be inventing a dialect.
    pub(super) fn create_table(&mut self) -> Result<crate::ddl::Ddl> {
        if !self.word_is("TABLE") {
            // Every other `CREATE` is still refused by name. The message a `Write` refusal
            // gives already says this surface does not write, which is the right sentence for
            // `CREATE INDEX` and `CREATE VIEW` alike.
            return Err(self.refuse(Refused::Write));
        }
        self.i += 1;
        let table = self.bare_ident("a table name")?;

        // Named rather than answered as a syntax error. `CREATE TABLE t (a int)` *is* a
        // statement and the writer knows what they meant; what they need to hear is that fields
        // are declared separately here and why, not that a bracket was unexpected.
        if matches!(self.peek(), Some(Tok::LParen)) {
            return Err(self.refuse(Refused::ColumnList));
        }

        let engine = if self.word_is("ENGINE") {
            self.i += 1;
            if !self.eat(&Tok::Op("=")) {
                return Err(self.syntax("= after ENGINE"));
            }
            Some(self.engine_name()?)
        } else {
            None
        };

        if self.peek().is_some() {
            return Err(self.syntax("the end of the statement"));
        }
        Ok(crate::ddl::Ddl::CreateTable { table, engine })
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

mod cond;
mod item;
mod select;
