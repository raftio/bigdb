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

//! Time quantum: the same facts written into extra views, one per granularity.
//!
//! Granularity is per field and defaults to day alone. Turning on year, month, day and hour
//! together is four times the write amplification and four times the b-trees, which is not a
//! default anybody should get by accident.

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Granularity {
    Year,
    Month,
    Day,
    Hour,
}

impl Granularity {
    pub fn as_char(self) -> char {
        match self {
            Self::Year => 'Y',
            Self::Month => 'M',
            Self::Day => 'D',
            Self::Hour => 'H',
        }
    }
}

/// Day is the only sensible default: it is the granularity most queries actually ask for.
pub const DEFAULT_GRANULARITY: &[Granularity] = &[Granularity::Day];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DateTime {
    pub year: i64,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
}

/// Civil date from days since the Unix epoch. Hinnant's algorithm, valid for the whole range
/// of an i64 day count, and it avoids pulling in a date library for four fields.
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

pub fn decompose(unix_seconds: i64) -> DateTime {
    let days = unix_seconds.div_euclid(86_400);
    let secs = unix_seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    DateTime { year, month, day, hour: (secs / 3600) as u32 }
}

/// View suffixes a fact at `unix_seconds` must also be written into.
///
/// Coarser granularities are prefixes of finer ones, so a day view answers a month query by
/// unioning at most 31 rows rather than needing its own copy of the data.
pub fn views(unix_seconds: i64, granularity: &[Granularity]) -> Vec<String> {
    let t = decompose(unix_seconds);
    granularity
        .iter()
        .map(|g| match g {
            Granularity::Year => format!("{:04}", t.year),
            Granularity::Month => format!("{:04}{:02}", t.year, t.month),
            Granularity::Day => format!("{:04}{:02}{:02}", t.year, t.month, t.day),
            Granularity::Hour => {
                format!("{:04}{:02}{:02}{:02}", t.year, t.month, t.day, t.hour)
            }
        })
        .collect()
}

/// The day views covering `[from, to]`, which is how a range query over a day-granular field
/// turns into a union of rows.
/// Number of characters in a day view's name, `YYYYMMDD`.
///
/// Names are zero padded, so they sort chronologically as strings. That is what lets a range
/// of days be found by comparing names instead of by generating them.
pub const DAY_VIEW_LEN: usize = 8;

/// The day view an instant falls in.
///
/// Deliberately not paired with a `day_views_between(from, to)`: enumerating a range means one
/// allocation per day in it, and an open-ended range is billions of days. Callers should scan
/// the views that exist between two of these names instead.
pub fn day_view(unix_seconds: i64) -> String {
    let (y, m, d) = civil_from_days(unix_seconds.div_euclid(86_400));
    format!("{y:04}{m:02}{d:02}")
}
