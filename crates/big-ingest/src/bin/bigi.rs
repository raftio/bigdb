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

//! The process around [`big_ingest::run`], which is the whole program.
//!
//! Everything here is what a `main` has to do and a test must not: take the real streams, ask
//! whether stderr is a terminal, and turn a return value into an exit status.

use std::io::{IsTerminal, Write};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let mut out = std::io::stdout().lock();
    let mut err = std::io::stderr().lock();
    // Progress is drawn on stderr, so it is stderr that decides how - a load whose output is
    // piped to a file still has a person watching it if the terminal is where errors go.
    let tty = std::io::stderr().is_terminal();

    let code = {
        let mut io = big_ingest::Io { input: &mut input, out: &mut out, err: &mut err, tty };
        big_ingest::run(&args, &mut io, &|k| std::env::var(k).ok())
    };
    let _ = out.flush();
    let _ = err.flush();
    std::process::exit(code);
}
