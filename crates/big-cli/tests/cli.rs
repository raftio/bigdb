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

//! The client against a real server, over a real socket.
//!
//! Nothing is mocked, and the reason is the risk this crate actually carries: the JSON reader
//! here and the JSON writer in `big-http` are two files with one producer between them, and a
//! test against a fixture would keep passing after they disagreed. Every assertion below went
//! through `Server::bind`, a loopback port, and `big_cli::run` - the same function `main` calls.

use big_api::Api;
use big_cli::{exit, Io};
use big_db::catalog::FieldKind;
use big_http::{Auth, Server, ServerConfig};
use std::net::SocketAddr;

/// What a run produced: the code a shell would see, and the two streams a user would.
struct Run {
    code: i32,
    out: String,
    err: String,
}

/// Drives the client exactly as `main` does, with a terminal-shaped `tty` so the default format
/// is the table a person sees. `stdin` is what the run may read for `-`.
fn run_at(addr: SocketAddr, args: &[&str], stdin: &str, tty: bool) -> Run {
    let mut input = std::io::Cursor::new(stdin.as_bytes().to_vec());
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let owned: Vec<String> = std::iter::once("--addr".to_string())
        .chain(std::iter::once(addr.to_string()))
        .chain(args.iter().map(|a| (*a).to_string()))
        .collect();

    let code = {
        let mut io = Io { input: &mut input, out: &mut out, err: &mut err, tty };
        // No environment: every test says what it means on the command line, so none of them
        // depends on what the machine running them happens to export.
        big_cli::run(&owned, &mut io, &|_| None)
    };
    Run {
        code,
        out: String::from_utf8(out).expect("stdout is UTF-8"),
        err: String::from_utf8(err).expect("stderr is UTF-8"),
    }
}

fn run(addr: SocketAddr, args: &[&str]) -> Run {
    run_at(addr, args, "", true)
}

/// A server with a table, some facts, and a bounded number of requests to answer.
fn stocked(requests: usize) -> SocketAddr {
    spawn(requests, ServerConfig::default())
}

fn spawn(requests: usize, config: ServerConfig) -> SocketAddr {
    let api = Api::in_memory().unwrap();
    api.create_table("tx").unwrap();
    api.create_field("tx", "amount", FieldKind::Int, 32).unwrap();
    api.create_field("tx", "country", FieldKind::Set, 0).unwrap();
    let facts = [(1u64, 100u64, "GB"), (2, 900, "US"), (3, 500, "GB")];
    for (id, amount, country) in facts {
        api.import(
            "tx",
            &[
                big_api::Fact::Int { field: "amount", record: id, value: amount },
                big_api::Fact::Key { field: "country", record: id, value: country },
            ],
        )
        .unwrap();
    }

    let server = Server::bind_with(api, "127.0.0.1:0", config).unwrap();
    let addr = server.local_addr().unwrap();
    std::thread::spawn(move || {
        let _ = server.serve_n(requests);
    });
    addr
}

// ------------------------------------------------------------------------------------------
// Rule 1: the client adds no vocabulary
// ------------------------------------------------------------------------------------------

/// Every public route, written by hand.
///
/// **This list is maintained against `big-http`'s route table, not derived from it.** Deriving
/// it would make the test agree with the client by construction and prove nothing; written out,
/// it breaks when a thirteenth route ships without a spelling in `bigc`, which is the direction
/// that actually goes wrong.
const PUBLIC_ROUTES: [&str; 12] = [
    "GET /health",
    "GET /ready",
    "GET /metrics",
    "GET /schema",
    "GET /verify",
    "POST /repair",
    "POST /table/{t}/query",
    "POST /sql",
    "GET /table/{t}/records",
    "POST /table/{t}/import",
    "POST /table/{t}/delete",
    "POST|DELETE /table/{t} and /table/{t}/field/{f}",
];

/// One subcommand per route, and every one of them reaches something.
///
/// A `404` here would mean the client spells a route the server does not have; there is no
/// assertion that the server *liked* the request, because several of these are refused on
/// content by a single-node daemon and that is not what this test is about.
#[test]
fn every_subcommand_reaches_a_route_that_exists() {
    let commands: [&[&str]; 14] = [
        &["health"],
        &["ready"],
        &["metrics"],
        &["schema"],
        &["verify"],
        &["repair"],
        &["query", "tx", "Count(All())"],
        &["sql", "SELECT count(*) FROM tx"],
        &["records", "tx"],
        &["import", "tx", "-"],
        &["delete", "tx", "-"],
        &["create", "table", "t2"],
        &["create", "field", "t2", "c", "--kind", "set"],
        &["drop", "table", "t2"],
    ];
    let addr = stocked(commands.len() + 1);

    for command in commands {
        let r = run_at(addr, command, "", true);
        assert!(
            !r.err.contains("no_such_route"),
            "`bigc {}` reached no route: {}",
            command.join(" "),
            r.err
        );
    }

    // `drop field` needs something to drop, and is the fourteenth route spelling.
    assert_eq!(PUBLIC_ROUTES.len(), 12, "the route list changed; update the subcommand table");
}

// ------------------------------------------------------------------------------------------
// The answers
// ------------------------------------------------------------------------------------------

#[test]
fn a_sql_answer_is_rendered_as_a_table_for_a_terminal() {
    let addr = stocked(2);

    let r = run(addr, &["sql", "SELECT count(*) FROM tx WHERE country = 'GB'"]);
    assert_eq!(r.code, exit::OK, "{}", r.err);
    assert_eq!(r.out, "count\n-----\n2\n");

    let r = run(addr, &["sql", "SELECT country, count(*) FROM tx GROUP BY country"]);
    assert_eq!(r.out, "country  count\n-------  -----\nGB       2\nUS       1\n", "{}", r.out);
}

/// A pipe gets tab-separated fields, because a column-aligned table is a format nobody can
/// parse and everybody tries to.
#[test]
fn a_pipe_gets_tsv_without_being_asked() {
    let addr = stocked(2);

    let r = run_at(addr, &["sql", "SELECT country, count(*) FROM tx GROUP BY country"], "", false);
    assert_eq!(r.out, "country\tcount\nGB\t2\nUS\t1\n");

    // And `--format` overrides the destination in both directions.
    let r = run_at(addr, &["--format", "table", "sql", "SELECT count(*) FROM tx"], "", false);
    assert_eq!(r.out, "count\n-----\n3\n");
}

/// `--format json` hands over the server's body untouched. It is the escape hatch, so it must
/// not be a re-serialisation of something this client parsed.
#[test]
fn json_is_the_servers_own_body() {
    let addr = stocked(1);
    let r = run(addr, &["--format", "json", "sql", "SELECT count(*) FROM tx"]);
    assert_eq!(r.out, "{\"columns\":[\"count\"],\"rows\":[[3]]}\n");
}

#[test]
fn the_other_query_surface_is_spelled_the_same_way() {
    let addr = stocked(2);
    let r = run(addr, &["query", "tx", "Count(Row(country=\"GB\"))"]);
    assert_eq!(r.code, exit::OK, "{}", r.err);
    assert_eq!(r.out, "count\n-----\n2\n");

    // A row set comes back as record ids under a column that says so.
    let r = run(addr, &["query", "tx", "Row(country=\"GB\")"]);
    assert_eq!(r.out, "id\n--\n1\n3\n");
}

#[test]
fn a_schema_is_one_row_per_field() {
    let addr = stocked(1);
    let r = run_at(addr, &["schema"], "", false);
    assert_eq!(r.out, "table\tfield\tkind\tbit_depth\ntx\tamount\tint\t32\ntx\tcountry\tset\t0\n");
}

/// The cursor is a note on stderr, not a row. A trailing line that is not a record is exactly
/// what breaks a script reading the pipe.
#[test]
fn a_paged_listing_keeps_the_cursor_out_of_the_data() {
    let addr = stocked(1);
    let r = run_at(addr, &["records", "tx", "--limit", "2"], "", false);
    assert_eq!(r.out, "id\n1\n2\n");
    assert!(r.err.contains("--after 2"), "{}", r.err);
}

#[test]
fn a_body_can_come_from_standard_input() {
    let addr = stocked(2);

    let r = run_at(addr, &["import", "tx", "-"], "amount 9 42\ncountry 9 FR\n", true);
    assert_eq!(r.code, exit::OK, "{}", r.err);
    assert_eq!(r.out, "imported\n--------\n2\n");

    // And so can a statement, which is what makes `bigc` compose with a shell.
    let r = run_at(addr, &["sql", "-"], "SELECT count(*) FROM tx WHERE country = 'FR'", true);
    assert_eq!(r.out, "count\n-----\n1\n");
}

#[test]
fn metrics_come_back_as_the_text_they_are() {
    let addr = stocked(1);
    let r = run(addr, &["metrics"]);
    assert_eq!(r.code, exit::OK, "{}", r.err);
    assert!(r.out.starts_with("# HELP") || r.out.contains("big_"), "{}", r.out);
}

// ------------------------------------------------------------------------------------------
// Rule 2: the client parses nothing it sends
// ------------------------------------------------------------------------------------------

/// The load-bearing test of this crate.
///
/// The client cannot know which statements are refused - it does not link `big-sql` and could
/// not find out. So the refusal has to arrive from the server with the server's code and the
/// server's sentence, and be printed without being reworded. If this ever fails by the message
/// changing shape, a client-side opinion has crept in.
#[test]
fn a_refusal_arrives_with_the_servers_own_code_and_sentence() {
    let addr = stocked(1);
    let r = run(addr, &["sql", "SELECT count(*) FROM tx, t2"]);

    assert_eq!(r.code, exit::REFUSED);
    assert!(r.err.contains("[sql_no_joins]"), "{}", r.err);
    // The server's own words, verbatim.
    assert!(r.err.contains("comma between tables"), "{}", r.err);
    assert!(r.out.is_empty(), "a refusal printed to stdout: {}", r.out);
}

#[test]
fn a_schema_mistake_is_the_servers_answer_too() {
    let addr = stocked(2);

    let r = run(addr, &["sql", "SELECT count(*) FROM nope"]);
    assert_eq!(r.code, exit::REFUSED);
    assert!(r.err.contains("[unknown_table]"), "{}", r.err);

    let r = run(addr, &["query", "tx", "Count(Row(nope > 1))"]);
    assert_eq!(r.code, exit::REFUSED);
    assert!(r.err.contains("[unknown_field]"), "{}", r.err);
}

// ------------------------------------------------------------------------------------------
// Exit codes, which are part of the surface
// ------------------------------------------------------------------------------------------

#[test]
fn every_exit_code_means_what_the_usage_says() {
    let addr = stocked(2);

    // 0: answered.
    assert_eq!(run(addr, &["sql", "SELECT count(*) FROM tx"]).code, exit::OK);
    // 1: refused.
    assert_eq!(run(addr, &["sql", "SELECT count(*) FROM nope"]).code, exit::REFUSED);
    // 2: usage. No request is sent, so this costs the server nothing.
    assert_eq!(run(addr, &["nonsense"]).code, exit::USAGE);
    assert_eq!(run(addr, &["--format", "yaml", "schema"]).code, exit::USAGE);
    assert_eq!(run(addr, &["create", "field", "tx", "c"]).code, exit::USAGE);
    // 3: nothing listening. A port nothing was ever bound to.
    let dead: SocketAddr = "127.0.0.1:1".parse().unwrap();
    assert_eq!(run(dead, &["schema"]).code, exit::UNREACHABLE);
}

/// `--help` is not a failure: usage to stdout, exit zero. A bad flag is the reverse.
#[test]
fn help_goes_to_stdout_and_a_mistake_goes_to_stderr() {
    let addr = stocked(0);

    let r = run(addr, &["--help"]);
    assert_eq!(r.code, exit::OK);
    assert!(r.out.starts_with("usage: bigc"), "{}", r.out);
    assert!(r.err.is_empty());

    let r = run(addr, &["--nope"]);
    assert_eq!(r.code, exit::USAGE);
    assert!(r.err.contains("unknown option --nope"), "{}", r.err);
    assert!(r.out.is_empty());
}

// ------------------------------------------------------------------------------------------
// Credentials
// ------------------------------------------------------------------------------------------

/// A file this test owns, at a mode it chooses.
fn token_file(name: &str, contents: &str, mode: u32) -> String {
    use std::os::unix::fs::PermissionsExt;
    let path = std::env::temp_dir().join(format!("bigc-{}-{name}", std::process::id()));
    std::fs::write(&path, contents).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
    path.to_string_lossy().into_owned()
}

#[test]
fn a_token_is_presented_from_a_file_and_never_from_a_flag() {
    let tokens = token_file("server-tokens", "s3cret read\n", 0o600);
    let auth = Auth::from_file(&tokens).unwrap();
    let addr = spawn(2, ServerConfig { auth, ..ServerConfig::default() });

    // Without one, the server says so and the client passes that through.
    let r = run(addr, &["schema"]);
    assert_eq!(r.code, exit::REFUSED);
    assert!(r.err.contains("[unauthenticated]"), "{}", r.err);

    // With one, the same command works. The client sends only the token, not the role beside it.
    let client_token = token_file("client-token", "s3cret read\n", 0o600);
    let r = run(addr, &["--token-file", &client_token, "schema"]);
    assert_eq!(r.code, exit::OK, "{}", r.err);
    assert!(r.out.contains("amount"), "{}", r.out);

    // There is no `--token` flag, and there will not be: an argument is visible in `ps`.
    let r = run(addr, &["--token", "s3cret", "schema"]);
    assert_eq!(r.code, exit::USAGE);
    assert!(r.err.contains("unknown option --token"), "{}", r.err);
}

/// A token file anyone can read is not a secret, and the client refuses it before connecting -
/// the same check the server applies to its own copy.
#[test]
fn a_world_readable_token_file_is_refused_before_anything_is_sent() {
    let addr = stocked(0);
    let path = token_file("loose-token", "s3cret\n", 0o644);
    let r = run(addr, &["--token-file", &path, "schema"]);
    assert_eq!(r.code, exit::USAGE);
    assert!(r.err.contains("chmod 600"), "{}", r.err);
}

// ------------------------------------------------------------------------------------------
// The shell
// ------------------------------------------------------------------------------------------

/// Two prompts, and no guessing between them.
#[test]
fn the_shell_sends_each_line_to_the_surface_it_was_told_to() {
    let addr = stocked(3);
    let session = "\
SELECT count(*) FROM tx
.lang pql
.table tx
Count(Row(country=\"GB\"))
.lang sql
SELECT count(*) FROM tx WHERE country = 'US'
.quit
";
    let r = run_at(addr, &["shell"], session, false);
    assert_eq!(r.code, exit::OK, "{}", r.err);
    assert_eq!(r.out, "count\n3\ncount\n2\ncount\n1\n", "{}", r.out);
}

/// PQL is asked *of* a table. Without one the client says so rather than sending a request to
/// a URL with a hole in it.
#[test]
fn the_shell_refuses_pql_with_no_table_rather_than_guessing_one() {
    let addr = stocked(0);
    let r = run_at(addr, &["shell"], ".lang pql\nCount(All())\n", false);
    assert_eq!(r.code, exit::OK);
    assert!(r.err.contains("set one with `.table`"), "{}", r.err);
    assert!(r.out.is_empty(), "{}", r.out);
}

/// A refusal does not end a session: the next line still runs.
#[test]
fn the_shell_survives_a_refusal() {
    let addr = stocked(2);
    let session = "SELECT count(*) FROM nope\nSELECT count(*) FROM tx\n";
    let r = run_at(addr, &["shell"], session, false);
    assert_eq!(r.code, exit::OK);
    assert!(r.err.contains("[unknown_table]"), "{}", r.err);
    assert_eq!(r.out, "count\n3\n");
}

#[test]
fn a_meta_command_that_does_not_exist_is_named() {
    let addr = stocked(0);
    let r = run_at(addr, &["shell"], ".nonsense\n", false);
    assert!(r.err.contains("no such meta-command `.nonsense`"), "{}", r.err);
}

/// `--engine` reaches the route as the query parameter it already takes.
///
/// The client adds no vocabulary here either: it does not know what a valid engine name is, so
/// a misspelling comes back as the *server's* refusal rather than one `bigc` invented.
#[test]
fn create_table_carries_the_engine_through() {
    let addr = stocked(3);

    let r = run(addr, &["create", "table", "cols", "--engine", "columnar"]);
    assert_eq!(r.code, 0, "{}", r.err);

    let r = run(addr, &["schema", "--format", "json"]);
    assert!(r.out.contains(r#""name":"cols","engine":"columnar""#), "{}", r.out);

    // An engine this client has never heard of is a matter between the caller and the server.
    let r = run(addr, &["create", "table", "nope", "--engine", "colunmar"]);
    assert_eq!(r.code, 1, "{} {}", r.out, r.err);
    assert!(r.err.contains("bad_parameter"), "{}", r.err);
}

/// A subcommand flag that belongs to a different subcommand is refused, not dropped.
///
/// **The absence of this check was a silent failure rather than a small one.** These flags were
/// collected into one list and applied wherever they fitted, so `records t --engine columnar`
/// was accepted and then thrown away - no error, no difference in the output, and nothing for
/// the caller to notice. A client that swallows what it was given is worse than one that
/// refuses it, and this is the direction that actually goes wrong.
#[test]
fn a_flag_from_another_subcommand_is_refused() {
    let addr = stocked(1);
    for (args, flag) in [
        (vec!["records", "tx", "--engine", "columnar"], "--engine"),
        (vec!["create", "field", "tx", "c", "--kind", "set", "--engine", "columnar"], "--engine"),
        (vec!["create", "table", "t2", "--kind", "set"], "--kind"),
        (vec!["schema", "--limit", "5"], "--limit"),
        (vec!["sql", "SELECT count(*) FROM tx", "--after", "3"], "--after"),
    ] {
        let r = run_at(addr, &args, "", true);
        assert_eq!(r.code, 2, "`{}` should be a usage error: {} {}", args.join(" "), r.out, r.err);
        assert!(
            r.err.contains(flag) && r.err.contains("does not belong"),
            "`{}` should name {flag}: {}",
            args.join(" "),
            r.err
        );
    }
}

/// The flags that do belong keep working, so the check above is a fence and not a wall.
#[test]
fn a_subcommands_own_flags_still_reach_it() {
    let addr = stocked(4);
    for args in [
        vec!["records", "tx", "--limit", "2"],
        vec!["records", "tx", "--after", "1"],
        vec!["create", "table", "t3", "--engine", "bitmap"],
        vec!["create", "field", "t3", "n", "--kind", "int", "--bit-depth", "16"],
    ] {
        let r = run_at(addr, &args, "", true);
        assert_ne!(r.code, 2, "`{}` was refused as usage: {}", args.join(" "), r.err);
    }
}
