//! IANA time zone lookups.
//!
//! Only the zone database and DST rules come from [`jiff`]; dates, times and
//! arithmetic are owned by this module's siblings, so the engine keeps its
//! wider year range. The bundled database makes lookups deterministic and
//! independent of the host's `/usr/share/zoneinfo`.

use crate::temporal::civil::{Date, civil_from_days};

/// Smallest civil year the time zone database can resolve. Jiff's civil
/// types stop at year 9999, while grafile's dates go far beyond; zone
/// lookups outside this window report an error.
pub const MIN_ZONE_YEAR: i64 = -9999;
/// Largest civil year the time zone database can resolve.
pub const MAX_ZONE_YEAR: i64 = 9999;

/// An offset from UTC in seconds.
pub type Offset = i32;

/// Whether `name` is a known IANA time zone identifier.
pub fn is_known(name: &str) -> bool {
    jiff::tz::TimeZone::get(name).is_ok()
}

/// The canonical name of a zone (`america/NEW_YORK` becomes
/// `America/New_York`), if the zone exists.
pub fn canonical_name(name: &str) -> Option<String> {
    let zone = jiff::tz::TimeZone::get(name).ok()?;
    zone.iana_name().map(str::to_owned)
}

/// The offset in effect in `name` at the given UTC instant (seconds and
/// nanoseconds east of 1970-01-01T00:00:00Z).
pub fn offset_at_instant(name: &str, seconds: i64, nanos: i32) -> Result<Offset, String> {
    let zone = jiff::tz::TimeZone::get(name).map_err(|_| format!("unknown time zone `{name}`"))?;
    let timestamp = jiff::Timestamp::new(seconds, nanos)
        .map_err(|_| "instant out of range for the time zone database".to_owned())?;
    Ok(zone.to_offset(timestamp).seconds())
}

/// The offset in effect in `name` for a civil datetime, resolving DST gaps
/// and folds the way jiff's "compatible" strategy does (the time after a gap,
/// the first occurrence in a fold).
pub fn offset_for_civil(name: &str, civil: Civil) -> Result<Offset, String> {
    let zone = jiff::tz::TimeZone::get(name).map_err(|_| format!("unknown time zone `{name}`"))?;
    let datetime = civil.to_jiff()?;
    let zoned = zone
        .to_ambiguous_zoned(datetime)
        .compatible()
        .map_err(|err| format!("cannot resolve `{name}` at {civil}: {err}"))?;
    Ok(zoned.offset().seconds())
}

/// A civil datetime for zone lookups.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Civil {
    /// Year.
    pub year: i64,
    /// Month (1-12).
    pub month: u32,
    /// Day (1-31).
    pub day: u32,
    /// Hour (0-23).
    pub hour: i64,
    /// Minute (0-59).
    pub minute: i64,
    /// Second (0-59).
    pub second: i64,
    /// Nanosecond within the second.
    pub nanos: i32,
}

impl Civil {
    /// Split a local timestamp (seconds and nanoseconds since 1970-01-01)
    /// into civil components.
    pub fn from_epoch(seconds: i64, nanos: i32) -> Self {
        let days = seconds.div_euclid(86_400);
        let second_of_day = seconds.rem_euclid(86_400);
        let (year, month, day) = civil_from_days(days);
        Self {
            year,
            month,
            day,
            hour: second_of_day / 3600,
            minute: (second_of_day % 3600) / 60,
            second: second_of_day % 60,
            nanos,
        }
    }

    /// Build from a date and a nanosecond-of-day value.
    pub fn from_date_and_nanos(date: Date, nanos_of_day: i64) -> Self {
        let (year, month, day) = civil_from_days(date.days());
        let seconds = nanos_of_day.div_euclid(1_000_000_000);
        Self {
            year,
            month,
            day,
            hour: seconds / 3600,
            minute: (seconds % 3600) / 60,
            second: seconds % 60,
            nanos: nanos_of_day.rem_euclid(1_000_000_000) as i32,
        }
    }

    fn to_jiff(self) -> Result<jiff::civil::DateTime, String> {
        let year = i16::try_from(self.year)
            .map_err(|_| "year out of range for the time zone database".to_owned())?;
        if !(MIN_ZONE_YEAR..=MAX_ZONE_YEAR).contains(&self.year) {
            return Err("year out of range for the time zone database".to_owned());
        }
        let date = jiff::civil::Date::new(year, self.month as i8, self.day as i8)
            .map_err(|err| format!("invalid civil date for zone lookup: {err}"))?;
        Ok(date.at(
            self.hour as i8,
            self.minute as i8,
            self.second as i8,
            self.nanos,
        ))
    }
}

impl std::fmt::Display for Civil {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
            self.year, self.month, self.day, self.hour, self.minute, self.second
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stockholm_offsets_include_dst() {
        // 2017-08-08 is in daylight saving time (+02:00).
        let summer = Civil {
            year: 2017,
            month: 8,
            day: 8,
            hour: 12,
            minute: 31,
            second: 14,
            nanos: 0,
        };
        assert_eq!(offset_for_civil("Europe/Stockholm", summer), Ok(7200));

        // 2017-01-08 is in standard time (+01:00).
        let winter = Civil {
            year: 2017,
            month: 1,
            day: 8,
            hour: 12,
            minute: 0,
            second: 0,
            nanos: 0,
        };
        assert_eq!(offset_for_civil("Europe/Stockholm", winter), Ok(3600));
    }

    #[test]
    fn dst_transition_day_has_a_25_hour_day() {
        // Europe/Stockholm left DST on 2017-10-29 at 03:00.
        let start = Civil {
            year: 2017,
            month: 10,
            day: 29,
            hour: 0,
            minute: 0,
            second: 0,
            nanos: 0,
        };
        let end = Civil {
            year: 2017,
            month: 10,
            day: 29,
            hour: 4,
            minute: 0,
            second: 0,
            nanos: 0,
        };
        let before = offset_for_civil("Europe/Stockholm", start).expect("offset");
        let after = offset_for_civil("Europe/Stockholm", end).expect("offset");
        assert_eq!((before, after), (7200, 3600));
    }

    #[test]
    fn canonical_names_and_unknown_zones() {
        assert_eq!(
            canonical_name("america/NEW_YORK").as_deref(),
            Some("America/New_York")
        );
        assert!(!is_known("Mars/Olympus"));
        assert!(offset_at_instant("Mars/Olympus", 0, 0).is_err());
    }
}
