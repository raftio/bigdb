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

//! The civil calendar, and the two spellings a date arrives in.
//!
//! # Why this is a crate and not a module
//!
//! The calendar is needed at both ends of the tree and the two ends cannot see each other.
//! `big-engine` names a time quantum's views (`YYYYMMDD`) and has always had the day-to-civil
//! half of this; `big-plan` turns `'2024-01-15'` into a number to compare against, and has no
//! dependencies at all on purpose. Neither can depend on the other without inverting the
//! layering, so a copy in each was the alternative - and `big_sql::render` says what this tree
//! thinks of an inverse kept in another crate.
//!
//! So: a leaf with no dependencies, holding one definition of each direction.
//!
//! # What a date is here
//!
//! A `DATE` is a count of days from 1970-01-01 and a `DATETIME` is a count of seconds from the
//! same instant, both signed, both proleptic Gregorian. There is **no timezone and no leap
//! second**. A written date is the date it says, and `2024-01-15 10:30:00` is 37800 seconds
//! after the start of that day everywhere - which is the only reading that survives being
//! compared against a value written by somebody else's clock.

#![deny(unsafe_code)]

/// Seconds in a day, with no leap second in it. See the module note.
pub const SECONDS_PER_DAY: i64 = 86_400;

/// A moment, taken apart. The fields a written date has, in the order it writes them.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Civil {
    pub year: i64,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
}

/// Civil date from days since the Unix epoch. Hinnant's algorithm, valid for the whole range of
/// an `i64` day count, and it avoids pulling in a date library for three fields.
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

/// Days since the Unix epoch from a civil date. The exact inverse of [`civil_from_days`], and
/// the half this tree did not have until dates could be written down.
///
/// Does not validate: `days_from_civil(2024, 2, 30)` answers the day after the 29th, the way the
/// algorithm does. [`parse_date`] is the boundary that refuses a date like that, because it is
/// the one that has a written string to point at.
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let mp = if m > 2 { m - 3 } else { m + 9 } as u64;
    let doy = (153 * mp + 2) / 5 + u64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe as i64 - 719_468
}

/// A moment taken apart, from seconds since the epoch.
///
/// `rem_euclid` rather than `%` so that a moment before 1970 decomposes to a time of day that is
/// still in `[0, 86400)`: `-1` is 23:59:59 on the 31st of December 1969, not minus one second
/// into the 1st of January 1970.
pub fn decompose(unix_seconds: i64) -> Civil {
    let days = unix_seconds.div_euclid(SECONDS_PER_DAY);
    let secs = unix_seconds.rem_euclid(SECONDS_PER_DAY);
    let (year, month, day) = civil_from_days(days);
    Civil {
        year,
        month,
        day,
        hour: (secs / 3600) as u32,
        minute: (secs / 60 % 60) as u32,
        second: (secs % 60) as u32,
    }
}

/// Whether a year has a 29th of February, proleptic Gregorian.
pub fn is_leap_year(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

/// How many days a month has. Zero for a month number that is not one, so that a caller checking
/// `day <= days_in_month(..)` rejects month 13 without a second test.
pub fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(y) => 29,
        2 => 28,
        _ => 0,
    }
}

/// `YYYY-MM-DD`, as days since the epoch. `None` when it is not that, or not a real date.
///
/// **Strict on purpose.** `2024-1-5` and `2024-01-15 ` are refused rather than read: a date that
/// is accepted in two spellings is a date that reads back in one of them, and a value that does
/// not read back as what was written is the bug the decimal round-trip already taught this tree
/// once. The 30th of February is refused here rather than silently becoming the 1st of March,
/// which is what the bare arithmetic in [`days_from_civil`] would do with it.
pub fn parse_date(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let year = digits(&b[0..4])? as i64;
    let month = digits(&b[5..7])?;
    let day = digits(&b[8..10])?;
    if month == 0 || day == 0 || day > days_in_month(year, month) {
        return None;
    }
    Some(days_from_civil(year, month, day))
}

/// `YYYY-MM-DD HH:MM:SS`, as seconds since the epoch. `None` when it is not that.
///
/// A bare `YYYY-MM-DD` is accepted and means midnight, because a written date is a real bound to
/// want against a timestamp column - `ts >= '2024-01-15'` is the question people ask - and the
/// alternative is making them write a time they did not mean to say anything about.
///
/// `T` is accepted in place of the space so that an ISO-8601 timestamp pasted from somewhere
/// else is read rather than refused over a separator.
pub fn parse_datetime(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() == 10 {
        return parse_date(s).map(|d| d * SECONDS_PER_DAY);
    }
    if b.len() != 19 || (b[10] != b' ' && b[10] != b'T') || b[13] != b':' || b[16] != b':' {
        return None;
    }
    let days = parse_date(&s[..10])?;
    let (hour, minute, second) = (digits(&b[11..13])?, digits(&b[14..16])?, digits(&b[17..19])?);
    // 24:00:00 is a legal spelling of midnight in ISO 8601 and is refused anyway: it is the same
    // instant as the next day's 00:00:00, and two spellings of one value do not read back.
    if hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    Some(
        days * SECONDS_PER_DAY
            + i64::from(hour) * 3600
            + i64::from(minute) * 60
            + i64::from(second),
    )
}

/// A day count, written the way [`parse_date`] reads it.
pub fn format_date(days: i64) -> String {
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}")
}

/// A second count, written the way [`parse_datetime`] reads it.
///
/// Always with the time, even at midnight. A timestamp column that rendered some of its values
/// as dates and some as timestamps would be a column a client has to sniff - the same reason
/// [`Datum::Real`](../big_api/result/enum.Datum.html) is kept apart from `Int`.
pub fn format_datetime(unix_seconds: i64) -> String {
    let t = decompose(unix_seconds);
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        t.year, t.month, t.day, t.hour, t.minute, t.second
    )
}

/// A boundary a moment can be rounded back to.
///
/// The set `date_trunc` takes, and no more. There is no `microsecond` because there is no
/// sub-second precision to round, and no `timezone` argument because every instant here is UTC.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Unit {
    Year,
    Quarter,
    Month,
    Week,
    Day,
    Hour,
    Minute,
    Second,
}

impl Unit {
    /// The spelling `date_trunc` takes, matched without regard to case.
    ///
    /// The plural is accepted because `'months'` is what half the dialects write and refusing it
    /// would be a refusal about an `s`.
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.to_ascii_lowercase().trim_end_matches('s') {
            "year" => Self::Year,
            "quarter" => Self::Quarter,
            "month" => Self::Month,
            "week" => Self::Week,
            "day" => Self::Day,
            "hour" => Self::Hour,
            "minute" | "min" => Self::Minute,
            "second" | "sec" => Self::Second,
            _ => return None,
        })
    }

    /// Whether rounding to this boundary says anything about a value counted in whole days.
    ///
    /// It does not below a day: truncating a `DATE` to the hour asks about a time of day the
    /// column never held, and answering it would hand back the same date wearing a precision it
    /// does not have.
    pub fn is_whole_days(self) -> bool {
        matches!(self, Self::Year | Self::Quarter | Self::Month | Self::Week | Self::Day)
    }

    /// Every spelling, for the refusal that has to list them.
    pub const NAMES: &'static str = "year, quarter, month, week, day, hour, minute or second";
}

/// The start of the `unit` that a day falls in, as a day count.
///
/// Separate from [`truncate`] rather than a division of it, because a `DATE` is not a timestamp
/// that happens to be at midnight: rounding it through seconds would be two conversions where
/// the calendar question is the same one.
pub fn truncate_days(days: i64, unit: Unit) -> i64 {
    let (y, m, _) = civil_from_days(days);
    match unit {
        Unit::Year => days_from_civil(y, 1, 1),
        Unit::Quarter => days_from_civil(y, (m - 1) / 3 * 3 + 1, 1),
        Unit::Month => days_from_civil(y, m, 1),
        // The Monday on or before. Day 0 is a Thursday, so the count is shifted by three before
        // the floor and back after it - which is what makes the division round the right way for
        // dates before the epoch as well as after.
        Unit::Week => (days + 3).div_euclid(7) * 7 - 3,
        // A day is already whole, and nothing finer means anything about one. `is_whole_days`
        // is what refuses those before they reach here.
        _ => days,
    }
}

/// The start of the `unit` that an instant falls in, as a second count.
pub fn truncate(unix_seconds: i64, unit: Unit) -> i64 {
    match unit {
        Unit::Second => unix_seconds,
        Unit::Minute => unix_seconds.div_euclid(60) * 60,
        Unit::Hour => unix_seconds.div_euclid(3600) * 3600,
        // Everything coarser is a question about the calendar rather than about the clock, so it
        // is answered in days and multiplied back up. `div_euclid` rather than `/` for the same
        // reason `decompose` uses it: a moment before 1970 has a negative count, and truncating
        // one has to round *down* rather than towards zero.
        other => truncate_days(unix_seconds.div_euclid(SECONDS_PER_DAY), other) * SECONDS_PER_DAY,
    }
}

/// The day an instant falls in, which is what `toDate` answers.
pub fn to_days(unix_seconds: i64) -> i64 {
    unix_seconds.div_euclid(SECONDS_PER_DAY)
}

/// A fixed-width run of ASCII digits, as a number. `None` if any byte is not one.
///
/// Its own function because every field of a date is this, and a hand-rolled loop per field is
/// four chances to accept a `+` that `str::parse` would have taken.
fn digits(b: &[u8]) -> Option<u32> {
    b.iter().try_fold(0u32, |n, c| c.is_ascii_digit().then(|| n * 10 + u32::from(c - b'0')))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_two_directions_are_inverses() {
        // The property everything else rests on: a date written and read back is the same date.
        // Spans the era boundaries Hinnant's algorithm turns on, and both sides of the epoch.
        for days in [-719_468, -365, -1, 0, 1, 19_737, 100_000, 2_932_896] {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days, "{days} -> {y}-{m}-{d}");
        }
    }

    #[test]
    fn the_epoch_is_where_it_says() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(parse_date("1970-01-01"), Some(0));
        assert_eq!(parse_datetime("1970-01-01 00:00:00"), Some(0));
    }

    #[test]
    fn a_date_round_trips_through_its_text() {
        for s in ["1970-01-01", "2024-01-15", "2024-02-29", "1969-12-31", "0001-01-01"] {
            assert_eq!(format_date(parse_date(s).unwrap()), s);
        }
        for s in ["1970-01-01 00:00:00", "2024-01-15 10:30:00", "1969-12-31 23:59:59"] {
            assert_eq!(format_datetime(parse_datetime(s).unwrap()), s);
        }
    }

    #[test]
    fn a_date_that_is_not_one_is_refused() {
        for s in [
            "2023-02-29",  // not a leap year
            "2024-13-01",  // no such month
            "2024-00-01",  // nor that one
            "2024-01-00",  // nor that day
            "2024-01-32",  // nor that one
            "2024-1-05",   // not zero padded
            "2024-01-15 ", // trailing space
            "2024/01/15",  // wrong separator
            "24-01-15",    // two digit year
            "+024-01-15",  // a sign is not a digit
            "",
        ] {
            assert_eq!(parse_date(s), None, "{s} should not parse");
        }
        assert_eq!(parse_datetime("2024-01-15 24:00:00"), None);
        assert_eq!(parse_datetime("2024-01-15 10:60:00"), None);
        assert_eq!(parse_datetime("2024-01-15 10:30:60"), None);
        assert_eq!(parse_datetime("2024-01-15T10:30"), None);
    }

    #[test]
    fn a_bare_date_is_midnight() {
        assert_eq!(parse_datetime("2024-01-15"), parse_datetime("2024-01-15 00:00:00"));
        assert_eq!(parse_datetime("2024-01-15T10:30:00"), parse_datetime("2024-01-15 10:30:00"));
    }

    #[test]
    fn before_the_epoch_the_time_of_day_still_runs_forwards() {
        // `%` rather than `rem_euclid` would put this at -1 seconds into the 1st of January.
        let t = decompose(-1);
        assert_eq!((t.year, t.month, t.day), (1969, 12, 31));
        assert_eq!((t.hour, t.minute, t.second), (23, 59, 59));
    }

    #[test]
    fn a_moment_rounds_back_to_the_boundary_it_falls_in() {
        let t = parse_datetime("2024-05-17 13:45:30").unwrap();
        for (unit, want) in [
            (Unit::Second, "2024-05-17 13:45:30"),
            (Unit::Minute, "2024-05-17 13:45:00"),
            (Unit::Hour, "2024-05-17 13:00:00"),
            (Unit::Day, "2024-05-17 00:00:00"),
            // The 17th of May 2024 is a Friday; the Monday of its week is the 13th.
            (Unit::Week, "2024-05-13 00:00:00"),
            (Unit::Month, "2024-05-01 00:00:00"),
            (Unit::Quarter, "2024-04-01 00:00:00"),
            (Unit::Year, "2024-01-01 00:00:00"),
        ] {
            assert_eq!(format_datetime(truncate(t, unit)), want, "{unit:?}");
        }
    }

    #[test]
    fn truncating_rounds_down_before_the_epoch_too() {
        // The case a plain `/` gets wrong: it rounds towards zero, so a moment in 1969 would
        // round *forwards* into 1970 and a truncation would move a value later than it was.
        let t = parse_datetime("1969-05-17 13:45:30").unwrap();
        assert!(t < 0);
        assert_eq!(format_datetime(truncate(t, Unit::Year)), "1969-01-01 00:00:00");
        assert_eq!(format_datetime(truncate(t, Unit::Day)), "1969-05-17 00:00:00");
        assert_eq!(format_datetime(truncate(t, Unit::Hour)), "1969-05-17 13:00:00");
    }

    #[test]
    fn a_week_starts_on_monday_on_both_sides_of_the_epoch() {
        for (day, monday) in [
            ("2024-05-17", "2024-05-13"), // Friday
            ("2024-05-13", "2024-05-13"), // Monday itself
            ("2024-05-19", "2024-05-13"), // Sunday ends the week
            ("1970-01-01", "1969-12-29"), // day zero is a Thursday
            ("1969-12-29", "1969-12-29"),
        ] {
            let d = parse_date(day).unwrap();
            assert_eq!(format_date(truncate_days(d, Unit::Week)), monday, "{day}");
        }
    }

    #[test]
    fn a_day_count_truncates_without_going_through_seconds() {
        let d = parse_date("2024-05-17").unwrap();
        assert_eq!(format_date(truncate_days(d, Unit::Month)), "2024-05-01");
        assert_eq!(format_date(truncate_days(d, Unit::Quarter)), "2024-04-01");
        assert_eq!(format_date(truncate_days(d, Unit::Year)), "2024-01-01");
        // A day is already whole.
        assert_eq!(truncate_days(d, Unit::Day), d);
    }

    #[test]
    fn to_days_is_the_day_an_instant_falls_in() {
        assert_eq!(
            to_days(parse_datetime("2024-01-15 23:59:59").unwrap()),
            parse_date("2024-01-15").unwrap()
        );
        // And rounds down before the epoch rather than towards zero.
        assert_eq!(
            to_days(parse_datetime("1969-12-31 12:00:00").unwrap()),
            parse_date("1969-12-31").unwrap()
        );
    }

    #[test]
    fn a_unit_is_spelled_the_way_dialects_spell_it() {
        assert_eq!(Unit::parse("month"), Some(Unit::Month));
        assert_eq!(Unit::parse("MONTHS"), Some(Unit::Month));
        assert_eq!(Unit::parse("Quarter"), Some(Unit::Quarter));
        assert_eq!(Unit::parse("min"), Some(Unit::Minute));
        assert_eq!(Unit::parse("fortnight"), None);
        assert_eq!(Unit::parse(""), None);
        assert!(Unit::Month.is_whole_days() && !Unit::Hour.is_whole_days());
    }

    #[test]
    fn leap_years_are_the_gregorian_ones() {
        assert!(is_leap_year(2024) && is_leap_year(2000) && is_leap_year(1600));
        assert!(!is_leap_year(2023) && !is_leap_year(1900) && !is_leap_year(2100));
        assert_eq!(days_in_month(2024, 2), 29);
        assert_eq!(days_in_month(2023, 2), 28);
        assert_eq!(days_in_month(2024, 13), 0);
    }
}
