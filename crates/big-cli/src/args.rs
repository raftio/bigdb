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

//! `argv` to one [`Command`], which is one route.
//!
//! Hand-rolled, in the shape `bigd`'s own `Options::parse` uses, and for the same reason: a
//! dozen options do not justify an argument-parsing dependency in a crate whose entire point is
//! that it has none.
//!
//! **Every command here is one request.** There is no subcommand that loops, pages, or composes
//! two routes, and that is not laziness - a client that could answer something the server
//! cannot has become a second engine with a worse test suite. Adding one means adding a route
//! first.

use std::time::Duration;

/// Where a statement or a body comes from.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Source {
    /// Written on the command line.
    Literal(String),
    /// `-`, meaning standard input. What makes `bigc` compose with a shell rather than replace
    /// one.
    Stdin,
}

/// How to print an answer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Format {
    /// Aligned columns, for a person.
    Table,
    /// Tab-separated, for a pipe.
    Tsv,
    /// The server's body, verbatim.
    Json,
}

impl Format {
    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "table" => Self::Table,
            "tsv" => Self::Tsv,
            "json" => Self::Json,
            _ => return None,
        })
    }
}

/// One subcommand, which is one public route.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Command {
    Sql(Source),
    Query { table: String, text: Source },
    Records { table: String, after: Option<u64>, limit: Option<u64> },
    Import { table: String, body: Source },
    Delete { table: String, body: Source },
    Schema,
    CreateTable { table: String, params: Vec<(String, String)> },
    CreateField { table: String, field: String, params: Vec<(String, String)> },
    DropTable { table: String },
    DropField { table: String, field: String },
    Verify,
    Repair,
    Health,
    Ready,
    Metrics,
    Shell,
}

/// Everything a run needs, resolved.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Options {
    pub addr: String,
    pub token_file: Option<String>,
    /// `None` means "decide from where the output is going" - see [`crate::render`].
    pub format: Option<Format>,
    pub timeout: Option<Duration>,
    pub command: Command,
}

/// `BIG_ADDR` when set, this otherwise. The same default `bigd` binds to.
pub const DEFAULT_ADDR: &str = "127.0.0.1:7654";

pub const USAGE: &str = "\
usage: bigc [options] <command> [args]

Asks a running bigd. Every command below is exactly one of its routes; there is no offline
mode and no --file, because a second query path is a path nobody tests. Start a daemon.

Queries:
  sql <statement>|-           one SELECT over one table, or CREATE TABLE
  query <table> <call>|-      one PQL call
  records <table>             every record id, in order
      --after <id>              resume after this id
      --limit <n>               how many to return
  shell                       a loop over sql and query

Schema:
  schema                      every table and field
  create table <table>
      --engine <e>              bitmap | bitmap+columnar | columnar
                                default bitmap+columnar
  create field <table> <field> --kind int|signed|decimal|set|mutex|bool|timequantum
      --bit-depth <n>           for int and signed
      --scale <n>               for decimal
      --granularity <chars>     for timequantum
  drop table <table>
  drop field <table> <field>

Data:
  import <table> <facts>|-    one fact per line: `field record value`
  delete <table> <ids>|-      one record id per line

Operations:
  verify                      do the copies of every range still agree
  repair                      catch up every copy that is behind
  health | ready | metrics    the three probes

Options:
  --addr <host:port>          default 127.0.0.1:7654, or $BIG_ADDR
  --token-file <file>         a bearer token, mode 600; or $BIG_TOKEN
  --format table|tsv|json     default: table to a terminal, tsv to a pipe
  --timeout <seconds>         give up on the exchange; default is to wait
  -h, --help

A token is read from a file and never taken as a flag: an argument is visible in `ps` and in
shell history, and a bearer token in either is a token that has leaked.

`bigc shell` has no line editing on purpose. `rlwrap bigc shell` gives it history and arrow
keys, and does it better than a hand-rolled termios mode would.

`import` and `delete` take the facts themselves, or `-` for standard input. Neither reads a
path: an argument is a body, the way it is for `sql`, and the whole body goes in one request
that the server bounds at 8 MiB. A file is `bigi`'s job - it cuts one into chunks, resumes an
interrupted load, and is a separate binary because this one sends exactly one request.

Exit codes: 0 answered, 1 the server refused, 2 usage, 3 nothing was listening.
";

/// Parses `argv`. `Err("")` means `--help` was asked for, which is not a failure.
pub fn parse(args: &[String], env: &dyn Fn(&str) -> Option<String>) -> Result<Options, String> {
    let mut addr = env("BIG_ADDR").unwrap_or_else(|| DEFAULT_ADDR.to_string());
    let mut token_file = env("BIG_TOKEN");
    let mut format = None;
    let mut timeout = None;
    // Flags that belong to a subcommand rather than to the client. Collected here because they
    // may be written before or after the positional arguments, which is what everybody expects
    // and what nobody says out loud.
    // One list rather than a variable each, because the check that was missing is a check
    // *across* them: which flags a subcommand accepts. See `command`.
    let mut scoped: Vec<(String, String)> = Vec::new();
    let mut positional: Vec<String> = Vec::new();

    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        let value = || args.get(i + 1).cloned().ok_or_else(|| format!("{arg} needs a value"));
        match arg {
            "--addr" => {
                addr = value()?;
                i += 2;
            }
            "--token-file" => {
                token_file = Some(value()?);
                i += 2;
            }
            "--format" => {
                let v = value()?;
                format = Some(
                    Format::parse(&v)
                        .ok_or_else(|| format!("--format takes table, tsv or json, got `{v}`"))?,
                );
                i += 2;
            }
            "--timeout" => {
                let secs: u64 = number(&value()?, arg)?;
                // Zero means "wait", not "give up immediately", which would be a confusing way
                // to spell a client that never works. Same reading `bigd` gives its own.
                timeout = (secs > 0).then(|| Duration::from_secs(secs));
                i += 2;
            }
            "--after" | "--limit" | "--kind" | "--bit-depth" | "--scale" | "--granularity"
            | "--engine" => {
                // Values are passed through as the query parameter the route already takes,
                // rather than re-spelled here. A field kind or an engine name this client has
                // never heard of is a matter between the caller and the server.
                //
                // Which *flags* a subcommand takes is this client's business, though, and is
                // checked in `command`.
                scoped.push((arg.trim_start_matches("--").replace('-', "_"), value()?));
                i += 2;
            }
            "-h" | "--help" => return Err(String::new()),
            other if other.starts_with("--") => return Err(format!("unknown option {other}")),
            other => {
                positional.push(other.to_string());
                i += 1;
            }
        }
    }

    let command = command(&positional, scoped)?;
    Ok(Options { addr, token_file, format, timeout, command })
}

/// The flag name as it was typed, for an error message. The list stores query-parameter names,
/// which spell `bit_depth` where the flag says `--bit-depth`.
fn flag(param: &str) -> String {
    format!("--{}", param.replace('_', "-"))
}

fn command(positional: &[String], scoped: Vec<(String, String)>) -> Result<Command, String> {
    let words: Vec<&str> = positional.iter().map(String::as_str).collect();

    /// Refuses a subcommand flag that belongs to a different subcommand.
    ///
    /// **This is the check that used to be missing, and its absence was a silent failure rather
    /// than a small one.** Every one of these flags was accepted on every command and then
    /// dropped wherever it meant nothing, so `records t --limit 5` worked and
    /// `records t --engine columnar` did nothing at all - with no error, no output difference,
    /// and nothing for the caller to notice. A client that swallows what it was given is worse
    /// than one that refuses it.
    fn only(scoped: &[(String, String)], command: &str, allowed: &[&str]) -> Result<(), String> {
        for (name, _) in scoped {
            if !allowed.contains(&name.as_str()) {
                return Err(format!("{} does not belong to `{command}`", flag(name)));
            }
        }
        Ok(())
    }

    /// A numeric flag, or `None` when it was not given.
    fn num(scoped: &[(String, String)], name: &str) -> Result<Option<u64>, String> {
        match scoped.iter().find(|(k, _)| k == name) {
            None => Ok(None),
            Some((_, v)) => number(v, &flag(name)).map(Some),
        }
    }

    Ok(match words.as_slice() {
        [] => return Err("a command is required".to_string()),

        ["sql", statement] => {
            only(&scoped, "sql", &[])?;
            Command::Sql(source(statement))
        }
        ["query", table, call] => {
            only(&scoped, "query", &[])?;
            Command::Query { table: (*table).to_string(), text: source(call) }
        }
        ["records", table] => {
            only(&scoped, "records", &["after", "limit"])?;
            Command::Records {
                table: (*table).to_string(),
                after: num(&scoped, "after")?,
                limit: num(&scoped, "limit")?,
            }
        }
        ["import", table, file] => {
            only(&scoped, "import", &[])?;
            Command::Import { table: (*table).to_string(), body: source(file) }
        }
        ["delete", table, file] => {
            only(&scoped, "delete", &[])?;
            Command::Delete { table: (*table).to_string(), body: source(file) }
        }

        ["schema"] => {
            only(&scoped, "schema", &[])?;
            Command::Schema
        }
        ["create", "table", table] => {
            only(&scoped, "create table", &["engine"])?;
            Command::CreateTable { table: (*table).to_string(), params: scoped }
        }
        ["create", "field", table, field] => {
            only(&scoped, "create field", &["kind", "bit_depth", "scale", "granularity"])?;
            if !scoped.iter().any(|(k, _)| k == "kind") {
                return Err("create field needs --kind".to_string());
            }
            Command::CreateField {
                table: (*table).to_string(),
                field: (*field).to_string(),
                params: scoped,
            }
        }
        ["drop", "table", table] => {
            only(&scoped, "drop table", &[])?;
            Command::DropTable { table: (*table).to_string() }
        }
        ["drop", "field", table, field] => {
            only(&scoped, "drop field", &[])?;
            Command::DropField { table: (*table).to_string(), field: (*field).to_string() }
        }

        ["verify"] => {
            only(&scoped, "verify", &[])?;
            Command::Verify
        }
        ["repair"] => {
            only(&scoped, "repair", &[])?;
            Command::Repair
        }
        ["health"] => {
            only(&scoped, "health", &[])?;
            Command::Health
        }
        ["ready"] => {
            only(&scoped, "ready", &[])?;
            Command::Ready
        }
        ["metrics"] => {
            only(&scoped, "metrics", &[])?;
            Command::Metrics
        }
        ["shell"] => {
            only(&scoped, "shell", &[])?;
            Command::Shell
        }

        // Named rather than answered with the usage alone: "wrong number of arguments to a
        // command that exists" and "no such command" are different mistakes.
        [name, ..] if KNOWN.contains(name) => return Err(format!("wrong arguments for `{name}`")),
        [name, ..] => return Err(format!("no such command `{name}`")),
    })
}

/// Every first word this client answers to, for telling a typo from a misuse.
const KNOWN: [&str; 15] = [
    "sql", "query", "records", "import", "delete", "schema", "create", "drop", "verify", "repair",
    "health", "ready", "metrics", "shell", "help",
];

fn source(arg: &str) -> Source {
    if arg == "-" {
        Source::Stdin
    } else {
        Source::Literal(arg.to_string())
    }
}

fn number(s: &str, flag: &str) -> Result<u64, String> {
    s.parse().map_err(|_| format!("{flag} needs a number, got `{s}`"))
}
