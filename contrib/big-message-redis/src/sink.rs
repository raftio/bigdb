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

//! The loop: entries out of a stream, rows into a table, acknowledgements after the write.
//!
//! # The order, which is the whole design
//!
//! `XREADGROUP` → send → **flush** → `XACK`. Every part of that order is load-bearing:
//!
//! - Acknowledging *before* the write would lose a batch to a crash. Acknowledging after repeats
//!   one instead, and repeating is the failure this sink can survive.
//! - The acknowledgement names exactly the entries the flush covered, which is why the sink
//!   flushes explicitly rather than letting the producer's linger decide. A producer that
//!   flushed on its own schedule would leave the sink acknowledging entries whose rows were
//!   still in a buffer.
//! - An [`Error::Unknown`] acknowledges nothing and stops. Those
//!   entries stay pending, which is the honest state: nobody knows whether they landed.
//!
//! # Where the mapping lives, and why it is the operator's
//!
//! A stream entry is `field value` pairs a producer chose; a table has columns a schema chose.
//! Nothing in either knows about the other, and this crate deliberately does not guess: it reads
//! no schema, infers no types, and matches no names by accident. The mapping is declared, and a
//! field the mapping does not name is not written.
//!
//! That is the same rule `bigctl` keeps - the client adds no vocabulary - reached from the other
//! side. What a value *means* is still the server's to decide: a [`Kind`] chooses which literal
//! to write, and `big_embed::fact::from_literal` decides whether that literal fits the field.

use std::time::Duration;

use big_message::{Config as ProducerConfig, Error, Producer, Reader, Value};

use crate::resp;
use crate::stream::{Entry, Redis};

/// Which literal a stream field is written as.
///
/// **Not a field kind.** The schema decides what a column holds; this decides how the text in a
/// stream is spelled on its way there, which is the only part a client is entitled to an opinion
/// about. `Text` is the default because a stream carries text and most columns are keyed.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Kind {
    #[default]
    Text,
    Int,
    Signed,
    Float,
    Decimal,
    Bool,
}

impl Kind {
    /// The spelling an operator writes in `--map`.
    pub fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "text" => Self::Text,
            "int" => Self::Int,
            "signed" => Self::Signed,
            "float" => Self::Float,
            "decimal" => Self::Decimal,
            "bool" => Self::Bool,
            _ => return None,
        })
    }

    /// Every spelling, for a usage message that cannot drift from [`Kind::parse`].
    pub const ALL: [&'static str; 6] = ["text", "int", "signed", "float", "decimal", "bool"];

    /// One stream value as the literal this kind writes.
    ///
    /// A value that will not read as the number it was declared to be is refused here rather
    /// than sent: the server would refuse the whole batch for it, and this way the entry that
    /// carried it is the thing named.
    fn value<'a>(self, text: &'a str) -> Result<Value<'a>, String> {
        Ok(match self {
            Self::Text => Value::Text(text),
            Self::Int => {
                Value::Int(text.parse().map_err(|_| format!("{text:?} is not a whole number"))?)
            }
            Self::Signed => {
                Value::Signed(text.parse().map_err(|_| format!("{text:?} is not a whole number"))?)
            }
            Self::Float => {
                Value::Float(text.parse().map_err(|_| format!("{text:?} is not a number"))?)
            }
            Self::Decimal => Value::Decimal(text),
            Self::Bool => match text {
                "true" | "1" => Value::Bool(true),
                "false" | "0" => Value::Bool(false),
                _ => return Err(format!("{text:?} is not true or false")),
            },
        })
    }
}

/// One stream field, and the column it is written to.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Mapping {
    /// The name in the stream entry.
    pub field: String,
    /// The column in the table.
    pub column: String,
    pub kind: Kind,
}

impl Mapping {
    /// `<field>[:<kind>]=<column>`, which is what an operator writes.
    pub fn parse(text: &str) -> Result<Self, String> {
        let Some((left, column)) = text.split_once('=') else {
            return Err(format!("{text:?} is not <field>[:<kind>]=<column>"));
        };
        if column.is_empty() {
            return Err(format!("{text:?} names no column"));
        }
        let (field, kind) = match left.split_once(':') {
            Some((field, kind)) => (
                field,
                Kind::parse(kind)
                    .ok_or_else(|| format!("{kind:?} is not a kind: {}", Kind::ALL.join(", ")))?,
            ),
            None => (left, Kind::Text),
        };
        if field.is_empty() {
            return Err(format!("{text:?} names no field"));
        }
        Ok(Self { field: field.to_string(), column: column.to_string(), kind })
    }
}

/// What a sink was told to do.
pub struct Config {
    pub redis: String,
    pub password: Option<String>,
    pub stream: String,
    pub group: String,
    /// This process's name in the group. Two sinks must not share one: a consumer name owns a
    /// pending list, and two processes sharing a name would each recover the other's work.
    pub consumer: String,
    pub addr: String,
    pub token: Option<String>,
    pub table: String,
    pub map: Vec<Mapping>,
    /// The column the entry's own stream id is written to.
    ///
    /// **What turns a restart from "writes duplicates" into "writes none".** Without it the
    /// pending entries a restart inherits have to be re-sent blind, because nothing in the table
    /// says whether they already landed - the record ids are the server's and are never handed
    /// out. With it, the sink asks once, for exactly the pending set, and skips what is there.
    pub dedup_field: Option<String>,
    /// Entries asked for per read.
    pub batch: usize,
    /// How long a read waits for the stream to say something.
    pub block: Duration,
    /// How long another consumer's entry must have been idle before this one takes it over.
    /// `None` never claims.
    pub claim_after: Option<Duration>,
    /// Whether an entry missing a mapped field is acknowledged and passed over, rather than
    /// stopping the sink.
    pub skip_incomplete: bool,
}

/// What the loop did.
#[derive(Clone, Copy, Default, Debug)]
pub struct Report {
    pub written: u64,
    pub acknowledged: u64,
    pub skipped: u64,
    /// Entries a restart found already in the table and acknowledged without rewriting.
    pub deduplicated: u64,
}

/// What stopped it.
#[derive(Debug)]
pub enum Stopped {
    Redis(resp::Error),
    Bigdb(Error),
    /// An entry the mapping does not fit, when `skip_incomplete` is off.
    Entry(String),
}

impl std::fmt::Display for Stopped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Redis(e) => write!(f, "redis: {e}"),
            Self::Bigdb(e) => write!(f, "bigdb: {e}"),
            Self::Entry(m) => write!(f, "{m}"),
        }
    }
}

impl From<resp::Error> for Stopped {
    fn from(e: resp::Error) -> Self {
        Self::Redis(e)
    }
}

impl From<Error> for Stopped {
    fn from(e: Error) -> Self {
        Self::Bigdb(e)
    }
}

/// The columns a producer is opened with: the mapped ones, and the dedup column last.
fn columns(config: &Config) -> Vec<String> {
    let mut out: Vec<String> = config.map.iter().map(|m| m.column.clone()).collect();
    if let Some(field) = &config.dedup_field {
        out.push(field.clone());
    }
    out
}

/// A running sink.
pub struct Sink {
    redis: Redis,
    producer: Producer,
    reader: Option<Reader>,
    config: Config,
    report: Report,
}

impl Sink {
    /// Connects to both sides and makes sure the group exists.
    pub fn open(config: Config, producer: ProducerConfig) -> Result<Self, Stopped> {
        if config.map.is_empty() {
            return Err(Stopped::Entry("a sink with no --map writes nothing".to_string()));
        }

        // Longer than the block, or a read doing exactly what it was told reads as a dead
        // socket. This is the one timeout that cannot simply be "the usual thirty seconds".
        let redis_timeout = config.block + Duration::from_secs(30);
        let mut redis = Redis::connect(&config.redis, redis_timeout)?;
        if let Some(password) = &config.password {
            redis.auth(password)?;
        }
        redis.create_group(&config.stream, &config.group)?;

        let names = columns(&config);
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let producer_handle = Producer::open(
            &config.addr,
            &config.table,
            &refs,
            config.token.as_deref(),
            producer.clone(),
        )?;
        let reader = config
            .dedup_field
            .as_ref()
            .map(|_| Reader::open(&config.addr, config.token.as_deref(), &producer));

        Ok(Self { redis, producer: producer_handle, reader, config, report: Report::default() })
    }

    /// Deals with whatever the last run left pending, before taking anything new.
    ///
    /// Without `dedup_field` this re-sends them, which is the at-least-once bargain: an entry
    /// that landed and was not acknowledged becomes a second record. With it, the sink asks the
    /// table which of them are already there - a question bounded by the pending set, which is
    /// bounded by what one process had in flight - and acknowledges those without rewriting.
    pub fn recover(&mut self) -> Result<(), Stopped> {
        let pending = self.redis.read_pending(
            &self.config.stream,
            &self.config.group,
            &self.config.consumer,
            self.config.batch,
        )?;
        if pending.is_empty() {
            return Ok(());
        }

        let (done, todo) = self.split_written(pending)?;
        if !done.is_empty() {
            let ids: Vec<String> = done.iter().map(|e| e.id.clone()).collect();
            self.redis.ack(&self.config.stream, &self.config.group, &ids)?;
            self.report.deduplicated += ids.len() as u64;
            self.report.acknowledged += ids.len() as u64;
        }
        if !todo.is_empty() {
            self.write(&todo)?;
        }
        Ok(())
    }

    /// One pass: read, write, acknowledge. Answers how many entries it handled.
    ///
    /// Zero means the block expired with nothing to take, which is the normal state of a quiet
    /// stream rather than a reason to stop.
    pub fn step(&mut self) -> Result<usize, Stopped> {
        if let Some(idle) = self.config.claim_after {
            self.claim(idle)?;
        }
        let entries = self.redis.read_group(
            &self.config.stream,
            &self.config.group,
            &self.config.consumer,
            self.config.batch,
            self.config.block,
        )?;
        if entries.is_empty() {
            return Ok(0);
        }
        let taken = entries.len();
        self.write(&entries)?;
        Ok(taken)
    }

    /// Runs until the stream is quiet for a whole block, or for ever when `once` is false.
    pub fn run(&mut self, once: bool) -> Result<Report, Stopped> {
        loop {
            let taken = self.step()?;
            if once && taken == 0 {
                return Ok(self.report);
            }
        }
    }

    pub fn report(&self) -> Report {
        self.report
    }

    /// Entries another consumer was given and has not finished, taken over.
    fn claim(&mut self, idle: Duration) -> Result<(), Stopped> {
        let (_, claimed) = self.redis.autoclaim(
            &self.config.stream,
            &self.config.group,
            &self.config.consumer,
            idle,
            "0-0",
            self.config.batch,
        )?;
        if claimed.is_empty() {
            return Ok(());
        }
        let (done, todo) = self.split_written(claimed)?;
        if !done.is_empty() {
            let ids: Vec<String> = done.iter().map(|e| e.id.clone()).collect();
            self.redis.ack(&self.config.stream, &self.config.group, &ids)?;
            self.report.deduplicated += ids.len() as u64;
            self.report.acknowledged += ids.len() as u64;
        }
        if !todo.is_empty() {
            self.write(&todo)?;
        }
        Ok(())
    }

    /// Which of these the table already holds, and which it does not.
    ///
    /// Everything is "does not" when there is no dedup column, because without one there is no
    /// question to ask: a record id would answer it and the server never hands one out.
    fn split_written(&mut self, entries: Vec<Entry>) -> Result<(Vec<Entry>, Vec<Entry>), Stopped> {
        let (Some(field), Some(reader)) = (&self.config.dedup_field, self.reader.as_mut()) else {
            return Ok((Vec::new(), entries));
        };
        let ids: Vec<&str> = entries.iter().map(|e| e.id.as_str()).collect();
        let seen = reader.seen(&self.config.table, field, &ids)?;
        let (done, todo) = entries.into_iter().partition(|e| seen.contains(&e.id));
        Ok((done, todo))
    }

    /// Sends these entries, flushes them, and only then acknowledges exactly them.
    fn write(&mut self, entries: &[Entry]) -> Result<(), Stopped> {
        let mut acked: Vec<String> = Vec::with_capacity(entries.len());
        let mut values: Vec<Value<'_>> = Vec::with_capacity(self.config.map.len() + 1);

        for entry in entries {
            values.clear();
            let mut incomplete = None;
            for mapping in &self.config.map {
                let Some(text) = entry.get(&mapping.field) else {
                    incomplete =
                        Some(format!("entry {} carries no field {:?}", entry.id, mapping.field));
                    break;
                };
                match mapping.kind.value(text) {
                    Ok(value) => values.push(value),
                    Err(why) => {
                        incomplete =
                            Some(format!("entry {} field {:?}: {why}", entry.id, mapping.field));
                        break;
                    }
                }
            }

            if let Some(why) = incomplete {
                if !self.config.skip_incomplete {
                    return Err(Stopped::Entry(why));
                }
                // Acknowledged as well as skipped: an entry left pending would be delivered
                // again for ever, and it will be as wrong the next time.
                self.report.skipped += 1;
                acked.push(entry.id.clone());
                continue;
            }

            if self.config.dedup_field.is_some() {
                values.push(Value::Text(&entry.id));
            }
            self.producer.send(&values)?;
            acked.push(entry.id.clone());
        }

        // **Before the acknowledgement, always.** The producer may have flushed on its own when
        // a batch filled; this is what makes the rest durable, so that every id below names a
        // row the server has taken.
        let flushed = self.producer.flush()?;
        self.report.written += flushed.inserted;

        let acknowledged = self.redis.ack(&self.config.stream, &self.config.group, &acked)?;
        let _ = acknowledged;
        self.report.acknowledged += acked.len() as u64;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mapping_reads_the_way_an_operator_writes_one() {
        assert_eq!(
            Mapping::parse("amount:int=amount").unwrap(),
            Mapping { field: "amount".into(), column: "amount".into(), kind: Kind::Int }
        );
        // No kind is text, because a stream carries text and most columns are keyed.
        assert_eq!(
            Mapping::parse("country=cc").unwrap(),
            Mapping { field: "country".into(), column: "cc".into(), kind: Kind::Text }
        );
    }

    #[test]
    fn a_mapping_that_says_nothing_useful_is_refused_with_what_is_wrong() {
        for bad in ["amount", "=column", "field=", "amount:nope=amount", ""] {
            assert!(Mapping::parse(bad).is_err(), "{bad:?} must not parse");
        }
        let err = Mapping::parse("amount:nope=amount").unwrap_err();
        assert!(err.contains("int"), "the refusal lists the kinds: {err}");
    }

    #[test]
    fn a_value_that_is_not_the_kind_it_was_declared_is_refused_at_the_entry() {
        assert!(Kind::Int.value("abc").is_err());
        assert!(Kind::Int.value("-1").is_err(), "an unsigned column takes no sign");
        assert!(Kind::Signed.value("-1").is_ok());
        assert!(Kind::Bool.value("yes").is_err());
        assert_eq!(Kind::Bool.value("1").unwrap(), Value::Bool(true));
        assert_eq!(Kind::Text.value("anything at all").unwrap(), Value::Text("anything at all"));
    }

    #[test]
    fn the_dedup_column_is_written_last_so_the_mapped_ones_keep_their_order() {
        let config = Config {
            redis: String::new(),
            password: None,
            stream: String::new(),
            group: String::new(),
            consumer: String::new(),
            addr: String::new(),
            token: None,
            table: String::new(),
            map: vec![
                Mapping::parse("amount:int=amount").unwrap(),
                Mapping::parse("country=cc").unwrap(),
            ],
            dedup_field: Some("msg_id".to_string()),
            batch: 1,
            block: Duration::ZERO,
            claim_after: None,
            skip_incomplete: false,
        };
        assert_eq!(columns(&config), vec!["amount", "cc", "msg_id"]);
    }
}
