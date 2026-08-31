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

//! `bigc` - ask a running `bigd` a question.
//!
//! Everything is in the library, so that the tests drive exactly what a user drives rather than
//! a shape arranged for testing. This file is the three things a `main` is for: the real
//! streams, the real environment, and the exit code.

use big_cli::Io;
use std::io::IsTerminal;

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let stdout = std::io::stdout();
    let tty = stdout.is_terminal();
    let mut out = stdout.lock();
    let mut err = std::io::stderr().lock();

    let mut io = Io { input: &mut input, out: &mut out, err: &mut err, tty };
    let code = big_cli::run(&args, &mut io, &|name| std::env::var(name).ok());

    // `u8` because that is what an exit status carries; every code this returns is small.
    std::process::ExitCode::from(code as u8)
}
