//! Temporal and spatial value types: constructors, accessors, arithmetic,
//! truncation, ordering and persistence.

mod support;

use grafile::api::Params;
use support::{connection, query};

#[test]
fn temporal_constructors_from_maps_and_strings() {
    let mut conn = connection();
    let rows = query(
        &mut conn,
        "RETURN date({year: 1984, month: 10, day: 11}) AS a, \
         date('2015-W30-2') AS b, \
         date({year: 1817, week: 1, dayOfWeek: 2}) AS c, \
         localtime({hour: 12, minute: 31, second: 14, nanosecond: 645876123}) AS d, \
         time({hour: 21, minute: 40, timezone: '+01:00'}) AS e",
    );
    assert_eq!(
        rows,
        vec![vec![
            "1984-10-11",
            "2015-07-21",
            "1816-12-31",
            "12:31:14.645876123",
            "21:40+01:00"
        ]]
    );
}

#[test]
fn temporal_composition_and_overrides() {
    let mut conn = connection();
    let rows = query(
        &mut conn,
        "WITH date({year: 1984, month: 10, day: 11}) AS other \
         RETURN localdatetime({date: other, hour: 10, minute: 10, second: 10}) AS a, \
         localdatetime({date: other, day: 28, hour: 10, minute: 10, second: 10}) AS b",
    );
    assert_eq!(
        rows,
        vec![vec!["1984-10-11T10:10:10", "1984-10-28T10:10:10"]]
    );
}

#[test]
fn temporal_accessors() {
    let mut conn = connection();
    let rows = query(
        &mut conn,
        "WITH datetime({year: 1984, month: 11, day: 11, hour: 12, minute: 31, second: 14, \
          nanosecond: 645876123, timezone: 'Europe/Stockholm'}) AS d \
         RETURN d.year, d.quarter, d.week, d.weekDay, d.dayOfQuarter, d.hour, d.nanosecond, \
                d.timezone, d.offset, d.offsetSeconds, d.epochSeconds, d.epochMillis",
    );
    assert_eq!(
        rows,
        vec![vec![
            "1984",
            "4",
            "45",
            "7",
            "42",
            "12",
            "645876123",
            "Europe/Stockholm",
            "+01:00",
            "3600",
            "469020674",
            "469020674645"
        ]]
    );
}

#[test]
fn temporal_serialization_round_trips() {
    let mut conn = connection();
    let rows = query(
        &mut conn,
        "WITH date({year: 1984, month: 10, day: 11}) AS d, \
              localtime({hour: 12, minute: 31, second: 14, nanosecond: 645876123}) AS t, \
              duration({years: 12, months: 5, days: 14, hours: 16, minutes: 12, seconds: 70, nanoseconds: 1}) AS du \
         RETURN date(toString(d)) = d AS a, localtime(toString(t)) = t AS b, duration(toString(du)) = du AS c, \
                toString(du) AS text",
    );
    assert_eq!(
        rows,
        vec![vec![
            "true",
            "true",
            "true",
            "P12Y5M14DT16H13M10.000000001S"
        ]]
    );
}

#[test]
fn temporal_truncation() {
    let mut conn = connection();
    let rows = query(
        &mut conn,
        "RETURN date.truncate('millennium', date({year: 2017, month: 10, day: 11}), {day: 2}) AS a, \
         date.truncate('week', date({year: 1984, month: 10, day: 11}), {dayOfWeek: 2}) AS b, \
         date.truncate('weekYear', date({year: 1984, month: 2, day: 1}), {}) AS c, \
         localtime.truncate('hour', localtime({hour: 12, minute: 31, second: 14, nanosecond: 645876123}), {nanosecond: 2}) AS d, \
         datetime.truncate('week', date({year: 1984, month: 10, day: 11}), {timezone: 'Europe/Stockholm'}) AS e",
    );
    assert_eq!(
        rows,
        vec![vec![
            "2000-01-02",
            "1984-10-09",
            "1984-01-02",
            "12:00:00.000000002",
            "1984-10-08T00:00+01:00[Europe/Stockholm]"
        ]]
    );
}

#[test]
fn temporal_values_store_and_compare() {
    let mut conn = connection();
    conn.execute(
        "CREATE (:Val {date: date({year: 1984, month: 10, day: 11}), \
          dates: [date({year: 1984, month: 10, day: 12})]})",
    )
    .expect("store temporals");
    let rows = query(
        &mut conn,
        "MATCH (v:Val) \
         RETURN v.date, v.dates, v.date < date('2000-01-01'), v.date = date('1984-10-11')",
    );
    assert_eq!(
        rows,
        vec![vec!["1984-10-11", "[1984-10-12]", "true", "true"]]
    );
}

#[test]
fn spatial_points_and_distance() {
    let mut conn = connection();
    let rows = query(
        &mut conn,
        "RETURN point({x: 1.5, y: 2.5}) AS p, \
         point({latitude: 12.0, longitude: 14.0}) AS g, \
         distance(point({x: 0, y: 0}), point({x: 3, y: 4})) AS d, \
         point({x: 1.5, y: 2.5, z: 3.0}) AS p3",
    );
    assert_eq!(
        rows,
        vec![vec![
            "point({x: 1.5, y: 2.5})",
            "point({srid: 4326, x: 14.0, y: 12.0})",
            "5",
            "point({srid: 9157, x: 1.5, y: 2.5, z: 3.0})"
        ]]
    );
    let rows = query(
        &mut conn,
        "WITH point({latitude: 12.0, longitude: 12.0}) AS p \
         RETURN p.latitude, p.longitude, p.srid, p.crs, \
                distance(p, point({latitude: 13.0, longitude: 12.0})) > 110000.0",
    );
    assert_eq!(rows, vec![vec!["12", "12", "4326", "wgs-84", "true"]]);
}

#[test]
fn temporal_duration_arithmetic_forms() {
    let mut conn = connection();
    let rows = query(
        &mut conn,
        "WITH duration({months: 1, days: -14, hours: 16, minutes: -12, seconds: 70}) AS dur \
         RETURN dur.years, dur.months, dur.days, dur.hours, dur.seconds, dur.minutes, dur.secondsOfMinute",
    );
    assert_eq!(
        rows,
        vec![vec!["0", "1", "-14", "15", "56950", "949", "10"]]
    );
}

#[test]
fn cross_temporal_ordering_is_an_error() {
    let mut conn = connection();
    let err = conn
        .query(
            "RETURN date('2020-01-01') < datetime('2020-01-01T00:00:00Z')",
            &Params::new(),
        )
        .expect_err("cross temporal");
    assert_eq!(err.code(), grafile::ErrorCode::Type);
    assert!(
        err.message().contains("Cannot compare date with datetime"),
        "{err}"
    );
    // Equality between different temporal types is false, not an error.
    let rows = query(
        &mut conn,
        "RETURN date('2020-01-01') = datetime('2020-01-01T00:00:00Z') AS eq, \
                localtime('12:00:00') = time('12:00:00Z') AS eq2",
    );
    assert_eq!(rows, vec![vec!["false", "false"]]);
    // ORDER BY ranks temporal types without comparing across them.
    let rows = query(
        &mut conn,
        "UNWIND [datetime('2020-01-02T00:00:00Z'), date('2020-01-01')] AS t \
         RETURN toString(t) ORDER BY t",
    );
    assert_eq!(rows.len(), 2);
}
