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

//! `bigc` - one command, one route, no vocabulary of its own.
//!
//! This crate links **nothing**. Not `big-api`, not `big-plan`, not `big-sql`. That is the
//! design rather than an economy: a client that cannot link the engine cannot grow an offline
//! query path, and cannot validate a statement it is about to send. Both would be a second
//! surface that drifts from the first, and the second one always loses.
//!
//! So a statement travels as bytes and an error comes back as the server's own code and the
//! server's own sentence. `sql_no_joins` means on the command line exactly what it means over
//! HTTP, because it *is* the same string.
//!
//! [`run`] is the whole program, taking its streams as arguments so that the tests can drive it
//! against a real `bigd` on a loopback port and read what a user would have seen.

#![deny(unsafe_code)]

pub mod args;
pub mod http;
pub mod json;
pub mod render;
pub mod shell;

pub use args::{Command, Format, Options, Source};
pub use http::{Client, Error as HttpError};
pub use json::{Answer, Failure};

use std::io::{BufRead, Write};

/// Exit codes, which are part of the surface: a script branches on them.
pub mod exit {
    /// The server answered.
    pub const OK: i32 = 0;
    /// The server refused. The code and the sentence are on stderr.
    pub const REFUSED: i32 = 1;
    /// The command line was wrong, or an input could not be read.
    pub const USAGE: i32 = 2;
    /// Nothing was listening, or the exchange did not complete.
    pub const UNREACHABLE: i32 = 3;
}

/// The streams a run works over, so that a test can supply its own.
pub struct Io<'a> {
    pub input: &'a mut dyn BufRead,
    pub out: &'a mut dyn Write,
    pub err: &'a mut dyn Write,
    /// Whether `out` is a terminal, which decides the default format and whether the shell
    /// prints a prompt. Passed in rather than asked, because `out` may not be a terminal *or* a
    /// pipe - in a test it is a `Vec<u8>`.
    pub tty: bool,
}

/// Parses, sends, prints. Returns the process's exit code.
pub fn run(args: &[String], io: &mut Io<'_>, env: &dyn Fn(&str) -> Option<String>) -> i32 {
    let options = match args::parse(args, env) {
        Ok(o) => o,
        // `--help` is not a failure: usage to stdout, exit zero. The same split `bigd` makes.
        Err(e) if e.is_empty() => {
            let _ = write!(io.out, "{}", args::USAGE);
            return exit::OK;
        }
        Err(e) => {
            let _ = writeln!(io.err, "bigc: {e}\n");
            let _ = write!(io.err, "{}", args::USAGE);
            return exit::USAGE;
        }
    };

    let token = match &options.token_file {
        None => None,
        Some(path) => match read_token(path) {
            Ok(t) => Some(t),
            Err(e) => {
                let _ = writeln!(io.err, "bigc: {e}");
                return exit::USAGE;
            }
        },
    };
    let client = Client { addr: options.addr.clone(), token, timeout: options.timeout };
    let format = options.format.unwrap_or_else(|| render::default_for(io.tty));

    if options.command == Command::Shell {
        return shell::run(&client, io.input, io.out, io.err, format, io.tty);
    }

    let request = match request(&options.command, io.input) {
        Ok(r) => r,
        Err(e) => {
            let _ = writeln!(io.err, "bigc: {e}");
            return exit::USAGE;
        }
    };

    let response = match client.send(request.method, &request.target, &request.body) {
        Ok(r) => r,
        Err(e) => {
            let _ = writeln!(io.err, "bigc: {e}");
            return exit::UNREACHABLE;
        }
    };

    if !response.ok() {
        let failure = Failure::read(&response.body);
        let _ = writeln!(io.err, "bigc: {} [{}]", failure.message, failure.code);
        return exit::REFUSED;
    }

    // `/metrics` is Prometheus text and the only route that does not answer with JSON. Printed
    // as it arrived: it is already a line format, and rendering it would mean parsing a second
    // one.
    if request.text {
        let _ = write!(io.out, "{}", response.body);
        return exit::OK;
    }

    if format == Format::Json {
        let _ = writeln!(io.out, "{}", response.body.trim_end());
        return exit::OK;
    }

    match Answer::read(&response.body) {
        Ok(answer) => {
            let _ = write!(io.out, "{}", render::answer(&answer, format));
            for note in &answer.notes {
                let _ = writeln!(io.err, "bigc: {note}");
            }
            exit::OK
        }
        Err(why) => {
            // The body is printed with the complaint rather than swallowed: the user asked a
            // question and the server answered it, and a client that cannot lay the answer out
            // should still hand it over.
            let _ = writeln!(io.err, "bigc: could not read the answer: {why}");
            let _ = writeln!(io.out, "{}", response.body.trim_end());
            exit::UNREACHABLE
        }
    }
}

/// One HTTP request, which is what every subcommand becomes.
struct Request {
    method: &'static str,
    target: String,
    body: String,
    /// The answer is text rather than JSON. True for `/metrics` and nothing else.
    text: bool,
}

impl Request {
    fn new(method: &'static str, target: String, body: String) -> Self {
        Self { method, target, body, text: false }
    }
}

/// **The whole of Rule 1, in one function.** Every arm is exactly one route: no arm sends two
/// requests, loops over pages, or filters an answer. A subcommand that needs more than one
/// exchange is a route that does not exist yet, and the fix is on the server.
fn request(command: &Command, input: &mut dyn BufRead) -> Result<Request, String> {
    Ok(match command {
        Command::Sql(source) => Request::new("POST", "/sql".to_string(), read(source, input)?),
        Command::Query { table, text } => {
            Request::new("POST", format!("/table/{}/query", escape(table)), read(text, input)?)
        }
        Command::Records { table, after, limit } => {
            let mut query = Vec::new();
            if let Some(a) = after {
                query.push(format!("after={a}"));
            }
            if let Some(l) = limit {
                query.push(format!("limit={l}"));
            }
            let suffix =
                if query.is_empty() { String::new() } else { format!("?{}", query.join("&")) };
            Request::new("GET", format!("/table/{}/records{suffix}", escape(table)), String::new())
        }
        Command::Import { table, body } => {
            Request::new("POST", format!("/table/{}/import", escape(table)), read(body, input)?)
        }
        Command::Delete { table, body } => {
            Request::new("POST", format!("/table/{}/delete", escape(table)), read(body, input)?)
        }

        Command::Schema => Request::new("GET", "/schema".to_string(), String::new()),
        Command::CreateTable { table, params } => {
            // Passed through as the query parameter the route already takes, exactly as
            // `create field` does: an engine name this client has never heard of is a matter
            // between the caller and the server.
            let query: Vec<String> =
                params.iter().map(|(k, v)| format!("{k}={}", escape(v))).collect();
            Request::new(
                "POST",
                format!("/table/{}?{}", escape(table), query.join("&")),
                String::new(),
            )
        }
        Command::CreateField { table, field, params } => {
            let query: Vec<String> =
                params.iter().map(|(k, v)| format!("{k}={}", escape(v))).collect();
            Request::new(
                "POST",
                format!("/table/{}/field/{}?{}", escape(table), escape(field), query.join("&")),
                String::new(),
            )
        }
        Command::DropTable { table } => {
            Request::new("DELETE", format!("/table/{}", escape(table)), String::new())
        }
        Command::DropField { table, field } => Request::new(
            "DELETE",
            format!("/table/{}/field/{}", escape(table), escape(field)),
            String::new(),
        ),

        Command::Verify => Request::new("GET", "/verify".to_string(), String::new()),
        Command::Repair => Request::new("POST", "/repair".to_string(), String::new()),
        Command::Health => Request::new("GET", "/health".to_string(), String::new()),
        Command::Ready => Request::new("GET", "/ready".to_string(), String::new()),
        Command::Metrics => Request {
            method: "GET",
            target: "/metrics".to_string(),
            body: String::new(),
            text: true,
        },

        Command::Shell => unreachable!("handled before the request is built"),
    })
}

/// A statement from the command line, or the whole of standard input.
fn read(source: &Source, input: &mut dyn BufRead) -> Result<String, String> {
    match source {
        Source::Literal(s) => Ok(s.clone()),
        Source::Stdin => {
            let mut body = String::new();
            input
                .read_to_string(&mut body)
                .map(|_| body)
                .map_err(|e| format!("could not read standard input: {e}"))
        }
    }
}

/// Percent-encodes anything that would change the shape of a URL.
///
/// A table name is user data and the engine allows more in one than a path segment does. Not a
/// general encoder: it escapes what the server's router splits on and what a query string is
/// delimited by, and leaves the rest, because a name that arrives mangled is worse than one
/// that arrives long.
///
/// Public because `big-ingest` spells the same two paths. A second encoder there would be a
/// second set of rules about which byte is safe, and the table that arrives at one binary would
/// not be the table that arrives at the other.
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '.' | '_' | '~' => out.push(c),
            other => {
                let mut buf = [0u8; 4];
                for b in other.encode_utf8(&mut buf).as_bytes() {
                    out.push_str(&format!("%{b:02X}"));
                }
            }
        }
    }
    out
}

/// Reads a bearer token from a file, refusing one anybody can read.
///
/// The mode check is `big_http::auth`'s, **copied rather than depended on**. Linking the server
/// to read one file would put the whole engine in this binary's dependency graph and undo the
/// property the empty `[dependencies]` section exists to guarantee. Two implementations of a
/// three-line check is the cheaper of the two prices.
/// Public for the same reason [`escape`] is: `bigi` presents the same credential from the
/// same file, and a second reader is a second opinion about what mode 600 means.
pub fn read_token(path: &str) -> Result<String, String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(path)
            .map_err(|e| format!("could not read {path}: {e}"))?
            .permissions()
            .mode()
            & 0o777;
        if mode & 0o077 != 0 {
            return Err(format!(
                "{path} is mode {mode:o}; a token file must not be readable by anyone else \
                 (chmod 600 {path})"
            ));
        }
    }
    let text = std::fs::read_to_string(path).map_err(|e| format!("could not read {path}: {e}"))?;
    for line in text.lines() {
        // The same reading the server gives its own token file: a comment is stripped, and the
        // first non-empty line is the secret.
        let line = line.split('#').next().unwrap_or("").trim();
        if !line.is_empty() {
            // The server's file is `token role` per line; a client presents only the token.
            return Ok(line.split_whitespace().next().unwrap_or(line).to_string());
        }
    }
    Err(format!("{path}: no token in this file"))
}
