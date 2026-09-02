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

/// Civil date from days since the Unix epoch.
///
/// The algorithm moved to `big-civil` when dates became a column type: the same arithmetic
/// reads `'2024-01-15'` in the planner, which cannot see this crate, and two copies of a
/// calendar are two calendars. This is the name it has always had here.
pub use big_civil::civil_from_days;

/// The four fields a view name is built from.
///
/// Narrower than [`big_civil::Civil`] on purpose - an hour is the finest granularity a view has,
/// so minutes and seconds would be two fields nothing here reads.
pub fn decompose(unix_seconds: i64) -> DateTime {
    let t = big_civil::decompose(unix_seconds);
    DateTime { year: t.year, month: t.month, day: t.day, hour: t.hour }
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
