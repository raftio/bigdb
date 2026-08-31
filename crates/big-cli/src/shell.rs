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

const HELP: &str = "\
.lang sql|pql      which surface a line is sent to
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

    loop {
        if interactive {
            let _ = write!(out, "{}", lang.prompt());
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

        // A PQL call is asked *of* a table, and the route puts it in the path. Refused here
        // rather than sent to a URL with a hole in it.
        let (method, target) = match lang {
            Lang::Sql => ("POST", "/sql".to_string()),
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
