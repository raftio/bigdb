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

//! Reading a password from the terminal without printing it.
//!
//! **Why this exists rather than a flag.** An argument is visible in `ps`, in shell history, and
//! in whatever collects both. The client has a test asserting a credential can never be given as
//! a flag; this is the other half of that rule, for the side that *creates* the credential.
//!
//! **Why it is thirty lines of `libc` rather than a crate.** `libc` is one of the four
//! dependencies the engine already has, and turning `ECHO` off is two calls and a guard. A crate
//! for it would be a fifth dependency for less code than its own manifest.

use std::io::{self, BufRead, IsTerminal, Write};

/// Reads one line with the terminal not echoing it.
///
/// **When stdin is not a terminal, one line is read from it and no terminal games are played.**
/// That is the only non-tty path and it exists so a provisioning script can pipe a password in -
/// which is a different thing from putting one on a command line, because a pipe is not in `ps`.
pub fn read_password(prompt: &str) -> io::Result<String> {
    if !io::stdin().is_terminal() {
        return read_line();
    }
    eprint!("{prompt}");
    io::stderr().flush()?;
    let _hidden = Echo::off()?;
    let line = read_line();
    // The newline the user typed was not echoed either, so without this the next thing printed
    // lands on the same line as the prompt.
    eprintln!();
    line
}

/// Reads a password twice and refuses if they differ.
///
/// Twice because this writes to a file that is the only copy: a typo here is an operator locked
/// out of their own database with no way to find out what they actually typed.
pub fn read_password_twice() -> io::Result<String> {
    if !io::stdin().is_terminal() {
        // A pipe gets one line. Asking a script to send it twice would be asking it to guard
        // against a typo it cannot make.
        return read_password("");
    }
    let first = read_password("password: ")?;
    if first.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "an empty password is refused"));
    }
    let again = read_password("password again: ")?;
    if first != again {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "the two did not match"));
    }
    Ok(first)
}

fn read_line() -> io::Result<String> {
    let mut line = String::new();
    io::stdin().lock().read_line(&mut line)?;
    // Trailing newline only. A password may legitimately end in a space, and trimming one would
    // silently change what the operator typed into something they cannot reproduce.
    Ok(line.trim_end_matches(['\n', '\r']).to_string())
}

/// Turns terminal echo off, and back on however this scope ends.
///
/// A guard rather than a pair of calls, because the restore has to happen on the error path too -
/// and the error path here is "the two passwords did not match", which is the common one.
///
/// A `SIGINT` between the two still leaves echo off. That is `ssh`'s behaviour as well, the fix
/// is `stty sane`, and it is documented rather than handled: installing a signal handler in a
/// short-lived command to cover one keystroke would be more machinery than the problem.
struct Echo {
    #[cfg(unix)]
    saved: libc::termios,
}

impl Echo {
    /// **The only `unsafe` in this crate**, and the whole of it is four `libc` calls around a
    /// `termios` struct. The crate is `#![deny(unsafe_code)]`, so this says so out loud rather
    /// than the deny being quietly relaxed at the top of the file where nobody would see it.
    #[allow(unsafe_code)]
    #[cfg(unix)]
    fn off() -> io::Result<Self> {
        use std::os::unix::io::AsRawFd;
        let fd = io::stdin().as_raw_fd();
        // SAFETY: `termios` is a plain C struct of integers with no invalid bit patterns, so a
        // zeroed one is a valid value to hand to `tcgetattr`, which overwrites all of it. `fd` is
        // owned by this process's stdin and outlives the call.
        let mut saved: libc::termios = unsafe { std::mem::zeroed() };
        // SAFETY: `fd` is a valid descriptor and `saved` is a valid, writable `termios`.
        if unsafe { libc::tcgetattr(fd, &mut saved) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let mut quiet = saved;
        quiet.c_lflag &= !libc::ECHO;
        // SAFETY: the same descriptor, and `quiet` is a `termios` this process just read and
        // modified one flag of.
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &quiet) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { saved })
    }

    #[cfg(not(unix))]
    fn off() -> io::Result<Self> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "this platform cannot hide a typed password; pipe it in on standard input instead",
        ))
    }
}

impl Drop for Echo {
    #[allow(unsafe_code)]
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            // SAFETY: restoring the exact `termios` read in `off`, on the same descriptor, which
            // is still stdin. Nothing is checked because a failure here has nowhere to go and the
            // process is about to exit either way.
            unsafe {
                libc::tcsetattr(io::stdin().as_raw_fd(), libc::TCSANOW, &self.saved);
            }
        }
    }
}
