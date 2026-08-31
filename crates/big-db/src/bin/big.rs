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

//! `big` - the offline half of operating a database: back it up, check it, shrink it.
//!
//! Deliberately separate from `bigd`. Every subcommand here takes the exclusive lock, so
//! running one against a served database fails immediately and says so, rather than doing
//! something clever behind the daemon's back.

// Unix only, like `bigd`: every subcommand works on a file through `MmapPager`, and the
// engine has no other file backend. Gating it would trade one confusing build error for
// another, so the tool follows the repository's existing convention instead.
use big_db::{copy, Db};

const USAGE: &str = "\
usage: big <command> [args]

  backup <file> <dest>   write a consistent copy of <file> to <dest>
                         safe while a writer is running; <dest> must not exist
  restore <src> <dest>   copy a backup into place; <dest> must not exist
  compact <file>         rewrite <file> as a compact copy of itself, in place
                         offline: nothing else may have the file open
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
";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();

    let outcome = match refs.as_slice() {
        ["backup", file, dest] | ["restore", file, dest] => backup(file, dest),
        ["compact", file] => compact(file),
        ["verify", file] => verify(file),
        ["scrub", file] => scrub(file),
        ["drop-days", file, table, field, before] => drop_days(file, table, field, before),
        _ => {
            eprint!("{USAGE}");
            std::process::exit(2);
        }
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
fn drop_days(file: &str, table: &str, field: &str, before: &str) -> Result<(), String> {
    let at: i64 = before
        .parse()
        .map_err(|_| format!("<unix-seconds> must be a number of seconds, got `{before}`"))?;
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

/// Opening a path that does not exist would *create* an empty database - the pager opens with
/// `create(true)` because that is what a server wants on first start. A tool that inspects or
/// copies must not, so the check is here rather than there.
fn open(file: &str) -> Result<Db<big_pager::MmapPager>, String> {
    if !std::path::Path::new(file).exists() {
        return Err(format!("no such file: {file}"));
    }
    Db::open_path(file).map_err(|e| format!("could not open {file}: {e}"))
}
