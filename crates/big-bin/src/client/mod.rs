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

//! `bigctl` - one command, one route, no vocabulary of its own.
//!
//! **This module links the engine and must never use it.** It once could not: the client was
//! its own package with an empty `[dependencies]` section, so "cannot grow an offline query
//! path" was a fact about the dependency graph. One package for both binaries gave that up -
//! see the note in `Cargo.toml`. What is left is this paragraph. Do not import `big_sql`,
//! `big_plan` or `big_embed` here. A client that validated a statement before sending it would
//! be a second surface drifting from the first, and the second one always loses.
//!
//! So a statement travels as bytes and an error comes back as the server's own code and the
//! server's own sentence. `sql_no_joins` means on the command line exactly what it means over
//! HTTP, because it *is* the same string.
//!
//! [`run`] is the whole program, taking its streams as arguments so that the tests can drive it
//! against a real `big serve` on a loopback port and read what a user would have seen.

pub mod args;
pub mod http;
pub mod json;
pub mod render;
pub mod shell;

pub use args::{Command, Format, Options, Source};
pub use http::{Client, Error as HttpError};
pub use json::{Answer, Failure};

use crate::{exit, Io};
use std::io::BufRead;

/// Parses, sends, prints. Returns the process's exit code.
pub fn run(args: &[String], io: &mut Io<'_>, env: &dyn Fn(&str) -> Option<String>) -> i32 {
    let options = match args::parse(args, env) {
        Ok(o) => o,
        // `--help` is not a failure: usage to stdout, exit zero. The same split `big serve` makes.
        Err(e) if e.is_empty() => {
            let _ = write!(io.out, "{}", args::USAGE);
            return exit::OK;
        }
        Err(e) => {
            let _ = writeln!(io.err, "bigctl: {e}\n");
            let _ = write!(io.err, "{}", args::USAGE);
            return exit::USAGE;
        }
    };

    let credentials = match resolve_credentials(&options) {
        Ok(c) => c,
        Err(e) => {
            let _ = writeln!(io.err, "bigctl: {e}");
            return exit::USAGE;
        }
    };
    // The scheme decides the transport, and the transport is built before anything is sent so
    // that a missing CA file is a usage error rather than a failed connection.
    let (addr, tls) = match http::transport(&options) {
        Ok(t) => t,
        Err(e) => {
            let _ = writeln!(io.err, "bigctl: {e}");
            return exit::USAGE;
        }
    };
    if options.insecure_skip_verify && tls.is_some() {
        // Every run, not once. An operator who turned this on during bring-up and forgot is the
        // person this line is for.
        let _ = writeln!(
            io.err,
            "bigctl: certificate verification is off; anyone on the path can read this"
        );
    }
    let client = Client { addr, credentials, tls, timeout: options.timeout };
    let format = options.format.unwrap_or_else(|| render::default_for(io.out_tty));

    if options.command == Command::Shell {
        return shell::run(&client, io.input, io.out, io.err, format, io.out_tty);
    }

    // A load is the one command that is a file rather than a request, so it has its own loop.
    // Everything above this line - the token, the address, the format - was resolved once and
    // means the same thing to both halves, which is the whole point of one parser.
    if let Command::Load { verb, table, input, load } = &options.command {
        return crate::ingest::run(&client, *verb, table, input, load, io, format);
    }

    // The other command that is not one request. Adding a node and starting it are two halves
    // that have to happen in that order, on two machines, and getting them the wrong way round
    // is the mistake this exists to remove - so one verb does the half that belongs here and
    // prints the half that does not.
    if let Command::ClusterJoin { name, addr } = &options.command {
        return cluster_join(&client, &options, name, addr, io);
    }

    let request = match request(&options.command, io.input) {
        Ok(r) => r,
        Err(e) => {
            let _ = writeln!(io.err, "bigctl: {e}");
            return exit::USAGE;
        }
    };

    let response = match client.send(request.method, &request.target, &request.body) {
        Ok(r) => r,
        Err(e) => {
            let _ = writeln!(io.err, "bigctl: {e}");
            return exit::UNREACHABLE;
        }
    };

    if !response.ok() {
        let failure = Failure::read(&response.body);
        let _ = writeln!(io.err, "bigctl: {} [{}]", failure.message, failure.code);
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
                let _ = writeln!(io.err, "bigctl: {note}");
            }
            exit::OK
        }
        Err(why) => {
            // The body is printed with the complaint rather than swallowed: the user asked a
            // question and the server answered it, and a client that cannot lay the answer out
            // should still hand it over.
            let _ = writeln!(io.err, "bigctl: could not read the answer: {why}");
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
        Command::ClusterTopology => {
            Request::new("GET", "/cluster/topology".to_string(), String::new())
        }
        Command::ClusterSplit { at, to } => {
            let path = match to {
                Some(node) => format!("/admin/cluster/split?at={at}&to={node}"),
                None => format!("/admin/cluster/split?at={at}"),
            };
            Request::new("POST", path, String::new())
        }
        Command::ClusterMerge { range } => {
            Request::new("POST", format!("/admin/cluster/merge?range={range}"), String::new())
        }
        // Forced, because a person typing this has asked for it - the policy decides whether
        // the cluster balances *itself*, not whether an operator may.
        Command::ClusterRebalance => {
            Request::new("POST", "/admin/cluster/rebalance?force=true".to_string(), String::new())
        }
        Command::ClusterSchemaLeader { to } => {
            Request::new("POST", format!("/admin/cluster/schema-leader?to={to}"), String::new())
        }
        Command::ClusterMove { range, to } => Request::new(
            "POST",
            format!("/admin/cluster/move?range={range}&to={to}"),
            String::new(),
        ),
        Command::ClusterReplica { add: true, range, node } => Request::new(
            "POST",
            format!("/admin/cluster/replica?range={range}&to={node}"),
            String::new(),
        ),
        Command::ClusterReplica { add: false, range, node } => Request::new(
            "DELETE",
            format!("/admin/cluster/replica?range={range}&from={node}"),
            String::new(),
        ),
        Command::ClusterCancel { range } => {
            Request::new("POST", format!("/admin/cluster/cancel?range={range}"), String::new())
        }
        // Handled before this point: it is two requests, not one. Named here so that adding a
        // command cannot forget to route it.
        Command::ClusterJoin { .. } => {
            return Err("cluster join is handled before a request is built".to_string())
        }
        Command::ClusterAddNode { name, addr } => Request::new(
            "POST",
            format!("/admin/cluster/node?name={name}&addr={addr}"),
            String::new(),
        ),
        // `remove` is the only one that takes a node away for good, so it is the only one
        // written as a deletion.
        Command::ClusterMember { verb: "remove", name } => {
            Request::new("DELETE", format!("/admin/cluster/node?name={name}"), String::new())
        }
        Command::ClusterMember { verb, name } => {
            Request::new("POST", format!("/admin/cluster/{verb}?name={name}"), String::new())
        }
        Command::Health => Request::new("GET", "/health".to_string(), String::new()),
        Command::Ready => Request::new("GET", "/ready".to_string(), String::new()),
        Command::Metrics => Request {
            method: "GET",
            target: "/metrics".to_string(),
            body: String::new(),
            text: true,
        },

        Command::Shell | Command::Load { .. } => {
            unreachable!("both are handled before the request is built")
        }
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

/// `cluster join`: admit the node here, then say what to run over there.
///
/// **Nothing is started remotely, and that is the point rather than a limitation.** This tool
/// talks to one daemon over one socket; a verb that reached a second machine would need a way
/// in to it, which is a much larger thing to own than the two lines it would save. What it can
/// do is make the order impossible to get wrong, and name the flags from what it can see.
fn cluster_join(
    client: &Client,
    options: &args::Options,
    name: &str,
    addr: &str,
    io: &mut Io<'_>,
) -> i32 {
    let added =
        match client.send("POST", &format!("/admin/cluster/node?name={name}&addr={addr}"), "") {
            Ok(r) => r,
            Err(e) => {
                let _ = writeln!(io.err, "bigctl: {e}");
                return exit::UNREACHABLE;
            }
        };
    if !added.ok() {
        let failure = Failure::read(&added.body);
        let _ = writeln!(io.err, "bigctl: {} [{}]", failure.message, failure.code);
        return exit::REFUSED;
    }

    // The cluster's name is the one flag the operator cannot work out from what they typed, so
    // it is read rather than asked for. A cluster that has none cannot be joined at all - the
    // daemon refuses the request with `cluster_unnamed` - so it is worth saying here, where
    // there is still something to do about it, instead of on the far machine.
    let id = match client.send("GET", "/cluster/topology", "") {
        Ok(r) if r.ok() => json::parse(&r.body)
            .ok()
            .and_then(|v| v.get("cluster_id").and_then(json::Value::cell))
            .filter(|s| !s.is_empty()),
        _ => None,
    };
    let Some(id) = id else {
        let _ = writeln!(
            io.err,
            "bigctl: `{name}` was added, but this cluster has no `cluster_id` - so it cannot be \
             joined by address. Set the same `cluster_id` in every node's cluster file, restart \
             them, and run this again"
        );
        return exit::REFUSED;
    };

    let _ = writeln!(io.out, "added `{name}` at {addr}, as a learner. Now run this on {addr}:");
    let _ = writeln!(io.out);
    let _ = writeln!(
        io.out,
        "  big serve <file> {addr} --join {} --cluster-id {id} --node {name}",
        // The address this tool is talking to: a node already in the cluster, which is exactly
        // what the joining one has to dial.
        options.addr.trim_start_matches("https://").trim_start_matches("http://")
    );
    let _ = writeln!(io.out);
    let _ = writeln!(
        io.out,
        "Add --peer-ca, --peer-cert and --peer-key where the nodes speak TLS to each other. It \
         is admitted once it has caught up, and holds a range once it is given one."
    );
    exit::OK
}

/// The credential this run will present, from a file or from the terminal.
///
/// Three ways, in the order somebody reaches for them: a file, a username with the password
/// asked for interactively, or nothing at all - which is what a loopback server with no users
/// file wants and is still a perfectly ordinary way to run this.
fn resolve_credentials(options: &args::Options) -> Result<Option<http::Credentials>, String> {
    if let Some(path) = &options.credentials_file {
        let (user, password) = read_credentials(path)?;
        return Ok(Some(http::Credentials { user, password }));
    }
    if let Some(user) = &options.user {
        let password = crate::tty::read_password(&format!("password for {user}: "))
            .map_err(|e| format!("could not read a password: {e}"))?;
        return Ok(Some(http::Credentials { user: user.clone(), password }));
    }
    Ok(None)
}

/// Reads `user:password` from a file, refusing one anybody can read.
///
/// The mode check is `big_tls::mode`'s, **copied rather than depended on**. The rule this crate
/// is protecting is that the client stays something somebody outside this repository could have
/// written, and reaching into the server's crates to read one file would undo it. Two
/// implementations of a three-line check is the cheaper of the two prices.
///
/// Split on the first colon, the same way the server splits a `Basic` header, so that a file and
/// a header cannot disagree about where a password starts.
pub fn read_credentials(path: &str) -> Result<(String, String), String> {
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
                "{path} is mode {mode:o}; a credentials file must not be readable by anyone \
                 else (chmod 600 {path})"
            ));
        }
    }
    let text = std::fs::read_to_string(path).map_err(|e| format!("could not read {path}: {e}"))?;
    for line in text.lines() {
        // A comment is stripped and the first non-empty line is the credential - the same
        // reading the server gives its own files, so that one habit covers both.
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let Some((user, password)) = line.split_once(':') else {
            return Err(format!("{path}: expected one `user:password` line"));
        };
        if user.is_empty() {
            return Err(format!("{path}: the username is empty"));
        }
        return Ok((user.to_string(), password.to_string()));
    }
    Err(format!("{path}: no credential in this file"))
}
