//! Neo4j-style durations: months, days and seconds that never fold into
//! each other.
//!
//! A duration keeps `months`, `days`, `seconds` and `nanos` as separate
//! fields, exactly like Neo4j: `P32H` stays 32 hours instead of becoming one
//! day and eight hours. Fractional input components cascade downward
//! (years to months, months to days using the average month of 2,629,746
//! seconds, days to hours, and so on), matching Neo4j's normalization.
//!
//! Components are kept as exact scaled integers (nine decimal places), so
//! decimal inputs like `-2.001` survive the conversion to nanoseconds
//! without binary floating point error, while multiplier results still
//! truncate sub-nanosecond leftovers the way the TCK expects.

use std::fmt;
use std::ops::{Add, Neg, Sub};

const FRACTION: i128 = 1_000_000_000;

const NS_PER_MILLISECOND: i128 = 1_000_000;
const NS_PER_MICROSECOND: i128 = 1_000;
const NS_PER_SECOND: i128 = 1_000_000_000;
const NS_PER_DAY: i128 = 86_400 * NS_PER_SECOND;
const NS_PER_MONTH: i128 = 2_629_746 * NS_PER_SECOND;

/// One nanosecond expressed in the accumulator's base unit (10^-9 ns).
const UNITS_PER_NANOSECOND: i128 = 1_000_000_000;
const UNITS_PER_SECOND: i128 = NS_PER_SECOND * UNITS_PER_NANOSECOND;
const UNITS_PER_DAY: i128 = NS_PER_DAY * UNITS_PER_NANOSECOND;

/// An exact decimal component: the value is `scaled / 10^9`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Component {
    scaled: i128,
}

impl Component {
    /// The zero component.
    pub const ZERO: Self = Self { scaled: 0 };

    /// Build from a whole number.
    pub const fn from_int(value: i64) -> Self {
        Self {
            scaled: value as i128 * FRACTION,
        }
    }

    /// Build from a float, rounding to nanosecond precision.
    ///
    /// Returns `None` for NaN and infinities.
    pub fn from_f64(value: f64) -> Option<Self> {
        if !value.is_finite() {
            return None;
        }
        Some(Self {
            scaled: (value * FRACTION as f64).round() as i128,
        })
    }

    /// Parse an exact decimal such as `-2.001` or `12.5`. Digits beyond the
    /// ninth are truncated.
    pub fn from_decimal(text: &str) -> Option<Self> {
        let (negative, digits) = match text.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, text.strip_prefix('+').unwrap_or(text)),
        };
        let (whole, fraction) = match digits.split_once('.') {
            Some((whole, fraction)) => (whole, fraction),
            None => (digits, ""),
        };
        if whole.is_empty() && fraction.is_empty() {
            return None;
        }
        if !whole.bytes().all(|byte| byte.is_ascii_digit())
            || !fraction.bytes().all(|byte| byte.is_ascii_digit())
        {
            return None;
        }
        let whole: i128 = if whole.is_empty() {
            0
        } else {
            whole.parse().ok()?
        };
        let mut scaled = whole.checked_mul(FRACTION)?;
        let mut scale = FRACTION;
        for digit in fraction.bytes().take(9) {
            scale /= 10;
            scaled += (digit - b'0') as i128 * scale;
        }
        Some(Self {
            scaled: if negative { -scaled } else { scaled },
        })
    }

    /// Multiply by a whole factor.
    pub fn times(self, factor: i64) -> Self {
        Self {
            scaled: self.scaled * factor as i128,
        }
    }

    /// Multiply by a float factor, truncating sub-nanosecond leftovers.
    pub fn scaled_f64(self, factor: f64) -> Option<Self> {
        if !factor.is_finite() {
            return None;
        }
        Some(Self {
            scaled: (self.scaled as f64 * factor) as i128,
        })
    }

    /// The component as a float (lossy for very large values).
    pub fn as_f64(self) -> f64 {
        self.scaled as f64 / FRACTION as f64
    }
}

/// Components accepted by `duration({...})`; missing entries default to zero.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Components {
    /// Years (12 months each).
    pub years: Component,
    /// Months.
    pub months: Component,
    /// Weeks (7 days each).
    pub weeks: Component,
    /// Days.
    pub days: Component,
    /// Hours.
    pub hours: Component,
    /// Minutes.
    pub minutes: Component,
    /// Seconds.
    pub seconds: Component,
    /// Milliseconds.
    pub milliseconds: Component,
    /// Microseconds.
    pub microseconds: Component,
    /// Nanoseconds.
    pub nanoseconds: Component,
}

/// A Neo4j duration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Duration {
    months: i64,
    days: i64,
    seconds: i64,
    nanos: i32,
}

/// Split a total nanosecond count into whole seconds and a sub-second part
/// with the same sign, using truncation so that `-1.999s` becomes -1 second
/// and -999ms, matching Neo4j's representation.
fn normalize_nanos(seconds: i64, nanos: i64) -> Option<(i64, i32)> {
    let total = (seconds as i128)
        .checked_mul(1_000_000_000)?
        .checked_add(nanos as i128)?;
    // Neo4j keeps the nanosecond field in `[0, 1e9)` and compensates in the
    // seconds field, so `-0.1s` is -1 second plus 900000000 nanoseconds.
    let whole = total.div_euclid(1_000_000_000);
    let frac = total.rem_euclid(1_000_000_000);
    Some((i64::try_from(whole).ok()?, i32::try_from(frac).ok()?))
}

impl Duration {
    /// Build a duration from whole components. `nanos` is folded into
    /// `seconds` when it exceeds a second.
    ///
    /// Returns `None` on overflow.
    pub fn new(months: i64, days: i64, seconds: i64, nanos: i64) -> Option<Self> {
        let (seconds, nanos) = normalize_nanos(seconds, nanos)?;
        Some(Self {
            months,
            days,
            seconds,
            nanos,
        })
    }

    /// Build a duration from potentially fractional components, cascading
    /// fractions downward like Neo4j. Exact for decimal inputs.
    pub fn from_components(components: Components) -> Option<Self> {
        // Months: whole months stay; the fraction becomes days.
        let month_scaled = components
            .years
            .times(12)
            .scaled
            .checked_add(components.months.scaled)?;
        let months = month_scaled / FRACTION;
        let month_fraction = month_scaled % FRACTION;
        let mut sub_units = month_fraction * NS_PER_MONTH;
        let days_from_months = sub_units / UNITS_PER_DAY;
        sub_units %= UNITS_PER_DAY;

        // Days: whole days stay; the fraction becomes hours.
        let day_scaled = components
            .weeks
            .times(7)
            .scaled
            .checked_add(components.days.scaled)?;
        let days = day_scaled / FRACTION + days_from_months;
        sub_units += (day_scaled % FRACTION) * NS_PER_DAY;

        // Hours, minutes and seconds all fold into seconds.
        let second_scaled = components
            .hours
            .times(3_600)
            .scaled
            .checked_add(components.minutes.times(60).scaled)?
            .checked_add(components.seconds.scaled)?;
        sub_units += (second_scaled % FRACTION) * NS_PER_SECOND;
        let mut seconds = second_scaled / FRACTION + sub_units / UNITS_PER_SECOND;
        let mut nanos_units = sub_units % UNITS_PER_SECOND;

        // Sub-second components accumulate in the same base unit.
        nanos_units = nanos_units
            .checked_add(components.milliseconds.scaled * NS_PER_MILLISECOND)?
            .checked_add(components.microseconds.scaled * NS_PER_MICROSECOND)?
            .checked_add(components.nanoseconds.scaled)?;
        seconds = seconds.checked_add(nanos_units / UNITS_PER_SECOND)?;
        nanos_units %= UNITS_PER_SECOND;

        let seconds = i64::try_from(seconds).ok()?;
        let nanos = i32::try_from(nanos_units / UNITS_PER_NANOSECOND).ok()?;
        let months = i64::try_from(months).ok()?;
        let days = i64::try_from(days).ok()?;
        Self::new(months, days, seconds, nanos as i64)
    }

    /// Years component (`months / 12`, truncated).
    pub const fn years(self) -> i64 {
        self.months / 12
    }

    /// Quarters component (`months / 3`, truncated).
    pub const fn quarters(self) -> i64 {
        self.months / 3
    }

    /// Total months.
    pub const fn months(self) -> i64 {
        self.months
    }

    /// Weeks component (`days / 7`, truncated).
    pub const fn weeks(self) -> i64 {
        self.days / 7
    }

    /// Total days.
    pub const fn days(self) -> i64 {
        self.days
    }

    /// Total hours.
    pub const fn hours(self) -> i64 {
        self.seconds / 3_600
    }

    /// Total minutes.
    pub const fn minutes(self) -> i64 {
        self.seconds / 60
    }

    /// Total seconds.
    pub const fn seconds(self) -> i64 {
        self.seconds
    }

    /// Total milliseconds.
    pub const fn milliseconds(self) -> i64 {
        self.seconds * 1_000 + self.nanos as i64 / 1_000_000
    }

    /// Total microseconds.
    pub const fn microseconds(self) -> i64 {
        self.seconds * 1_000_000 + self.nanos as i64 / 1_000
    }

    /// Total nanoseconds.
    pub const fn nanoseconds(self) -> i64 {
        self.seconds * 1_000_000_000 + self.nanos as i64
    }

    /// Quarters of the year (`(months % 12) / 3`).
    pub const fn quarters_of_year(self) -> i64 {
        (self.months % 12) / 3
    }

    /// Months of the quarter (`(months % 12) % 3`).
    pub const fn months_of_quarter(self) -> i64 {
        (self.months % 12) % 3
    }

    /// Months of the year (`months % 12`).
    pub const fn months_of_year(self) -> i64 {
        self.months % 12
    }

    /// Days of the week (`days % 7`).
    pub const fn days_of_week(self) -> i64 {
        self.days % 7
    }

    /// Minutes of the hour (`(seconds % 3600) / 60`).
    pub const fn minutes_of_hour(self) -> i64 {
        (self.seconds % 3_600) / 60
    }

    /// Seconds of the minute (`seconds % 60`).
    pub const fn seconds_of_minute(self) -> i64 {
        self.seconds % 60
    }

    /// Nanoseconds field, signed like `seconds`.
    pub const fn nanos(self) -> i32 {
        self.nanos
    }

    /// Milliseconds within the second.
    pub const fn milliseconds_of_second(self) -> i64 {
        self.nanos as i64 / 1_000_000
    }

    /// Microseconds within the second.
    pub const fn microseconds_of_second(self) -> i64 {
        self.nanos as i64 / 1_000
    }

    /// Nanoseconds within the second.
    pub const fn nanoseconds_of_second(self) -> i64 {
        self.nanos as i64
    }

    /// Total nanoseconds as a wide integer (overflow-safe).
    pub const fn total_nanos(self) -> i128 {
        self.seconds as i128 * 1_000_000_000 + self.nanos as i128
    }

    /// The duration's length in nanoseconds using Neo4j's ordering averages
    /// (a year is 365.2425 days, a month 2,629,746 seconds). Used to order
    /// durations, which have no exact total length otherwise.
    pub const fn normalized_nanos(self) -> i128 {
        self.months as i128 * NS_PER_MONTH + self.days as i128 * NS_PER_DAY + self.total_nanos()
    }

    /// Scale every field by a factor, cascading fractional results like
    /// Neo4j's `duration * number`.
    pub fn multiplied(self, factor: f64) -> Option<Self> {
        let components = Components {
            months: Component::from_int(self.months).scaled_f64(factor)?,
            days: Component::from_int(self.days).scaled_f64(factor)?,
            seconds: Component::from_int(self.seconds).scaled_f64(factor)?,
            nanoseconds: Component::from_int(self.nanos as i64).scaled_f64(factor)?,
            ..Components::default()
        };
        Self::from_components(components)
    }

    /// Divide by a number, equivalent to multiplying by its reciprocal.
    pub fn divided(self, divisor: f64) -> Option<Self> {
        if divisor == 0.0 {
            return None;
        }
        self.multiplied(1.0 / divisor)
    }
}

impl PartialOrd for Duration {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Duration {
    /// Durations order by their normalized length, then component-wise, so
    /// the order is total and matches [`Duration::normalized_nanos`].
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.normalized_nanos()
            .cmp(&other.normalized_nanos())
            .then_with(|| {
                (self.months, self.days, self.seconds, self.nanos).cmp(&(
                    other.months,
                    other.days,
                    other.seconds,
                    other.nanos,
                ))
            })
    }
}

impl Add for Duration {
    type Output = Option<Duration>;

    fn add(self, other: Self) -> Self::Output {
        let months = self.months.checked_add(other.months)?;
        let days = self.days.checked_add(other.days)?;
        let nanos = self.nanos as i64 + other.nanos as i64;
        Duration::new(
            months,
            days,
            self.seconds.checked_add(other.seconds)?,
            nanos,
        )
    }
}

impl Sub for Duration {
    type Output = Option<Duration>;

    fn sub(self, other: Self) -> Self::Output {
        let months = self.months.checked_sub(other.months)?;
        let days = self.days.checked_sub(other.days)?;
        let nanos = self.nanos as i64 - other.nanos as i64;
        Duration::new(
            months,
            days,
            self.seconds.checked_sub(other.seconds)?,
            nanos,
        )
    }
}

impl Neg for Duration {
    type Output = Duration;

    fn neg(self) -> Self::Output {
        let months = self.months.checked_neg().unwrap_or(self.months);
        let days = self.days.checked_neg().unwrap_or(self.days);
        let seconds = self.seconds.checked_neg().unwrap_or(self.seconds);
        Self::new(months, days, seconds, -(self.nanos as i64)).unwrap_or(self)
    }
}

/// Write the time part of an ISO duration from the total seconds and
/// nanoseconds, signing each component for negative lengths
/// (`-23H-59M-59.9S`).
fn write_time_part(out: &mut String, seconds: i64, nanos: i32) {
    let total = seconds as i128 * 1_000_000_000 + nanos as i128;
    if total == 0 {
        return;
    }
    let negative = total < 0;
    let absolute = total.unsigned_abs();
    let sign = if negative { "-" } else { "" };
    let hours = absolute / 3_600_000_000_000;
    let minutes = absolute / 60_000_000_000 % 60;
    let seconds = absolute / 1_000_000_000 % 60;
    let nanos = (absolute % 1_000_000_000) as i64;
    if hours != 0 {
        out.push_str(&format!("{sign}{hours}H"));
    }
    if minutes != 0 {
        out.push_str(&format!("{sign}{minutes}M"));
    }
    if seconds != 0 || nanos != 0 {
        out.push_str(&format!("{sign}{seconds}"));
        if nanos != 0 {
            let mut fraction = format!("{nanos:09}");
            while fraction.ends_with('0') {
                fraction.pop();
            }
            out.push('.');
            out.push_str(&fraction);
        }
        out.push('S');
    }
}

impl fmt::Display for Duration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut out = String::from("P");
        let years = self.months / 12;
        let months = self.months % 12;
        if years != 0 {
            out.push_str(&format!("{years}Y"));
        }
        if months != 0 {
            out.push_str(&format!("{months}M"));
        }
        if self.days != 0 {
            out.push_str(&format!("{}D", self.days));
        }
        let mut time = String::new();
        write_time_part(&mut time, self.seconds, self.nanos);
        if time.is_empty() {
            if out.len() == 1 {
                // A zero duration prints as `PT0S`.
                out.push_str("T0S");
            }
        } else {
            out.push('T');
            out.push_str(&time);
        }
        f.write_str(&out)
    }
}

impl std::str::FromStr for Duration {
    type Err = String;

    /// Parse an ISO 8601 duration such as `P12Y5M14DT16H13M10.000000001S`.
    /// Signs are accepted per component and seconds may carry a fraction.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let invalid = || format!("invalid duration `{text}`");
        let rest = text.strip_prefix('P').ok_or_else(invalid)?;
        if rest.is_empty() {
            return Err(invalid());
        }
        // The ISO 8601 alternative form `P2012-02-02T14:37:21.545` places
        // the components in date and time positions instead of suffixes.
        if rest.len() >= 10
            && rest.as_bytes().get(4) == Some(&b'-')
            && rest.as_bytes().get(7) == Some(&b'-')
        {
            return parse_iso_alternative(rest).ok_or_else(invalid);
        }
        let mut components = Components::default();
        let mut current = String::new();
        let mut in_time = false;
        let mut saw_component = false;
        for ch in rest.chars() {
            match ch {
                'T' => {
                    if in_time || !current.is_empty() {
                        return Err(invalid());
                    }
                    in_time = true;
                }
                '0'..='9' | '.' | '-' | '+' => current.push(ch),
                _ => {
                    let value = Component::from_decimal(&current).ok_or_else(invalid)?;
                    current.clear();
                    saw_component = true;
                    match (in_time, ch) {
                        (false, 'Y') => components.years = value,
                        (false, 'M') => components.months = value,
                        (false, 'W') => components.weeks = value,
                        (false, 'D') => components.days = value,
                        (true, 'H') => components.hours = value,
                        (true, 'M') => components.minutes = value,
                        (true, 'S') => components.seconds = value,
                        _ => return Err(invalid()),
                    }
                }
            }
        }
        if !current.is_empty() || !saw_component {
            return Err(invalid());
        }
        Duration::from_components(components).ok_or_else(invalid)
    }
}

/// Parse the alternative ISO form `YYYY-MM-DD[THH:MM:SS[.fff]]`.
fn parse_iso_alternative(rest: &str) -> Option<Duration> {
    let (date, time) = match rest.split_once('T') {
        Some((date, time)) => (date, Some(time)),
        None => (rest, None),
    };
    let mut parts = date.split('-');
    let years: i64 = parts.next()?.parse().ok()?;
    let months: i64 = parts.next()?.parse().ok()?;
    let days: i64 = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    let (hours, minutes, seconds) = match time {
        None => (0, 0, Component::ZERO),
        Some(time) => {
            let mut parts = time.split(':');
            let hours: i64 = parts.next()?.parse().ok()?;
            let minutes: i64 = match parts.next() {
                Some(minutes) => minutes.parse().ok()?,
                None => 0,
            };
            let seconds = match parts.next() {
                Some(seconds) => Component::from_decimal(seconds)?,
                None => Component::ZERO,
            };
            if parts.next().is_some() {
                return None;
            }
            (hours, minutes, seconds)
        }
    };
    Duration::from_components(Components {
        years: Component::from_int(years),
        months: Component::from_int(months),
        days: Component::from_int(days),
        hours: Component::from_int(hours),
        minutes: Component::from_int(minutes),
        seconds,
        ..Components::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn component(value: f64) -> Component {
        Component::from_f64(value).expect("component")
    }

    fn duration(components: Components) -> Duration {
        Duration::from_components(components).expect("duration")
    }

    #[test]
    fn whole_components_stay_separate() {
        let value = duration(Components {
            years: component(12.0),
            months: component(5.0),
            days: component(14.0),
            hours: component(16.0),
            minutes: component(12.0),
            seconds: component(70.0),
            nanoseconds: component(1.0),
            ..Components::default()
        });
        assert_eq!(value.months(), 149);
        assert_eq!(value.days(), 14);
        assert_eq!(value.seconds(), 16 * 3600 + 12 * 60 + 70);
        assert_eq!(value.nanos(), 1);
        assert_eq!(value.to_string(), "P12Y5M14DT16H13M10.000000001S");
    }

    #[test]
    fn time_seconds_normalize_with_shared_sign() {
        let cases = [
            (
                Components {
                    minutes: component(12.0),
                    seconds: component(-60.0),
                    ..Components::default()
                },
                "PT11M",
            ),
            (
                Components {
                    seconds: component(2.0),
                    milliseconds: component(-1.0),
                    ..Components::default()
                },
                "PT1.999S",
            ),
            (
                Components {
                    seconds: component(-2.0),
                    milliseconds: component(1.0),
                    ..Components::default()
                },
                "PT-1.999S",
            ),
            (
                Components {
                    seconds: component(-2.0),
                    milliseconds: component(-1.0),
                    ..Components::default()
                },
                "PT-2.001S",
            ),
            (
                Components {
                    days: component(1.0),
                    milliseconds: component(1.0),
                    ..Components::default()
                },
                "P1DT0.001S",
            ),
            (
                Components {
                    days: component(1.0),
                    milliseconds: component(-1.0),
                    ..Components::default()
                },
                "P1DT-0.001S",
            ),
            (
                Components {
                    seconds: component(60.0),
                    milliseconds: component(-1.0),
                    ..Components::default()
                },
                "PT59.999S",
            ),
            (
                Components {
                    seconds: component(-60.0),
                    milliseconds: component(1.0),
                    ..Components::default()
                },
                "PT-59.999S",
            ),
            (
                Components {
                    seconds: component(-60.0),
                    milliseconds: component(-1.0),
                    ..Components::default()
                },
                "PT-1M-0.001S",
            ),
        ];
        for (components, expected) in cases {
            assert_eq!(duration(components).to_string(), expected);
        }
        assert_eq!(duration(Components::default()).to_string(), "PT0S");
    }

    #[test]
    fn fractional_components_cascade() {
        // Temporal8: the diff between the 12.5-year sample and the
        // 12-year sample.
        let value = duration(Components {
            years: component(0.5),
            months: component(0.5),
            days: component(0.5),
            hours: component(0.5),
            minutes: component(0.5),
            seconds: component(0.5),
            nanoseconds: component(2.0),
            ..Components::default()
        });
        assert_eq!(value.months(), 6);
        assert_eq!(value.days(), 15);
        assert_eq!(value.seconds(), 17 * 3600 + 45 * 60 + 3);
        assert_eq!(value.nanos(), 500_000_002);
        assert_eq!(value.to_string(), "P6M15DT17H45M3.500000002S");
    }

    #[test]
    fn sample_duration_normalizes_like_neo4j() {
        let value = duration(Components {
            years: component(12.5),
            months: component(5.5),
            days: component(14.5),
            hours: component(16.5),
            minutes: component(12.5),
            seconds: component(70.5),
            nanoseconds: component(3.0),
            ..Components::default()
        });
        assert_eq!(value.months(), 155);
        assert_eq!(value.days(), 29);
        assert_eq!(value.seconds(), 33 * 3600 + 58 * 60 + 13);
        assert_eq!(value.nanos(), 500_000_003);
    }

    #[test]
    fn addition_and_scaling() {
        let left = duration(Components {
            years: component(12.0),
            months: component(5.0),
            days: component(14.0),
            hours: component(16.0),
            minutes: component(12.0),
            seconds: component(70.0),
            nanoseconds: component(1.0),
            ..Components::default()
        });
        let doubled = left.multiplied(2.0).expect("scale");
        assert_eq!(doubled.to_string(), "P24Y10M28DT32H26M20.000000002S");
        let halved = left.multiplied(0.5).expect("scale");
        assert_eq!(halved.to_string(), "P6Y2M22DT13H21M8S");
        let sum = (left + left).expect("add");
        assert_eq!(sum, doubled);
        let diff = (doubled - left).expect("sub");
        assert_eq!(diff, left);
        assert_eq!((-left).months(), -left.months());
    }

    #[test]
    fn accessors_match_the_tck() {
        let value = duration(Components {
            years: component(1.0),
            months: component(4.0),
            days: component(10.0),
            hours: component(1.0),
            minutes: component(1.0),
            seconds: component(1.0),
            nanoseconds: component(111_111_111.0),
            ..Components::default()
        });
        assert_eq!(value.years(), 1);
        assert_eq!(value.quarters(), 5);
        assert_eq!(value.months(), 16);
        assert_eq!(value.weeks(), 1);
        assert_eq!(value.days(), 10);
        assert_eq!(value.hours(), 1);
        assert_eq!(value.minutes(), 61);
        assert_eq!(value.seconds(), 3661);
        assert_eq!(value.milliseconds(), 3_661_111);
        assert_eq!(value.microseconds(), 3_661_111_111);
        assert_eq!(value.nanoseconds(), 3_661_111_111_111);
        assert_eq!(value.quarters_of_year(), 1);
        assert_eq!(value.months_of_quarter(), 1);
        assert_eq!(value.months_of_year(), 4);
        assert_eq!(value.days_of_week(), 3);
        assert_eq!(value.minutes_of_hour(), 1);
        assert_eq!(value.seconds_of_minute(), 1);
        assert_eq!(value.milliseconds_of_second(), 111);
        assert_eq!(value.microseconds_of_second(), 111_111);
        assert_eq!(value.nanoseconds_of_second(), 111_111_111);
    }

    #[test]
    fn parses_its_own_output() {
        for text in [
            "P12Y5M14DT16H13M10.000000001S",
            "P12Y5M-14DT16H",
            "PT11M",
            "PT-2.001S",
            "P1DT-0.001S",
            "PT0S",
            "P-1Y-2M-3DT-4H-5M-6.5S",
            "P12Y5M14DT16H13M10S",
        ] {
            let parsed: Duration = text.parse().expect(text);
            assert_eq!(parsed.to_string(), text, "{text}");
        }
        assert!("P".parse::<Duration>().is_err());
        assert!("P1X".parse::<Duration>().is_err());
    }

    #[test]
    fn decimal_components_are_exact() {
        // The canonical form floors the seconds and keeps the nanoseconds
        // in `[0, 1e9)`: -2.001s is -3 seconds plus 999000000 nanoseconds.
        let parsed: Duration = "PT-2.001S".parse().expect("parse");
        assert_eq!(parsed.seconds(), -3);
        assert_eq!(parsed.nanos(), 999_000_000);
        assert_eq!(parsed.to_string(), "PT-2.001S");
        let component = Component::from_decimal("-2.001").expect("component");
        assert_eq!(component.as_f64(), -2.001);
    }
}
