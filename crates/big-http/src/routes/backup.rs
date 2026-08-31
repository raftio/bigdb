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

//! Taking a backup of this node's file without stopping it.
//!
//! **Why this route exists at all.** The engine has always been able to copy itself while
//! writers run: the walk holds a read transaction, which pins the reclaim horizon, so a
//! concurrent commit can happen but cannot reuse a page the walk still needs. What could not
//! happen was reaching that walk from outside - `big backup` is a separate process and every
//! subcommand takes the file's exclusive lock, so the only way to back up a served database was
//! to stop serving it. The capability was there and the door was not; this is the door.
//!
//! **It is about this process, not the cluster.** Like `/metrics` and `/ready`, it answers for
//! the file this daemon has open. Backing up a fanned-out database is one call per node, and
//! what that produces is *not* a cluster-wide snapshot: each node's copy sits at its own
//! transaction, for the same reason a fanned-out read can straddle two commits. `docs/clustering.md`
//! says why that is not fixable without a different engine, and a route that implied otherwise
//! would be the most expensive kind of wrong.
//!
//! **The destination is a name, not a path.** The directory comes from `--backup-dir` on the
//! command line and the request may only choose a file inside it. An admin token can already
//! drop every table, so the power being withheld here is not over the data - it is over the
//! rest of the filesystem, which is a different thing to hold and a much larger one. Without
//! the flag the route is not configured and says so, rather than defaulting to somewhere.
//!
//! **One at a time.** Two concurrent walks would each pin the reclaim horizon for their whole
//! run while writers kept committing, which is the shape that makes a file grow without
//! bound. The second caller is refused rather than queued: a queued backup is one that starts
//! at an unpredictable time, and the caller cannot tell the difference from a slow one.

use super::*;
use std::sync::atomic::Ordering;

/// Longest name a backup may be given. Long enough for a date, a node name and a suffix.
const MAX_NAME: usize = 128;

/// `POST /admin/backup?name=<name>` - an online, compact copy of this node's file.
pub(super) fn backup<P: PagerMut + Sync>(ctx: &Ctx<'_, P>, req: &Request) -> Response {
    let Some(dir) = ctx.backup_dir else {
        return Response::failure(
            501,
            "backup_not_configured",
            "this daemon was started without --backup-dir, so it has nowhere to write a \
             backup. The directory is a command-line decision rather than a request one \
             because a request that chose its own path could write anywhere this process can",
        );
    };

    let Some(name) = req.param("name") else {
        return Response::failure(
            400,
            "bad_parameter",
            "name is required, and is the file to create inside the backup directory",
        );
    };
    if let Err(why) = check_name(name) {
        return Response::failure(400, "bad_parameter", &why);
    }

    // Taken before the walk starts and released whatever happens, so a failed backup does not
    // leave the route refusing for the life of the process.
    if ctx.backup_running.swap(true, Ordering::SeqCst) {
        return Response::failure(
            409,
            "backup_in_progress",
            "a backup is already running on this node. Two walks at once would each hold the \
             reclaim horizon down for their whole run, so the file would grow by everything \
             both of them saw",
        );
    }
    let outcome = ctx.api().backup_to(std::path::Path::new(dir).join(name));
    ctx.backup_running.store(false, Ordering::SeqCst);

    match outcome {
        Err(e) => Response::from_error(&e),
        Ok(done) => {
            // The size of what was written, which is the number an operator budgets with and
            // is not derivable from the source's page count: the copy is compact, so it is
            // smaller by whatever the freelist was holding.
            let bytes = std::fs::metadata(std::path::Path::new(dir).join(name))
                .map(|m| m.len())
                .unwrap_or(0);
            Response::ok(format!(
                "{{\"backup\":{},\"txn_id\":{},\"pages\":{},\"bytes\":{bytes}}}",
                json::string(name),
                done.txn_id,
                done.pages
            ))
        }
    }
}

/// A file name and nothing else.
///
/// Rejecting a separator is the whole point, and rejecting a leading dot is the rest of it: a
/// name is not allowed to be `..`, and a name that begins with `.` is one an operator listing
/// the directory would not see. The set is deliberately smaller than what the filesystem
/// permits, because every character allowed here has to be one that survives a shell, a
/// `curl`, and whatever the backup ends up being copied by.
fn check_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("name cannot be empty".to_string());
    }
    if name.len() > MAX_NAME {
        return Err(format!("name is {} bytes, longer than the {MAX_NAME}-byte limit", name.len()));
    }
    if name.starts_with('.') {
        return Err("name cannot begin with a dot".to_string());
    }
    if let Some(bad) =
        name.chars().find(|c| !matches!(c, 'a'..='z' | 'A'..='Z' | '0'..='9' | '.' | '_' | '-'))
    {
        return Err(format!(
            "name may hold letters, digits, dot, underscore and dash; found `{bad}`. It names a \
             file inside the backup directory, so it is not allowed to name anything outside it"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::check_name;

    #[test]
    fn accepts_an_ordinary_backup_name() {
        assert!(check_name("data-2026-08-30.big").is_ok());
    }

    #[test]
    fn refuses_every_way_of_leaving_the_directory() {
        for bad in ["..", "../etc/passwd", "a/b", "/absolute", ".hidden", ""] {
            assert!(check_name(bad).is_err(), "`{bad}` should not be an acceptable name");
        }
    }

    #[test]
    fn refuses_a_name_longer_than_the_limit() {
        assert!(check_name(&"a".repeat(super::MAX_NAME + 1)).is_err());
    }
}
