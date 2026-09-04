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
//! Hand-rolled, in the shape `big serve`'s own `Options::parse` uses, and for the same reason: a
//! dozen options do not justify an argument-parsing dependency for a surface this size.
//!
//! **This is the only parser `bigctl` has.** There were two, one per binary, agreeing by hand
//! that `--addr`, `--token-file` and `--timeout` meant the same thing on both - and agreement
//! by hand is the arrangement that eventually stops agreeing. Merging them is also what lets
//! `only` refuse `--dry-run` on `schema`: neither parser could once see the other's flags.
//!
//! **Every command here is one request, except [`Command::Load`], which is one file.** There is
//! no subcommand that pages or composes two routes, and that is not laziness - a client that
//! could answer something the server cannot has become a second engine with a worse test suite.
//! Adding one means adding a route first.

use crate::ingest::args::{Input, Load, Verb, MAX_IN_FLIGHT};
use std::time::Duration;

/// Where a statement or a body comes from.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Source {
    /// Written on the command line.
    Literal(String),
    /// `-`, meaning standard input. What makes `bigctl` compose with a shell rather than replace
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
    Query {
        table: String,
        text: Source,
    },
    Records {
        table: String,
        after: Option<u64>,
        limit: Option<u64>,
    },
    /// `import` and `delete`, which are the one pair that is a **file** rather than a request.
    ///
    /// Every other command here is exactly one route. These two are that same route sent again
    /// with the next slice of the same file, which is why they carry [`Load`] and nothing else
    /// does. A file under `--chunk-bytes` is one slice, so the common case is still one
    /// request. There is no second, smaller code path for it: a path that only fires on inputs
    /// too small for anyone to notice is a path that breaks quietly.
    Load {
        verb: Verb,
        table: String,
        input: Input,
        load: Load,
    },
    Schema,
    CreateTable {
        table: String,
        params: Vec<(String, String)>,
    },
    CreateField {
        table: String,
        field: String,
        params: Vec<(String, String)>,
    },
    DropTable {
        table: String,
    },
    DropField {
        table: String,
        field: String,
    },
    Verify,
    Repair,
    /// `cluster topology` - what the cluster looks like right now.
    ClusterTopology,
    /// `cluster split <shard> [to <node>]` - cut a range in two.
    ClusterSplit {
        at: u64,
        to: Option<String>,
    },
    /// `cluster merge <range>` - join a range to the one after it.
    ClusterMerge {
        range: u64,
    },
    /// `cluster add-node <name> <addr>` - a node joins, as a learner.
    ClusterAddNode {
        name: String,
        addr: String,
    },
    /// `cluster admit|drain|remove <name>` - the three one-node changes.
    ClusterMember {
        verb: &'static str,
        name: String,
    },
    Health,
    Ready,
    Metrics,
    Shell,
}

/// Everything a run needs, resolved.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Options {
    pub addr: String,
    /// A file holding one `user:password` line, mode 600.
    pub credentials_file: Option<String>,
    /// A username on its own, for the interactive path: the password is then asked for on the
    /// terminal. A username is not a secret and is logged by the server anyway, so unlike the
    /// password it is allowed to be a flag.
    pub user: Option<String>,
    /// The CA a server's certificate must chain to, when the address is `https://`.
    pub ca_file: Option<String>,
    /// Connect without checking the server's certificate at all. Announced on every run.
    pub insecure_skip_verify: bool,
    /// `None` means "decide from where the output is going" - see [`crate::client::render`].
    pub format: Option<Format>,
    pub timeout: Option<Duration>,
    pub command: Command,
}

/// `BIG_ADDR` when set, this otherwise. The same default `big serve` binds to.
pub const DEFAULT_ADDR: &str = "127.0.0.1:7654";

pub const USAGE: &str = "\
usage: bigctl [options] <command> [args]

Asks a running `big serve`. Every command below is exactly one of its routes; there is no offline
mode and no --file, because a second query path is a path nobody tests. Start a daemon.

Queries:
  sql <statement>|-           one SELECT over one table, a write, or a schema change:
                                INSERT INTO t (a, n) VALUES ('GB', 5)
                                CREATE TABLE IF NOT EXISTS t (a TEXT, n INT)
                                ALTER TABLE t ADD COLUMN b BIGINT, DROP COLUMN a
                                DROP TABLE IF EXISTS t
                                DESCRIBE t | SHOW TABLES | SHOW CREATE TABLE t
                              An INSERT needs a write token; a schema change an
                              admin one. Volume goes through `import`, not here.
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
  import <table> <file>|-     one fact per line: `field record value`
  delete <table> <file>|-     one record id per line
      --chunk-bytes <n>         bytes per request; default 7340032, ceiling 8388608
      --chunk-lines <n>         lines per request; default 1000000
      --resume <file>           write the acknowledged offset here, and start from it
      --retries <n>             retry a dropped connection this many times; default 3
      --in-flight <n>           requests waiting on the server at once; default 2, ceiling 8
                                the second lets the server parse one body while it commits
                                the one before, which is worth about 1.6x. It does not make
                                writes concurrent - the engine has one writer - so a third
                                buys little. Drop to 1 to bound what a resumed load repeats
      --progress|--no-progress  default: progress when stderr is a terminal
      --dry-run                 chunk the input and report, without sending anything

Operations:
  verify                      do the copies of every range still agree
  repair                      catch up every copy that is behind
  health | ready | metrics    the three probes

Options:
  --addr <host:port>          default 127.0.0.1:7654, or $BIG_ADDR
                              `https://host:port` speaks TLS; a bare host:port does not
  --credentials-file <file>   one `user:password` line, mode 600; or $BIG_CREDENTIALS
  --user <name>               ask for the password on the terminal
  --ca-file <file>            the CA a server's certificate must chain to
  --insecure-skip-verify      do not check the certificate at all. Says so on every run
  --format table|tsv|json     default: table to a terminal, tsv to a pipe
  --timeout <seconds>         give up on the exchange; default is to wait
  -h, --help

A password is read from a file or from the terminal and never taken as a flag: an argument is
visible in `ps` and in shell history, and a password in either has already leaked. A username
is not a secret and may be a flag.

TLS is chosen by the scheme, not guessed. A bare `host:port` is plaintext, exactly as it has
always been, and `https://host:port` is not - because a default that guessed from whether the
host looked like loopback would be the kind of cleverness that fails in the one deployment
nobody tested.

`bigctl shell` has no line editing on purpose. `rlwrap bigctl shell` gives it history and arrow
keys, and does it better than a hand-rolled termios mode would.

`import` and `delete` read a FILE, or `-` for standard input. They are the one pair that is not
one request: the server bounds a body at 8 MiB, so a larger file is cut into chunks and sent as
several. A file under --chunk-bytes is one chunk and therefore one request, so the small case
costs nothing. (`bigc` took the facts themselves on the command line and could not read a path.
Write them to a file, or pipe them with `-`.)

A load can be resumed and can be run twice. Every fact is a bit set at a record id written in
the line, so sending a chunk twice writes what sending it once wrote - which is what --resume
rests on, and what lets a dropped connection be retried at all. --resume needs a seekable file,
so it does not go with `-`. A retry covers a dropped connection, never a refusal.

Exit codes: 0 answered, 1 the server refused, 2 usage, 3 nothing was listening.
";

/// Parses `argv`. `Err("")` means `--help` was asked for, which is not a failure.
pub fn parse(args: &[String], env: &dyn Fn(&str) -> Option<String>) -> Result<Options, String> {
    let mut addr = env("BIG_ADDR").unwrap_or_else(|| DEFAULT_ADDR.to_string());
    let mut credentials_file = env("BIG_CREDENTIALS");
    let mut user = None;
    let mut ca_file = env("BIG_CA");
    let mut insecure_skip_verify = false;

    // **Refused, not ignored.** The worst outcome here is a script that keeps working against a
    // loopback development server and silently stops authenticating in production, which is
    // exactly what silently dropping a now-meaningless variable would produce.
    if env("BIG_TOKEN").is_some() && credentials_file.is_none() {
        return Err("BIG_TOKEN is no longer used: bearer tokens were replaced by usernames and \
                    passwords. Set BIG_CREDENTIALS to a file holding one `user:password` line, \
                    readable only by you."
            .to_string());
    }
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
            "--credentials-file" => {
                credentials_file = Some(value()?);
                i += 2;
            }
            "--user" => {
                user = Some(value()?);
                i += 2;
            }
            "--ca-file" => {
                ca_file = Some(value()?);
                i += 2;
            }
            "--insecure-skip-verify" => {
                insecure_skip_verify = true;
                i += 1;
            }
            // Recognised for one release so that it can say what happened, rather than falling
            // through to "unknown option" and sending somebody to check their spelling.
            "--token-file" => {
                return Err("--token-file is gone: bearer tokens were replaced by usernames and \
                            passwords. Use --credentials-file with a file holding one \
                            `user:password` line."
                    .to_string())
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
                // to spell a client that never works. Same reading `big serve` gives its own.
                timeout = (secs > 0).then(|| Duration::from_secs(secs));
                i += 2;
            }
            // The boolean ones go into the same list as the rest, carrying an empty value, so
            // that `only()` polices them too. A flag that is silently dropped where it means
            // nothing is the failure `only()` exists to prevent, and a `--dry-run` that did
            // nothing on `schema` would be exactly that one level up.
            "--progress" | "--no-progress" | "--dry-run" => {
                scoped.push((arg.trim_start_matches("--").replace('-', "_"), String::new()));
                i += 1;
            }
            "--after" | "--limit" | "--kind" | "--bit-depth" | "--scale" | "--granularity"
            | "--engine" | "--chunk-bytes" | "--chunk-lines" | "--resume" | "--retries"
            | "--in-flight" => {
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
    Ok(Options {
        addr,
        credentials_file,
        user,
        ca_file,
        insecure_skip_verify,
        format,
        timeout,
        command,
    })
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

    /// The knobs `import` and `delete` accept, and no other command does.
    fn load(scoped: &[(String, String)]) -> Result<Load, String> {
        let has = |name: &str| scoped.iter().any(|(k, _)| k == name);
        let text = |name: &str| scoped.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone());

        // `--progress` and `--no-progress` are two keys rather than one carrying `false`, so
        // that `only()` can name the flag the caller actually typed.
        let progress = match (has("progress"), has("no_progress")) {
            (true, true) => return Err("--progress and --no-progress contradict".to_string()),
            (true, false) => Some(true),
            (false, true) => Some(false),
            (false, false) => None,
        };

        let mut out =
            Load { progress, dry_run: has("dry_run"), resume: text("resume"), ..Load::default() };

        if let Some(n) = num(scoped, "chunk_bytes")? {
            // The ceiling is the server's, and a chunk above it is refused for the whole
            // chunk rather than trimmed - so it is caught here, where the flag has a name.
            if n == 0 || n as usize > big_http::MAX_BODY {
                return Err(format!(
                    "--chunk-bytes must be between 1 and {}, got {n}",
                    big_http::MAX_BODY
                ));
            }
            out.chunk_bytes = n as usize;
        }
        if let Some(n) = num(scoped, "chunk_lines")? {
            if n == 0 {
                return Err("--chunk-lines cannot be zero".to_string());
            }
            out.chunk_lines = n as usize;
        }
        if let Some(n) = num(scoped, "retries")? {
            out.retries = n as u32;
        }
        if let Some(n) = num(scoped, "in_flight")? {
            if n == 0 || n as usize > MAX_IN_FLIGHT {
                return Err(format!("--in-flight must be between 1 and {MAX_IN_FLIGHT}, got {n}"));
            }
            out.in_flight = n as usize;
        }
        Ok(out)
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
        [verb @ ("import" | "delete"), table, file] => {
            only(&scoped, verb, &LOAD)?;
            let verb = if *verb == "import" { Verb::Import } else { Verb::Delete };
            let input = if *file == "-" { Input::Stdin } else { Input::Path((*file).to_string()) };
            Command::Load { verb, table: (*table).to_string(), input, load: load(&scoped)? }
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

        // **Three verbs, and the shape of the cluster is all of them.** An operator types
        // these; an autoscaler and a Kubernetes controller reach the same routes directly.
        ["cluster", "topology"] => {
            only(&scoped, "cluster topology", &[])?;
            Command::ClusterTopology
        }
        ["cluster", "split", at] => {
            only(&scoped, "cluster split", &[])?;
            let at = at
                .parse()
                .map_err(|_| format!("`{at}` is not a shard number; write `cluster split 900`"))?;
            Command::ClusterSplit { at, to: None }
        }
        ["cluster", "split", at, "to", node] => {
            only(&scoped, "cluster split", &[])?;
            let at = at
                .parse()
                .map_err(|_| format!("`{at}` is not a shard number; write `cluster split 900`"))?;
            Command::ClusterSplit { at, to: Some((*node).to_string()) }
        }
        ["cluster", "add-node", name, addr] => {
            only(&scoped, "cluster add-node", &[])?;
            Command::ClusterAddNode { name: (*name).to_string(), addr: (*addr).to_string() }
        }
        ["cluster", "admit", name] => {
            only(&scoped, "cluster admit", &[])?;
            Command::ClusterMember { verb: "admit", name: (*name).to_string() }
        }
        ["cluster", "drain", name] => {
            only(&scoped, "cluster drain", &[])?;
            Command::ClusterMember { verb: "drain", name: (*name).to_string() }
        }
        ["cluster", "remove", name] => {
            only(&scoped, "cluster remove", &[])?;
            Command::ClusterMember { verb: "remove", name: (*name).to_string() }
        }
        ["cluster", "merge", range] => {
            only(&scoped, "cluster merge", &[])?;
            let range = range
                .parse()
                .map_err(|_| format!("`{range}` is not a range id; write `cluster merge 2`"))?;
            Command::ClusterMerge { range }
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

/// The flags a load accepts. Every other subcommand's allow-list is unchanged, which now means
/// `bigctl schema --dry-run` and `bigctl sql "..." --resume f` are refused by name - coverage
/// the two separate binaries could not have had, because neither knew the other's flags.
const LOAD: [&str; 8] = [
    "chunk_bytes",
    "chunk_lines",
    "resume",
    "retries",
    "in_flight",
    "progress",
    "no_progress",
    "dry_run",
];

/// Every first word this client answers to, for telling a typo from a misuse.
const KNOWN: [&str; 16] = [
    "sql", "query", "records", "import", "delete", "schema", "create", "drop", "verify", "repair",
    "health", "ready", "metrics", "shell", "help", "cluster",
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
