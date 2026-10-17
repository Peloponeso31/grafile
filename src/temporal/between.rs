//! `duration.between` and the `duration.inMonths`/`inDays`/`inSeconds`
//! family.
//!
//! The three `in*` functions answer "how many whole units apart are these
//! instants"; `between` decomposes the difference into whole months, then
//! whole days, then the remaining time. Missing components are filled from
//! the other side: a time-only value borrows the other's date, and when only
//! one side carries a time zone the other side assumes it (resolving offsets
//! through the zone rules for its own civil time).

use std::sync::Arc;

use crate::temporal::civil::Date;
use crate::temporal::datetime::{DateTime, LocalDateTime, LocalTime, Time};
use crate::temporal::duration::Duration;

/// A temporal value normalized for difference computations.
#[derive(Debug, Clone)]
pub struct Instant {
    /// Civil datetime.
    pub local: LocalDateTime,
    /// Whether the source value carried a date component.
    pub has_date: bool,
    /// IANA zone name, when the source carried one.
    pub zone: Option<Arc<str>>,
    /// Fixed offset in seconds (stored offset, or none for zone-less values).
    pub offset: Option<i32>,
}

const NANOS_PER_DAY: i128 = 86_400_000_000_000;

impl Instant {
    /// Normalize a temporal engine value; `None` for non-temporal values.
    pub fn from_parts(
        date: Option<Date>,
        time: Option<LocalTime>,
        zone: Option<Arc<str>>,
        offset: Option<i32>,
    ) -> Self {
        let has_date = date.is_some();
        let date = date.unwrap_or_else(|| Date::new(1970, 1, 1).expect("epoch"));
        let time = time.unwrap_or_else(|| LocalTime::new(0, 0, 0, 0).expect("midnight"));
        Self {
            local: LocalDateTime::new(date, time),
            has_date,
            zone,
            offset,
        }
    }

    /// The offset in effect for this value's civil datetime.
    fn resolved_offset(&self) -> i32 {
        if let Some(zone) = &self.zone {
            crate::temporal::construct::zone_offset(zone, self.local.date(), self.local.time())
                .unwrap_or(self.offset.unwrap_or(0))
        } else {
            self.offset.unwrap_or(0)
        }
    }

    /// Nanoseconds since the epoch.
    fn instant_nanos(&self) -> i128 {
        self.local.epoch_nanos() - self.resolved_offset() as i128 * 1_000_000_000
    }

    /// Give this value the zone/offset of the other side when only the
    /// other side has one.
    fn inherit_zone(&mut self, other: &Self) {
        if self.zone.is_none() && self.offset.is_none() {
            self.zone = other.zone.clone();
            self.offset = other.offset;
        }
    }

    /// Borrow the other side's date when this side is time-only.
    fn inherit_date(&mut self, other: &Self) {
        if !self.has_date {
            self.local = LocalDateTime::new(other.local.date(), self.local.time());
        }
    }
}

/// Convert a temporal engine value into an [`Instant`], or `None` for
/// non-temporal values.
pub fn instant_of(value: &crate::record::Value) -> Option<Instant> {
    use crate::record::Value;
    Some(match value {
        Value::Date(date) => Instant::from_parts(Some(*date), None, None, None),
        Value::LocalTime(time) => Instant::from_parts(None, Some(*time), None, None),
        Value::Time(time) => {
            Instant::from_parts(None, Some(time.local()), None, Some(time.offset()))
        }
        Value::LocalDateTime(datetime) => {
            Instant::from_parts(Some(datetime.date()), Some(datetime.time()), None, None)
        }
        Value::DateTime(datetime) => Instant::from_parts(
            Some(datetime.local().date()),
            Some(datetime.local().time()),
            datetime.zone().map(Arc::<str>::from),
            Some(datetime.offset()),
        ),
        _ => return None,
    })
}

/// Prepare two values: fill zone and date defaults on both sides.
fn prepare(from: &Instant, to: &Instant) -> (Instant, Instant) {
    let (mut from, mut to) = (from.clone(), to.clone());
    from.inherit_zone(&to);
    to.inherit_zone(&from);
    from.inherit_date(&to);
    to.inherit_date(&from);
    (from, to)
}
/// Whole months between two values: the calendar-month difference, walked
/// back when `from` plus that many months would overshoot `to`.
fn months_between(from: &Instant, to: &Instant) -> i64 {
    let (from_date, to_date) = (from.local.date(), to.local.date());
    let months = (to_date.year() - from_date.year()) * 12
        + (to_date.month() as i64 - from_date.month() as i64);
    let Some(intermediate_date) = from_date.add_months(months) else {
        return months;
    };
    let intermediate = Instant {
        local: LocalDateTime::new(intermediate_date, from.local.time()),
        has_date: true,
        zone: from.zone.clone(),
        offset: from.offset,
    };
    let overshot = match months.cmp(&0) {
        std::cmp::Ordering::Greater => intermediate.instant_nanos() > to.instant_nanos(),
        std::cmp::Ordering::Less => intermediate.instant_nanos() < to.instant_nanos(),
        std::cmp::Ordering::Equal => false,
    };
    if overshot {
        months - months.signum()
    } else {
        months
    }
}

/// Split a nanosecond difference into (whole days, seconds, nanoseconds),
/// truncating toward zero like Neo4j.
fn split_difference(nanos: i128) -> Option<Duration> {
    let days = nanos / NANOS_PER_DAY;
    let rest = nanos % NANOS_PER_DAY;
    let seconds = rest / 1_000_000_000;
    let sub = (rest % 1_000_000_000) as i64;
    Duration::new(
        0,
        i64::try_from(days).ok()?,
        i64::try_from(seconds).ok()?,
        sub,
    )
}

/// `duration.inMonths(from, to)`: whole months, dates only.
pub fn in_months(from: &Instant, to: &Instant) -> Option<Duration> {
    let (from, to) = prepare(from, to);
    Duration::new(months_between(&from, &to), 0, 0, 0)
}

/// `duration.inDays(from, to)`: whole days of the instant difference,
/// truncated toward zero.
pub fn in_days(from: &Instant, to: &Instant) -> Option<Duration> {
    let (from, to) = prepare(from, to);
    let days = (to.instant_nanos() - from.instant_nanos()) / NANOS_PER_DAY;
    Duration::new(0, i64::try_from(days).ok()?, 0, 0)
}

/// `duration.inSeconds(from, to)`: the exact instant difference, with no
/// day component (`PT25H`, never `P1DT1H`).
pub fn in_seconds(from: &Instant, to: &Instant) -> Option<Duration> {
    let (from, to) = prepare(from, to);
    let nanos = to.instant_nanos() - from.instant_nanos();
    let seconds = nanos / 1_000_000_000;
    let sub = (nanos % 1_000_000_000) as i64;
    Duration::new(0, 0, i64::try_from(seconds).ok()?, sub)
}

/// `duration.between(from, to)`: whole months, then whole days, then the
/// remaining time.
pub fn between(from: &Instant, to: &Instant) -> Option<Duration> {
    let (from, to) = prepare(from, to);
    let months = months_between(&from, &to);
    let intermediate_date = from.local.date().add_months(months)?;
    let intermediate = Instant {
        local: LocalDateTime::new(intermediate_date, from.local.time()),
        has_date: true,
        zone: from.zone.clone(),
        offset: from.offset,
    };
    let remainder = to.instant_nanos() - intermediate.instant_nanos();
    let rest = split_difference(remainder)?;
    Duration::new(months, rest.days(), rest.seconds(), rest.nanos() as i64)
}

/// `duration.inSeconds` of two datetimes sharing a named zone, resolving the
/// assumed zone's offset for a civil time.
pub fn zoned_offset(zone: &str, local: LocalDateTime) -> Result<i32, String> {
    crate::temporal::construct::zone_offset(zone, local.date(), local.time())
}

/// Build a `DateTime` from its components, used by tests.
pub fn datetime_from(
    local: LocalDateTime,
    offset: i32,
    zone: Option<Arc<str>>,
) -> Option<DateTime> {
    DateTime::new(local, offset, zone)
}

/// Build a `Time` from its components, used by tests.
pub fn time_from(local: LocalTime, offset: i32) -> Option<Time> {
    Time::new(local, offset)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::Value;

    fn value(text: &str) -> Value {
        let (kind, inner) = match text.split_once('(') {
            Some((kind, rest)) => (kind, rest.trim_end_matches(')').trim_matches('\'')),
            None => ("", text),
        };
        match kind {
            "date" => Value::Date(inner.parse().expect(inner)),
            "datetime" => Value::DateTime(inner.parse().expect(inner)),
            "localdatetime" => Value::LocalDateTime(inner.parse().expect(inner)),
            "time" => Value::Time(inner.parse().expect(inner)),
            "localtime" => Value::LocalTime(inner.parse().expect(inner)),
            _ => panic!("cannot parse {text}"),
        }
    }

    fn assert_between(from: &str, to: &str, expected: &str) {
        let from = instant_of(&value(from)).expect("from");
        let to = instant_of(&value(to)).expect("to");
        let result = between(&from, &to).expect("between");
        assert_eq!(result.to_string(), expected, "{from:?} -> {to:?}");
    }

    #[test]
    fn months_days_and_seconds() {
        let cases = [
            ("date('1984-10-11')", "date('2015-06-24')", "P30Y8M"),
            ("date('1984-10-11')", "localtime('16:30')", "PT0S"),
            (
                "localdatetime('2015-07-21T21:40:32.142')",
                "localdatetime('2016-07-21T21:45:22.142')",
                "P1Y",
            ),
        ];
        for (from, to, expected) in cases {
            let from = instant_of(&value(from)).expect("from");
            let to = instant_of(&value(to)).expect("to");
            assert_eq!(
                in_months(&from, &to).expect("months").to_string(),
                expected,
                "{from:?} -> {to:?}"
            );
        }

        let from = instant_of(&value("date('1984-10-11')")).expect("from");
        let to = instant_of(&value("datetime('2015-07-21T21:40:32.142+0100')")).expect("to");
        assert_eq!(in_days(&from, &to).expect("days").to_string(), "P11240D");

        let from = instant_of(&value("date('1984-10-11')")).expect("from");
        let to = instant_of(&value("localtime('16:30')")).expect("to");
        assert_eq!(
            in_seconds(&from, &to).expect("seconds").to_string(),
            "PT16H30M"
        );
    }

    #[test]
    fn between_decomposes_like_the_tck() {
        assert_between("date('1984-10-11')", "date('2015-06-24')", "P30Y8M13D");
        assert_between(
            "date('1984-10-11')",
            "localdatetime('2016-07-21T21:45:22.142')",
            "P31Y9M10DT21H45M22.142S",
        );
        assert_between(
            "datetime('2014-07-21T21:40:36.143+0200')",
            "date('2015-06-24')",
            "P11M2DT2H19M23.857S",
        );
        assert_between(
            "localdatetime('2015-07-21T21:40:32.142')",
            "date('2015-06-24')",
            "P-27DT-21H-40M-32.142S",
        );
        assert_between(
            "localtime('14:30')",
            "datetime('2015-07-21T21:40:32.142+0100')",
            "PT7H10M32.142S",
        );
    }

    #[test]
    fn between_split_boundaries() {
        assert_between(
            "localdatetime('2018-01-01T12:00')",
            "localdatetime('2018-01-02T10:00')",
            "PT22H",
        );
        assert_between(
            "localdatetime('2018-01-01T10:00:00.2')",
            "localdatetime('2018-01-02T10:00:00.1')",
            "PT23H59M59.9S",
        );
        let from = instant_of(&value("localdatetime('2018-01-02T10:00:00.1')")).expect("from");
        let to = instant_of(&value("localdatetime('2018-01-01T10:00:00.2')")).expect("to");
        let result = between(&from, &to).expect("between");
        assert_eq!(result.to_string(), "PT-23H-59M-59.9S");
        assert_eq!(result.seconds(), -86_400);
        assert_eq!(result.nanoseconds_of_second(), 100_000_000);
    }

    #[test]
    fn daylight_saving_seconds() {
        let from = instant_of(&value(
            "datetime('2017-10-29T00:00+02:00[Europe/Stockholm]')",
        ))
        .expect("from");
        let to = instant_of(&value("localdatetime('2017-10-29T04:00')")).expect("to");
        assert_eq!(in_seconds(&from, &to).expect("seconds").to_string(), "PT5H");
        let from = instant_of(&value(
            "datetime('2017-10-29T00:00+02:00[Europe/Stockholm]')",
        ))
        .expect("from");
        let to = instant_of(&value("date('2017-10-30')")).expect("to");
        assert_eq!(
            in_seconds(&from, &to).expect("seconds").to_string(),
            "PT25H"
        );
    }
}
