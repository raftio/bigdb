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

//! The one question a writer has to be able to ask: *did this already land?*
//!
//! # Why this is one call and not a query surface
//!
//! [`Producer`](crate::Producer) is at-least-once and cannot be anything else, because the
//! server allocates the record ids - so a caller recovering from an [`Error::Unknown`], or a
//! sink restarting with unfinished work, has exactly one thing it needs to know: which of these
//! keys are already in the table. That is what this answers, and nothing else.
//!
//! It would have been less code to expose "send any statement and hand back the rows". It would
//! also have moved the boundary: a caller writing its own SQL is a caller this crate cannot keep
//! from writing SQL that disagrees with what [`Producer`](crate::Producer) writes, and the
//! escaping - the one place in this crate where a mistake is a security bug rather than a
//! failure - would have had two callers instead of one.
//!
//! # Retry here is the *opposite* of retry there
//!
//! A `SELECT` is idempotent, so unlike a flush this may be sent again after a failure of any
//! kind. That asymmetry is the whole difference between the two files, and it is why they do not
//! share a retry loop: the producer's rule exists to protect against a duplicate that reading
//! cannot cause.

use crate::config::Config;
use crate::error::Error;
use crate::http::{Conn, Failure};
use crate::json::{self, Value};
use crate::sql;

/// How many keys one question carries.
///
/// The statement is bounded by `big_http::MAX_BODY` like any other, and a key is short, so this
/// is far under it. It exists so that a caller handing over an unbounded list gets several
/// questions rather than one refusal.
const KEYS_PER_QUESTION: usize = 1_000;

/// Asks a bigdb table what it already holds.
pub struct Reader {
    conn: Conn,
    retries: u32,
}

impl Reader {
    /// Opens a reader against one server. No connection is made until something is asked.
    pub fn open(addr: &str, token: Option<&str>, config: &Config) -> Self {
        Self { conn: Conn::new(addr, token, Some(config.io_timeout)), retries: config.retries }
    }

    /// Which of `keys` the table already holds in `field`.
    ///
    /// Answers a subset of what it was given, in no particular order. A key the table does not
    /// hold is simply absent - there is no way to ask "does this exist" that distinguishes an
    /// empty table from a missing one, because both mean the same thing to a caller deciding
    /// whether to write.
    ///
    /// **Only useful when the caller put the key there.** A field a producer writes the message's
    /// own identifier into is what makes this answerable; a table with no such column has nothing
    /// to match on, because the record ids are the server's and are never handed out.
    pub fn seen(&mut self, table: &str, field: &str, keys: &[&str]) -> Result<Vec<String>, Error> {
        sql::check_column(field)?;
        let mut found = Vec::new();
        for chunk in keys.chunks(KEYS_PER_QUESTION) {
            if chunk.is_empty() {
                continue;
            }
            let statement = self.question(table, field, chunk)?;
            let body = self.ask(&statement)?;
            read_column(&body, &mut found)?;
        }
        Ok(found)
    }

    /// `SELECT "f" FROM "t" WHERE "f" IN ('a','b')`, escaped by the one escaper this crate has.
    fn question(&self, table: &str, field: &str, keys: &[&str]) -> Result<String, Error> {
        let mut out = String::with_capacity(64 + keys.len() * 16);
        out.push_str("SELECT ");
        sql::push_ident(field, &mut out)?;
        out.push_str(" FROM ");
        sql::push_table(table, &mut out)?;
        out.push_str(" WHERE ");
        sql::push_ident(field, &mut out)?;
        out.push_str(" IN (");
        for (i, key) in keys.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            sql::push_text(key, &mut out);
        }
        out.push(')');
        Ok(out)
    }

    /// One statement, retried on any transport failure - which a read may do and a write may not.
    fn ask(&mut self, statement: &str) -> Result<String, Error> {
        let mut attempt = 0u32;
        let mut wait = std::time::Duration::from_millis(250);
        loop {
            let again = attempt < self.retries;
            match self.conn.send("POST", "/sql", statement) {
                Ok(response) if response.ok() => return Ok(response.body),
                Ok(response) => {
                    let (code, message) = json::failure(&response.body);
                    return Err(Error::Refused { status: response.status, code, message });
                }
                // Every one of these is retried, including the one a flush must never retry:
                // asking the same question twice gets the same answer and changes nothing.
                Err(Failure::NotSent(why) | Failure::Unknown(why)) if again => {
                    let _ = why;
                }
                Err(Failure::NotSent(why) | Failure::Unknown(why)) => {
                    return Err(Error::Connect(why))
                }
                Err(Failure::Protocol(why)) => return Err(Error::Protocol(why)),
            }
            attempt += 1;
            std::thread::sleep(wait);
            wait = (wait * 2).min(std::time::Duration::from_secs(8));
        }
    }
}

/// The first column of every row of `{"columns":[...],"rows":[[...]]}`.
fn read_column(body: &str, out: &mut Vec<String>) -> Result<(), Error> {
    let value = json::parse(body).map_err(Error::Protocol)?;
    let Some(Value::Arr(rows)) = value.get("rows") else {
        return Err(Error::Protocol(format!("an answer with no rows: {body}")));
    };
    for row in rows {
        if let Some(cell) = row.at(0).and_then(Value::cell) {
            out.push(cell.to_string());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reader() -> Reader {
        Reader::open("127.0.0.1:1", None, &Config::default())
    }

    #[test]
    fn a_question_names_the_field_and_quotes_every_key() {
        let q = reader().question("tx", "msg_id", &["1700-0", "1700-1"]).unwrap();
        assert_eq!(q, "SELECT \"msg_id\" FROM \"tx\" WHERE \"msg_id\" IN ('1700-0','1700-1')");
    }

    #[test]
    fn a_key_holding_a_quote_cannot_end_the_question() {
        let q = reader().question("tx", "msg_id", &["x') OR ('1'='1"]).unwrap();
        assert!(q.ends_with("IN ('x'') OR (''1''=''1')"), "{q}");
    }

    #[test]
    fn the_record_id_is_not_a_field_this_will_ask_about() {
        assert!(reader().seen("tx", "_record_id", &["1"]).is_err());
    }

    #[test]
    fn an_answer_reads_as_the_keys_it_holds() {
        let mut out = Vec::new();
        read_column(r#"{"columns":["msg_id"],"rows":[["1700-0"],["1700-2"]]}"#, &mut out).unwrap();
        assert_eq!(out, vec!["1700-0".to_string(), "1700-2".to_string()]);
    }

    #[test]
    fn an_empty_answer_is_no_keys_rather_than_an_error() {
        let mut out = Vec::new();
        read_column(r#"{"columns":["msg_id"],"rows":[]}"#, &mut out).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn an_answer_that_is_not_one_is_reported_rather_than_read_as_empty() {
        // Reading this as "nothing was found" would make a dedup pass rewrite everything.
        let mut out = Vec::new();
        assert!(read_column("<html>502</html>", &mut out).is_err());
        assert!(read_column(r#"{"error":"nope","code":"x"}"#, &mut out).is_err());
    }
}
