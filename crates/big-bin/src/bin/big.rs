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
//! **The dispatcher used to be a `match` on the first word that fell through to the offline
//! half for anything it did not recognise**, which is how `big backup` worked without a
//! `backup` arm here. That fall-through is now one flat subcommand list: the two halves are
//! still two modules, and `#[command(flatten)]` is what keeps them one command line.
//!
//! Asking a *running* daemon anything is `bigctl`.

use big_bin::{offline, passwd, serve};
use clap::Parser;

const AFTER_LONG_HELP: &str = "\
A backup is an ordinary database file. Restoring is opening it - `restore` exists so that the
procedure has a name, not because the file needs converting.

Copying a live database with `cp` is NOT safe: a commit can land between the bytes cp has
already read and the ones it has not. Use `backup`.

Every subcommand but `serve` needs the exclusive lock, so a daemon must be stopped first.
Asking a running daemon anything is `bigctl`.
";

#[derive(Parser)]
#[command(
    name = "big",
    version,
    about = "Serve a database file, or work on one offline",
    after_long_help = AFTER_LONG_HELP
)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

// `serve`'s thirty-five options make its variant far larger than `compact { file }`, which is
// what clippy is pointing at. Boxing it would move one allocation's worth of bytes off a value
// that is constructed exactly once, at startup, and immediately consumed - and `clap::Args` is
// not implemented for `Box<T>`, so it would mean a wrapper type as well. The size is the shape
// of the command line, and the command line is the point.
#[allow(clippy::large_enum_variant)]
#[derive(clap::Subcommand)]
enum Cmd {
    /// Serve a database over HTTP. `big serve --help` for the flags, of which there are many.
    Serve(serve::Options),

    /// Add, change or remove a user in a users file.
    ///
    /// The only thing that writes one: there is deliberately no route that does, because one
    /// would let an `admin` credential rewrite the credential file over the network.
    #[command(verbatim_doc_comment)]
    Passwd(passwd::Args),

    /// The offline half, on a file nothing else has open.
    #[command(flatten)]
    Offline(offline::Cmd),
}

fn main() -> std::io::Result<()> {
    match Cli::parse().command {
        Cmd::Serve(options) => serve::main(options),
        Cmd::Passwd(args) => passwd::main(args),
        Cmd::Offline(cmd) => {
            offline::main(cmd);
            Ok(())
        }
    }
}
