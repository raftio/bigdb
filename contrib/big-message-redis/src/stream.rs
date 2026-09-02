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

//! One connection to Redis, and the six commands a stream consumer needs.
//!
//! # Why a consumer group rather than a plain `XREAD`
//!
//! `XREAD` hands out entries and forgets them; the reader has to write down where it got to,
//! and where it writes that down is a second store to keep consistent with the first. A
//! consumer group makes Redis hold that: an entry delivered but not acknowledged stays in the
//! **pending list**, so a sink that dies mid-batch finds its unfinished work waiting rather
//! than having to reason about an offset it may not have flushed.
//!
//! That is also the whole reason the acknowledgement comes *after* bigdb answers. Ack first and
//! a crash loses the batch; ack after and a crash repeats it. Repeating is the failure this
//! sink can survive - see the crate documentation for what it costs.

use std::io::{BufReader, Write};
use std::net::TcpStream;
use std::time::Duration;

use crate::resp::{self, Value};

/// One entry: the id Redis gave it, and its fields in the order they were written.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Entry {
    /// `1700000000000-0`. Monotonic per stream, and unique - which is what makes it usable as
    /// the key a restarting sink deduplicates on.
    pub id: String,
    /// `field, value` pairs, flattened exactly as the wire carries them.
    pub fields: Vec<(String, String)>,
}

impl Entry {
    /// The value of one field, or `None` when the entry does not carry it.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.fields.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
}

/// A connection to one Redis server.
pub struct Redis {
    reader: BufReader<TcpStream>,
    socket: TcpStream,
}

impl Redis {
    /// Connects, and applies a deadline to both directions.
    ///
    /// The read timeout has to be longer than the longest `BLOCK` this sink will ask for, or a
    /// block that is working exactly as intended reads as a dead socket.
    pub fn connect(addr: &str, timeout: Duration) -> Result<Self, resp::Error> {
        let socket = TcpStream::connect(addr)?;
        socket.set_read_timeout(Some(timeout))?;
        socket.set_write_timeout(Some(timeout))?;
        // A command is one write and then a wait for the answer, so there is never a second
        // small write to coalesce with - only a delay to add.
        let _ = socket.set_nodelay(true);
        Ok(Self { reader: BufReader::new(socket.try_clone()?), socket })
    }

    /// Presents a password, for a server that wants one.
    pub fn auth(&mut self, password: &str) -> Result<(), resp::Error> {
        self.call(&[b"AUTH", password.as_bytes()]).map(|_| ())
    }

    /// Creates the group, and treats "it already exists" as the success it is.
    ///
    /// `MKSTREAM` so that a sink started before its producer does not fail on a stream nobody
    /// has written to yet. `$` starts the group at the end: entries written before the group
    /// existed are not this sink's to deliver, and a group that began at `0` would replay the
    /// whole stream the first time anyone started it.
    pub fn create_group(&mut self, stream: &str, group: &str) -> Result<bool, resp::Error> {
        match self.call(&[
            b"XGROUP",
            b"CREATE",
            stream.as_bytes(),
            group.as_bytes(),
            b"$",
            b"MKSTREAM",
        ]) {
            Ok(_) => Ok(true),
            // The ordinary path on every start after the first. Matched on the code Redis
            // chose rather than on the sentence after it.
            Err(resp::Error::Server(m)) if m.starts_with("BUSYGROUP") => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Entries never delivered to anyone, waiting up to `block` for at least one.
    ///
    /// `>` is what asks for new entries rather than for this consumer's pending ones. A block
    /// that expires answers nothing, which is not an error - a quiet stream is the normal state
    /// of most streams.
    pub fn read_group(
        &mut self,
        stream: &str,
        group: &str,
        consumer: &str,
        count: usize,
        block: Duration,
    ) -> Result<Vec<Entry>, resp::Error> {
        let count = count.to_string();
        let block = block.as_millis().to_string();
        let value = self.call(&[
            b"XREADGROUP",
            b"GROUP",
            group.as_bytes(),
            consumer.as_bytes(),
            b"COUNT",
            count.as_bytes(),
            b"BLOCK",
            block.as_bytes(),
            b"STREAMS",
            stream.as_bytes(),
            b">",
        ])?;
        entries_of(&value)
    }

    /// This consumer's own entries that were delivered and never acknowledged.
    ///
    /// `0` rather than `>`: the same command, asking for the pending list instead of for new
    /// work. This is what a sink reads first on start-up, because it is exactly the set whose
    /// fate is unknown - each of them may or may not have reached bigdb before the last process
    /// stopped.
    pub fn read_pending(
        &mut self,
        stream: &str,
        group: &str,
        consumer: &str,
        count: usize,
    ) -> Result<Vec<Entry>, resp::Error> {
        let count = count.to_string();
        let value = self.call(&[
            b"XREADGROUP",
            b"GROUP",
            group.as_bytes(),
            consumer.as_bytes(),
            b"COUNT",
            count.as_bytes(),
            b"STREAMS",
            stream.as_bytes(),
            b"0",
        ])?;
        entries_of(&value)
    }

    /// Takes over entries another consumer was given and has not finished.
    ///
    /// A consumer name is a process, and a process that dies leaves its pending entries owned by
    /// a name nobody is running any more. Without this they wait for ever for a consumer with
    /// that exact name to come back. Answers the entries claimed and the cursor to continue
    /// from, which is `0-0` when there are no more.
    pub fn autoclaim(
        &mut self,
        stream: &str,
        group: &str,
        consumer: &str,
        min_idle: Duration,
        from: &str,
        count: usize,
    ) -> Result<(String, Vec<Entry>), resp::Error> {
        let idle = min_idle.as_millis().to_string();
        let count = count.to_string();
        let value = self.call(&[
            b"XAUTOCLAIM",
            stream.as_bytes(),
            group.as_bytes(),
            consumer.as_bytes(),
            idle.as_bytes(),
            from.as_bytes(),
            b"COUNT",
            count.as_bytes(),
        ])?;
        // `[cursor, [entries], [deleted]]` on Redis 7; `[cursor, [entries]]` on 6.2. Read by
        // position with the third element optional, so both answer.
        let parts = value.array()?;
        let Some(cursor) = parts.first() else {
            return Err(resp::Error::Protocol("XAUTOCLAIM answered nothing".to_string()));
        };
        let cursor = cursor.text()?.to_string();
        let entries = match parts.get(1) {
            Some(list) => entries_in(list)?,
            None => Vec::new(),
        };
        Ok((cursor, entries))
    }

    /// Acknowledges entries, which removes them from the pending list.
    ///
    /// Answers how many were still pending, which is not always how many were named: an entry
    /// acknowledged twice counts once. That is a property worth having rather than a wrinkle -
    /// it is what makes a re-run of the same acknowledgement harmless.
    pub fn ack(&mut self, stream: &str, group: &str, ids: &[String]) -> Result<i64, resp::Error> {
        if ids.is_empty() {
            return Ok(0);
        }
        let mut args: Vec<&[u8]> = Vec::with_capacity(ids.len() + 3);
        args.push(b"XACK");
        args.push(stream.as_bytes());
        args.push(group.as_bytes());
        for id in ids {
            args.push(id.as_bytes());
        }
        match self.call(&args)? {
            Value::Int(n) => Ok(n),
            other => Err(resp::Error::Protocol(format!("XACK answered {other:?}"))),
        }
    }

    /// One command, and the answer to it.
    fn call(&mut self, args: &[&[u8]]) -> Result<Value, resp::Error> {
        resp::write_command(&mut self.socket, args)?;
        resp::read_reply(&mut self.reader)
    }
}

impl Write for Redis {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.socket.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.socket.flush()
    }
}

/// The entries out of an `XREADGROUP` answer: `[[stream, [entries]]]`.
///
/// One stream is asked for, so one is expected; a `Nil` - the block expired - reads as none.
fn entries_of(value: &Value) -> Result<Vec<Entry>, resp::Error> {
    let mut out = Vec::new();
    for stream in value.array()? {
        let pair = stream.array()?;
        let Some(list) = pair.get(1) else {
            return Err(resp::Error::Protocol("a stream with no entries element".to_string()));
        };
        out.extend(entries_in(list)?);
    }
    Ok(out)
}

/// A list of `[id, [field, value, ...]]`.
fn entries_in(value: &Value) -> Result<Vec<Entry>, resp::Error> {
    let mut out = Vec::new();
    for entry in value.array()? {
        let parts = entry.array()?;
        let Some(id) = parts.first() else {
            return Err(resp::Error::Protocol("an entry with no id".to_string()));
        };
        let id = id.text()?.to_string();
        // An entry whose fields were trimmed by `XDEL` arrives with a nil body rather than an
        // empty one. It carries nothing to write, so it is an entry with no fields - and the
        // sink acknowledges it rather than stopping on it.
        let flat = match parts.get(1) {
            Some(list) => list.array()?,
            None => &[],
        };
        let mut fields = Vec::with_capacity(flat.len() / 2);
        for pair in flat.chunks(2) {
            let [name, value] = pair else {
                return Err(resp::Error::Protocol(format!("entry {id} has a field with no value")));
            };
            fields.push((name.text()?.to_string(), value.text()?.to_string()));
        }
        out.push(Entry { id, fields });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(frame: &str) -> Vec<Entry> {
        let value = resp::read_value(&mut std::io::BufReader::new(frame.as_bytes())).unwrap();
        entries_of(&value).unwrap()
    }

    #[test]
    fn an_answer_becomes_the_entries_it_carries() {
        let frame = "*1\r\n*2\r\n$6\r\nevents\r\n*2\r\n\
             *2\r\n$6\r\n1700-0\r\n*4\r\n$6\r\namount\r\n$3\r\n100\r\n$7\r\ncountry\r\n$2\r\nGB\r\n\
             *2\r\n$6\r\n1700-1\r\n*2\r\n$6\r\namount\r\n$3\r\n900\r\n";
        let entries = parsed(frame);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].id, "1700-0");
        assert_eq!(entries[0].get("amount"), Some("100"));
        assert_eq!(entries[0].get("country"), Some("GB"));
        assert_eq!(entries[1].id, "1700-1");
        assert_eq!(entries[1].get("country"), None);
    }

    #[test]
    fn a_block_that_expired_is_no_entries_rather_than_an_error() {
        // What a quiet stream answers, which is the normal state of most streams.
        assert!(parsed("*-1\r\n").is_empty());
        assert!(parsed("*0\r\n").is_empty());
    }

    #[test]
    fn an_entry_whose_fields_were_deleted_carries_none_rather_than_failing() {
        let frame = "*1\r\n*2\r\n$6\r\nevents\r\n*1\r\n*2\r\n$6\r\n1700-0\r\n*-1\r\n";
        let entries = parsed(frame);
        assert_eq!(entries.len(), 1);
        assert!(entries[0].fields.is_empty());
    }

    #[test]
    fn a_field_with_no_value_is_refused_rather_than_half_read() {
        let frame = "*1\r\n*2\r\n$6\r\nevents\r\n*1\r\n*2\r\n$6\r\n1700-0\r\n*1\r\n$1\r\na\r\n";
        let value = resp::read_value(&mut std::io::BufReader::new(frame.as_bytes())).unwrap();
        assert!(entries_of(&value).is_err());
    }
}
