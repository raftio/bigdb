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

//! `big` - the binary that owns the file.
//!
//! Two groups of subcommand, and the split is the one the engine already made. `serve` holds
//! the file open and answers over HTTP; every other command takes the exclusive lock instead,
//! so running one against a served database fails immediately and says so rather than doing
//! something clever behind the daemon's back.
//!
//! They are one binary because they are one decision - which process owns the file - and
//! shipping that decision as two executables (`bigd` and `big`) meant an operator had to know
//! which name held the lock before they could find out that the other one could not have it.
//!
//! Asking a *running* daemon anything is `bigctl`.

use big_bin::{offline, serve};

const USAGE: &str = "\
usage: big <command> [args]

Serving:
  serve <file> [addr]    serve <file> over HTTP; addr defaults to 127.0.0.1:7654
                         `big serve --help` for the flags, of which there are many

Offline, on a file nothing else has open:
  backup <file> <dest>   write a consistent copy of <file> to <dest>
                         safe while a writer is running; <dest> must not exist
  restore <src> <dest>   copy a backup into place; <dest> must not exist
  compact <file>         rewrite <file> as a compact copy of itself, in place
  verify <file>          open <file> and report what it holds
  drop-days <file> <table> <field> <unix-seconds>
                         drop a time quantum field's day views older than <unix-seconds>
                         the day that instant falls in is kept; the records are not
                         removed, only the per-day index over them
  scrub <file>           recompute every checksum <file> can reach
                         opening checks the meta page and the chains; this checks the
                         trees, which nothing on the query path ever does

A backup is an ordinary database file. Restoring is opening it - `restore` exists so that the
procedure has a name, not because the file needs converting.

Copying a live database with `cp` is NOT safe: a commit can land between the bytes cp has
already read and the ones it has not. Use `backup`.

Every subcommand but `serve` needs the exclusive lock, so a daemon must be stopped first.
Asking a running daemon anything is `bigctl`.
";

fn main() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    match args.first().map(String::as_str) {
        // `serve` takes the rest verbatim, so its own `--help` and its own errors are the ones
        // an operator reads. The word is stripped here and nowhere else.
        Some("serve") => serve::main(&args[1..]),

        // `--help` is not a failure: usage to stdout, exit zero. Being given nothing is,
        // so it goes to stderr with a non-zero code - the same split every binary here makes.
        Some("-h" | "--help") => {
            print!("{USAGE}");
            Ok(())
        }

        // Everything else is offline, and `offline` names the command it did not recognise.
        // That includes the empty case, which it answers with the usage and exit 2.
        _ => {
            offline::main(&args, USAGE);
            Ok(())
        }
    }
}
