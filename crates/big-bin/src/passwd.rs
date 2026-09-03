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

//! `big passwd <users-file> <command>` - the only thing that writes a users file.
//!
//! **A subcommand of `big`, not of `bigctl`, and deliberately not a route.** It edits a file on
//! the server's own disk. A route that could write the users file would let an `admin` credential
//! rewrite the credential file over the network, which turns "can drop every table" into "can
//! lock the operator out and let themselves back in" - a much larger power, and one nobody asked
//! for.
//!
//! **The file is rewritten atomically and keeps its shape.** An operator's `# ops team` heading
//! has to outlive a password change, and a users file half-written by a `^C` is an operator
//! locked out of their own database.

use big_http::auth::{hash_password, Role};
use std::io::Write;
use std::path::Path;

const USAGE: &str = "\
usage: big passwd <users-file> <command>

  set <user> [--role read|write|admin]   add a user, or change an existing password
  role <user> <read|write|admin>         change a role, leaving the password alone
  delete <user>                          remove a user
  list                                   names and roles, never a hash

The password is read from the terminal, twice, and echoed nowhere. It is never a flag and
never an environment variable: an argument is visible in `ps` and in shell history, and a
password in either has already leaked. When standard input is not a terminal one line is read
from it, which is the only non-tty path and is there so a provisioning script can pipe one in.

`set` on a file that does not exist creates it at mode 600. On one that does, the same mode
check `big serve` makes runs first, so this refuses any file the server would refuse.
";

/// `big passwd`, with the word already stripped by the dispatcher.
pub fn main(args: &[String]) -> std::io::Result<()> {
    let strings: Vec<&str> = args.iter().map(String::as_str).collect();
    match strings.as_slice() {
        [] | ["-h"] | ["--help"] => {
            print!("{USAGE}");
            Ok(())
        }
        [path, rest @ ..] => run(Path::new(path), rest).map_err(|e| {
            eprintln!("big passwd: {e}");
            std::process::exit(2)
        }),
    }
}

fn run(path: &Path, args: &[&str]) -> Result<(), String> {
    match args {
        ["set", user, flags @ ..] => set(path, user, role_flag(flags)?),
        ["role", user, role] => {
            let role = Role::parse(role)
                .ok_or_else(|| format!("`{role}` is not a role; use read, write or admin"))?;
            change_role(path, user, role)
        }
        ["delete", user] => delete(path, user),
        ["list"] => list(path),
        [] => Err("a command is required; see --help".to_string()),
        [other, ..] => Err(format!("`{other}` is not a command; see --help")),
    }
}

/// `--role` out of what is left of the command line. Defaults to `read`.
///
/// The least role, on purpose: a user created without anybody saying what they should be able to
/// do should be able to do the least, not the most.
fn role_flag(flags: &[&str]) -> Result<Role, String> {
    match flags {
        [] => Ok(Role::Read),
        ["--role", name] => Role::parse(name)
            .ok_or_else(|| format!("`{name}` is not a role; use read, write or admin")),
        ["--role"] => Err("--role needs a value".to_string()),
        [other, ..] => Err(format!("unknown option {other}")),
    }
}

fn set(path: &Path, user: &str, role: Role) -> Result<(), String> {
    let existed = path.exists();
    let mut lines = read(path)?;
    let password = crate::tty::read_password_twice().map_err(|e| e.to_string())?;
    let hash = hash_password(&password).map_err(|e| e.to_string())?;
    let line = format!("{user} {} {hash}", role.as_str());

    match lines.iter().position(|l| names(l) == Some(user)) {
        // Replaced in place rather than removed and appended, so that a password change does not
        // shuffle the file and turn a one-line diff into two.
        Some(at) => lines[at] = line,
        None => lines.push(line),
    }
    write(path, &lines)?;
    if existed {
        eprintln!("big passwd: set `{user}` ({}) in {}", role.as_str(), path.display());
    } else {
        eprintln!("big passwd: created {} with `{user}` ({})", path.display(), role.as_str());
    }
    Ok(())
}

fn change_role(path: &Path, user: &str, role: Role) -> Result<(), String> {
    let mut lines = read(path)?;
    let at = lines
        .iter()
        .position(|l| names(l) == Some(user))
        .ok_or_else(|| format!("no user `{user}` in {}", path.display()))?;
    // The hash is the third field and is carried across untouched: changing what somebody may do
    // is not a reason to make them choose a new password.
    let hash = lines[at].split_whitespace().nth(2).unwrap_or_default().to_string();
    lines[at] = format!("{user} {} {hash}", role.as_str());
    write(path, &lines)?;
    eprintln!("big passwd: `{user}` is now `{}`", role.as_str());
    Ok(())
}

fn delete(path: &Path, user: &str) -> Result<(), String> {
    let mut lines = read(path)?;
    let before = lines.len();
    lines.retain(|l| names(l) != Some(user));
    if lines.len() == before {
        return Err(format!("no user `{user}` in {}", path.display()));
    }
    write(path, &lines)?;
    eprintln!("big passwd: removed `{user}`");
    Ok(())
}

fn list(path: &Path) -> Result<(), String> {
    for line in read(path)? {
        let mut parts = line.split_whitespace();
        // Two fields printed out of three. A hash on a terminal ends up in a scrollback and then
        // in a paste, and an argon2 hash in a paste is an offline attack somebody was handed.
        if let (Some(user), Some(role)) = (parts.next(), parts.next()) {
            if !user.starts_with('#') {
                println!("{user}\t{role}");
            }
        }
    }
    Ok(())
}

/// The username a line grants, or `None` for a comment or a blank.
fn names(line: &str) -> Option<&str> {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return None;
    }
    trimmed.split_whitespace().next()
}

/// Every line of the file, comments and blanks included.
///
/// Kept verbatim, because the whole file is written back: an operator's headings and the order
/// they put people in are theirs, and a tool that reformatted them would be a tool nobody runs
/// twice.
fn read(path: &Path) -> Result<Vec<String>, String> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    // The same rule `big serve` applies, so this refuses a file the server would refuse - rather
    // than editing one happily and then failing to start.
    big_tls::mode::check_permissions(path, "a users file").map_err(|e| e.to_string())?;
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(text.lines().map(str::to_string).collect())
}

/// Writes the whole file at mode 600, atomically.
///
/// **A temporary file in the same directory, then a rename.** A users file truncated by a `^C`
/// or a full disk is an operator locked out of their own database, and `rename` within a
/// directory is the one filesystem operation that cannot leave a half-written result. The
/// directory itself is synced afterwards, because on most filesystems the rename is what has to
/// survive, not the bytes.
fn write(path: &Path, lines: &[String]) -> Result<(), String> {
    let tmp = path.with_extension("tmp");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // Created at 0600 rather than created and then chmodded: between those two calls the
        // file would exist, world-readable, with hashes in it.
        options.mode(0o600);
    }
    let mut file = options.open(&tmp).map_err(|e| format!("{}: {e}", tmp.display()))?;
    for line in lines {
        writeln!(file, "{line}").map_err(|e| format!("{}: {e}", tmp.display()))?;
    }
    file.sync_all().map_err(|e| format!("{}: {e}", tmp.display()))?;
    drop(file);

    std::fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))?;
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        if let Ok(handle) = std::fs::File::open(dir) {
            let _ = handle.sync_all();
        }
    }
    Ok(())
}
