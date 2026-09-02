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

//! One file, one route, many requests.
//!
//! Every other `bigctl` subcommand is exactly one request, which is what keeps the client from
//! growing a second query surface. `import` and `delete` cannot be: `POST /import` is bounded
//! at [`big_http::MAX_BODY`], so anything larger has to be sent as several requests, and
//! several requests is a loop. This module is that loop and nothing else - it was its own
//! binary, `bigi`, for exactly as long as that difference seemed to need one.
//!
//! It adds no vocabulary either. It does not know what a field is, does not parse a value, does
//! not ask for the schema. A line goes to the server as bytes and a refusal comes back as the
//! server's own code and sentence.
//!
//! **What makes the loop safe is not care taken here.** Every fact is a bit set at a record id
//! the caller wrote into the line - `set_int`, `set_key`, `set_bool`, never an increment - so
//! sending a chunk twice writes what sending it once wrote. That is why a dropped connection
//! can be retried, why an interrupted load can resume at an offset, and why this binary must
//! never invent a record id: a loader that did could not be run twice.

pub mod args;
pub mod checkpoint;
pub mod chunk;
pub mod progress;

pub use args::{Input, Load, Verb};
pub use checkpoint::Checkpoint;
pub use chunk::{Chunk, Chunker};

use crate::client::args::Format;
use crate::client::http::{Client, Error as HttpError};
use crate::client::json::{self, Answer, Failure, Value};
use crate::client::{escape, render};
use crate::{exit, Io};
use progress::Progress;
use std::io::{BufRead, BufReader, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::time::Duration;

/// How long the first retry waits. Each one after it waits twice as long, up to a ceiling: a
/// server that is restarting takes seconds, and hammering it during those seconds is how a
/// client turns a blip into an outage.
const FIRST_BACKOFF: Duration = Duration::from_millis(250);
const MAX_BACKOFF: Duration = Duration::from_secs(8);

/// Parses, loads, prints. Returns the process's exit code.
///
/// What is left here is the loop and nothing else: read a chunk, send it, write down where it
/// got to. Everything on either side of that - the token, the file, the checkpoint, the retry -
/// is one named step below, each returning the exit code it would have printed.
#[allow(clippy::too_many_arguments)]
pub fn run(
    client: &Client,
    verb: Verb,
    table: &str,
    input: &Input,
    load: &Load,
    io: &mut Io<'_>,
    format: Format,
) -> i32 {
    let target = format!("/table/{}/{}", escape(table), verb.as_str());

    // Opened before the reader is built, so that the size, the checkpoint and the seek are
    // settled while there is still one owner of the file.
    let (opened, name, total) = match open_input(input, io.err) {
        Ok(o) => o,
        Err(code) => return code,
    };

    // What a resumed run carries forward: where to start, and the totals from its earlier legs,
    // so the summary is about the load rather than about this attempt.
    let checkpoint_path = load.resume.as_ref().map(PathBuf::from);
    let resumed = match resume_from(checkpoint_path.as_deref(), &target, &name, total, verb, io.err)
    {
        Ok(r) => r,
        Err(code) => return code,
    };
    let Resumed { start, lines: mut lines_done, wrote: mut wrote_total } = resumed;

    let mut source: Box<dyn BufRead + '_> = match opened {
        Some(mut file) => {
            if start > 0 {
                if let Err(e) = file.seek(SeekFrom::Start(start)) {
                    let _ = writeln!(io.err, "bigctl: could not seek {name} to {start}: {e}");
                    return exit::USAGE;
                }
            }
            Box::new(BufReader::with_capacity(1 << 20, file))
        }
        None => Box::new(&mut *io.input),
    };

    let mut chunker = Chunker::new(&mut source, load.chunk_bytes, load.chunk_lines, start);
    let mut bar = Progress::new(load.progress.unwrap_or(io.err_tty), io.err_tty, total);

    let mut offset = start;
    let mut chunks = 0u64;
    let mut reported: Vec<String> = Vec::new();

    // **A window of requests, retired in the order they were sent.**
    //
    // The server parses a body before it can commit it, and those are different resources: with
    // one request in flight the parse of the next cannot begin until the commit of the last has
    // finished, and one of the two machines is idle throughout. A second request in flight lets
    // them overlap. It does *not* make the writes concurrent - the engine has a single writer,
    // and that is a design decision rather than a lock waiting to be removed.
    //
    // **FIFO retirement is what keeps `--resume` honest.** Acks can arrive in any order, but a
    // checkpoint is a single offset meaning "everything before this is written" - so it may only
    // advance across a *contiguous* run of successes. Joining in send order gives that for free:
    // the checkpoint is written after each chunk retires, and a failure stops the walk there,
    // leaving the offset at the last chunk for which every earlier one also succeeded.
    //
    // Chunks still in flight when that happens may well land on the server. That is safe for the
    // reason the whole file rests on: a fact is a bit set at a record id the caller wrote, so a
    // resend writes what the first send wrote. The resumed run sends them again and is right.
    let depth = load.in_flight;
    let mut code = exit::OK;

    std::thread::scope(|scope| {
        let mut window: std::collections::VecDeque<Flight<'_>> = std::collections::VecDeque::new();
        let mut drained = false;

        loop {
            while !drained && window.len() < depth {
                match chunker.next_chunk() {
                    Ok(Some(chunk)) => {
                        // Where this chunk begins, which is what an operator needs when the
                        // server refuses it.
                        let began = chunk.end - chunk.body.len() as u64;
                        chunks += 1;

                        if load.dry_run {
                            offset = chunk.end;
                            lines_done += chunk.lines as u64;
                            bar.tick(io.err, offset, lines_done);
                            continue;
                        }

                        let client = &client;
                        let target = target.as_str();
                        let retries = load.retries;
                        let body = chunk.body;
                        window.push_back(Flight {
                            began,
                            end: chunk.end,
                            lines: chunk.lines,
                            handle: scope.spawn(move || post_chunk(client, target, &body, retries)),
                        });
                    }
                    Ok(None) => drained = true,
                    Err(e) => {
                        bar.clear(io.err);
                        let _ = writeln!(io.err, "bigctl: {e}");
                        code = exit::USAGE;
                        // Not an immediate return: what is already in flight has been sent, and
                        // retiring it is what lets the checkpoint record how far the load got.
                        drained = true;
                    }
                }
            }

            let Some(flight) = window.pop_front() else { break };
            let sent = flight.handle.join().expect("a request thread must not panic");

            // Said here rather than in the thread, so notes from several requests do not
            // interleave halfway through each other's lines.
            for note in &sent.notes {
                bar.clear(io.err);
                let _ = writeln!(io.err, "bigctl: {note}");
            }

            let response = match sent.result {
                Ok(r) => r,
                Err(c) => {
                    let _ = writeln!(io.err, "bigctl: stopped at byte {} of {name}", flight.began);
                    code = c;
                    break;
                }
            };

            if !response.ok() {
                // Never retried. A chunk the server understood and rejected is rejected
                // identically the second time, and the operator is owed the sentence rather
                // than the delay.
                bar.clear(io.err);
                let failure = Failure::read(&response.body);
                let _ = writeln!(io.err, "bigctl: {} [{}]", failure.message, failure.code);
                let _ = writeln!(
                    io.err,
                    "bigctl: stopped at byte {} of {name}; {} {} before it",
                    flight.began,
                    progress::thousands(wrote_total),
                    verb.noun()
                );
                code = exit::REFUSED;
                break;
            }

            let (count, missed) = outcome(&response.body, verb);
            wrote_total += count;
            lines_done += flight.lines as u64;
            offset = flight.end;

            // A copy that did not take the write is a divergence, not a retry: sending the
            // chunk again reaches the same copies. `POST /repair` is what closes it, so the
            // note says so - once per distinct copy, because a load of ten thousand chunks
            // would otherwise print the same sentence ten thousand times.
            for note in missed {
                if !reported.contains(&note) {
                    bar.clear(io.err);
                    let _ = writeln!(
                        io.err,
                        "bigctl: a copy did not take this write: {note} (run `bigctl repair`)"
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
                if let Err(c) = write_checkpoint(&record, path, &mut bar, io.err) {
                    code = c;
                    break;
                }
            }

            bar.tick(io.err, offset, lines_done);
        }
        // Whatever is left in the window is joined by the scope on the way out. Its results are
        // deliberately dropped: they are chunks after the one that failed, and a checkpoint that
        // counted them would skip the failure on resume.
    });

    if code != exit::OK {
        return code;
    }

    bar.finish(io.err, offset, lines_done);

    // A finished load's checkpoint is removed. Left behind, the same command run again would
    // resume at the end of the file and report a load that did nothing - which looks exactly
    // like a load that worked. Removing it means a re-run really re-runs, which is safe here
    // for the reason everything else in this file is safe.
    if let Some(path) = &checkpoint_path {
        clear_checkpoint(path, io.err);
    }

    // **stdout carries what the server said; stderr carries what the loop did.** That is the
    // same split the rest of this client already makes - a records cursor is a note, and
    // `Answer::notes` is documented as being about the answer rather than in it. A load's
    // `chunks` and `elapsed` are numbers no server body ever held, and a script asking how many
    // facts landed should not have to skip four lines it did not ask for.
    //
    // It also means `--format` finally means something here. It used to be accepted and
    // dropped, which is the exact failure `only()` exists to refuse one level up.
    let column = if load.dry_run { "would_send" } else { verb.noun() };
    if format == Format::Json {
        // Before `render::answer`, which has an `unreachable!()` on this arm: the raw body is
        // what `json` promises, and there is no raw body for a summary this side invented.
        let _ = writeln!(io.out, "{{\"{column}\":{wrote_total}}}");
    } else {
        let answer = Answer {
            columns: vec![column.to_string()],
            rows: vec![vec![wrote_total.to_string()]],
            notes: Vec::new(),
        };
        let _ = write!(io.out, "{}", render::answer(&answer, format));
    }

    let _ = writeln!(io.err, "bigctl: lines {lines_done}");
    let _ = writeln!(io.err, "bigctl: chunks {chunks}");
    let _ = writeln!(io.err, "bigctl: bytes {}", offset - start);
    let _ = writeln!(io.err, "bigctl: elapsed {:.1}", bar.elapsed().as_secs_f64());
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
            let _ = writeln!(err, "bigctl: could not open {path}: {e}");
            return Err(exit::USAGE);
        }
    };
    match file.metadata() {
        Ok(m) => Ok((Some(file), path.clone(), Some(m.len()))),
        Err(e) => {
            let _ = writeln!(err, "bigctl: could not measure {path}: {e}");
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
    verb: Verb,
    err: &mut dyn Write,
) -> Result<Resumed, i32> {
    let nothing = Resumed { start: 0, lines: 0, wrote: 0 };
    let Some(path) = path else { return Ok(nothing) };
    let found = match Checkpoint::read(path) {
        Ok(None) => return Ok(nothing),
        Ok(Some(f)) => f,
        Err(e) => {
            let _ = writeln!(err, "bigctl: {e}");
            return Err(exit::USAGE);
        }
    };
    let offset = match found.resume_at(target, name, total.unwrap_or_default()) {
        Ok(o) => o,
        Err(why) => {
            let _ = writeln!(err, "bigctl: {}: {why}", path.display());
            return Err(exit::USAGE);
        }
    };
    if offset > 0 {
        let _ = writeln!(
            err,
            "bigctl: resuming {name} at byte {offset} ({} already {})",
            progress::thousands(found.wrote),
            verb.noun()
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
/// One request the window is waiting on.
struct Flight<'s> {
    began: u64,
    end: u64,
    lines: usize,
    handle: std::thread::ScopedJoinHandle<'s, Sent>,
}

/// A request's outcome, and whatever it wanted to say on the way.
///
/// Collected rather than printed, because a request now runs on its own thread and two of them
/// writing retry notes at once would interleave mid-line.
struct Sent {
    result: Result<crate::client::http::Response, i32>,
    notes: Vec<String>,
}

fn post_chunk(client: &Client, target: &str, body: &str, retries: u32) -> Sent {
    let mut attempt = 0u32;
    let mut wait = FIRST_BACKOFF;
    let mut notes = Vec::new();
    loop {
        match client.send("POST", target, body) {
            Ok(r) => return Sent { result: Ok(r), notes },
            Err(HttpError::Unreachable(why)) if attempt < retries => {
                attempt += 1;
                notes.push(format!(
                    "{why}; retrying in {} ({attempt} of {retries})",
                    progress::short(wait)
                ));
                std::thread::sleep(wait);
                wait = (wait * 2).min(MAX_BACKOFF);
            }
            Err(e) => {
                notes.push(e.to_string());
                return Sent { result: Err(exit::UNREACHABLE), notes };
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
    let _ = writeln!(err, "bigctl: {e}");
    let _ = writeln!(
        err,
        "bigctl: the load reached byte {} and stops here rather than continuing without a way \
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
            let _ = writeln!(
                err,
                "bigctl: the load finished; could not remove {}: {e}",
                path.display()
            );
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
