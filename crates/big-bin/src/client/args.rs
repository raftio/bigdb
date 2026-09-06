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
//! **This was hand-rolled, and the reason it no longer is worth writing down.** The argument
//! for a hand-rolled parser was that a dozen options do not justify a dependency, and that was
//! true. What broke it was not the option count but a bug class the parser grew for itself:
//! flags were collected into one flat list and applied wherever they fitted, so
//! `records t --engine columnar` was accepted and then silently dropped. The fix was `only()`,
//! a hand-written table of which flags belong to which subcommand, checked by hand, tested by
//! hand, and one edit away from disagreeing with the parser above it. A parser that has to
//! police its own design is not cheaper than the crate. clap gives that check by construction:
//! a flag declared on `import` does not exist on `schema`, so there is nothing to enforce.
//!
//! **Every command here is one request, except [`Command::Load`], which is one file.** There is
//! no subcommand that pages or composes two routes, and that is not laziness - a client that
//! could answer something the server cannot has become a second engine with a worse test suite.
//! Adding one means adding a route first.
//!
//! Two things this module deliberately does *not* let clap do:
//!
//! - **It does not read the environment.** [`crate::client::run`] takes an injected `env`
//!   closure so its tests are hermetic, so `BIG_ADDR`, `BIG_CREDENTIALS` and `BIG_CA` are
//!   applied in [`Cli::resolve`] from that closure rather than by `#[arg(env = ...)]`, which
//!   would reach the real process environment and make the tests depend on the shell that ran
//!   them.
//! - **It does not exit.** `run` returns an exit code so that `bigctl` can flush its streams
//!   before the process ends, so parse failures come back as a [`clap::Error`] to be rendered
//!   into the caller's streams rather than clap's.

use crate::ingest::args::{
    Input, Load, Verb, DEFAULT_CHUNK_BYTES, DEFAULT_CHUNK_LINES, DEFAULT_IN_FLIGHT,
    DEFAULT_RETRIES, MAX_IN_FLIGHT,
};
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
#[derive(Clone, Copy, PartialEq, Eq, Debug, clap::ValueEnum)]
pub enum Format {
    /// Aligned columns, for a person.
    Table,
    /// Tab-separated, for a pipe.
    Tsv,
    /// The server's body, verbatim.
    Json,
}

/// One subcommand, which is one public route.
///
/// This is the parser's *output* and is deliberately not the clap type: the command line has a
/// `create table` / `create field` shape that reads well and a flat enum that sends well, and
/// keeping them separate is what lets either move without the other.
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
    /// `cluster join <name> <addr>` - `add-node`, then the command to run on the new machine.
    ClusterJoin {
        name: String,
        addr: String,
    },
    /// `cluster add-replica <range> to <node>` / `drop-replica <range> from <node>`.
    ClusterReplica {
        add: bool,
        range: u64,
        node: String,
    },
    /// `cluster admit|drain|remove <name>` - the three one-node changes.
    ClusterMember {
        verb: &'static str,
        name: String,
    },
    /// `cluster move <range> to <node>` - hand a populated range over.
    ClusterMove {
        range: u64,
        to: String,
    },
    /// `cluster cancel <range>` - abandon a move in flight.
    ClusterCancel {
        range: u64,
    },
    /// `cluster rebalance` - take one balancing step, if the facts call for one.
    ClusterRebalance,
    /// `cluster schema-leader <node>` - hand the row-key namespace over.
    ClusterSchemaLeader {
        to: String,
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

const ABOUT: &str = "Ask a running `big serve`";

const LONG_ABOUT: &str = "\
Asks a running `big serve`. Every command below is exactly one of its routes; there is no offline
mode and no --file, because a second query path is a path nobody tests. Start a daemon.

Exit codes: 0 answered, 1 the server refused, 2 usage, 3 nothing was listening.";

// ---------------------------------------------------------------------------------------------
// The command line itself.
// ---------------------------------------------------------------------------------------------

#[derive(clap::Parser, Debug)]
#[command(
    name = "bigctl",
    version,
    about = ABOUT,
    long_about = LONG_ABOUT,
    disable_help_flag = false,
    // A misspelling should cost one line, not a re-read of the whole help.
    infer_subcommands = false,
    // `wrap_help` is off - it is a crate, `terminal_size`, rather than code - so the prose below
    // is wrapped by hand and must reach `--help` the way it is written here.
    verbatim_doc_comment
)]
pub struct Cli {
    /// Where the daemon is; `https://host:port` speaks TLS.
    ///
    /// Defaults to $BIG_ADDR, or 127.0.0.1:7654 when that is unset. A bare `host:port` does not
    /// speak TLS - the scheme is how this client is told, because a client that guessed would
    /// eventually guess "plaintext" against a server that wanted otherwise.
    #[arg(long, global = true, value_name = "HOST:PORT", verbatim_doc_comment)]
    addr: Option<String>,

    /// A file holding one `user:password` line, mode 600; or $BIG_CREDENTIALS.
    #[arg(long, global = true, value_name = "FILE")]
    credentials_file: Option<String>,

    /// A username; the password is then asked for on the terminal.
    ///
    /// A username is not a secret and the server logs it anyway, so unlike a password it is
    /// allowed to be a flag. There is deliberately no `--password`.
    #[arg(long, global = true, value_name = "NAME", verbatim_doc_comment)]
    user: Option<String>,

    /// The CA the server's certificate must chain to; or $BIG_CA.
    #[arg(long, global = true, value_name = "FILE")]
    ca_file: Option<String>,

    /// Connect without checking the server's certificate at all.
    ///
    /// Announced on stderr on every run, because a flag that turns off the thing TLS is for
    /// should not be quiet about it.
    #[arg(long, global = true, verbatim_doc_comment)]
    insecure_skip_verify: bool,

    /// How to print an answer. Default: table on a terminal, tsv into a pipe.
    #[arg(long, global = true, value_enum, value_name = "FORMAT")]
    format: Option<Format>,

    /// Give up after this many seconds. `0` means wait.
    ///
    /// Zero means "wait", not "give up immediately", which would be a confusing way to spell a
    /// client that never works. The same reading `big serve` gives its own.
    #[arg(long, global = true, value_name = "SECONDS", verbatim_doc_comment)]
    timeout: Option<u64>,

    /// Gone: bearer tokens were replaced by usernames and passwords.
    ///
    /// Recognised for one release so that it can say what happened, rather than falling through
    /// to "unknown option" and sending somebody to check their spelling.
    #[arg(long, global = true, hide = true, num_args = 0..=1, value_name = "FILE", verbatim_doc_comment)]
    token_file: Option<Option<String>>,

    #[command(subcommand)]
    command: Cmd,
}

#[derive(clap::Subcommand, Debug)]
enum Cmd {
    /// One SELECT over one table, a write, or a schema change. `-` reads stdin.
    #[command(long_about = "\
One SELECT over one table, a write, or a schema change:

  INSERT INTO t (a, n) VALUES ('GB', 5)
  CREATE TABLE IF NOT EXISTS t (a TEXT, n INT)
  ALTER TABLE t ADD COLUMN b BIGINT, DROP COLUMN a
  DROP TABLE IF EXISTS t
  DESCRIBE t | SHOW TABLES | SHOW CREATE TABLE t

An INSERT needs a write token; a schema change an admin one. Volume goes through `import`, not
here.")]
    Sql {
        /// The statement, or `-` to read it from stdin.
        #[arg(value_name = "STATEMENT")]
        statement: String,
    },

    /// One PQL call. `-` reads stdin.
    Query {
        table: String,
        /// The call, or `-` to read it from stdin.
        #[arg(value_name = "CALL")]
        call: String,
    },

    /// Every record id, in order.
    Records {
        table: String,
        /// Resume after this id.
        #[arg(long, value_name = "ID")]
        after: Option<u64>,
        /// How many to return.
        #[arg(long, value_name = "N")]
        limit: Option<u64>,
    },

    /// Load facts from a file: one `field record value` per line. `-` reads stdin.
    Import {
        table: String,
        /// The file, or `-` for standard input.
        #[arg(value_name = "FILE")]
        file: String,
        #[command(flatten)]
        load: LoadArgs,
    },

    /// Delete records listed in a file: one record id per line. `-` reads stdin.
    Delete {
        table: String,
        /// The file, or `-` for standard input.
        #[arg(value_name = "FILE")]
        file: String,
        #[command(flatten)]
        load: LoadArgs,
    },

    /// Every table and field.
    Schema,

    /// Add a table or a field.
    #[command(subcommand)]
    Create(CreateCmd),

    /// Remove a table or a field.
    #[command(subcommand)]
    Drop(DropCmd),

    /// Do the copies of every range still agree.
    Verify,

    /// Catch up every copy that is behind.
    Repair,

    /// Who is in the cluster, and the changes that reshape it.
    #[command(subcommand)]
    Cluster(ClusterCmd),

    /// Is the process alive.
    Health,

    /// Is it ready to serve.
    Ready,

    /// The metrics endpoint, passed through verbatim.
    Metrics,

    /// A loop over sql and query.
    Shell,
}

#[derive(clap::Subcommand, Debug)]
#[command(verbatim_doc_comment)]
enum CreateCmd {
    /// Add a table.
    Table {
        table: String,
        /// bitmap | bitmap+columnar | columnar. Default bitmap+columnar.
        ///
        /// Passed through to the server rather than checked here: an engine name this client
        /// has never heard of is a matter between the caller and the server.
        #[arg(long, value_name = "ENGINE", verbatim_doc_comment)]
        engine: Option<String>,
    },
    /// Add a field to a table.
    Field {
        table: String,
        field: String,
        /// int | signed | decimal | set | mutex | bool | timequantum.
        #[arg(long, required = true, value_name = "KIND")]
        kind: String,
        /// For int and signed.
        #[arg(long, value_name = "N")]
        bit_depth: Option<String>,
        /// For decimal.
        #[arg(long, value_name = "N")]
        scale: Option<String>,
        /// For timequantum.
        #[arg(long, value_name = "CHARS")]
        granularity: Option<String>,
    },
}

#[derive(clap::Subcommand, Debug)]
enum DropCmd {
    /// Remove a table.
    Table { table: String },
    /// Remove a field from a table.
    Field { table: String, field: String },
}

#[derive(clap::Subcommand, Debug)]
#[command(verbatim_doc_comment)]
enum ClusterCmd {
    /// Who is in it, what each holds, and who leads.
    Topology,

    /// Cut a range in two, optionally handing the upper half to a node.
    Split {
        /// The shard to cut at.
        #[arg(value_name = "SHARD")]
        at: u64,
        /// The word `to`, when a destination follows.
        #[arg(value_parser = ["to"], value_name = "to", requires = "node")]
        to: Option<String>,
        /// The node that takes the upper half.
        node: Option<String>,
    },

    /// Join a range to the one after it.
    Merge {
        /// The range to fold into the one after it.
        #[arg(value_name = "RANGE")]
        range: u64,
    },

    #[command(verbatim_doc_comment)]
    /// Add a node, as a learner.
    ///
    /// The first half of a join on its own, for when the second half is scripted elsewhere.
    AddNode {
        name: String,
        #[arg(id = "node_addr", value_name = "ADDR")]
        addr: String,
    },

    #[command(verbatim_doc_comment)]
    /// Add a node and print the command that starts it.
    ///
    /// Two steps on two machines, in an order that only works one way round, so this does the
    /// half that belongs here.
    Join {
        name: String,
        #[arg(id = "node_addr", value_name = "ADDR")]
        addr: String,
    },

    #[command(verbatim_doc_comment)]
    /// One more copy of a range that is already serving.
    ///
    /// It refuses nothing while it copies: the copy joins the group marked behind, so writes
    /// reach it at once and no read does until it agrees.
    AddReplica {
        /// The range that gains a copy.
        #[arg(value_name = "RANGE")]
        range: u64,
        /// The word `to`, so the command reads as a sentence.
        #[arg(value_parser = ["to"], value_name = "to")]
        _to: String,
        /// The node that gains the copy.
        node: String,
    },

    /// One copy fewer. The map stops naming it; nothing is deleted.
    DropReplica {
        /// The range that loses a copy.
        #[arg(value_name = "RANGE")]
        range: u64,
        /// The word `from`, so the command reads as a sentence.
        #[arg(value_parser = ["from"], value_name = "from")]
        _from: String,
        /// The node that loses the copy.
        node: String,
    },

    /// Let a learner start serving.
    Admit { name: String },

    /// Move a node's work elsewhere, leaving it in the cluster.
    Drain { name: String },

    /// Take a node out of the cluster.
    Remove { name: String },

    /// Hand a populated range over to another node.
    Move {
        /// The range to hand over.
        #[arg(value_name = "RANGE")]
        range: u64,
        /// The word `to`, so the command reads as a sentence.
        #[arg(value_parser = ["to"], value_name = "to")]
        _to: String,
        /// The node that takes it.
        node: String,
    },

    /// Abandon a move in flight.
    Cancel {
        /// The range whose move should be abandoned.
        #[arg(value_name = "RANGE")]
        range: u64,
    },

    /// Take one balancing step, against facts gathered afresh.
    Rebalance,

    /// Hand the row-key namespace over.
    SchemaLeader { node: String },
}

/// The knobs `import` and `delete` accept, and no other command does.
///
/// This is the whole of what `only()` used to enforce by hand. Declaring them here means
/// `bigctl schema --dry-run` is refused because `--dry-run` does not exist on `schema`, not
/// because a table said it should not.
#[derive(clap::Args, Debug)]
#[command(verbatim_doc_comment)]
struct LoadArgs {
    /// Bytes per request. Ceiling 8388608, the server's body limit.
    #[arg(long, value_name = "N", default_value_t = DEFAULT_CHUNK_BYTES, value_parser = chunk_bytes)]
    chunk_bytes: usize,

    /// Lines per request.
    #[arg(long, value_name = "N", default_value_t = DEFAULT_CHUNK_LINES, value_parser = chunk_lines)]
    chunk_lines: usize,

    /// Write the acknowledged offset here, and start from it.
    ///
    /// Needs a seekable file, so it does not go with `-`. A load can be run twice: every fact
    /// is a bit set at a record id written in the line, so sending a chunk twice writes what
    /// sending it once wrote - which is what this rests on, and what lets a dropped connection
    /// be retried at all.
    #[arg(long, value_name = "FILE", verbatim_doc_comment)]
    resume: Option<String>,

    /// Retry a dropped connection this many times. Never a refusal.
    #[arg(long, value_name = "N", default_value_t = DEFAULT_RETRIES)]
    retries: u32,

    /// Requests waiting on the server at once. Ceiling 8.
    #[arg(long, value_name = "N", default_value_t = DEFAULT_IN_FLIGHT, value_parser = in_flight,
          long_help = "\
Requests waiting on the server at once; default 2, ceiling 8.

With a single request outstanding the load is strictly alternating: the client waits while the
server parses and commits, then the server waits while the client reads and sends. A second
request lets the server parse one body while it commits the one before, which is the only
overlap available - the engine has one writer, so this does not make writes concurrent. It is
worth about 1.6x; a third is inside the noise and a fourth is nothing at all.

What it costs is how much a resumed load repeats. A checkpoint is one offset meaning
\"everything before this is written\", so it may only advance across a contiguous run of
acknowledged chunks. Drop to 1 to bound that at a single --chunk-bytes.")]
    in_flight: usize,

    /// Show progress on stderr. Default: progress when stderr is a terminal.
    #[arg(long, conflicts_with = "no_progress")]
    progress: bool,

    /// Do not show progress.
    #[arg(long)]
    no_progress: bool,

    /// Chunk the input and report, without sending anything.
    #[arg(long)]
    dry_run: bool,
}

// ---------------------------------------------------------------------------------------------
// Value parsers. Bounds live here rather than in the operation, so a refusal names the flag.
// ---------------------------------------------------------------------------------------------

fn chunk_bytes(s: &str) -> Result<usize, String> {
    // The ceiling is the server's, and a chunk above it is refused for the whole chunk rather
    // than trimmed - so it is caught here, where the flag has a name.
    let n: usize = s.parse().map_err(|_| format!("`{s}` is not a number"))?;
    if n == 0 || n > big_http::MAX_BODY {
        return Err(format!("must be between 1 and {}, got {n}", big_http::MAX_BODY));
    }
    Ok(n)
}

fn chunk_lines(s: &str) -> Result<usize, String> {
    let n: usize = s.parse().map_err(|_| format!("`{s}` is not a number"))?;
    if n == 0 {
        return Err("cannot be zero".to_string());
    }
    Ok(n)
}

fn in_flight(s: &str) -> Result<usize, String> {
    let n: usize = s.parse().map_err(|_| format!("`{s}` is not a number"))?;
    if n == 0 || n > MAX_IN_FLIGHT {
        return Err(format!("must be between 1 and {MAX_IN_FLIGHT}, got {n}"));
    }
    Ok(n)
}

// ---------------------------------------------------------------------------------------------
// Lowering: the command line to the run.
// ---------------------------------------------------------------------------------------------

impl Cli {
    /// Applies the environment and folds the parsed command line into one [`Options`].
    ///
    /// `env` is injected rather than read, so that the tests are hermetic - see the module doc.
    pub fn resolve(self, env: &dyn Fn(&str) -> Option<String>) -> Result<Options, String> {
        // **Refused, not ignored.** The worst outcome here is a script that keeps working
        // against a loopback development server and silently stops authenticating in
        // production, which is exactly what silently dropping a now-meaningless flag or
        // variable would produce.
        if self.token_file.is_some() {
            return Err("--token-file is gone: bearer tokens were replaced by usernames and \
                        passwords. Use --credentials-file with a file holding one \
                        `user:password` line."
                .to_string());
        }

        let credentials_file = self.credentials_file.or_else(|| env("BIG_CREDENTIALS"));
        if env("BIG_TOKEN").is_some() && credentials_file.is_none() {
            return Err("BIG_TOKEN is no longer used: bearer tokens were replaced by usernames \
                        and passwords. Set BIG_CREDENTIALS to a file holding one \
                        `user:password` line, readable only by you."
                .to_string());
        }

        Ok(Options {
            addr: self.addr.or_else(|| env("BIG_ADDR")).unwrap_or_else(|| DEFAULT_ADDR.to_string()),
            credentials_file,
            user: self.user,
            ca_file: self.ca_file.or_else(|| env("BIG_CA")),
            insecure_skip_verify: self.insecure_skip_verify,
            format: self.format,
            // Zero means "wait", not "give up immediately".
            timeout: self.timeout.filter(|s| *s > 0).map(Duration::from_secs),
            command: self.command.into(),
        })
    }
}

impl From<Cmd> for Command {
    fn from(cmd: Cmd) -> Self {
        match cmd {
            Cmd::Sql { statement } => Command::Sql(source(&statement)),
            Cmd::Query { table, call } => Command::Query { table, text: source(&call) },
            Cmd::Records { table, after, limit } => Command::Records { table, after, limit },
            Cmd::Import { table, file, load } => {
                Command::Load { verb: Verb::Import, table, input: input(&file), load: load.into() }
            }
            Cmd::Delete { table, file, load } => {
                Command::Load { verb: Verb::Delete, table, input: input(&file), load: load.into() }
            }
            Cmd::Schema => Command::Schema,
            Cmd::Create(CreateCmd::Table { table, engine }) => {
                Command::CreateTable { table, params: params([("engine", engine)]) }
            }
            Cmd::Create(CreateCmd::Field { table, field, kind, bit_depth, scale, granularity }) => {
                Command::CreateField {
                    table,
                    field,
                    // Query-parameter names, which spell `bit_depth` where the flag says
                    // `--bit-depth`. The route already takes them in this spelling.
                    params: params([
                        ("kind", Some(kind)),
                        ("bit_depth", bit_depth),
                        ("scale", scale),
                        ("granularity", granularity),
                    ]),
                }
            }
            Cmd::Drop(DropCmd::Table { table }) => Command::DropTable { table },
            Cmd::Drop(DropCmd::Field { table, field }) => Command::DropField { table, field },
            Cmd::Verify => Command::Verify,
            Cmd::Repair => Command::Repair,
            Cmd::Cluster(c) => c.into(),
            Cmd::Health => Command::Health,
            Cmd::Ready => Command::Ready,
            Cmd::Metrics => Command::Metrics,
            Cmd::Shell => Command::Shell,
        }
    }
}

impl From<ClusterCmd> for Command {
    fn from(cmd: ClusterCmd) -> Self {
        match cmd {
            ClusterCmd::Topology => Command::ClusterTopology,
            ClusterCmd::Split { at, node, .. } => Command::ClusterSplit { at, to: node },
            ClusterCmd::Merge { range } => Command::ClusterMerge { range },
            ClusterCmd::AddNode { name, addr } => Command::ClusterAddNode { name, addr },
            ClusterCmd::Join { name, addr } => Command::ClusterJoin { name, addr },
            ClusterCmd::AddReplica { range, node, .. } => {
                Command::ClusterReplica { add: true, range, node }
            }
            ClusterCmd::DropReplica { range, node, .. } => {
                Command::ClusterReplica { add: false, range, node }
            }
            ClusterCmd::Admit { name } => Command::ClusterMember { verb: "admit", name },
            ClusterCmd::Drain { name } => Command::ClusterMember { verb: "drain", name },
            ClusterCmd::Remove { name } => Command::ClusterMember { verb: "remove", name },
            ClusterCmd::Move { range, node, .. } => Command::ClusterMove { range, to: node },
            ClusterCmd::Cancel { range } => Command::ClusterCancel { range },
            ClusterCmd::Rebalance => Command::ClusterRebalance,
            ClusterCmd::SchemaLeader { node } => Command::ClusterSchemaLeader { to: node },
        }
    }
}

impl From<LoadArgs> for Load {
    fn from(a: LoadArgs) -> Self {
        Load {
            chunk_bytes: a.chunk_bytes,
            chunk_lines: a.chunk_lines,
            resume: a.resume,
            retries: a.retries,
            in_flight: a.in_flight,
            // Two flags rather than one carrying `false`, so that each can be named in help and
            // refused where it does not belong. `conflicts_with` is what makes them exclusive.
            progress: match (a.progress, a.no_progress) {
                (true, _) => Some(true),
                (_, true) => Some(false),
                _ => None,
            },
            dry_run: a.dry_run,
        }
    }
}

/// The flags that were given, in the query-parameter spelling the route takes.
fn params<const N: usize>(given: [(&str, Option<String>); N]) -> Vec<(String, String)> {
    given.into_iter().filter_map(|(name, value)| value.map(|v| (name.to_string(), v))).collect()
}

fn source(arg: &str) -> Source {
    if arg == "-" {
        Source::Stdin
    } else {
        Source::Literal(arg.to_string())
    }
}

fn input(arg: &str) -> Input {
    if arg == "-" {
        Input::Stdin
    } else {
        Input::Path(arg.to_string())
    }
}
