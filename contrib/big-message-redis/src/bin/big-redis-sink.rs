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

//! `big-redis-sink`: one stream, one table, until stopped.
//!
//! Argv is parsed by hand, as everywhere else in this repository. Secrets come from the
//! environment rather than from flags, because a flag is in the shell history and in `ps`.

use std::time::Duration;

use big_message::Config as ProducerConfig;
use big_message_redis::{Config, Kind, Mapping, Sink};

const USAGE: &str = "\
big-redis-sink - consume a Redis stream into a bigdb table

USAGE
  big-redis-sink --stream <name> --table <name> --map <spec>[,<spec>...] [options]

REQUIRED
  --stream <name>          the Redis stream to read
  --table <name>           the bigdb table to write
  --map <spec>[,<spec>]    <field>[:<kind>]=<column>, one per column written
                           kinds: text (default), int, signed, float, decimal, bool

WHERE
  --redis <host:port>      default 127.0.0.1:6379
  --addr <host:port>       bigdb, default 127.0.0.1:8080

GROUP
  --group <name>           consumer group, default big-sink
  --consumer <name>        this process in the group, default the hostname and pid
                           Two sinks must not share one: a consumer name owns a pending list.

DELIVERY
  --dedup-field <column>   write each entry's stream id here, and skip on restart what is
                           already written. Without it a restart re-sends its pending
                           entries, and because the server allocates record ids, that is
                           duplicate records rather than the same ones rewritten.
  --skip-incomplete        acknowledge and pass over an entry the mapping does not fit,
                           instead of stopping. Off by default: a mapping that does not fit
                           is usually wrong for every entry, not for one.
  --claim-after <seconds>  take over entries idle this long in another consumer's pending
                           list. Off by default.

PACE
  --batch <n>              entries per read, default 1000
  --block <seconds>        how long a read waits on a quiet stream, default 5
  --once                   stop when the stream goes quiet, rather than waiting for more

ENVIRONMENT
  BIG_CREDENTIALS          path to a file holding one `user:password` line, mode 600
  REDIS_PASSWORD           password for AUTH

EXIT
  0 the stream went quiet and --once was given   1 stopped   2 usage
";

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        print!("{USAGE}");
        return std::process::ExitCode::from(0);
    }

    let (config, once) = match parse(&args) {
        Ok(parsed) => parsed,
        Err(why) => {
            eprintln!("big-redis-sink: {why}");
            eprintln!("big-redis-sink: --help for usage");
            return std::process::ExitCode::from(2);
        }
    };

    let mut sink = match Sink::open(config, ProducerConfig::default()) {
        Ok(sink) => sink,
        Err(e) => {
            eprintln!("big-redis-sink: {e}");
            return std::process::ExitCode::from(1);
        }
    };

    // Before anything new is taken: whatever the last run left behind is this run's first work.
    if let Err(e) = sink.recover() {
        eprintln!("big-redis-sink: recovering: {e}");
        report(&sink);
        return std::process::ExitCode::from(1);
    }

    match sink.run(once) {
        Ok(_) => {
            report(&sink);
            std::process::ExitCode::from(0)
        }
        Err(e) => {
            eprintln!("big-redis-sink: {e}");
            report(&sink);
            std::process::ExitCode::from(1)
        }
    }
}

/// The counters, on stderr, so a pipe carries nothing this did not write to the table.
fn report(sink: &Sink) {
    let r = sink.report();
    eprintln!("big-redis-sink: written {}", r.written);
    eprintln!("big-redis-sink: acknowledged {}", r.acknowledged);
    if r.deduplicated > 0 {
        eprintln!("big-redis-sink: already written {}", r.deduplicated);
    }
    if r.skipped > 0 {
        eprintln!("big-redis-sink: skipped {}", r.skipped);
    }
}

/// One `user:password` line out of a file, refusing one anybody can read.
///
/// The same rule the server applies to its own files and the same one `bigctl` applies to its
/// credentials file, written a third time. That is the price of this crate having no
/// dependencies, and it is the price the empty `[dependencies]` section is there to pay.
fn read_credential(path: &str) -> Result<String, String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(path)
            .map_err(|e| format!("could not read {path}: {e}"))?
            .permissions()
            .mode()
            & 0o777;
        if mode & 0o077 != 0 {
            return Err(format!(
                "{path} is mode {mode:o}; a credentials file must not be readable by anyone \
                 else (chmod 600 {path})"
            ));
        }
    }
    let text = std::fs::read_to_string(path).map_err(|e| format!("could not read {path}: {e}"))?;
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if !line.contains(':') {
            return Err(format!("{path}: expected one `user:password` line"));
        }
        return Ok(line.to_string());
    }
    Err(format!("{path}: no credential in this file"))
}

fn parse(args: &[String]) -> Result<(Config, bool), String> {
    let mut redis = "127.0.0.1:6379".to_string();
    let mut addr = "127.0.0.1:8080".to_string();
    let mut stream = None;
    let mut table = None;
    let mut map: Vec<Mapping> = Vec::new();
    let mut group = "big-sink".to_string();
    let mut consumer = None;
    let mut dedup_field = None;
    let mut batch = 1000usize;
    let mut block = 5u64;
    let mut claim_after = None;
    let mut skip_incomplete = false;
    let mut once = false;

    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        let value = || -> Result<String, String> {
            args.get(i + 1)
                .cloned()
                .filter(|v| !v.starts_with("--"))
                .ok_or_else(|| format!("{arg} needs a value"))
        };
        match arg {
            "--redis" => redis = value()?,
            "--addr" => addr = value()?,
            "--stream" => stream = Some(value()?),
            "--table" => table = Some(value()?),
            "--group" => group = value()?,
            "--consumer" => consumer = Some(value()?),
            "--dedup-field" => dedup_field = Some(value()?),
            "--skip-incomplete" => {
                skip_incomplete = true;
                i += 1;
                continue;
            }
            "--once" => {
                once = true;
                i += 1;
                continue;
            }
            "--batch" => {
                batch = value()?.parse().map_err(|_| "--batch takes a number".to_string())?
            }
            "--block" => {
                block = value()?.parse().map_err(|_| "--block takes seconds".to_string())?
            }
            "--claim-after" => {
                let seconds: u64 =
                    value()?.parse().map_err(|_| "--claim-after takes seconds".to_string())?;
                claim_after = Some(Duration::from_secs(seconds));
            }
            "--map" => {
                for spec in value()?.split(',') {
                    map.push(Mapping::parse(spec.trim())?);
                }
            }
            other => return Err(format!("{other} is not an option")),
        }
        i += 2;
    }

    let stream = stream.ok_or("--stream is required")?;
    let table = table.ok_or("--table is required")?;
    if map.is_empty() {
        return Err(format!(
            "--map is required: <field>[:<kind>]=<column>, kinds {}",
            Kind::ALL.join(", ")
        ));
    }
    if batch == 0 {
        return Err("--batch 0 would read nothing".to_string());
    }

    Ok((
        Config {
            redis,
            password: std::env::var("REDIS_PASSWORD").ok(),
            stream,
            group,
            consumer: consumer.unwrap_or_else(default_consumer),
            addr,
            // **A path, not the credential itself.** The old `BIG_TOKEN` here held the raw
            // token while `bigctl`'s held a path, which meant one name with two meanings across
            // two programs that talk to the same server. `BIG_CREDENTIALS` is a path everywhere.
            credential: match std::env::var("BIG_CREDENTIALS") {
                Err(_) => None,
                Ok(path) => Some(read_credential(&path)?),
            },
            table,
            map,
            dedup_field,
            batch,
            block: Duration::from_secs(block),
            claim_after,
            skip_incomplete,
        },
        once,
    ))
}

/// A name that is this process and not another.
///
/// The hostname alone would be shared by two sinks on one machine, and a consumer name owns a
/// pending list - so sharing one means each recovering the other's unfinished work.
fn default_consumer() -> String {
    let host = std::env::var("HOSTNAME").unwrap_or_else(|_| "host".to_string());
    format!("{host}-{}", std::process::id())
}
