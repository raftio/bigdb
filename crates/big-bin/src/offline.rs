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

//! The offline half of `big`: back a database up, check it, shrink it.
//!
//! Every subcommand here takes the exclusive lock, so running one against a served database
//! fails immediately and says so, rather than doing something clever behind the daemon's back.
//! That is the line this file sits on the other side of from [`crate::serve`].

// Unix only, like `big serve`: every subcommand works on a file through `MmapPager`, and the
// engine has no other file backend. Gating it would trade one confusing build error for
// another, so the tool follows the repository's existing convention instead.
use big_db::{copy, Db};

// The offline subcommands, flattened into `big`'s own list rather than nested under a word.
//
// They were slice patterns over `argv` with a hand-kept `KNOWN` table beside them, whose only
// job was to tell "wrong arguments for a real command" from "no such command". Declaring the
// commands is what answers both, and `restore` is an alias rather than a second arm because it
// was never a second operation: a backup is an ordinary database file, so restoring one is
// copying it, and the word exists so the procedure has a name.
//
// **Deliberately not a doc comment.** `#[command(flatten)]` lifts an enum's `long_about` into
// its parent, so a `///` here would become what `big --help` says `big` is for. The prose lives
// in the module doc above instead, where it is one scroll from this and reaches rustdoc anyway.
#[derive(clap::Subcommand, Debug)]
pub enum Cmd {
    /// Write a consistent copy of a database to <DEST>, which must not exist.
    ///
    /// Safe while a writer is running. Copying a live database with `cp` is NOT safe: a commit
    /// can land between the bytes cp has already read and the ones it has not.
    #[command(visible_alias = "restore", verbatim_doc_comment)]
    Backup {
        #[arg(value_name = "FILE")]
        file: String,
        #[arg(value_name = "DEST")]
        dest: String,
    },

    /// Rewrite a database as a compact copy of itself, in place.
    Compact {
        #[arg(value_name = "FILE")]
        file: String,
    },

    /// Open a database and report what it holds.
    Verify {
        #[arg(value_name = "FILE")]
        file: String,
    },

    /// Recompute every checksum the file can reach.
    ///
    /// Opening checks the meta page and the chains; this checks the trees, which nothing on the
    /// query path ever does.
    #[command(verbatim_doc_comment)]
    Scrub {
        #[arg(value_name = "FILE")]
        file: String,
    },

    /// Account for every page: reachable, free, or neither.
    ///
    /// A page in neither is one nothing will ever read and nothing will ever reuse; one at the
    /// end of the file stops `compact` giving anything back. Reads only, and never repairs.
    #[command(verbatim_doc_comment)]
    Leaks {
        #[arg(value_name = "FILE")]
        file: String,
    },

    /// Drop a time quantum field's day views older than <UNIX-SECONDS>.
    ///
    /// The day that instant falls in is kept; the records are not removed, only the per-day
    /// index over them.
    #[command(verbatim_doc_comment)]
    DropDays {
        #[arg(value_name = "FILE")]
        file: String,
        table: String,
        field: String,
        #[arg(value_name = "UNIX-SECONDS")]
        before: i64,
    },
}

/// Runs one offline subcommand.
pub fn main(cmd: Cmd) {
    let outcome = match &cmd {
        Cmd::Backup { file, dest } => backup(file, dest),
        Cmd::Compact { file } => compact(file),
        Cmd::Verify { file } => verify(file),
        Cmd::Scrub { file } => scrub(file),
        Cmd::Leaks { file } => leaks(file),
        Cmd::DropDays { file, table, field, before } => drop_days(file, table, field, *before),
    };

    if let Err(e) = outcome {
        eprintln!("big: {e}");
        std::process::exit(1);
    }
}

fn backup(file: &str, dest: &str) -> Result<(), String> {
    let db = open(file)?;
    db.backup_to(dest).map_err(|e| format!("could not back up to {dest}: {e}"))?;
    let pages = db.store().metrics().page_count;
    println!("backed up {file} ({pages} pages) to {dest}");
    Ok(())
}

fn compact(file: &str) -> Result<(), String> {
    let report = copy::compact_path(file).map_err(|e| format!("could not compact {file}: {e}"))?;
    println!(
        "compacted {file}: {} -> {} pages, {} reclaimed",
        report.pages_before,
        report.pages_after,
        report.pages_reclaimed()
    );
    Ok(())
}

/// Opening is the check. A bad checksum, an unreadable meta page or a version this build does
/// not know is an error on the way in, so anything that opens is structurally sound.
fn verify(file: &str) -> Result<(), String> {
    let db = open(file)?;
    let m = db.store().metrics();
    println!("{file}: txn {}, {} pages, {} fragments", m.txn_id, m.page_count, m.fragments);
    println!(
        "  free {} reusable, {} pending readers, {} pending retention",
        m.free_pages_reusable, m.pages_pending_reclaim_reader, m.pages_pending_reclaim_retention
    );

    let catalog = db.catalog();
    for table in catalog.tables() {
        let fields: Vec<&str> = catalog.fields_of(table.id).map(|f| f.name.as_str()).collect();
        println!("  table {} ({})", table.name, fields.join(", "));
    }
    Ok(())
}

/// Retention for a time quantum field.
///
/// Offline and per file, which is the honest place for it today: dropping day views changes
/// what a window query answers, and a cluster where one node has dropped a day and another has
/// not would answer that window with part of the truth and no symptom. A cluster-wide spelling
/// belongs with the other DDL, which travels over the wire; this one is for a single file and
/// says so by living in the offline tool.
fn drop_days(file: &str, table: &str, field: &str, at: i64) -> Result<(), String> {
    let db = open(file)?;
    let dropped = db
        .drop_days_before(table, field, at)
        .map_err(|e| format!("could not drop days of {table}.{field}: {e}"))?;
    println!(
        "dropped {dropped} fragments from {table}.{field}, every day before {}",
        big_db::day_view(at)
    );
    println!("  the records are still there; what went is the per-day index over them");
    println!("  run `big compact {file}` to give the pages back to the filesystem");
    Ok(())
}

/// Recomputes every checksum in every tree.
///
/// `verify` proves the file opens, which checks the meta page and the three fixed chains. It
/// says nothing about the trees, because opening does not read them - and a branch or leaf
/// checksum is never checked on the query path either, since a crc over 8 KiB would be the
/// whole cost of a point read. So a rotted b-tree page is not an error when a query lands on
/// it; it is a different answer. This is the walk that finds that, and the reason it is a
/// separate subcommand is that it costs a full read of the live data while `verify` costs
/// almost nothing.
fn scrub(file: &str) -> Result<(), String> {
    let db = open(file)?;
    let found = db.scrub().map_err(|e| format!("{file} failed its scrub: {e}"))?;
    println!(
        "scrubbed {file}: {} tree pages and {} dense pages, {} in all, every checksum matched",
        found.pages,
        found.bitmaps,
        found.total()
    );
    Ok(())
}

/// Accounts for every page in the file: reachable, free, or neither.
///
/// `verify` already prints `page_count` and the free counters, but nothing ties them to each
/// other or to the size of the file. This does: it walks every tree the current roots and every
/// snapshot can reach, adds the meta pages, the four chains and the freelist, and reports what
/// is left over. **A page in neither set is one the database will never read and never hand out
/// again**, and one of those at the end of the file is enough to stop `compact` giving anything
/// back to the filesystem.
///
/// A separate subcommand for the same reason as `scrub`: it costs a full read of the live data,
/// while `verify` costs almost nothing. It only reads - there is no repair here, and the counts
/// it reports are exact as of the transaction it opened at rather than of any later one.
///
/// This is not a scrub. The walk parses pages without verifying their checksums, so run
/// `big scrub <file>` first if the file is under suspicion.
fn leaks(file: &str) -> Result<(), String> {
    let db = open(file)?;
    let r = db.audit_pages().map_err(|e| format!("{file} could not be audited: {e}"))?;

    println!("{file}: txn {}, {} pages", r.txn_id, r.page_count);
    println!(
        "  {} reachable ({} meta, {} chain, {} tree, {} snapshot chain, {} snapshot tree)",
        r.reachable,
        r.by_class.meta,
        r.by_class.chains,
        r.by_class.trees,
        r.by_class.snapshot_chains,
        r.by_class.snapshot_trees
    );
    println!(
        "  {} free ({} reusable now, {} still pending)",
        r.free_total,
        r.free_reusable,
        r.free_total - r.free_reusable
    );
    println!("  {} unaccounted for", r.leaked);

    if r.leaked > 0 {
        println!(
            "    highest is {}, of {} pages: {:?}",
            r.highest_leaked.map_or("none".to_string(), |p| p.to_string()),
            r.page_count,
            r.leaked_sample
        );
        println!("    `big compact {file}` rewrites the file without them");
    }
    if r.file_pages != r.page_count {
        println!(
            "  {} pages past what the meta page records ({} on disk against {})",
            r.beyond_meta, r.file_pages, r.page_count
        );
        println!("    a crash between a meta flip and a truncation leaves these; harmless, and `compact` clears them");
    }

    // Only these two are corruption rather than waste, and only these two fail the command.
    if r.double_allocated > 0 || r.dangling > 0 {
        return Err(format!(
            "{file} is corrupt: {} page(s) handed out twice ({:?}), {} reference(s) past the end of the file",
            r.double_allocated, r.double_allocated_sample, r.dangling
        ));
    }
    Ok(())
}

/// Opening a path that does not exist would *create* an empty database - the pager opens with
/// `create(true)` because that is what a server wants on first start. A tool that inspects or
/// copies must not, so the check is here rather than there.
fn open(file: &str) -> Result<Db<big_pager::MmapPager>, String> {
    if !std::path::Path::new(file).exists() {
        return Err(format!("no such file: {file}"));
    }
    Db::open_path(file).map_err(|e| format!("could not open {file}: {e}"))
}
