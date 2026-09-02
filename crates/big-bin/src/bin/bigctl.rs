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

//! `bigctl` - ask a running `big serve` a question, or load a file into one.
//!
//! Everything is in the library, so that the tests drive exactly what a user drives rather than
//! a shape arranged for testing. This file is the four things a `main` is for: the real
//! streams, which of them are terminals, the real environment, and the exit code.

use big_bin::Io;
use std::io::{IsTerminal, Write};

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    let stdin = std::io::stdin();
    let mut input = stdin.lock();

    // **Both handles are asked, and they are asked before either is locked.** They answer
    // different questions: `out` decides the format, because that is where the answer goes, and
    // `err` decides the progress bar, because that is where the noise goes. A load whose table
    // is piped to a file still has a person watching it, and that person is looking at stderr.
    let stdout = std::io::stdout();
    let out_tty = stdout.is_terminal();
    let err_tty = std::io::stderr().is_terminal();
    let mut out = stdout.lock();
    let mut err = std::io::stderr().lock();

    let code = {
        let mut io = Io { input: &mut input, out: &mut out, err: &mut err, out_tty, err_tty };
        big_bin::client::run(&args, &mut io, &|name| std::env::var(name).ok())
    };

    // Flushed by hand rather than left to the drop order: a load writes its summary last, and
    // `ExitCode` is returned rather than `std::process::exit` for the same reason - the latter
    // does not run them at all.
    let _ = out.flush();
    let _ = err.flush();

    // `u8` because that is what an exit status carries; every code this returns is small.
    std::process::ExitCode::from(code as u8)
}
