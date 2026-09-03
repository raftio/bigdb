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

//! Messages in, `INSERT`s out.
//!
//! # The retry rule, which is the opposite of the loader's
//!
//! `bigctl import` retries every transport failure, and `docs/ingest-plan.md` explains why it
//! is allowed to: the line format makes the caller choose the record id, every fact is a `set`
//! at that address, and so sending a chunk twice writes the same bits twice - which is writing
//! them once. Idempotency is a property of the format, not a discipline the loader keeps.
//!
//! **An `INSERT` that leaves the record to the server has the opposite property.** Sending it
//! twice allocates twice and writes two records. So this producer retries only what it can
//! prove never arrived - a connection that would not open, a request whose write did not
//! finish, and a `503`, which `big_http::shed` writes from the accepting thread without ever
//! giving the connection a worker, so the body is not read and the statement cannot have run.
//!
//! Everything else stops the producer:
//!
//! | | |
//! |---|---|
//! | [`Error::Connect`] | retried first, reported when the retries run out |
//! | [`Error::Unknown`] | **never retried** - the batch may or may not be in the table |
//! | [`Error::Refused`] | never retried; the server understood it and said no |
//! | [`Error::Protocol`] | never retried; the outcome is as unknown as [`Error::Unknown`] |
//!
//! # Why a stopped producer stays stopped
//!
//! A refusal here is almost always systematic rather than about one message: the column list is
//! fixed for the producer's whole life, so `unknown_field` and `unknown_table` will refuse the
//! next batch exactly as they refused this one. And after an [`Error::Unknown`] there is a batch
//! whose fate nobody knows, which is not a state to keep writing on top of. Either way the
//! answer is a person, so the producer holds the error and hands it back to every later call
//! rather than pretending the next one might go differently.

use core::time::Duration;
use std::time::Instant;

use crate::batch::{Batch, Fit};
use crate::config::Config;
use crate::error::Error;
use crate::http::{Conn, Failure};
use crate::json;
use crate::value::Value;

/// The route. One, for the whole crate.
const TARGET: &str = "/sql";

/// How long to wait before sending again what provably never arrived.
const FIRST_BACKOFF: Duration = Duration::from_millis(250);

/// How long that wait is allowed to grow to.
const MAX_BACKOFF: Duration = Duration::from_secs(8);

/// What one flush wrote.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Flushed {
    /// Rows the server reported writing, which is the statement's own row count.
    ///
    /// From [`Producer::flush`] this is that one flush. From [`Producer::close`] it is every
    /// flush the producer ever made - see there.
    ///
    /// **Not record ids, and there is no way to ask for those.** See the crate documentation.
    pub inserted: u64,
}

/// A stream of messages into one table.
pub struct Producer {
    conn: Conn,
    batch: Batch,
    linger: Duration,
    retries: u32,
    /// When the batch took its first row, which is what [`Producer::due`] measures from.
    since: Option<Instant>,
    /// How many messages have been taken, so a refused one can be named by its place.
    seen: usize,
    /// Everything acknowledged so far, which is what [`Producer::close`] answers.
    written: u64,
    /// Set once, by the first failure that ends the producer. Every later call gets it back.
    stopped: Option<Error>,
}

impl Producer {
    /// Opens a producer for one table and one fixed list of columns.
    ///
    /// The column list is fixed because one statement has one, and it is checked here rather
    /// than at the first flush: a name that cannot be written is a mistake in the caller's
    /// setup, and finding out at start-up is worth more than finding out an hour in.
    ///
    /// **`_record_id` is refused, in any case.** Naming it would turn an allocating statement
    /// into one that addresses records itself, which is the thing this crate does not do.
    pub fn open(
        addr: &str,
        table: &str,
        columns: &[&str],
        credential: Option<&str>,
        config: Config,
    ) -> Result<Self, Error> {
        if config.max_rows == 0 {
            return Err(Error::Value("a batch of no rows would never be sent".to_string()));
        }
        let batch = Batch::new(table, columns, config.max_rows, config.max_bytes)?;
        Ok(Self {
            conn: Conn::new(addr, credential, Some(config.io_timeout)),
            batch,
            linger: config.linger,
            retries: config.retries,
            since: None,
            seen: 0,
            written: 0,
            stopped: None,
        })
    }

    /// Takes one message, whose values are in the order the columns were named.
    ///
    /// Rendered into the pending statement immediately, which is why [`Value`] may borrow: the
    /// borrow ends when this returns. Sends when the batch fills, and when it has been open
    /// longer than the configured linger.
    pub fn send(&mut self, values: &[Value<'_>]) -> Result<(), Error> {
        self.check()?;

        match self.batch.offer(values)? {
            Fit::Added => {}
            Fit::TooLarge(len) => return Err(self.too_large(len)),
            Fit::Full => {
                self.flush()?;
                match self.batch.offer(values)? {
                    Fit::Added => {}
                    Fit::TooLarge(len) => return Err(self.too_large(len)),
                    // Unreachable: an empty batch answers `TooLarge` rather than `Full`, and
                    // `open` refuses a `max_rows` of zero. Reported rather than panicked,
                    // because a library that aborts a caller's process to make a point about
                    // its own invariants is a library nobody can wrap.
                    Fit::Full => {
                        return Err(Error::Value(
                            "a message fits no batch, empty or otherwise".to_string(),
                        ))
                    }
                }
            }
        }

        self.seen += 1;
        match self.since {
            None => self.since = Some(Instant::now()),
            Some(started) if started.elapsed() >= self.linger => {
                self.flush()?;
            }
            Some(_) => {}
        }
        Ok(())
    }

    /// Whether the pending batch has waited longer than the linger.
    ///
    /// For a caller whose messages arrive in bursts: [`Producer::send`] can only notice the time
    /// when it is called, so a stream that goes quiet leaves its last few rows pending. A loop
    /// that blocks on its source with a timeout checks this when the timeout expires.
    pub fn due(&self) -> bool {
        self.since.is_some_and(|started| started.elapsed() >= self.linger)
    }

    /// How many rows are waiting to be sent.
    pub fn pending(&self) -> usize {
        self.batch.rows()
    }

    /// Sends whatever is pending.
    ///
    /// Nothing pending is not an error and not a request: it answers zero without touching the
    /// connection.
    pub fn flush(&mut self) -> Result<Flushed, Error> {
        self.check()?;
        if self.batch.is_empty() {
            return Ok(Flushed::default());
        }

        let rows = self.batch.rows() as u64;
        let result = self.post(rows);

        match result {
            Ok(flushed) => {
                self.batch.clear();
                self.since = None;
                self.written += flushed.inserted;
                Ok(flushed)
            }
            Err(e) => {
                // The batch is deliberately left as it is. Whether those rows can be sent again
                // is exactly the question the caller has to answer, and clearing them here would
                // answer it for them by throwing the rows away.
                self.stopped = Some(e.clone());
                Err(e)
            }
        }
    }

    /// Everything this producer has had acknowledged, across every flush.
    pub fn written(&self) -> u64 {
        self.written
    }

    /// Sends whatever is pending and closes the connection.
    ///
    /// **Answers the producer's whole life, not the last flush.** That is the difference between
    /// this and [`Producer::flush`], and it is deliberate: a caller asking a closing producer
    /// what it did means all of it, and answering with the tail of the last batch is an answer
    /// that looks right and is off by every batch before it.
    pub fn close(mut self) -> Result<Flushed, Error> {
        self.flush()?;
        Ok(Flushed { inserted: self.written })
    }

    /// One statement, with the retries that are provably safe and no others.
    fn post(&mut self, rows: u64) -> Result<Flushed, Error> {
        let mut attempt = 0u32;
        let mut wait = FIRST_BACKOFF;

        loop {
            let sent = self.conn.send("POST", TARGET, self.batch.statement());
            let again = attempt < self.retries;

            match sent {
                Ok(response) if response.ok() => {
                    // The server answers with the statement's own row count, so a body that will
                    // not parse is not a reason to report a different number - it is a reason to
                    // report the number already known.
                    let inserted = json::inserted(&response.body).unwrap_or(rows);
                    return Ok(Flushed { inserted });
                }

                // Written from the accepting thread, before the connection is given a worker -
                // so the body was never read and the statement never ran. The one refusal in
                // this crate that is safe to send again.
                Ok(response) if response.status == 503 && again => {}

                Ok(response) => {
                    let (code, message) = json::failure(&response.body);
                    return Err(Error::Refused { status: response.status, code, message });
                }

                Err(Failure::NotSent(why)) if again => {
                    let _ = why;
                }
                Err(Failure::NotSent(why)) => return Err(Error::Connect(why)),

                Err(Failure::Unknown(why)) => return Err(Error::Unknown(why)),
                Err(Failure::Protocol(why)) => return Err(Error::Protocol(why)),
            }

            attempt += 1;
            std::thread::sleep(wait);
            wait = (wait * 2).min(MAX_BACKOFF);
        }
    }

    fn check(&self) -> Result<(), Error> {
        match &self.stopped {
            Some(e) => Err(e.clone()),
            None => Ok(()),
        }
    }

    fn too_large(&self, len: usize) -> Error {
        Error::MessageTooLarge { at: self.seen, len, cap: self.batch.max_bytes() }
    }
}
