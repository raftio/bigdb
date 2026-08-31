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

//! How far in, on stderr.
//!
//! A load can run for an hour, so silence is the wrong default for a person and the right one
//! for a log. The split is made on whether stderr is a terminal: a terminal gets one line
//! rewritten in place with `\r`, and anything else gets a plain line every few seconds that
//! `grep` and `journalctl` can hold. No colour, no cursor movement, no bar - `\r` is the whole
//! of the terminal handling, because everything past it is a termios mode this repository has
//! already declined once for `bigc shell`.
//!
//! On **stderr** rather than stdout so that the summary a script reads is not interleaved with
//! the noise a person watches.

use std::io::Write;
use std::time::{Duration, Instant};

/// How often the line is redrawn. Faster than this is a redraw a person cannot read, and on a
/// slow terminal it is a redraw that costs more than the chunk did.
const TERMINAL_EVERY: Duration = Duration::from_millis(200);

/// How often a plain line is written when nothing is watching. Long, because these accumulate
/// in a log for the whole length of the load.
const LOG_EVERY: Duration = Duration::from_secs(10);

/// Reports how far a load has got.
pub struct Progress {
    on: bool,
    tty: bool,
    /// The input's size, or `None` for a pipe - where there is no percentage and no estimate,
    /// and saying so is better than inventing one.
    total: Option<u64>,
    started: Instant,
    last: Option<Instant>,
    /// Whether a `\r` line is currently on screen and owes an erase before anything else prints.
    dirty: bool,
}

impl Progress {
    pub fn new(on: bool, tty: bool, total: Option<u64>) -> Self {
        Self { on, tty, total, started: Instant::now(), last: None, dirty: false }
    }

    /// How long the load has been running, which the summary reports whether or not progress was.
    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// Called after every chunk. Prints at most as often as the interval allows.
    pub fn tick(&mut self, err: &mut dyn Write, done: u64, lines: u64) {
        if !self.on {
            return;
        }
        let every = if self.tty { TERMINAL_EVERY } else { LOG_EVERY };
        if let Some(last) = self.last {
            if last.elapsed() < every {
                return;
            }
        }
        self.last = Some(Instant::now());
        self.draw(err, done, lines, false);
    }

    /// The last line, drawn whatever the interval says, and then got out of the way.
    pub fn finish(&mut self, err: &mut dyn Write, done: u64, lines: u64) {
        if !self.on {
            return;
        }
        self.draw(err, done, lines, true);
    }

    /// Erases the in-place line, so that a refusal or a note is not printed on top of it.
    pub fn clear(&mut self, err: &mut dyn Write) {
        if self.dirty {
            let _ = write!(err, "\r{:80}\r", "");
            let _ = err.flush();
            self.dirty = false;
        }
    }

    fn draw(&mut self, err: &mut dyn Write, done: u64, lines: u64, last: bool) {
        let secs = self.started.elapsed().as_secs_f64().max(0.001);
        let rate = done as f64 / secs;

        // A pipe has no total, so it gets neither a percentage nor an estimate. Inventing one
        // from "how much has arrived so far" would be a number that means nothing and looks
        // like it means something.
        let line = match self.total.filter(|t| *t > 0) {
            Some(total) => {
                let percent = (done as f64 / total as f64 * 100.0).min(100.0);
                let eta = if rate > 0.0 && total > done {
                    format!(" eta {}", short(Duration::from_secs_f64((total - done) as f64 / rate)))
                } else {
                    String::new()
                };
                format!(
                    "{} / {} {percent:.0}% {} lines {}/s{eta}",
                    bytes(done),
                    bytes(total),
                    thousands(lines),
                    bytes(rate as u64)
                )
            }
            None => format!("{} {} lines {}/s", bytes(done), thousands(lines), bytes(rate as u64)),
        };

        if self.tty {
            let _ = write!(err, "\rbigi: {line:<72}");
            self.dirty = true;
            if last {
                let _ = writeln!(err);
                self.dirty = false;
            }
        } else {
            let _ = writeln!(err, "bigi: {line}");
        }
        let _ = err.flush();
    }
}

/// Powers of 1024, because the ceiling this is measured against is `8 << 20`.
pub fn bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// `14000000` is unreadable and `14,000,000` is not, and the separator never reaches stdout -
/// the summary a script parses prints the number itself.
pub fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// A duration a person reads at a glance rather than one that is precise.
pub fn short(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        0..=59 => format!("{}s", s.max(1)),
        60..=3599 => format!("{}m{:02}s", s / 60, s % 60),
        _ => format!("{}h{:02}m", s / 3600, (s % 3600) / 60),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_read_the_way_the_ceiling_is_written() {
        assert_eq!(bytes(0), "0 B");
        assert_eq!(bytes(999), "999 B");
        assert_eq!(bytes(8 << 20), "8.0 MiB");
        assert_eq!(bytes(7 << 20), "7.0 MiB");
    }

    #[test]
    fn thousands_are_grouped() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_000), "1,000");
        assert_eq!(thousands(14_000_000), "14,000,000");
    }

    #[test]
    fn a_duration_is_readable_at_every_scale() {
        assert_eq!(short(Duration::from_millis(10)), "1s");
        assert_eq!(short(Duration::from_secs(45)), "45s");
        assert_eq!(short(Duration::from_secs(125)), "2m05s");
        assert_eq!(short(Duration::from_secs(7_800)), "2h10m");
    }

    /// Off is off: a load with `--no-progress` writes nothing to stderr until something is wrong.
    #[test]
    fn progress_that_is_off_prints_nothing() {
        let mut out = Vec::new();
        let mut p = Progress::new(false, true, Some(100));
        p.tick(&mut out, 50, 5);
        p.finish(&mut out, 100, 10);
        assert!(out.is_empty(), "{}", String::from_utf8_lossy(&out));
    }

    #[test]
    fn a_pipe_gets_plain_lines_and_no_percentage() {
        let mut out = Vec::new();
        let mut p = Progress::new(true, false, None);
        p.finish(&mut out, 2048, 10);
        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with("bigi: "), "{text}");
        assert!(text.ends_with('\n'), "{text:?}");
        assert!(!text.contains('%'), "a pipe has no total to be a percentage of: {text}");
        assert!(!text.contains('\r'), "{text:?}");
    }
}
