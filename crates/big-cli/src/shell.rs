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

//! A loop, not a language.
//!
//! It reads a line, sends it, prints the answer. **It does not guess which language the line is
//! in.** `Count(...)` is a legal PQL call and not SQL; `SELECT ...` is the reverse. A client
//! that guessed would report a parse error from the wrong parser, which is the least helpful
//! error this system can produce - so there are two prompts and `.lang` moves between them.
//!
//! Meta-commands begin with a dot, which neither language uses, so nothing here can shadow
//! something a user meant to send. The list does not grow without a reason written down next to
//! it, and there is one addition to the five the plan named: **`.table`**, because a PQL call is
//! asked *of* a table and the route carries it in the path. Without it the `pql>` prompt could
//! not send anything at all.

use crate::args::Format;
use crate::http::Client;
use crate::json::{Answer, Failure};
use crate::render;
use std::io::{BufRead, Write};
use std::time::Instant;

/// Which surface a line is sent to.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Lang {
    Sql,
    Pql,
}

impl Lang {
    fn prompt(self) -> &'static str {
        match self {
            Self::Sql => "sql> ",
            Self::Pql => "pql> ",
        }
    }
}

/// Where `USE` is answered.
///
/// **The server has no session to put it in.** `POST /sql` answers one statement and remembers
/// nothing, which is what lets any node answer any request - so a database that persisted
/// across statements would have to be state somewhere, and the somewhere that costs nothing is
/// here. The shell holds it and sends it as `?database=` on every line, which is exactly what
/// ClickHouse's HTTP interface does with the same word.
fn used_database(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("USE ").or_else(|| line.strip_prefix("use "))?;
    let name = rest.trim().trim_end_matches(';').trim();
    // One bare word. `USE sales.orders` is not a database, and passing it on would produce a
    // `?database=` nothing can resolve.
    (!name.is_empty() && !name.contains(|c: char| c.is_whitespace() || c == '.')).then_some(name)
}

const HELP: &str = "\
.lang sql|pql      which surface a line is sent to
USE <database>     which database an unqualified table name means
.table <name>      the table PQL calls are asked of; prints it when given no name
.schema            every table and field
.format table|tsv|json
.timing on|off     print how long each exchange took
.quit              or end of input
";

/// Runs the loop until end of input or `.quit`. Returns the process's exit code.
pub fn run(
    client: &Client,
    input: &mut dyn BufRead,
    out: &mut dyn Write,
    err: &mut dyn Write,
    mut format: Format,
    interactive: bool,
) -> i32 {
    let mut lang = Lang::Sql;
    let mut table: Option<String> = None;
    let mut timing = false;
    let mut database: Option<String> = None;

    loop {
        if interactive {
            // The database is in the prompt because it silently changes what every unqualified
            // name in the next line means, and a shell that hid it would let somebody drop the
            // wrong table for the right reason.
            match &database {
                Some(d) => {
                    let _ = write!(out, "{}({d})> ", lang.prompt().trim_end_matches("> "));
                }
                None => {
                    let _ = write!(out, "{}", lang.prompt());
                }
            }
            let _ = out.flush();
        }
        let mut line = String::new();
        match input.read_line(&mut line) {
            Ok(0) => return 0,
            Ok(_) => {}
            Err(e) => {
                let _ = writeln!(err, "bigc: could not read input: {e}");
                return 2;
            }
        }
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        // `.schema` is the one meta-command that talks to the server, so it is handled where
        // the client is rather than in the pure function below.
        if line == ".schema" {
            show(client, "GET", "/schema", "", out, err, format);
            continue;
        }

        if let Some(rest) = line.strip_prefix('.') {
            match meta(rest, &mut lang, &mut table, &mut format, &mut timing) {
                Meta::Continue => continue,
                Meta::Quit => return 0,
                Meta::Say(message) => {
                    let _ = writeln!(out, "{message}");
                    continue;
                }
                Meta::Complain(message) => {
                    let _ = writeln!(err, "bigc: {message}");
                    continue;
                }
            }
        }

        // `USE` is answered here rather than sent: the server refuses it, with a sentence
        // saying the database is per request - and this is the client that makes it so.
        if lang == Lang::Sql {
            if let Some(name) = used_database(line) {
                database = Some(name.to_string());
                continue;
            }
        }

        // A PQL call is asked *of* a table, and the route puts it in the path. Refused here
        // rather than sent to a URL with a hole in it.
        let (method, target) = match lang {
            Lang::Sql => (
                "POST",
                match &database {
                    Some(d) => format!("/sql?database={d}"),
                    None => "/sql".to_string(),
                },
            ),
            Lang::Pql => match &table {
                Some(t) => ("POST", format!("/table/{t}/query")),
                None => {
                    let _ = writeln!(err, "bigc: PQL is asked of a table; set one with `.table`");
                    continue;
                }
            },
        };

        let started = Instant::now();
        show(client, method, &target, line, out, err, format);
        if timing {
            let _ = writeln!(err, "{:.3}ms", started.elapsed().as_secs_f64() * 1000.0);
        }
    }
}

/// One exchange, printed. Nothing here ends the loop: a daemon restarted mid-session should not
/// end a session, and the next line reconnects.
fn show(
    client: &Client,
    method: &str,
    target: &str,
    body: &str,
    out: &mut dyn Write,
    err: &mut dyn Write,
    format: Format,
) {
    let response = match client.send(method, target, body) {
        Ok(r) => r,
        Err(e) => {
            let _ = writeln!(err, "bigc: {e}");
            return;
        }
    };

    if !response.ok() {
        let failure = Failure::read(&response.body);
        let _ = writeln!(err, "bigc: {} [{}]", failure.message, failure.code);
        return;
    }

    if format == Format::Json {
        let _ = writeln!(out, "{}", response.body);
        return;
    }
    match Answer::read(&response.body) {
        Ok(answer) => {
            let _ = write!(out, "{}", render::answer(&answer, format));
            for note in &answer.notes {
                let _ = writeln!(err, "bigc: {note}");
            }
        }
        Err(why) => {
            let _ = writeln!(err, "bigc: could not read the answer: {why}");
        }
    }
}

enum Meta {
    Continue,
    Quit,
    Say(String),
    Complain(String),
}

fn meta(
    rest: &str,
    lang: &mut Lang,
    table: &mut Option<String>,
    format: &mut Format,
    timing: &mut bool,
) -> Meta {
    let mut words = rest.split_whitespace();
    let (command, argument) = (words.next().unwrap_or(""), words.next());
    match (command, argument) {
        ("quit", _) | ("exit", _) => Meta::Quit,
        ("help", _) => Meta::Say(HELP.trim_end().to_string()),

        ("lang", Some("sql")) => {
            *lang = Lang::Sql;
            Meta::Continue
        }
        ("lang", Some("pql")) => {
            *lang = Lang::Pql;
            Meta::Continue
        }
        ("lang", _) => Meta::Complain(".lang takes sql or pql".to_string()),

        ("table", Some(name)) => {
            *table = Some(name.to_string());
            Meta::Continue
        }
        ("table", None) => match table {
            Some(t) => Meta::Say(t.clone()),
            None => Meta::Complain("no table set".to_string()),
        },

        ("format", Some("table")) => {
            *format = Format::Table;
            Meta::Continue
        }
        ("format", Some("tsv")) => {
            *format = Format::Tsv;
            Meta::Continue
        }
        ("format", Some("json")) => {
            *format = Format::Json;
            Meta::Continue
        }
        ("format", _) => Meta::Complain(".format takes table, tsv or json".to_string()),

        ("timing", Some("on")) => {
            *timing = true;
            Meta::Continue
        }
        ("timing", Some("off")) => {
            *timing = false;
            Meta::Continue
        }
        ("timing", _) => Meta::Complain(".timing takes on or off".to_string()),

        (other, _) => Meta::Complain(format!("no such meta-command `.{other}`; try `.help`")),
    }
}

#[cfg(test)]
mod tests {
    use super::used_database;

    /// `USE` is the one statement this shell answers itself, so what counts as one is worth
    /// pinning: the server has no session to hold a database in, and this is what makes the
    /// word work anyway.
    #[test]
    fn use_names_a_database_and_nothing_else_does() {
        assert_eq!(used_database("USE sales"), Some("sales"));
        assert_eq!(used_database("use sales"), Some("sales"));
        // A trailing semicolon is how half of everybody types SQL.
        assert_eq!(used_database("USE sales;"), Some("sales"));
        assert_eq!(used_database("USE   sales  ;  "), Some("sales"));

        // Not a `USE`. Each of these has to reach the server, which has its own answer for it.
        assert_eq!(used_database("SELECT count(*) FROM tx"), None);
        assert_eq!(used_database("USE"), None);
        assert_eq!(used_database("USE "), None);
        // A database is one bare word: `USE sales.orders` names a table, and passing it on
        // would produce a `?database=` nothing can resolve.
        assert_eq!(used_database("USE sales.orders"), None);
        assert_eq!(used_database("USE sales orders"), None);
        // `USED` is not `USE`, and a table called `used` is a real thing to select from.
        assert_eq!(used_database("USED sales"), None);
    }
}
