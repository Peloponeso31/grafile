//! Construction of temporal values from field maps and from other temporal
//! values, following Neo4j's rules.
//!
//! A field map may mix calendar keys (`year`, `month`, `week`,
//! `ordinalDay`, `quarter`, `dayOfQuarter`), clock keys (`hour` through
//! `nanosecond`), and whole-values (`date`, `time`, `datetime`) plus a
//! `timezone` (offset string or IANA name). Explicit keys override the
//! components of the whole-value keys; missing calendar parts default to
//! year 0 / month 1 / day 1 and missing clock parts to zero.

use std::sync::Arc;

use crate::temporal::civil::Date;
use crate::temporal::datetime::{DateTime, LocalDateTime, LocalTime, Time};
use crate::temporal::duration::{Component, Components, Duration};
use crate::temporal::zone;

/// A value from a temporal field map.
#[derive(Debug, Clone)]
pub enum Field {
    /// A number (integer or float).
    Number(f64),
    /// Text (offset strings and zone names).
    Text(String),
    /// A date value.
    Date(Date),
    /// A local time value.
    LocalTime(LocalTime),
    /// A time value.
    Time(Time),
    /// A local datetime value.
    LocalDateTime(LocalDateTime),
    /// A datetime value.
    DateTime(DateTime),
}

impl Field {
    fn as_int(&self) -> Option<i64> {
        match self {
            Self::Number(value) if value.fract() == 0.0 => Some(*value as i64),
            _ => None,
        }
    }

    fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text(text) => Some(text),
            _ => None,
        }
    }
}

fn find<'a>(fields: &'a [(String, Field)], key: &str) -> Option<&'a Field> {
    fields
        .iter()
        .find(|(name, _)| name == key)
        .map(|(_, field)| field)
}

fn year(fields: &[(String, Field)], base: Date) -> Result<i64, String> {
    match find(fields, "year") {
        Some(field) => field
            .as_int()
            .ok_or_else(|| "`year` must be an integer".to_owned()),
        None => Ok(base.year()),
    }
}

fn component(fields: &[(String, Field)], key: &str) -> Result<Option<i64>, String> {
    match find(fields, key) {
        Some(field) => field
            .as_int()
            .map(Some)
            .ok_or_else(|| format!("`{key}` must be an integer")),
        None => Ok(None),
    }
}

/// The civil date described by the calendar keys, layered over `base`.
fn date_part(fields: &[(String, Field)], base: Date) -> Result<Date, String> {
    if let Some(field) = find(fields, "week") {
        let week = field
            .as_int()
            .ok_or_else(|| "`week` must be an integer".to_owned())? as u32;
        // An explicit weekday wins; otherwise a `date` key contributes its
        // weekday and a bare `week` key defaults to Monday. Without an
        // explicit year, the base date's ISO week year applies:
        // `{date: date('1816-12-31'), week: 2}` is a week-2 Tuesday.
        let default_weekday = match find(fields, "date") {
            Some(Field::Date(_)) => base.iso_weekday(),
            _ => 1,
        };
        let weekday = component(fields, "dayOfWeek")?.unwrap_or(default_weekday as i64) as u32;
        let week_year = match find(fields, "year") {
            Some(_) => year(fields, base)?,
            None => base.iso_week().0,
        };
        return Date::from_iso_week(week_year, week, weekday)
            .ok_or_else(|| "invalid week date".to_owned());
    }
    if let Some(ordinal) = component(fields, "ordinalDay")? {
        return Date::from_ordinal(year(fields, base)?, ordinal as u32)
            .ok_or_else(|| "invalid ordinal day".to_owned());
    }
    if let Some(quarter) = component(fields, "quarter")? {
        let day = component(fields, "dayOfQuarter")?.unwrap_or(1) as u32;
        return Date::from_quarter(year(fields, base)?, quarter as u32, day)
            .ok_or_else(|| "invalid quarter date".to_owned());
    }
    let year = year(fields, base)?;
    let month = component(fields, "month")?.unwrap_or(base.month() as i64) as u32;
    let day = component(fields, "day")?.unwrap_or(base.day() as i64) as u32;
    Date::new(year, month, day).ok_or_else(|| "invalid date".to_owned())
}

/// The time of day described by the clock keys, layered over `base`.
fn time_part(fields: &[(String, Field)], base: LocalTime) -> Result<LocalTime, String> {
    let hour = component(fields, "hour")?.unwrap_or(base.hour());
    let minute = component(fields, "minute")?.unwrap_or(base.minute());
    let second = component(fields, "second")?.unwrap_or(base.second());
    let base_nanos = base.nanosecond();
    let millisecond = component(fields, "millisecond")?;
    let microsecond = component(fields, "microsecond")?;
    let nanosecond = component(fields, "nanosecond")?;
    let nanos = if millisecond.is_none() && microsecond.is_none() && nanosecond.is_none() {
        base_nanos
    } else {
        millisecond.unwrap_or(0) * 1_000_000
            + microsecond.unwrap_or(0) * 1_000
            + nanosecond.unwrap_or(0)
    };
    LocalTime::new(hour, minute, second, nanos)
        .ok_or_else(|| "time component out of range".to_owned())
}

/// A timezone field: a fixed offset or a named zone.
enum Timezone {
    Offset(i32),
    Named(String),
}

fn timezone_part(fields: &[(String, Field)]) -> Result<Option<Timezone>, String> {
    let Some(field) = find(fields, "timezone") else {
        return Ok(None);
    };
    let text = field
        .as_text()
        .ok_or_else(|| "`timezone` must be a string".to_owned())?;
    if text.starts_with('+') || text.starts_with('-') || text == "Z" || text == "z" {
        let offset = parse_offset_text(text)?;
        return Ok(Some(Timezone::Offset(offset)));
    }
    let canonical =
        zone::canonical_name(text).ok_or_else(|| format!("unknown time zone `{text}`"))?;
    Ok(Some(Timezone::Named(canonical)))
}

fn parse_offset_text(text: &str) -> Result<i32, String> {
    let invalid = || format!("invalid offset `{text}`");
    if text.eq_ignore_ascii_case("z") {
        return Ok(0);
    }
    let sign = match text.as_bytes().first() {
        Some(b'+') => 1,
        Some(b'-') => -1,
        _ => return Err(invalid()),
    };
    let rest = &text[1..];
    let (hours, minutes, seconds) = if rest.contains(':') {
        let mut parts = rest.split(':');
        let hours = parts.next().unwrap_or_default();
        let minutes = parts.next().unwrap_or("0");
        let seconds = parts.next().unwrap_or("0");
        if parts.next().is_some() {
            return Err(invalid());
        }
        (hours, minutes, seconds)
    } else {
        match rest.len() {
            2 => (rest, "0", "0"),
            4 => (&rest[..2], &rest[2..], "0"),
            6 => (&rest[..2], &rest[2..4], &rest[4..]),
            _ => return Err(invalid()),
        }
    };
    let hours: i32 = hours.parse().map_err(|_| invalid())?;
    let minutes: i32 = minutes.parse().map_err(|_| invalid())?;
    let seconds: i32 = seconds.parse().map_err(|_| invalid())?;
    if hours > 18 || minutes > 59 || seconds > 59 {
        return Err(invalid());
    }
    let offset = sign * (hours * 3_600 + minutes * 60 + seconds);
    if offset.abs() > crate::temporal::datetime::MAX_OFFSET_SECONDS {
        return Err(invalid());
    }
    Ok(offset)
}

/// Build a date from field map keys.
pub fn date_from_fields(fields: &[(String, Field)]) -> Result<Date, String> {
    let base = match find(fields, "date") {
        Some(Field::Date(date)) => *date,
        Some(_) => return Err("`date` must be a date value".to_owned()),
        None => Date::new(0, 1, 1).expect("year zero is valid"),
    };
    date_part(fields, base)
}

/// Build a local time from field map keys.
pub fn localtime_from_fields(fields: &[(String, Field)]) -> Result<LocalTime, String> {
    let base = match find(fields, "time") {
        Some(Field::LocalTime(time)) => *time,
        Some(Field::Time(time)) => time.local(),
        Some(_) => return Err("`time` must be a time value".to_owned()),
        None => LocalTime::new(0, 0, 0, 0).expect("midnight"),
    };
    time_part(fields, base)
}

/// Build a time (with offset) from field map keys.
pub fn time_from_fields(fields: &[(String, Field)]) -> Result<Time, String> {
    let (base, base_offset) = match find(fields, "time") {
        Some(Field::Time(time)) => (time.local(), time.offset()),
        Some(Field::LocalTime(time)) => (*time, 0),
        Some(_) => return Err("`time` must be a time value".to_owned()),
        None => (LocalTime::new(0, 0, 0, 0).expect("midnight"), 0),
    };
    let local = time_part(fields, base)?;
    let offset = match timezone_part(fields)? {
        Some(Timezone::Offset(offset)) => offset,
        Some(Timezone::Named(name)) => {
            return Err(format!("`{name}` cannot be used as a time zone for a time"));
        }
        None => base_offset,
    };
    Time::new(local, offset).ok_or_else(|| "offset out of range".to_owned())
}

/// Build a local datetime from field map keys.
pub fn localdatetime_from_fields(fields: &[(String, Field)]) -> Result<LocalDateTime, String> {
    let base = datetime_base(fields)?;
    let date = date_part(fields, base.local().date())?;
    let time = time_part(fields, base.local().time())?;
    Ok(LocalDateTime::new(date, time))
}

/// Build a datetime from field map keys.
pub fn datetime_from_fields(fields: &[(String, Field)]) -> Result<DateTime, String> {
    let base = datetime_base(fields)?;
    let date = date_part(fields, base.local().date())?;
    let time = time_part(fields, base.local().time())?;
    let local = LocalDateTime::new(date, time);
    let timezone = timezone_part(fields)?;
    let (offset, zone) = match timezone {
        Some(Timezone::Offset(offset)) => (offset, None),
        Some(Timezone::Named(name)) => {
            let civil = zone::Civil::from_date_and_nanos(date, time.nanos());
            let offset = zone::offset_for_civil(&name, civil)?;
            (offset, Some(Arc::from(name.as_str())))
        }
        None => (base.offset(), base.zone().map(Arc::from)),
    };
    DateTime::new(local, offset, zone).ok_or_else(|| "offset out of range".to_owned())
}

/// The whole-value keys a datetime map may start from.
fn datetime_base(fields: &[(String, Field)]) -> Result<DateTime, String> {
    if let Some(Field::DateTime(datetime)) = find(fields, "datetime") {
        return Ok(datetime.clone());
    }
    if let Some(field) = find(fields, "datetime") {
        if let Field::LocalDateTime(local) = field {
            return Ok(DateTime::new(*local, 0, None).expect("zero offset"));
        }
        return Err("`datetime` must be a datetime value".to_owned());
    }
    if let Some(Field::Date(date)) = find(fields, "date") {
        return Ok(
            DateTime::new(LocalDateTime::new(*date, midnight()), 0, None).expect("zero offset"),
        );
    }
    Ok(DateTime::new(LocalDateTime::new(year_zero(), midnight()), 0, None).expect("zero offset"))
}

fn midnight() -> LocalTime {
    LocalTime::new(0, 0, 0, 0).expect("midnight")
}

fn year_zero() -> Date {
    Date::new(0, 1, 1).expect("year zero is valid")
}

/// Truncation units accepted by `*.truncate(unit, value, map)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TruncateUnit {
    /// Thousands of years.
    Millennium,
    /// Hundreds of years.
    Century,
    /// Tens of years.
    Decade,
    /// Whole year.
    Year,
    /// First day of the first ISO week of the ISO week year.
    WeekYear,
    /// First day of the quarter.
    Quarter,
    /// First day of the month.
    Month,
    /// Monday of the ISO week.
    Week,
    /// Midnight (or midnight of the day for time values).
    Day,
    /// Whole hour.
    Hour,
    /// Whole minute.
    Minute,
    /// Whole second.
    Second,
    /// Whole millisecond.
    Millisecond,
    /// Whole microsecond.
    Microsecond,
}

impl TruncateUnit {
    /// Parse a unit name (case-insensitive, like Neo4j's `weekYear`).
    pub fn parse(text: &str) -> Option<Self> {
        Some(match text.to_ascii_lowercase().as_str() {
            "millennium" => Self::Millennium,
            "century" => Self::Century,
            "decade" => Self::Decade,
            "year" => Self::Year,
            "weekyear" => Self::WeekYear,
            "quarter" => Self::Quarter,
            "month" => Self::Month,
            "week" => Self::Week,
            "day" => Self::Day,
            "hour" => Self::Hour,
            "minute" => Self::Minute,
            "second" => Self::Second,
            "millisecond" => Self::Millisecond,
            "microsecond" => Self::Microsecond,
            _ => return None,
        })
    }

    /// Whether the unit stays below one day.
    pub const fn is_time_unit(self) -> bool {
        matches!(
            self,
            Self::Hour | Self::Minute | Self::Second | Self::Millisecond | Self::Microsecond
        )
    }
}

/// Truncate a date to a unit, then apply map overrides (`day` for calendar
/// units, `dayOfWeek` for weeks).
pub fn truncate_date(
    unit: TruncateUnit,
    date: Date,
    overrides: &[(String, Field)],
) -> Result<Date, String> {
    let truncated = match unit {
        TruncateUnit::Millennium => Date::new(date.year().div_euclid(1000) * 1000, 1, 1),
        TruncateUnit::Century => Date::new(date.year().div_euclid(100) * 100, 1, 1),
        TruncateUnit::Decade => Date::new(date.year().div_euclid(10) * 10, 1, 1),
        TruncateUnit::Year => Date::new(date.year(), 1, 1),
        TruncateUnit::WeekYear => {
            let (week_year, _) = date.iso_week();
            Date::from_iso_week(week_year, 1, 1)
        }
        TruncateUnit::Quarter => Date::new(date.year(), (date.quarter() - 1) * 3 + 1, 1),
        TruncateUnit::Month => Date::new(date.year(), date.month(), 1),
        TruncateUnit::Week => Some(Date::from_days(
            date.days() - (date.iso_weekday() as i64 - 1),
        )),
        // `Day` and the time units leave the date untouched.
        _ => Some(date),
    }
    .ok_or_else(|| "invalid truncated date".to_owned())?;
    if unit == TruncateUnit::Week {
        return match component(overrides, "dayOfWeek")? {
            Some(day_of_week) => Ok(Date::from_days(truncated.days() + day_of_week - 1)),
            None => Ok(truncated),
        };
    }
    date_part(overrides, truncated)
}

/// Truncate a time of day to a unit, then apply map overrides. The
/// overrides replace the millisecond, microsecond and nanosecond slots of
/// the truncated value individually (`truncate('millisecond', ...)` with
/// `{nanosecond: 2}` keeps the milliseconds).
pub fn truncate_time(
    unit: TruncateUnit,
    base: LocalTime,
    overrides: &[(String, Field)],
) -> Result<LocalTime, String> {
    let truncated = match unit {
        TruncateUnit::Hour => LocalTime::new(base.hour(), 0, 0, 0),
        TruncateUnit::Minute => LocalTime::new(base.hour(), base.minute(), 0, 0),
        TruncateUnit::Second => LocalTime::new(base.hour(), base.minute(), base.second(), 0),
        TruncateUnit::Millisecond => LocalTime::new(
            base.hour(),
            base.minute(),
            base.second(),
            base.nanosecond() / 1_000_000 * 1_000_000,
        ),
        TruncateUnit::Microsecond => LocalTime::new(
            base.hour(),
            base.minute(),
            base.second(),
            base.nanosecond() / 1_000 * 1_000,
        ),
        _ => LocalTime::new(0, 0, 0, 0),
    }
    .ok_or_else(|| "invalid truncated time".to_owned())?;
    let hour = component(overrides, "hour")?.unwrap_or(truncated.hour());
    let minute = component(overrides, "minute")?.unwrap_or(truncated.minute());
    let second = component(overrides, "second")?.unwrap_or(truncated.second());
    let millisecond =
        component(overrides, "millisecond")?.unwrap_or(truncated.nanosecond() / 1_000_000);
    let microsecond =
        component(overrides, "microsecond")?.unwrap_or(truncated.nanosecond() / 1_000 % 1_000);
    let nanosecond = component(overrides, "nanosecond")?.unwrap_or(truncated.nanosecond() % 1_000);
    let nanos = millisecond * 1_000_000 + microsecond * 1_000 + nanosecond;
    LocalTime::new(hour, minute, second, nanos)
        .ok_or_else(|| "time component out of range".to_owned())
}

/// A resolved timezone: the offset in seconds and an optional zone name.
pub type ResolvedZone = (i32, Option<Arc<str>>);

/// Resolve a `timezone` override against a truncated civil datetime.
pub fn timezone_override(
    fields: &[(String, Field)],
    date: Date,
    time: LocalTime,
) -> Result<Option<ResolvedZone>, String> {
    match timezone_part(fields)? {
        None => Ok(None),
        Some(Timezone::Offset(offset)) => Ok(Some((offset, None))),
        Some(Timezone::Named(name)) => {
            let civil = zone::Civil::from_date_and_nanos(date, time.nanos());
            let offset = zone::offset_for_civil(&name, civil)?;
            Ok(Some((offset, Some(Arc::from(name.as_str())))))
        }
    }
}

/// The offset in effect in `name` for a civil datetime, for recomputing
/// named-zone offsets after truncation.
pub fn zone_offset(name: &str, date: Date, time: LocalTime) -> Result<i32, String> {
    zone::offset_for_civil(name, zone::Civil::from_date_and_nanos(date, time.nanos()))
}

/// Build a duration from field map keys (`years`, `months`, `weeks`,
/// `days`, `hours`, `minutes`, `seconds`, `milliseconds`, `microseconds`,
/// `nanoseconds`).
pub fn duration_from_fields(fields: &[(String, Field)]) -> Result<Duration, String> {
    let mut components = Components::default();
    let mut any = false;
    for (key, field) in fields {
        let number = match field {
            Field::Number(value) => *value,
            _ => return Err(format!("`{key}` must be a number")),
        };
        let component = Component::from_f64(number)
            .ok_or_else(|| format!("`{key}` must be a finite number"))?;
        any = true;
        match key.as_str() {
            "years" => components.years = component,
            "months" => components.months = component,
            "weeks" => components.weeks = component,
            "days" => components.days = component,
            "hours" => components.hours = component,
            "minutes" => components.minutes = component,
            "seconds" => components.seconds = component,
            "milliseconds" => components.milliseconds = component,
            "microseconds" => components.microseconds = component,
            "nanoseconds" => components.nanoseconds = component,
            other => return Err(format!("unknown duration component `{other}`")),
        }
    }
    if !any {
        return Err("duration() requires at least one component".to_owned());
    }
    Duration::from_components(components).ok_or_else(|| "duration out of range".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(value: &str) -> Field {
        Field::Text(value.to_owned())
    }

    fn number(value: i64) -> Field {
        Field::Number(value as f64)
    }

    fn fields(entries: &[(&str, Field)]) -> Vec<(String, Field)> {
        entries
            .iter()
            .map(|(key, field)| ((*key).to_owned(), field.clone()))
            .collect()
    }

    #[test]
    fn week_dates_match_the_tck() {
        let date = date_from_fields(&fields(&[("year", number(1816)), ("week", number(1))]))
            .expect("date");
        assert_eq!(date.to_string(), "1816-01-01");
        let date = date_from_fields(&fields(&[("year", number(1818)), ("week", number(53))]))
            .expect("date");
        assert_eq!(date.to_string(), "1818-12-28");
        let date = date_from_fields(&fields(&[
            ("date", Field::Date(Date::new(1816, 12, 30).expect("date"))),
            ("week", number(2)),
            ("dayOfWeek", number(3)),
        ]))
        .expect("date");
        assert_eq!(date.to_string(), "1817-01-08");
    }

    #[test]
    fn calendar_and_quarter_keys() {
        let date = date_from_fields(&fields(&[
            ("year", number(1984)),
            ("month", number(10)),
            ("day", number(11)),
        ]))
        .expect("date");
        assert_eq!(date.to_string(), "1984-10-11");
        let date = date_from_fields(&fields(&[
            ("year", number(1984)),
            ("ordinalDay", number(202)),
        ]))
        .expect("date");
        assert_eq!(date.to_string(), "1984-07-20");
        let date = date_from_fields(&fields(&[
            ("year", number(1984)),
            ("quarter", number(3)),
            ("dayOfQuarter", number(45)),
        ]))
        .expect("date");
        assert_eq!(date.to_string(), "1984-08-14");
    }

    #[test]
    fn clock_keys_assemble_hierarchically() {
        let time = localtime_from_fields(&fields(&[
            ("hour", number(12)),
            ("minute", number(31)),
            ("second", number(14)),
            ("millisecond", number(123)),
            ("microsecond", number(456)),
            ("nanosecond", number(789)),
        ]))
        .expect("time");
        assert_eq!(time.to_string(), "12:31:14.123456789");
        let time = localtime_from_fields(&fields(&[("hour", number(12))])).expect("time");
        assert_eq!(time.to_string(), "12:00");
    }

    #[test]
    fn timezones_resolve_offsets_and_names() {
        let datetime = datetime_from_fields(&fields(&[
            ("year", number(1984)),
            ("month", number(10)),
            ("day", number(11)),
            ("hour", number(12)),
            ("timezone", text("+01:00")),
        ]))
        .expect("datetime");
        assert_eq!(datetime.to_string(), "1984-10-11T12:00+01:00");

        let datetime = datetime_from_fields(&fields(&[
            ("year", number(2017)),
            ("month", number(8)),
            ("day", number(8)),
            ("hour", number(12)),
            ("timezone", text("Europe/Stockholm")),
        ]))
        .expect("datetime");
        assert_eq!(
            datetime.to_string(),
            "2017-08-08T12:00+02:00[Europe/Stockholm]"
        );
    }

    #[test]
    fn whole_values_lay_the_foundation() {
        let base = Field::LocalDateTime(LocalDateTime::new(
            Date::new(1984, 3, 7).expect("date"),
            LocalTime::new(12, 31, 14, 645_000_000).expect("time"),
        ));
        let datetime =
            localdatetime_from_fields(&fields(&[("datetime", base), ("day", number(28))]))
                .expect("datetime");
        assert_eq!(datetime.to_string(), "1984-03-28T12:31:14.645");
    }

    #[test]
    fn durations_require_known_components() {
        let duration = duration_from_fields(&fields(&[
            ("years", number(12)),
            ("months", number(5)),
            ("seconds", number(70)),
            ("nanoseconds", number(1)),
        ]))
        .expect("duration");
        assert_eq!(duration.to_string(), "P12Y5MT1M10.000000001S");
        assert!(duration_from_fields(&fields(&[("centuries", number(1))])).is_err());
        assert!(duration_from_fields(&[]).is_err());
    }
}
