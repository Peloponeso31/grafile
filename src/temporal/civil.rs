//! Proleptic Gregorian civil dates with ISO week, ordinal and quarter
//! conversions.
//!
//! Dates are stored as a day count from 1970-01-01 and support the full year
//! range Neo4j accepts, -999,999,999 through 999,999,999. The conversion
//! between day counts and year/month/day uses Howard Hinnant's
//! `days_from_civil`/`civil_from_days` algorithms, which are exact for the
//! whole range.

use std::fmt;

/// Smallest supported year.
pub const MIN_YEAR: i64 = -999_999_999;
/// Largest supported year.
pub const MAX_YEAR: i64 = 999_999_999;

const DAYS_IN_MONTH: [i64; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];

/// A civil date in the proleptic Gregorian calendar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Date {
    days: i64,
}

/// Whether `year` is a leap year.
pub fn is_leap_year(year: i64) -> bool {
    year.rem_euclid(4) == 0 && (year.rem_euclid(100) != 0 || year.rem_euclid(400) == 0)
}

/// Number of days in `month` (1-based) of `year`.
pub fn days_in_month(year: i64, month: u32) -> i64 {
    if month == 2 && is_leap_year(year) {
        29
    } else {
        DAYS_IN_MONTH[(month - 1) as usize]
    }
}

/// Number of days in `year`.
pub fn days_in_year(year: i64) -> i64 {
    if is_leap_year(year) { 366 } else { 365 }
}

/// Day count from 1970-01-01 for a civil date (Hinnant).
pub const fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month_prime = (month as i64 + 9) % 12;
    let day_of_year = (153 * month_prime + 2) / 5 + day as i64 - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// Civil date for a day count from 1970-01-01 (Hinnant).
pub const fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let shifted = days + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_prime + 2) / 5 + 1) as u32;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    } as u32;
    if month <= 2 {
        year += 1;
    }
    (year, month, day)
}

/// ISO weekday for a day count: 1 (Monday) through 7 (Sunday).
const fn iso_weekday_from_days(days: i64) -> u32 {
    // 1970-01-01 was a Thursday.
    (days + 3).rem_euclid(7) as u32 + 1
}

impl Date {
    /// Build a date from a day count.
    pub const fn from_days(days: i64) -> Self {
        Self { days }
    }

    /// Build a date from civil components.
    ///
    /// Returns `None` when the year is outside the supported range, the month
    /// is not 1-12, or the day is not valid for that month.
    pub fn new(year: i64, month: u32, day: u32) -> Option<Self> {
        if !(MIN_YEAR..=MAX_YEAR).contains(&year) || !(1..=12).contains(&month) {
            return None;
        }
        if day < 1 || day as i64 > days_in_month(year, month) {
            return None;
        }
        Some(Self {
            days: days_from_civil(year, month, day),
        })
    }

    /// Build a date from an ISO week date.
    pub fn from_iso_week(year: i64, week: u32, weekday: u32) -> Option<Self> {
        if !(MIN_YEAR..=MAX_YEAR).contains(&year) || !(1..=7).contains(&weekday) {
            return None;
        }
        if week < 1 || week > iso_weeks_in_year(year) {
            return None;
        }
        let january_4 = Self::new(year, 1, 4)?;
        let week_1_monday = january_4.days - (january_4.iso_weekday() as i64 - 1);
        Some(Self {
            days: week_1_monday + (week as i64 - 1) * 7 + weekday as i64 - 1,
        })
    }

    /// Build a date from an ordinal day (`1` through 365/366).
    pub fn from_ordinal(year: i64, ordinal: u32) -> Option<Self> {
        if !(MIN_YEAR..=MAX_YEAR).contains(&year) {
            return None;
        }
        if ordinal < 1 || ordinal as i64 > days_in_year(year) {
            return None;
        }
        Some(Self {
            days: days_from_civil(year, 1, 1) + ordinal as i64 - 1,
        })
    }

    /// Build a date from a quarter and a day within that quarter
    /// (`dayOfQuarter`, 1-based).
    pub fn from_quarter(year: i64, quarter: u32, day_of_quarter: u32) -> Option<Self> {
        if !(1..=4).contains(&quarter) {
            return None;
        }
        let first_month = (quarter - 1) * 3 + 1;
        let ordinal = ordinal_of(year, first_month, 1)? + day_of_quarter as i64 - 1;
        Self::from_ordinal(year, u32::try_from(ordinal).ok()?)
    }

    /// Day count from 1970-01-01.
    pub const fn days(self) -> i64 {
        self.days
    }

    /// Year.
    pub const fn year(self) -> i64 {
        civil_from_days(self.days).0
    }

    /// Month (1-12).
    pub const fn month(self) -> u32 {
        civil_from_days(self.days).1
    }

    /// Day of month (1-31).
    pub const fn day(self) -> u32 {
        civil_from_days(self.days).2
    }

    /// ISO weekday: 1 (Monday) through 7 (Sunday).
    pub const fn iso_weekday(self) -> u32 {
        iso_weekday_from_days(self.days)
    }

    /// ISO week year and week number.
    pub fn iso_week(self) -> (i64, u32) {
        let thursday = self.days + (4 - self.iso_weekday() as i64);
        let (week_year, _, _) = civil_from_days(thursday);
        let week = ((thursday - days_from_civil(week_year, 1, 1)) / 7 + 1) as u32;
        (week_year, week)
    }

    /// Day of year (1-365/366).
    pub fn ordinal_day(self) -> u32 {
        (self.days - days_from_civil(self.year(), 1, 1) + 1) as u32
    }

    /// Quarter (1-4).
    pub fn quarter(self) -> u32 {
        (self.month() - 1) / 3 + 1
    }

    /// Day within the quarter (1-based).
    pub fn day_of_quarter(self) -> u32 {
        let first_month = (self.quarter() - 1) * 3 + 1;
        (self.days - days_from_civil(self.year(), first_month, 1) + 1) as u32
    }

    /// Add a whole number of months, clamping the day to the end of the
    /// target month (`1984-10-31` plus one month is `1984-11-30`).
    pub fn add_months(self, months: i64) -> Option<Self> {
        let (year, month, day) = (self.year(), self.month(), self.day());
        let total = year
            .checked_mul(12)?
            .checked_add(month as i64 - 1)?
            .checked_add(months)?;
        let new_year = total.div_euclid(12);
        let new_month = total.rem_euclid(12) as u32 + 1;
        let new_day = day.min(days_in_month(new_year, new_month) as u32);
        Self::new(new_year, new_month, new_day)
    }

    /// Add a whole number of days.
    pub fn add_days(self, days: i64) -> Option<Self> {
        Some(Self {
            days: self.days.checked_add(days)?,
        })
    }

    /// Replace the year, keeping month and day (clamped to the new month).
    pub fn with_year(self, year: i64) -> Option<Self> {
        let day = self.day().min(days_in_month(year, self.month()) as u32);
        Self::new(year, self.month(), day)
    }

    /// Replace the month, keeping the day (clamped to the new month).
    pub fn with_month(self, month: u32) -> Option<Self> {
        let day = self.day().min(days_in_month(self.year(), month) as u32);
        Self::new(self.year(), month, day)
    }

    /// Replace the day of month.
    pub fn with_day(self, day: u32) -> Option<Self> {
        Self::new(self.year(), self.month(), day)
    }
}

/// Number of ISO weeks (52 or 53) in the given ISO week year.
pub fn iso_weeks_in_year(year: i64) -> u32 {
    let january_1 = days_from_civil(year, 1, 1);
    let weekday = iso_weekday_from_days(january_1);
    if weekday == 4 || (is_leap_year(year) && weekday == 3) {
        53
    } else {
        52
    }
}

fn ordinal_of(year: i64, month: u32, day: u32) -> Option<i64> {
    if !(1..=12).contains(&month) {
        return None;
    }
    let ordinal = days_from_civil(year, month, day) - days_from_civil(year, 1, 1) + 1;
    Some(ordinal)
}

impl fmt::Display for Date {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (year, month, day) = civil_from_days(self.days);
        if year < 0 {
            write!(f, "-{:04}-{month:02}-{day:02}", year.unsigned_abs())
        } else if year > 9999 {
            write!(f, "+{year}-{month:02}-{day:02}")
        } else {
            write!(f, "{year:04}-{month:02}-{day:02}")
        }
    }
}

impl std::str::FromStr for Date {
    type Err = String;

    /// Parse the ISO 8601 date forms the TCK exercises: `YYYY`, `YYYY-MM`,
    /// `YYYYMM`, `YYYY-MM-DD`, `YYYYMMDD`, week dates (`YYYY-Www[-D]`,
    /// `YYYYWwwD`) and ordinal dates (`YYYY-DDD`, `YYYYDDD`). Signed years
    /// (`+999999999`, `-999999999`) use all leading digits.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let (year, rest) = parse_year(text)?;
        if rest.is_empty() {
            return Date::new(year, 1, 1).ok_or_else(|| format!("year {year} out of range"));
        }
        let rest = rest.strip_prefix('-').unwrap_or(rest);
        if let Some(week) = rest.strip_prefix('W').or_else(|| rest.strip_prefix('w')) {
            return parse_week_date(year, week);
        }
        if !rest.bytes().all(|byte| byte.is_ascii_digit()) && !rest.contains('-') {
            return Err(format!("invalid date `{text}`"));
        }
        match rest.split_once('-') {
            Some((month, day)) => {
                let month: u32 = month
                    .parse()
                    .map_err(|_| format!("invalid month `{month}`"))?;
                let day: u32 = day.parse().map_err(|_| format!("invalid day `{day}`"))?;
                Date::new(year, month, day).ok_or_else(|| format!("invalid date `{text}`"))
            }
            None => match rest.len() {
                2 => {
                    let month: u32 = rest
                        .parse()
                        .map_err(|_| format!("invalid month `{rest}`"))?;
                    Date::new(year, month, 1).ok_or_else(|| format!("invalid date `{text}`"))
                }
                3 => {
                    let ordinal: u32 = rest
                        .parse()
                        .map_err(|_| format!("invalid ordinal `{rest}`"))?;
                    Date::from_ordinal(year, ordinal)
                        .ok_or_else(|| format!("invalid date `{text}`"))
                }
                4 => {
                    let month: u32 = rest[..2]
                        .parse()
                        .map_err(|_| format!("invalid month `{rest}`"))?;
                    let day: u32 = rest[2..]
                        .parse()
                        .map_err(|_| format!("invalid day `{rest}`"))?;
                    Date::new(year, month, day).ok_or_else(|| format!("invalid date `{text}`"))
                }
                _ => Err(format!("invalid date `{text}`")),
            },
        }
    }
}

fn parse_year(text: &str) -> Result<(i64, &str), String> {
    let (sign, digits) = match text.strip_prefix('+') {
        Some(rest) => (1, rest),
        None => match text.strip_prefix('-') {
            Some(rest) => (-1, rest),
            None => (1, text),
        },
    };
    let take = if text.starts_with('+') || text.starts_with('-') {
        digits.len()
    } else {
        4
    };
    let split = digits
        .char_indices()
        .take(take)
        .take_while(|(_, c)| c.is_ascii_digit())
        .last()
        .map(|(index, c)| index + c.len_utf8())
        .unwrap_or(0);
    if split == 0 {
        return Err(format!("invalid year in `{text}`"));
    }
    let year: i64 = digits[..split]
        .parse()
        .map_err(|_| format!("invalid year in `{text}`"))?;
    Ok((sign * year, &digits[split..]))
}

fn parse_week_date(year: i64, week: &str) -> Result<Date, String> {
    let (week_digits, weekday) = match week.split_once('-') {
        Some((week, weekday)) => (week, Some(weekday)),
        None => match week.len() {
            2 => (week, None),
            3 => (&week[..2], Some(&week[2..])),
            _ => return Err(format!("invalid week date `{week}`")),
        },
    };
    let week: u32 = week_digits
        .parse()
        .map_err(|_| format!("invalid week `{week_digits}`"))?;
    let weekday: u32 = match weekday {
        Some(weekday) => weekday
            .parse()
            .map_err(|_| format!("invalid weekday `{weekday}`"))?,
        None => 1,
    };
    Date::from_iso_week(year, week, weekday).ok_or_else(|| "invalid week date".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_across_the_range() {
        for days in [
            -3_652_470_000_i64,
            -719_162,
            -1,
            0,
            1,
            719_162,
            3_652_470_000,
        ] {
            let (year, month, day) = civil_from_days(days);
            assert_eq!(
                days_from_civil(year, month, day),
                days,
                "{year}-{month}-{day}"
            );
        }
    }

    #[test]
    fn epoch_and_known_dates() {
        assert_eq!(Date::new(1970, 1, 1).expect("epoch").days(), 0);
        assert_eq!(Date::new(1970, 1, 1).expect("epoch").iso_weekday(), 4);
        assert_eq!(Date::from_days(0).to_string(), "1970-01-01");
        assert_eq!(
            Date::new(1984, 10, 11).expect("date").to_string(),
            "1984-10-11"
        );
    }

    #[test]
    fn iso_week_conversions_match_the_tck() {
        // Temporal1: week dates of 1816-1819.
        assert_eq!(
            Date::from_iso_week(1816, 1, 1).expect("date"),
            Date::new(1816, 1, 1).expect("date")
        );
        assert_eq!(
            Date::from_iso_week(1817, 1, 1).expect("date"),
            Date::new(1816, 12, 30).expect("date")
        );
        assert_eq!(
            Date::from_iso_week(1817, 10, 1).expect("date"),
            Date::new(1817, 3, 3).expect("date")
        );
        assert_eq!(
            Date::from_iso_week(1818, 53, 1).expect("date"),
            Date::new(1818, 12, 28).expect("date")
        );
        assert_eq!(
            Date::from_iso_week(1817, 1, 2).expect("date"),
            Date::new(1816, 12, 31).expect("date")
        );
        // Temporal5: 1984-01-01 is in the last week of ISO week year 1983.
        let date = Date::new(1984, 1, 1).expect("date");
        assert_eq!(date.iso_week(), (1983, 52));
        assert_eq!(date.iso_weekday(), 7);
        // 1984-10-11 is ISO week 41.
        assert_eq!(
            Date::new(1984, 10, 11).expect("date").iso_week(),
            (1984, 41)
        );
    }

    #[test]
    fn ordinal_and_quarter_conversions() {
        assert_eq!(
            Date::from_ordinal(1984, 202).expect("date"),
            Date::new(1984, 7, 20).expect("date")
        );
        assert_eq!(
            Date::from_quarter(1984, 3, 45).expect("date"),
            Date::new(1984, 8, 14).expect("date")
        );
        let date = Date::new(1984, 11, 11).expect("date");
        assert_eq!(date.quarter(), 4);
        assert_eq!(date.day_of_quarter(), 42);
        assert_eq!(date.ordinal_day(), 316);
    }

    #[test]
    fn month_addition_clamps() {
        let date = Date::new(1984, 10, 31).expect("date");
        assert_eq!(
            date.add_months(1).expect("date"),
            Date::new(1984, 11, 30).expect("date")
        );
        assert_eq!(
            date.add_months(-1).expect("date"),
            Date::new(1984, 9, 30).expect("date")
        );
        assert_eq!(
            Date::new(1984, 1, 31)
                .expect("date")
                .add_months(1)
                .expect("date"),
            Date::new(1984, 2, 29).expect("date")
        );
    }

    #[test]
    fn parses_every_tck_form() {
        let cases = [
            ("2015-07-21", (2015, 7, 21)),
            ("20150721", (2015, 7, 21)),
            ("2015-07", (2015, 7, 1)),
            ("201507", (2015, 7, 1)),
            ("2015-W30-2", (2015, 7, 21)),
            ("2015W302", (2015, 7, 21)),
            ("2015-W30", (2015, 7, 20)),
            ("2015W30", (2015, 7, 20)),
            ("2015-202", (2015, 7, 21)),
            ("2015202", (2015, 7, 21)),
            ("2015", (2015, 1, 1)),
            ("-999999999-01-01", (-999_999_999, 1, 1)),
        ];
        for (text, (year, month, day)) in cases {
            let parsed: Date = text.parse().expect(text);
            assert_eq!(parsed, Date::new(year, month, day).expect(text), "{text}");
        }
    }

    #[test]
    fn rejects_invalid_dates() {
        for text in ["2015-02-30", "2015-13-01", "2015-00-01", "2015-W54-1", "x"] {
            assert!(text.parse::<Date>().is_err(), "{text}");
        }
        assert!(Date::new(MAX_YEAR + 1, 1, 1).is_none());
    }

    #[test]
    fn week_year_53_rule() {
        assert_eq!(iso_weeks_in_year(1818), 53);
        assert_eq!(iso_weeks_in_year(1819), 52);
        assert_eq!(iso_weeks_in_year(2015), 53);
        assert_eq!(iso_weeks_in_year(2016), 52);
    }
}
