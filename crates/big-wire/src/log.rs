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

//! One JSON object per line, on stderr.
//!
//! **Why there is no logging crate here.** `tracing` and `log` are facades, and a facade earns
//! its keep when many crates need to emit through one configurable sink. In this tree exactly
//! one layer has anything to say: the edge. Nothing below it swallows an error - every layer
//! returns `Result` and the edge is where a failure stops being a value and becomes a status -
//! so a logging facade wired through twelve crates would be paying for a generality nobody
//! uses. If that stops being true, this module is the thing to replace, not to extend.
//!
//! **Why JSON rather than a human line.** The one question this log has to answer is "what
//! happened to request X", and answering it means a filter over a field. Prose has to be
//! re-parsed to be filtered; an object does not. The timestamp is RFC 3339 so a person reading
//! it directly still gets a readable one.
//!
//! **Why stderr.** It is unbuffered by default, it survives a process that dies badly, and it
//! is what every supervisor already collects. A file would mean rotation, and rotation is the
//! supervisor's job.

use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

/// How much the server says. Set once from `BIG_LOG`, ordered so a level enables everything
/// below it.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Level {
    /// Nothing at all, not even errors.
    Off = 0,
    /// Only what failed.
    Error = 1,
    /// Failures, and what was refused or shed.
    Warn = 2,
    /// The above, plus one line per request. The default.
    Info = 3,
    /// The above, plus detail useful only when something is being diagnosed.
    Debug = 4,
}

impl Level {
    fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Error => "error",
            Self::Warn => "warn",
            Self::Info => "info",
            Self::Debug => "debug",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "off" | "none" | "silent" => Self::Off,
            "error" => Self::Error,
            "warn" | "warning" => Self::Warn,
            "info" => Self::Info,
            "debug" | "trace" => Self::Debug,
            _ => return None,
        })
    }
}

/// The configured level, read from `BIG_LOG` once.
///
/// Once, not per line: a log level that can change under a running server is a feature nobody
/// asked for and a syscall on every request.
pub fn level() -> Level {
    static LEVEL: OnceLock<Level> = OnceLock::new();
    *LEVEL.get_or_init(|| {
        // An unset variable means the default; a *misspelled* one means the operator meant
        // something and this is the last moment anyone will notice. It is not worth failing
        // to start over, so it says so at the level it is falling back to.
        match std::env::var("BIG_LOG") {
            Ok(v) => Level::parse(&v).unwrap_or_else(|| {
                eprintln!(
                    "{{\"level\":\"warn\",\"event\":\"bad_log_level\",\
                     \"msg\":\"BIG_LOG={v} is not a level; using info\"}}"
                );
                Level::Info
            }),
            Err(_) => Level::Info,
        }
    })
}

/// Whether a line at this level would be printed. Worth checking before building an
/// expensive one.
pub fn enabled(l: Level) -> bool {
    l <= level() && level() != Level::Off
}

/// A process-unique prefix for request ids.
///
/// Without it two runs of the server both start counting at one, and a log aggregator holding
/// both cannot tell request 7 from request 7.
fn nonce() -> u32 {
    static NONCE: OnceLock<u32> = OnceLock::new();
    *NONCE.get_or_init(|| {
        SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.subsec_nanos())
            ^ std::process::id()
    })
}

/// The next request id. Monotonic within a process, prefixed to be unique across processes.
pub fn next_request_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{:08x}-{n}", nonce())
}

/// One field of a log line. Numbers and booleans go out unquoted so a consumer does not have
/// to un-string them to compare.
pub enum F<'a> {
    /// A string, quoted and escaped.
    S(&'a str),
    /// A number, unquoted.
    N(u64),
    /// A boolean, unquoted.
    B(bool),
}

impl F<'_> {
    fn render(&self) -> String {
        match self {
            Self::S(s) => crate::json::string(s),
            Self::N(n) => n.to_string(),
            Self::B(b) => b.to_string(),
        }
    }
}

/// Writes one line, if `level` passes the filter.
///
/// The whole line is built and then written with a single `write_all` on a held lock. Two
/// threads finishing at the same moment must not interleave halves of two objects, which is
/// what a sequence of small writes would eventually produce.
pub fn emit(level: Level, event: &str, fields: &[(&str, F<'_>)]) {
    if !enabled(level) {
        return;
    }
    let mut line = format!(
        "{{\"ts\":{},\"level\":\"{}\",\"event\":{}",
        json_now(),
        level.as_str(),
        crate::json::string(event)
    );
    for (name, value) in fields {
        line.push_str(&format!(",{}:{}", crate::json::string(name), value.render()));
    }
    line.push_str("}\n");

    let stderr = std::io::stderr();
    let mut lock = stderr.lock();
    // A log line that cannot be written is not worth crashing a database over, and there is
    // nowhere left to report it to.
    let _ = lock.write_all(line.as_bytes());
}

fn json_now() -> String {
    crate::json::string(&rfc3339(SystemTime::now()))
}

/// `1970-01-01T00:00:00.000Z`, in UTC, without a date library.
///
/// A machine-readable epoch would have been three lines, but the first thing anyone does with
/// a log is read it, and nobody reads epoch milliseconds.
pub fn rfc3339(t: SystemTime) -> String {
    let d = match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d,
        // Before 1970 means the clock is wrong, not that the log line should be lost.
        Err(_) => return "1970-01-01T00:00:00.000Z".to_string(),
    };
    let secs = d.as_secs();
    let millis = d.subsec_millis();
    let days = (secs / 86_400) as i64;
    let tod = secs % 86_400;
    let (y, m, dd) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{dd:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        tod / 3600,
        (tod % 3600) / 60,
        tod % 60
    )
}

/// Days since the Unix epoch to a calendar date.
///
/// Howard Hinnant's `civil_from_days`, which is the standard way to do this without a table:
/// it shifts the year to start in March so that the leap day lands at the end of a 146097-day
/// 400-year era, after which every division is exact.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn at(secs: u64) -> String {
        rfc3339(UNIX_EPOCH + Duration::from_secs(secs))
    }

    #[test]
    fn formats_the_epoch_itself() {
        assert_eq!(at(0), "1970-01-01T00:00:00.000Z");
    }

    #[test]
    fn formats_a_leap_day() {
        // 2024-02-29T12:34:56Z, which is 19782 days plus the time of day. The constant is
        // `date -u -d @1709210096`, not a number anyone should trust from memory.
        assert_eq!(at(1_709_210_096), "2024-02-29T12:34:56.000Z");
    }

    #[test]
    fn formats_a_century_non_leap_year() {
        // 1900 was not a leap year and 2000 was; 2100-03-01 is the far side of the next one
        // that is not, which is the case the era arithmetic exists to get right.
        assert_eq!(at(4_107_542_400), "2100-03-01T00:00:00.000Z");
    }

    #[test]
    fn keeps_milliseconds() {
        let t = UNIX_EPOCH + Duration::from_millis(1_709_210_096_007);
        assert_eq!(rfc3339(t), "2024-02-29T12:34:56.007Z");
    }

    #[test]
    fn a_clock_before_the_epoch_does_not_lose_the_line() {
        assert_eq!(rfc3339(UNIX_EPOCH - Duration::from_secs(1)), "1970-01-01T00:00:00.000Z");
    }

    #[test]
    fn levels_order_from_quietest_to_loudest() {
        assert!(Level::Error < Level::Info);
        assert_eq!(Level::parse("WARNING"), Some(Level::Warn));
        assert_eq!(Level::parse("chatty"), None);
    }

    #[test]
    fn request_ids_do_not_repeat() {
        let a = next_request_id();
        let b = next_request_id();
        assert_ne!(a, b);
        assert!(a.contains('-'));
    }
}
