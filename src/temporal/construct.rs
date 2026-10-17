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
        let weekday = component(fields, "dayOfWeek")?.unwrap_or(1) as u32;
        // Without an explicit year, the base date's ISO week year applies:
        // `{date: date('1816-12-30'), week: 2}` is week 2 of 1817.
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
    let (hours, minutes) = match rest.split_once(':') {
        Some((hours, minutes)) => (hours, minutes),
        None => match rest.len() {
            2 => (rest, "0"),
            4 => (&rest[..2], &rest[2..]),
            _ => return Err(invalid()),
        },
    };
    let hours: i32 = hours.parse().map_err(|_| invalid())?;
    let minutes: i32 = minutes.parse().map_err(|_| invalid())?;
    if hours > 18 || minutes > 59 {
        return Err(invalid());
    }
    let offset = sign * (hours * 3_600 + minutes * 60);
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
