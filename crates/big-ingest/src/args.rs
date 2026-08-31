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

//! `argv`, parsed by hand and shaped like `bigc`'s.
//!
//! The flags two binaries share are spelled identically - `--addr`, `--token-file`,
//! `--timeout`, and the two environment variables behind them - because an operator who has
//! learned one should not discover that the other renamed anything.

use std::time::Duration;

/// Where `bigd` is when nobody says.
const DEFAULT_ADDR: &str = "127.0.0.1:7654";

/// How much of a chunk a request carries by default.
///
/// The server's ceiling is `big_http::MAX_BODY`, 8 MiB. This sits below it rather than on it: a
/// request that is refused with `413` costs the whole chunk, and the megabyte of headroom is
/// cheap insurance against a proxy in front of `bigd` with a smaller idea of large. It is a
/// throughput knob and not decoration - the server commits once per request, so doubling this
/// halves the number of commits. `--chunk-bytes` raises it for a deployment that measured.
pub const DEFAULT_CHUNK_BYTES: usize = 7 << 20;

/// The second ceiling, generous on purpose: the byte ceiling is meant to be the one that binds,
/// and this is here so that a file of two-byte lines cannot put four million facts in one
/// commit and call it a chunk.
pub const DEFAULT_CHUNK_LINES: usize = 1_000_000;

/// How many times a *transport* failure is tried again. Never a refusal - see `run`.
pub const DEFAULT_RETRIES: u32 = 3;

pub const USAGE: &str = "\
usage: bigi [options] import|delete <table> <file>|-

Loads a file into a running bigd, one request at a time. Every request is the same route with
the next slice of the same file; nothing here validates a line, and a refusal arrives as the
server's own code and sentence.

Commands:
  import <table> <file>|-     one fact per line: `field record value`
  delete <table> <file>|-     one record id per line

Options:
  --chunk-bytes <n>           bytes per request; default 7340032, ceiling 8388608
  --chunk-lines <n>           lines per request; default 1000000
  --resume <file>             write the acknowledged offset here, and start from it
  --retries <n>               retry a dropped connection this many times; default 3
  --progress | --no-progress  default: progress when stderr is a terminal
  --dry-run                   chunk the file and report, without sending anything
  --addr <host:port>          default 127.0.0.1:7654, or $BIG_ADDR
  --token-file <file>         a bearer token, mode 600; or $BIG_TOKEN
  --timeout <seconds>         give up on one exchange; default is to wait
  -h, --help

A load is resumable because a fact is a bit set at a record id written in the line: sending a
chunk twice writes the same bits twice, which is writing them once. That is what --resume rests
on, and what lets a dropped connection be retried at all. It is also why bigi never invents a
record id - a load that did could not be run twice.

--resume needs a file it can seek, so it is refused with `-`. A pipe has no offset to record.

A retry covers a connection that died, never a request the server understood and refused: a
chunk answered with `malformed_line` is answered the same way the second time, and retrying it
only delays the sentence naming the line.

Exit codes: 0 loaded, 1 the server refused, 2 usage, 3 nothing was listening.
";

/// The two routes this binary sends to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Verb {
    Import,
    Delete,
}

impl Verb {
    /// The last segment of the route, which is also the word on the command line.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Import => "import",
            Self::Delete => "delete",
        }
    }

    /// What one line of this route's body is, for the summary a person reads.
    pub fn noun(self) -> &'static str {
        match self {
            Self::Import => "imported",
            Self::Delete => "deleted",
        }
    }
}

/// Where the lines come from.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Input {
    /// A path, which can be seeked and therefore resumed.
    Path(String),
    /// `-`. What makes `bigi` the far end of a pipe, at the cost of `--resume`.
    Stdin,
}

/// Everything a run needs.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Options {
    pub addr: String,
    pub token_file: Option<String>,
    pub timeout: Option<Duration>,
    pub verb: Verb,
    pub table: String,
    pub input: Input,
    pub chunk_bytes: usize,
    pub chunk_lines: usize,
    pub resume: Option<String>,
    pub retries: u32,
    /// `None` means "decide from whether stderr is a terminal".
    pub progress: Option<bool>,
    pub dry_run: bool,
}

/// Parses `argv`. `Err("")` means `--help` was asked for, which is not a failure.
pub fn parse(args: &[String], env: &dyn Fn(&str) -> Option<String>) -> Result<Options, String> {
    let mut addr = env("BIG_ADDR").unwrap_or_else(|| DEFAULT_ADDR.to_string());
    let mut token_file = env("BIG_TOKEN");
    let mut timeout = None;
    let mut chunk_bytes = DEFAULT_CHUNK_BYTES;
    let mut chunk_lines = DEFAULT_CHUNK_LINES;
    let mut resume = None;
    let mut retries = DEFAULT_RETRIES;
    let mut progress = None;
    let mut dry_run = false;
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
            "--timeout" => {
                // Zero means "wait", the reading `bigc` and `bigd` both give it.
                let secs = number(&value()?, arg)?;
                timeout = (secs > 0).then(|| Duration::from_secs(secs));
                i += 2;
            }
            "--chunk-bytes" => {
                chunk_bytes = usize::try_from(number(&value()?, arg)?).map_err(|_| {
                    "--chunk-bytes does not fit in this machine's usize".to_string()
                })?;
                if chunk_bytes == 0 {
                    return Err("--chunk-bytes must be at least 1".to_string());
                }
                i += 2;
            }
            "--chunk-lines" => {
                chunk_lines = usize::try_from(number(&value()?, arg)?).map_err(|_| {
                    "--chunk-lines does not fit in this machine's usize".to_string()
                })?;
                if chunk_lines == 0 {
                    return Err("--chunk-lines must be at least 1".to_string());
                }
                i += 2;
            }
            "--resume" => {
                resume = Some(value()?);
                i += 2;
            }
            "--retries" => {
                retries = u32::try_from(number(&value()?, arg)?)
                    .map_err(|_| "--retries is larger than any load needs".to_string())?;
                i += 2;
            }
            "--progress" => {
                progress = Some(true);
                i += 1;
            }
            "--no-progress" => {
                progress = Some(false);
                i += 1;
            }
            "--dry-run" => {
                dry_run = true;
                i += 1;
            }
            "-h" | "--help" => return Err(String::new()),
            // `-` is the input, not a flag, so it is tested before the `-`-prefixed catch-all.
            "-" => {
                positional.push(arg.to_string());
                i += 1;
            }
            other if other.starts_with('-') => return Err(format!("unknown option {other}")),
            other => {
                positional.push(other.to_string());
                i += 1;
            }
        }
    }

    let words: Vec<&str> = positional.iter().map(String::as_str).collect();
    let source = |file: &str| {
        if file == "-" {
            Input::Stdin
        } else {
            Input::Path(file.to_string())
        }
    };
    let (verb, table, input) = match words.as_slice() {
        [] => return Err("a command is required".to_string()),
        ["import", table, file] => (Verb::Import, (*table).to_string(), source(file)),
        ["delete", table, file] => (Verb::Delete, (*table).to_string(), source(file)),
        ["import" | "delete", ..] => {
            return Err("import and delete each take a table and a file".to_string())
        }
        [other, ..] => return Err(format!("unknown command `{other}`")),
    };

    // Refused here rather than silently dropped: a load started with `--resume` and a pipe would
    // report an offset nobody could use, and the operator would find out at the restart.
    if resume.is_some() && input == Input::Stdin {
        return Err("--resume needs a file it can seek; standard input has no offset".to_string());
    }

    Ok(Options {
        addr,
        token_file,
        timeout,
        verb,
        table,
        input,
        chunk_bytes,
        chunk_lines,
        resume,
        retries,
        progress,
        dry_run,
    })
}

fn number(s: &str, flag: &str) -> Result<u64, String> {
    s.parse().map_err(|_| format!("{flag} needs a number, got `{s}`"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_ok(args: &[&str]) -> Options {
        let owned: Vec<String> = args.iter().map(|a| (*a).to_string()).collect();
        parse(&owned, &|_| None).expect("these arguments parse")
    }

    fn parse_err(args: &[&str]) -> String {
        let owned: Vec<String> = args.iter().map(|a| (*a).to_string()).collect();
        parse(&owned, &|_| None).expect_err("these arguments do not parse")
    }

    #[test]
    fn the_shortest_load_that_works() {
        let o = parse_ok(&["import", "tx", "facts.txt"]);
        assert_eq!(o.verb, Verb::Import);
        assert_eq!(o.table, "tx");
        assert_eq!(o.input, Input::Path("facts.txt".to_string()));
        assert_eq!(o.addr, DEFAULT_ADDR);
        assert_eq!(o.chunk_bytes, DEFAULT_CHUNK_BYTES);
    }

    #[test]
    fn a_dash_is_the_input_and_not_an_unknown_option() {
        assert_eq!(parse_ok(&["delete", "tx", "-"]).input, Input::Stdin);
    }

    #[test]
    fn the_environment_supplies_what_the_flags_do_not() {
        let owned: Vec<String> = ["import", "tx", "f"].iter().map(|a| (*a).to_string()).collect();
        let env = |k: &str| match k {
            "BIG_ADDR" => Some("db:9999".to_string()),
            "BIG_TOKEN" => Some("/etc/big/token".to_string()),
            _ => None,
        };
        let o = parse(&owned, &env).unwrap();
        assert_eq!(o.addr, "db:9999");
        assert_eq!(o.token_file.as_deref(), Some("/etc/big/token"));
    }

    /// The one combination that is a mistake rather than a preference.
    #[test]
    fn resume_and_a_pipe_are_refused_together() {
        let err = parse_err(&["import", "tx", "-", "--resume", "load.ck"]);
        assert!(err.contains("seek"), "{err}");
    }

    #[test]
    fn help_is_not_a_failure() {
        assert_eq!(parse_err(&["--help"]), "");
    }

    #[test]
    fn a_zero_chunk_is_refused_rather_than_rounded_up() {
        assert!(parse_err(&["import", "tx", "f", "--chunk-bytes", "0"]).contains("at least 1"));
    }

    #[test]
    fn an_unknown_command_says_so() {
        assert!(parse_err(&["query", "tx", "All()"]).contains("query"));
    }
}
