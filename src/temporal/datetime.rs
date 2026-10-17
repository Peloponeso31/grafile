//! Nanosecond-precision times and datetimes.
//!
//! Four value shapes mirror Neo4j: a wall-clock [`LocalTime`], a
//! [`Time`] that adds a fixed UTC offset, a zone-less [`LocalDateTime`]
//! stored as civil seconds, and a [`DateTime`] that carries an offset plus
//! an optional IANA zone name. Display and parsing follow the ISO 8601
//! forms the TCK exercises, including compact times (`214032.142`), short
//! offsets (`+01`, `-0100`) and zone suffixes (`[Europe/Stockholm]`).

use std::fmt;
use std::sync::Arc;

use crate::temporal::civil::Date;

/// Nanoseconds in a day.
pub const NANOS_PER_DAY: i64 = 86_400_000_000_000;
/// Nanoseconds in a second.
pub const NANOS_PER_SECOND: i64 = 1_000_000_000;
/// Largest offset the engine accepts, in seconds (`+18:00`).
pub const MAX_OFFSET_SECONDS: i32 = 18 * 3_600;

/// A wall-clock time of day with nanosecond precision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LocalTime {
    nanos: i64,
}

impl LocalTime {
    /// Build from time-of-day components.
    ///
    /// Returns `None` when any component is out of range.
    pub fn new(hour: i64, minute: i64, second: i64, nanos: i64) -> Option<Self> {
        if !(0..24).contains(&hour)
            || !(0..60).contains(&minute)
            || !(0..60).contains(&second)
            || !(0..NANOS_PER_SECOND).contains(&nanos)
        {
            return None;
        }
        Some(Self {
            nanos: hour * 3_600 * NANOS_PER_SECOND
                + minute * 60 * NANOS_PER_SECOND
                + second * NANOS_PER_SECOND
                + nanos,
        })
    }

    /// Build from a nanosecond-of-day count in `0..NANOS_PER_DAY`.
    pub fn from_nanos(nanos: i64) -> Option<Self> {
        if !(0..NANOS_PER_DAY).contains(&nanos) {
            return None;
        }
        Some(Self { nanos })
    }

    /// Build from any nanosecond count, wrapping into a day.
    pub fn from_nanos_wrapped(nanos: i64) -> Self {
        Self {
            nanos: nanos.rem_euclid(NANOS_PER_DAY),
        }
    }

    /// Nanoseconds since midnight.
    pub const fn nanos(self) -> i64 {
        self.nanos
    }

    /// Hour (0-23).
    pub const fn hour(self) -> i64 {
        self.nanos / (3_600 * NANOS_PER_SECOND)
    }

    /// Minute (0-59).
    pub const fn minute(self) -> i64 {
        (self.nanos / (60 * NANOS_PER_SECOND)) % 60
    }

    /// Second (0-59).
    pub const fn second(self) -> i64 {
        (self.nanos / NANOS_PER_SECOND) % 60
    }

    /// Milliseconds within the second.
    pub const fn millisecond(self) -> i64 {
        (self.nanos / 1_000_000) % 1_000
    }

    /// Microseconds within the second.
    pub const fn microsecond(self) -> i64 {
        (self.nanos / 1_000) % 1_000_000
    }

    /// Nanoseconds within the second.
    pub const fn nanosecond(self) -> i64 {
        self.nanos % NANOS_PER_SECOND
    }
}

/// Write a time of day, trimming seconds and the fraction when zero.
fn write_time(out: &mut String, nanos: i64) {
    let time = LocalTime::from_nanos_wrapped(nanos);
    out.push_str(&format!("{:02}:{:02}", time.hour(), time.minute()));
    if time.second() != 0 || time.nanosecond() != 0 {
        out.push_str(&format!(":{:02}", time.second()));
        write_fraction(out, time.nanosecond());
    }
}

/// Write a trimmed nanosecond fraction (`.142`, `.645876123`) or nothing.
fn write_fraction(out: &mut String, nanos: i64) {
    if nanos == 0 {
        return;
    }
    let mut fraction = format!("{nanos:09}");
    while fraction.ends_with('0') {
        fraction.pop();
    }
    out.push('.');
    out.push_str(&fraction);
}

/// Write an offset: `Z` for UTC, else `+01:00`.
fn write_offset(out: &mut String, offset: i32) {
    if offset == 0 {
        out.push('Z');
        return;
    }
    let sign = if offset < 0 { '-' } else { '+' };
    let offset = offset.unsigned_abs();
    out.push(sign);
    out.push_str(&format!(
        "{:02}:{:02}",
        offset / 3_600,
        (offset % 3_600) / 60
    ));
}

impl fmt::Display for LocalTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut out = String::new();
        write_time(&mut out, self.nanos);
        f.write_str(&out)
    }
}

impl std::str::FromStr for LocalTime {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        parse_local_time(text)
    }
}

/// A local time with a fixed UTC offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Time {
    local: LocalTime,
    offset: i32,
}

impl Time {
    /// Build from a local time and an offset in seconds.
    pub fn new(local: LocalTime, offset: i32) -> Option<Self> {
        if offset.abs() > MAX_OFFSET_SECONDS {
            return None;
        }
        Some(Self { local, offset })
    }

    /// The wall-clock time.
    pub const fn local(self) -> LocalTime {
        self.local
    }

    /// The offset from UTC in seconds.
    pub const fn offset(self) -> i32 {
        self.offset
    }

    /// Nanoseconds since midnight in UTC (may wrap outside a day).
    pub const fn utc_nanos(self) -> i64 {
        self.local.nanos() - self.offset as i64 * NANOS_PER_SECOND
    }

    /// Offset minutes.
    pub const fn offset_minutes(self) -> i64 {
        (self.offset / 60) as i64
    }

    /// Offset seconds.
    pub const fn offset_seconds(self) -> i64 {
        self.offset as i64
    }
}

impl PartialOrd for Time {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Time {
    /// Times compare by their UTC instant, then by offset.
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (self.utc_nanos(), self.offset).cmp(&(other.utc_nanos(), other.offset))
    }
}

impl fmt::Display for Time {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut out = String::new();
        write_time(&mut out, self.local.nanos());
        write_offset(&mut out, self.offset);
        f.write_str(&out)
    }
}

impl std::str::FromStr for Time {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let (local, offset) = split_time_offset(text)?;
        let local = parse_local_time(local)?;
        Time::new(local, offset).ok_or_else(|| format!("offset out of range in `{text}`"))
    }
}

/// A civil datetime without a time zone, as seconds and nanoseconds since
/// 1970-01-01T00:00:00 with the nanoseconds always in `0..1e9` (floor
/// semantics, so negative values stay canonical).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LocalDateTime {
    seconds: i64,
    nanos: i32,
}

impl LocalDateTime {
    /// Build from whole seconds and a nanosecond part (canonicalized).
    pub fn from_epoch(seconds: i64, nanos: i64) -> Option<Self> {
        let total = (seconds as i128) * NANOS_PER_SECOND as i128 + nanos as i128;
        let seconds = i64::try_from(total.div_euclid(NANOS_PER_SECOND as i128)).ok()?;
        let nanos = (total.rem_euclid(NANOS_PER_SECOND as i128)) as i32;
        Some(Self { seconds, nanos })
    }

    /// Build from a civil date and time.
    pub fn new(date: Date, time: LocalTime) -> Self {
        let seconds = date
            .days()
            .saturating_mul(86_400)
            .saturating_add(time.nanos() / NANOS_PER_SECOND);
        Self {
            seconds,
            nanos: (time.nanos() % NANOS_PER_SECOND) as i32,
        }
    }

    /// Seconds since 1970-01-01T00:00 (floor).
    pub const fn seconds(self) -> i64 {
        self.seconds
    }

    /// Nanoseconds within the second.
    pub const fn nanos(self) -> i32 {
        self.nanos
    }

    /// The civil date.
    pub fn date(self) -> Date {
        Date::from_days(self.seconds.div_euclid(86_400))
    }

    /// The wall-clock time.
    pub fn time(self) -> LocalTime {
        let second_of_day = self.seconds.rem_euclid(86_400);
        LocalTime::from_nanos(second_of_day * NANOS_PER_SECOND + self.nanos as i64)
            .expect("canonical nanoseconds")
    }

    /// Nanoseconds since the epoch as a wide integer.
    pub const fn epoch_nanos(self) -> i128 {
        self.seconds as i128 * NANOS_PER_SECOND as i128 + self.nanos as i128
    }
}

impl fmt::Display for LocalDateTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut out = self.date().to_string();
        out.push('T');
        write_time(&mut out, self.time().nanos());
        f.write_str(&out)
    }
}

impl std::str::FromStr for LocalDateTime {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        parse_local_datetime(text)
    }
}

/// A datetime with a fixed offset and an optional IANA zone name.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DateTime {
    /// Local civil seconds (see [`LocalDateTime`]).
    seconds: i64,
    /// Nanoseconds within the local second.
    nanos: i32,
    /// Offset from UTC in seconds.
    offset: i32,
    /// Canonical IANA zone name, when the value carries one.
    zone: Option<Arc<str>>,
}

impl DateTime {
    /// Build from a local datetime, an offset and an optional zone.
    pub fn new(local: LocalDateTime, offset: i32, zone: Option<Arc<str>>) -> Option<Self> {
        if offset.abs() > MAX_OFFSET_SECONDS {
            return None;
        }
        Some(Self {
            seconds: local.seconds(),
            nanos: local.nanos(),
            offset,
            zone,
        })
    }

    /// The local civil datetime.
    pub fn local(&self) -> LocalDateTime {
        LocalDateTime::from_epoch(self.seconds, self.nanos as i64).expect("canonical nanoseconds")
    }

    /// The offset from UTC in seconds.
    pub const fn offset(&self) -> i32 {
        self.offset
    }

    /// The IANA zone name, when present.
    pub fn zone(&self) -> Option<&str> {
        self.zone.as_deref()
    }

    /// Nanoseconds since the epoch (UTC) as a wide integer.
    pub const fn instant_nanos(&self) -> i128 {
        (self.seconds as i128 - self.offset as i128) * NANOS_PER_SECOND as i128 + self.nanos as i128
    }

    /// Whole seconds since the epoch (floor).
    pub const fn epoch_seconds(&self) -> i64 {
        let total = self.instant_nanos();
        (total.div_euclid(NANOS_PER_SECOND as i128)) as i64
    }

    /// Milliseconds since the epoch.
    pub const fn epoch_millis(&self) -> i64 {
        let total = self.instant_nanos();
        (total.div_euclid(1_000_000)) as i64
    }
}

impl PartialOrd for DateTime {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for DateTime {
    /// Datetimes compare by instant, then offset, then zone name.
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (self.instant_nanos(), self.offset, self.zone.as_deref()).cmp(&(
            other.instant_nanos(),
            other.offset,
            other.zone.as_deref(),
        ))
    }
}

impl fmt::Display for DateTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut out = self.local().to_string();
        write_offset(&mut out, self.offset);
        if let Some(zone) = &self.zone {
            out.push('[');
            out.push_str(zone);
            out.push(']');
        }
        f.write_str(&out)
    }
}

impl std::str::FromStr for DateTime {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        parse_datetime(text)
    }
}

/// Parse a time of day: `21`, `2140`, `214032.142`, `21:40:32`.
fn parse_local_time(text: &str) -> Result<LocalTime, String> {
    let invalid = || format!("invalid time `{text}`");
    let (digits, fraction) = split_fraction(text);
    if fraction.is_empty() && text.contains('.') {
        return Err(invalid());
    }
    let nanos = parse_fraction(fraction)?;
    let (hour, minute, second) = if digits.contains(':') {
        let mut parts = digits.split(':');
        let hour = parts.next().ok_or_else(invalid)?;
        let minute = parts.next().unwrap_or("0");
        let second = parts.next().unwrap_or("0");
        if parts.next().is_some() || hour.is_empty() || hour.len() > 2 {
            return Err(invalid());
        }
        (
            parse_hour(hour).ok_or_else(invalid)?,
            parse_component(minute).ok_or_else(invalid)?,
            parse_component(second).ok_or_else(invalid)?,
        )
    } else {
        match digits.len() {
            1 | 2 => (parse_hour(digits).ok_or_else(invalid)?, 0, 0),
            4 => (
                parse_hour(&digits[..2]).ok_or_else(invalid)?,
                parse_component(&digits[2..]).ok_or_else(invalid)?,
                0,
            ),
            6 => (
                parse_hour(&digits[..2]).ok_or_else(invalid)?,
                parse_component(&digits[2..4]).ok_or_else(invalid)?,
                parse_component(&digits[4..]).ok_or_else(invalid)?,
            ),
            _ => return Err(invalid()),
        }
    };
    LocalTime::new(hour, minute, second, nanos).ok_or_else(invalid)
}

fn split_fraction(text: &str) -> (&str, &str) {
    match text.split_once('.') {
        Some((whole, fraction)) => (whole, fraction),
        None => (text, ""),
    }
}

fn parse_fraction(fraction: &str) -> Result<i64, String> {
    if fraction.is_empty() {
        return Ok(0);
    }
    if !fraction.bytes().all(|byte| byte.is_ascii_digit()) || fraction.len() > 9 {
        return Err(format!("invalid fraction `.{fraction}`"));
    }
    let mut nanos: i64 = fraction
        .parse()
        .map_err(|_| "invalid fraction".to_owned())?;
    for _ in fraction.len()..9 {
        nanos *= 10;
    }
    Ok(nanos)
}

fn parse_hour(text: &str) -> Option<i64> {
    let value: i64 = text.parse().ok()?;
    (0..24).contains(&value).then_some(value)
}

fn parse_component(text: &str) -> Option<i64> {
    let value: i64 = text.parse().ok()?;
    (0..60).contains(&value).then_some(value)
}

/// Split a time string into its local part and an offset in seconds. Handles
/// `Z`, `+01`, `-0100` and `+01:30`.
fn split_time_offset(text: &str) -> Result<(&str, i32), String> {
    let invalid = || format!("invalid offset in `{text}`");
    if let Some(local) = text.strip_suffix('Z') {
        return Ok((local, 0));
    }
    let split = text
        .char_indices()
        .find(|(_, ch)| *ch == '+' || *ch == '-')
        .map(|(index, _)| index);
    let Some(index) = split else {
        return Err(invalid());
    };
    let local = &text[..index];
    parse_offset(&text[index..]).map(|offset| (local, offset))
}

/// Parse an offset: `Z`, `+01`, `-0100`, `+01:30`.
fn parse_offset(text: &str) -> Result<i32, String> {
    let invalid = || format!("invalid offset `{text}`");
    if text == "Z" {
        return Ok(0);
    }
    let (sign, rest) = match text.strip_prefix('+') {
        Some(rest) => (1, rest),
        None => match text.strip_prefix('-') {
            Some(rest) => (-1, rest),
            None => return Err(invalid()),
        },
    };
    let (hours, minutes) = if let Some((hours, minutes)) = rest.split_once(':') {
        (hours, minutes)
    } else {
        match rest.len() {
            2 => (rest, "0"),
            4 => (&rest[..2], &rest[2..]),
            _ => return Err(invalid()),
        }
    };
    if hours.len() != 2 || minutes.len() > 2 {
        return Err(invalid());
    }
    let hours: i32 = hours.parse().map_err(|_| invalid())?;
    let minutes: i32 = minutes.parse().map_err(|_| invalid())?;
    if hours > 18 || minutes > 59 {
        return Err(invalid());
    }
    let offset = sign * (hours * 3_600 + minutes * 60);
    if offset.abs() > MAX_OFFSET_SECONDS {
        return Err(invalid());
    }
    Ok(offset)
}

/// Parse `YYYY-MM-DDT<time>` forms, including compact dates and times.
fn parse_local_datetime(text: &str) -> Result<LocalDateTime, String> {
    let invalid = || format!("invalid datetime `{text}`");
    let (date, time) = text.split_once(['T', 't']).ok_or_else(invalid)?;
    let date: Date = date.parse()?;
    let time = parse_local_time(time)?;
    Ok(LocalDateTime::new(date, time))
}

/// Parse a datetime with an offset and optional `[zone]` suffix.
fn parse_datetime(text: &str) -> Result<DateTime, String> {
    let invalid = || format!("invalid datetime `{text}`");
    let (rest, zone) = match text.split_once('[') {
        Some((rest, zone)) => {
            let zone = zone.strip_suffix(']').ok_or_else(invalid)?;
            (rest, Some(zone))
        }
        None => (text, None),
    };
    // Only the time part carries an offset; the date has its own dashes.
    let (date, time) = rest.split_once(['T', 't']).ok_or_else(invalid)?;
    let (time, offset) = split_time_offset(time)?;
    let local = LocalDateTime::new(date.parse()?, parse_local_time(time)?);
    let zone = match zone {
        Some(name) => {
            let canonical = crate::temporal::zone::canonical_name(name)
                .ok_or_else(|| format!("unknown time zone `{name}`"))?;
            Some(Arc::from(canonical.as_str()))
        }
        None => None,
    };
    DateTime::new(local, offset, zone).ok_or_else(invalid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_time_display_trims_seconds_and_fraction() {
        let cases = [
            ((21, 40, 0, 0), "21:40"),
            ((21, 40, 32, 0), "21:40:32"),
            ((21, 40, 32, 142_000_000), "21:40:32.142"),
            ((12, 31, 14, 645_876_123), "12:31:14.645876123"),
            ((0, 0, 0, 0), "00:00"),
        ];
        for ((hour, minute, second, nanos), expected) in cases {
            let time = LocalTime::new(hour, minute, second, nanos).expect("time");
            assert_eq!(time.to_string(), expected);
        }
    }

    #[test]
    fn parses_every_tck_time_form() {
        let cases = [
            ("21:40:32.142", "21:40:32.142"),
            ("214032.142", "21:40:32.142"),
            ("21:40:32", "21:40:32"),
            ("214032", "21:40:32"),
            ("21:40", "21:40"),
            ("2140", "21:40"),
            ("21", "21:00"),
        ];
        for (text, expected) in cases {
            let parsed: LocalTime = text.parse().expect(text);
            assert_eq!(parsed.to_string(), expected, "{text}");
        }
    }

    #[test]
    fn parses_time_offsets() {
        let cases = [
            ("21:40:32.142+0100", "21:40:32.142+01:00"),
            ("214032.142Z", "21:40:32.142Z"),
            ("21:40:32+01:00", "21:40:32+01:00"),
            ("214032-0100", "21:40:32-01:00"),
            ("21:40-01:30", "21:40-01:30"),
            ("2140-00:00", "21:40Z"),
            ("2140-02", "21:40-02:00"),
            ("22+18:00", "22:00+18:00"),
        ];
        for (text, expected) in cases {
            let parsed: Time = text.parse().expect(text);
            assert_eq!(parsed.to_string(), expected, "{text}");
        }
    }

    #[test]
    fn compares_times_by_utc_instant() {
        // 10:00+01:00 is 09:00 UTC, before 09:35 UTC.
        let first: Time = "10:00+01:00".parse().expect("time");
        let second: Time = "09:35:14.645876123+00:00".parse().expect("time");
        assert!(first < second);
    }

    #[test]
    fn parses_local_datetimes() {
        let cases = [
            ("2015-07-21T21:40:32.142", "2015-07-21T21:40:32.142"),
            ("2015-W30-2T214032.142", "2015-07-21T21:40:32.142"),
            ("2015-202T21:40:32", "2015-07-21T21:40:32"),
            ("2015T214032", "2015-01-01T21:40:32"),
            ("20150721T21:40", "2015-07-21T21:40"),
            ("2015-W30T2140", "2015-07-20T21:40"),
            ("2015202T21", "2015-07-21T21:00"),
        ];
        for (text, expected) in cases {
            let parsed: LocalDateTime = text.parse().expect(text);
            assert_eq!(parsed.to_string(), expected, "{text}");
        }
    }

    #[test]
    fn parses_datetimes() {
        let cases = [
            (
                "2015-07-21T21:40:32.142+0100",
                "2015-07-21T21:40:32.142+01:00",
            ),
            ("2015-W30-2T214032.142Z", "2015-07-21T21:40:32.142Z"),
            ("2015-202T21:40:32+01:00", "2015-07-21T21:40:32+01:00"),
            ("2015T214032-0100", "2015-01-01T21:40:32-01:00"),
            ("20150721T21:40-01:30", "2015-07-21T21:40-01:30"),
            ("2015-W30T2140-00:00", "2015-07-20T21:40Z"),
            ("2015-W30T2140-02", "2015-07-20T21:40-02:00"),
            ("2015202T21+18:00", "2015-07-21T21:00+18:00"),
        ];
        for (text, expected) in cases {
            let parsed: DateTime = text.parse().expect(text);
            assert_eq!(parsed.to_string(), expected, "{text}");
        }
    }

    #[test]
    fn parses_named_zone_datetimes() {
        let text = "2015-07-21T21:40:32.142+02:00[Europe/Stockholm]";
        let parsed: DateTime = text.parse().expect(text);
        assert_eq!(parsed.to_string(), text);
        assert_eq!(parsed.zone(), Some("Europe/Stockholm"));
    }

    #[test]
    fn instant_and_accessors() {
        let text = "1984-11-11T12:31:14.645876123+01:00";
        let parsed: DateTime = text.parse().expect(text);
        let local = parsed.local();
        assert_eq!(local.date().year(), 1984);
        assert_eq!(local.date().month(), 11);
        assert_eq!(local.date().day(), 11);
        assert_eq!(local.date().quarter(), 4);
        assert_eq!(local.date().iso_week(), (1984, 45));
        assert_eq!(local.date().ordinal_day(), 316);
        assert_eq!(local.date().iso_weekday(), 7);
        assert_eq!(local.date().day_of_quarter(), 42);
        assert_eq!(local.time().hour(), 12);
        assert_eq!(local.time().millisecond(), 645);
        assert_eq!(local.time().microsecond(), 645_876);
        assert_eq!(local.time().nanosecond(), 645_876_123);
        assert_eq!(parsed.offset(), 3_600);
        assert_eq!(parsed.epoch_seconds(), 469_020_674);
        assert_eq!(parsed.epoch_millis(), 469_020_674_645);
    }

    #[test]
    fn rejects_malformed_text() {
        for text in ["25:00", "2161", "21:40:32."] {
            assert!(text.parse::<LocalTime>().is_err(), "{text}");
        }
        for text in ["2015-07-21 21:40", "2015-07-21T25:00"] {
            assert!(text.parse::<LocalDateTime>().is_err(), "{text}");
        }
        for text in ["2015-07-21T21:40+19:00", "x"] {
            assert!(text.parse::<DateTime>().is_err(), "{text}");
        }
    }
}
