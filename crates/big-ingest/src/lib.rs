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

//! `bigi` - one file, one route, many requests.
//!
//! `bigc` promises one request per subcommand, which is what keeps it from growing a second
//! query surface. That promise is also why it cannot load a file: `POST /import` is bounded at
//! `big_http::MAX_BODY`, 8 MiB, so anything larger has to be sent as several requests and
//! several requests is a loop. This binary is that loop and nothing else.
//!
//! It adds no vocabulary either. It does not know what a field is, does not parse a value, does
//! not ask for the schema. A line goes to the server as bytes and a refusal comes back as the
//! server's own code and sentence - the same property `bigc` has, kept by the same means: the
//! only dependency is `big-cli`, whose own `[dependencies]` section is empty.
//!
//! **What makes the loop safe is not care taken here.** Every fact is a bit set at a record id
//! the caller wrote into the line - `set_int`, `set_key`, `set_bool`, never an increment - so
//! sending a chunk twice writes what sending it once wrote. That is why a dropped connection
//! can be retried, why an interrupted load can resume at an offset, and why this binary must
//! never invent a record id: a loader that did could not be run twice.

#![deny(unsafe_code)]

pub mod args;
pub mod checkpoint;
pub mod chunk;
pub mod progress;

pub use args::{Input, Options, Verb};
pub use checkpoint::Checkpoint;
pub use chunk::{Chunk, Chunker};

use big_cli::exit;
use big_cli::http::{Client, Error as HttpError};
use big_cli::json::{self, Failure, Value};
use progress::Progress;
use std::io::{BufRead, BufReader, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::time::Duration;

/// How long the first retry waits. Each one after it waits twice as long, up to a ceiling: a
/// server that is restarting takes seconds, and hammering it during those seconds is how a
/// client turns a blip into an outage.
const FIRST_BACKOFF: Duration = Duration::from_millis(250);
const MAX_BACKOFF: Duration = Duration::from_secs(8);

/// The streams a run works over, so that a test can supply its own.
pub struct Io<'a> {
    /// Where `-` reads from.
    pub input: &'a mut dyn BufRead,
    /// The summary. One `key value` line each, so `awk` can have it.
    pub out: &'a mut dyn Write,
    /// Progress, notes, and refusals.
    pub err: &'a mut dyn Write,
    /// Whether `err` is a terminal, which decides whether progress redraws one line or writes
    /// plain ones. Passed in rather than asked, because in a test it is a `Vec<u8>`.
    pub tty: bool,
}

/// Parses, loads, prints. Returns the process's exit code.
///
/// What is left here is the loop and nothing else: read a chunk, send it, write down where it
/// got to. Everything on either side of that - the token, the file, the checkpoint, the retry -
/// is one named step below, each returning the exit code it would have printed.
pub fn run(args: &[String], io: &mut Io<'_>, env: &dyn Fn(&str) -> Option<String>) -> i32 {
    let options = match args::parse(args, env) {
        Ok(o) => o,
        // `--help` is not a failure: usage to stdout, exit zero. The split `bigd` and `bigc`
        // both make.
        Err(e) if e.is_empty() => {
            let _ = write!(io.out, "{}", args::USAGE);
            return exit::OK;
        }
        Err(e) => {
            let _ = writeln!(io.err, "bigi: {e}\n");
            let _ = write!(io.err, "{}", args::USAGE);
            return exit::USAGE;
        }
    };

    let token = match token_of(&options, io.err) {
        Ok(t) => t,
        Err(code) => return code,
    };

    let target = format!("/table/{}/{}", big_cli::escape(&options.table), options.verb.as_str());

    // Opened before the reader is built, so that the size, the checkpoint and the seek are
    // settled while there is still one owner of the file.
    let (opened, name, total) = match open_input(&options.input, io.err) {
        Ok(o) => o,
        Err(code) => return code,
    };

    // What a resumed run carries forward: where to start, and the totals from its earlier legs,
    // so the summary is about the load rather than about this attempt.
    let checkpoint_path = options.resume.as_ref().map(PathBuf::from);
    let resumed =
        match resume_from(checkpoint_path.as_deref(), &target, &name, total, &options, io.err) {
            Ok(r) => r,
            Err(code) => return code,
        };
    let Resumed { start, lines: mut lines_done, wrote: mut wrote_total } = resumed;

    let mut source: Box<dyn BufRead + '_> = match opened {
        Some(mut file) => {
            if start > 0 {
                if let Err(e) = file.seek(SeekFrom::Start(start)) {
                    let _ = writeln!(io.err, "bigi: could not seek {name} to {start}: {e}");
                    return exit::USAGE;
                }
            }
            Box::new(BufReader::with_capacity(1 << 20, file))
        }
        None => Box::new(&mut *io.input),
    };

    let client = Client { addr: options.addr.clone(), token, timeout: options.timeout };
    let mut chunker = Chunker::new(&mut source, options.chunk_bytes, options.chunk_lines, start);
    let mut bar = Progress::new(options.progress.unwrap_or(io.tty), io.tty, total);

    let mut offset = start;
    let mut chunks = 0u64;
    let mut reported: Vec<String> = Vec::new();

    loop {
        let chunk = match chunker.next_chunk() {
            Ok(Some(c)) => c,
            Ok(None) => break,
            Err(e) => {
                bar.clear(io.err);
                let _ = writeln!(io.err, "bigi: {e}");
                return exit::USAGE;
            }
        };
        // Where this chunk begins, which is what an operator needs when the server refuses it.
        let began = chunk.end - chunk.body.len() as u64;
        chunks += 1;

        if options.dry_run {
            offset = chunk.end;
            lines_done += chunk.lines as u64;
            bar.tick(io.err, offset, lines_done);
            continue;
        }

        let response =
            match post_chunk(&client, &target, &chunk.body, options.retries, &mut bar, io.err) {
                Ok(r) => r,
                Err(code) => {
                    let _ = writeln!(io.err, "bigi: stopped at byte {began} of {name}");
                    return code;
                }
            };

        if !response.ok() {
            // Never retried. A chunk the server understood and rejected is rejected identically
            // the second time, and the operator is owed the sentence rather than the delay.
            bar.clear(io.err);
            let failure = Failure::read(&response.body);
            let _ = writeln!(io.err, "bigi: {} [{}]", failure.message, failure.code);
            let _ = writeln!(
                io.err,
                "bigi: stopped at byte {began} of {name}; {} {} before it",
                progress::thousands(wrote_total),
                options.verb.noun()
            );
            return exit::REFUSED;
        }

        let (count, missed) = outcome(&response.body, options.verb);
        wrote_total += count;
        lines_done += chunk.lines as u64;
        offset = chunk.end;

        // A copy that did not take the write is a divergence, not a retry: sending the chunk
        // again reaches the same copies. `POST /repair` is what closes it, so the note says so -
        // once per distinct copy, because a load of ten thousand chunks would otherwise print
        // the same sentence ten thousand times.
        for note in missed {
            if !reported.contains(&note) {
                bar.clear(io.err);
                let _ = writeln!(
                    io.err,
                    "bigi: a copy did not take this write: {note} (run `bigc repair`)"
                );
                reported.push(note);
            }
        }

        if let Some(path) = &checkpoint_path {
            let record = Checkpoint {
                target: target.clone(),
                input: name.clone(),
                size: total.unwrap_or_default(),
                offset,
                lines: lines_done,
                wrote: wrote_total,
            };
            if let Err(code) = write_checkpoint(&record, path, &mut bar, io.err) {
                return code;
            }
        }

        bar.tick(io.err, offset, lines_done);
    }

    bar.finish(io.err, offset, lines_done);

    // A finished load's checkpoint is removed. Left behind, the same command run again would
    // resume at the end of the file and report a load that did nothing - which looks exactly
    // like a load that worked. Removing it means a re-run really re-runs, which is safe here
    // for the reason everything else in this file is safe.
    if let Some(path) = &checkpoint_path {
        clear_checkpoint(path, io.err);
    }

    let verb = if options.dry_run { "would send" } else { options.verb.noun() };
    let _ = writeln!(io.out, "{verb} {wrote_total}");
    let _ = writeln!(io.out, "lines {lines_done}");
    let _ = writeln!(io.out, "chunks {chunks}");
    let _ = writeln!(io.out, "bytes {}", offset - start);
    let _ = writeln!(io.out, "elapsed {:.1}", bar.elapsed().as_secs_f64());
    exit::OK
}

/// What a resumed run carries forward from its earlier legs.
///
/// The totals travel with the offset because the summary is about the *load*, not about this
/// attempt: a run that resumes at byte nine million and writes one more fact has written nine
/// million and one.
struct Resumed {
    /// The byte to start reading at.
    start: u64,
    /// Lines already sent.
    lines: u64,
    /// Facts already written, as the server counted them.
    wrote: u64,
}

/// The bearer token, read from the file the options named.
///
/// The same reader `bigc` uses, including its refusal of a token file anyone can read.
fn token_of(options: &Options, err: &mut dyn Write) -> Result<Option<String>, i32> {
    let Some(path) = &options.token_file else { return Ok(None) };
    match big_cli::read_token(path) {
        Ok(t) => Ok(Some(t)),
        Err(e) => {
            let _ = writeln!(err, "bigi: {e}");
            Err(exit::USAGE)
        }
    }
}

/// Opens the input and measures it, before anything else holds it.
///
/// The size is taken here rather than later so that the checkpoint and the seek are settled
/// while there is still one owner of the file. Standard input has no size and no name, which is
/// also why it cannot be resumed.
fn open_input(
    input: &Input,
    err: &mut dyn Write,
) -> Result<(Option<std::fs::File>, String, Option<u64>), i32> {
    let Input::Path(path) = input else { return Ok((None, "-".to_string(), None)) };
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) => {
            let _ = writeln!(err, "bigi: could not open {path}: {e}");
            return Err(exit::USAGE);
        }
    };
    match file.metadata() {
        Ok(m) => Ok((Some(file), path.clone(), Some(m.len()))),
        Err(e) => {
            let _ = writeln!(err, "bigi: could not measure {path}: {e}");
            Err(exit::USAGE)
        }
    }
}

/// Where a resumed load picks up, and what it has done already.
///
/// A checkpoint that does not describe *this* load - a different route, a different file, a
/// file that has changed size - is refused rather than resumed from: continuing at an offset
/// into the wrong file is a load that silently skips its beginning.
fn resume_from(
    path: Option<&std::path::Path>,
    target: &str,
    name: &str,
    total: Option<u64>,
    options: &Options,
    err: &mut dyn Write,
) -> Result<Resumed, i32> {
    let nothing = Resumed { start: 0, lines: 0, wrote: 0 };
    let Some(path) = path else { return Ok(nothing) };
    let found = match Checkpoint::read(path) {
        Ok(None) => return Ok(nothing),
        Ok(Some(f)) => f,
        Err(e) => {
            let _ = writeln!(err, "bigi: {e}");
            return Err(exit::USAGE);
        }
    };
    let offset = match found.resume_at(target, name, total.unwrap_or_default()) {
        Ok(o) => o,
        Err(why) => {
            let _ = writeln!(err, "bigi: {}: {why}", path.display());
            return Err(exit::USAGE);
        }
    };
    if offset > 0 {
        let _ = writeln!(
            err,
            "bigi: resuming {name} at byte {offset} ({} already {})",
            progress::thousands(found.wrote),
            options.verb.noun()
        );
    }
    Ok(Resumed { start: offset, lines: found.lines, wrote: found.wrote })
}

/// One chunk, sent, with the retries the options allow.
///
/// **Only an unreachable server is retried**, and the reason is the property this whole binary
/// rests on: a chunk is a set of bits at record ids the caller wrote down, so sending it twice
/// writes what sending it once wrote. A connection that died - possibly *after* the server
/// committed - is therefore free to re-send. A `Protocol` error is not retried, because what
/// came back was not an answer this client can read and it will not be one next time either.
fn post_chunk(
    client: &Client,
    target: &str,
    body: &str,
    retries: u32,
    bar: &mut Progress,
    err: &mut dyn Write,
) -> Result<big_cli::http::Response, i32> {
    let mut attempt = 0u32;
    let mut wait = FIRST_BACKOFF;
    loop {
        match client.send("POST", target, body) {
            Ok(r) => return Ok(r),
            Err(HttpError::Unreachable(why)) if attempt < retries => {
                attempt += 1;
                bar.clear(err);
                let _ = writeln!(
                    err,
                    "bigi: {why}; retrying in {} ({attempt} of {retries})",
                    progress::short(wait)
                );
                std::thread::sleep(wait);
                wait = (wait * 2).min(MAX_BACKOFF);
            }
            Err(e) => {
                bar.clear(err);
                let _ = writeln!(err, "bigi: {e}");
                return Err(exit::UNREACHABLE);
            }
        }
    }
}

/// Writes down how far the load has got.
///
/// Called *after* the acknowledgement, never before. Dying between the two resends one chunk,
/// which is free; writing first would skip one, which is not. A checkpoint that cannot be
/// written stops the load rather than letting it continue with no way to resume - the whole
/// point of the file is the run that gets interrupted.
fn write_checkpoint(
    record: &Checkpoint,
    path: &std::path::Path,
    bar: &mut Progress,
    err: &mut dyn Write,
) -> Result<(), i32> {
    let Err(e) = record.write(path) else { return Ok(()) };
    bar.clear(err);
    let _ = writeln!(err, "bigi: {e}");
    let _ = writeln!(
        err,
        "bigi: the load reached byte {} and stops here rather than continuing without a way \
         to resume",
        record.offset
    );
    Err(exit::USAGE)
}

/// Removes a finished load's checkpoint.
///
/// Left behind, the same command run again would resume at the end of the file and report a
/// load that did nothing - which looks exactly like a load that worked. Removing it means a
/// re-run really re-runs, which is safe here for the reason everything else in this file is.
fn clear_checkpoint(path: &std::path::Path, err: &mut dyn Write) {
    if let Err(e) = std::fs::remove_file(path) {
        if e.kind() != std::io::ErrorKind::NotFound {
            let _ =
                writeln!(err, "bigi: the load finished; could not remove {}: {e}", path.display());
        }
    }
}

/// What the server said it wrote, and which copies did not take it.
///
/// `big-cli`'s reader rather than a second one: the writer on the other end is
/// `big_http::json::wrote`, and two readers for one writer is how the two ends drift. A body
/// this cannot read counts as nothing written rather than as a failure - the server answered
/// `2xx`, so the facts are in, and a summary that is short is better than a load that stops.
fn outcome(body: &str, verb: Verb) -> (u64, Vec<String>) {
    let Ok(value) = json::parse(body) else {
        return (0, Vec::new());
    };
    let count = value
        .get(verb.noun())
        .and_then(Value::cell)
        .and_then(|c| c.parse::<u64>().ok())
        .unwrap_or(0);
    let missed = match value.get("missed") {
        Some(Value::Arr(items)) => items.iter().filter_map(Value::cell).collect(),
        _ => Vec::new(),
    };
    (count, missed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_ordinary_answer_is_read() {
        assert_eq!(outcome("{\"imported\":42}", Verb::Import), (42, Vec::new()));
    }

    #[test]
    fn a_copy_that_missed_the_write_is_carried_out() {
        let (count, missed) =
            outcome("{\"imported\":2,\"missed\":[\"node-b: connection refused\"]}", Verb::Import);
        assert_eq!(count, 2);
        assert_eq!(missed, vec!["node-b: connection refused".to_string()]);
    }

    #[test]
    fn the_delete_route_is_read_under_its_own_name() {
        assert_eq!(outcome("{\"deleted\":7}", Verb::Delete), (7, Vec::new()));
        // And not under the other one, which would silently report zero for every chunk.
        assert_eq!(outcome("{\"deleted\":7}", Verb::Import).0, 0);
    }

    /// A `2xx` whose body this cannot read still wrote the facts. Reporting a short total is a
    /// worse summary; stopping the load would be a worse outcome.
    #[test]
    fn an_unreadable_body_does_not_stop_a_load() {
        assert_eq!(outcome("not json at all", Verb::Import), (0, Vec::new()));
    }
}
