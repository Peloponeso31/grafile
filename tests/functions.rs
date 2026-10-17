//! Scalar, conversion, list/string and math functions.

mod support;

use grafile::api::Params;
use support::{connection, query, seed};

#[test]
fn scalar_functions() {
    let mut conn = connection();
    seed(&mut conn);
    let rows = query(
        &mut conn,
        "MATCH (n:Person {name: 'alice'}) RETURN id(n) > 0, labels(n), n.age + 1, toString(n.age), size(n.name)",
    );
    assert_eq!(rows, vec![vec!["true", "[Person]", "31", "30", "5"]]);
    let rows = query(
        &mut conn,
        "MATCH (a)-[r:KNOWS]->(b) RETURN startNode(r).name, endNode(r).name ORDER BY startNode(r).name",
    );
    assert_eq!(rows, vec![vec!["alice", "bob"], vec!["bob", "carol"]]);
}

#[test]
fn case_and_null_handling() {
    let mut conn = connection();
    seed(&mut conn);
    let rows = query(
        &mut conn,
        "MATCH (n:Person) RETURN n.name, CASE WHEN n.age >= 30 THEN 'senior' ELSE 'junior' END AS band ORDER BY n.name",
    );
    assert_eq!(
        rows,
        vec![
            vec!["alice", "senior"],
            vec!["bob", "junior"],
            vec!["carol", "senior"]
        ]
    );
    let rows = query(
        &mut conn,
        "MATCH (n:Person {name: 'alice'}) RETURN n.missing IS NULL, n.missing",
    );
    assert_eq!(rows, vec![vec!["true", "null"]]);
}

#[test]
fn to_integer_truncates_and_parses() {
    let mut conn = connection();
    let rows = query(
        &mut conn,
        "WITH [82.9, 2, true, '2.9', 'foo', '', null] AS xs \
         RETURN [x IN xs | toInteger(x)] AS ints",
    );
    assert_eq!(rows, vec![vec!["[82, 2, 1, 2, null, null, null]"]]);
}

#[test]
fn to_float_converts_numbers_and_strings() {
    let mut conn = connection();
    let rows = query(
        &mut conn,
        "WITH [3.4, 3, '5', 'foo', null] AS xs RETURN [x IN xs | toFloat(x)] AS floats",
    );
    assert_eq!(rows, vec![vec!["[3.4, 3.0, 5.0, null, null]"]]);
}

#[test]
fn to_boolean_accepts_boolean_and_string() {
    let mut conn = connection();
    let rows = query(
        &mut conn,
        "WITH [true, 'TRUE', 'False', ' tru ', 'x', null] AS xs \
         RETURN [x IN xs | toBoolean(x)] AS bools",
    );
    assert_eq!(rows, vec![vec!["[true, true, false, null, null, null]"]]);
}

#[test]
fn to_string_renders_scalars() {
    let mut conn = connection();
    let rows = query(
        &mut conn,
        "WITH [42, 2.3, true, 'apa', null] AS xs RETURN [x IN xs | toString(x)] AS texts",
    );
    assert_eq!(rows, vec![vec!["[42, 2.3, true, apa, null]"]]);
}

#[test]
fn plus_coerces_the_non_string_operand() {
    let mut conn = connection();
    let rows = query(
        &mut conn,
        "RETURN 'p-' + 1 AS a, 1 + 'p' AS b, 'v' + true AS c, 'x' + 2.5 AS d, \
                'a' + null AS e, 'a' + [1, 2] AS f, 'd:' + date('2020-01-01') AS g",
    );
    assert_eq!(
        rows,
        vec![vec![
            "p-1",
            "1p",
            "vtrue",
            "x2.5",
            "null",
            "[a, 1, 2]",
            "d:2020-01-01"
        ]]
    );
    let err = conn
        .query("RETURN {a: 1} + 'x'", &Params::new())
        .expect_err("map plus string");
    assert_eq!(err.code(), grafile::ErrorCode::Type);
}

#[test]
fn range_builds_inclusive_sequences() {
    let mut conn = connection();
    let rows = query(
        &mut conn,
        "RETURN range(0, 5) AS a, range(5, 0, -2) AS b, range(0, -3) AS c, range(0, 0, -7) AS d",
    );
    assert_eq!(
        rows,
        vec![vec!["[0, 1, 2, 3, 4, 5]", "[5, 3, 1]", "[]", "[0]"]]
    );
}

#[test]
fn range_rejects_zero_step_and_fractional_bounds() {
    let mut conn = connection();
    let err = conn
        .query("RETURN range(2, 8, 0)", &Params::new())
        .expect_err("zero step");
    assert_eq!(err.code(), grafile::ErrorCode::Range);
    let err = conn
        .query("RETURN range(1.1, 8, 1)", &Params::new())
        .expect_err("float bound");
    assert_eq!(err.code(), grafile::ErrorCode::Misuse);
}

#[test]
fn head_last_and_tail() {
    let mut conn = connection();
    let rows = query(
        &mut conn,
        "RETURN head([1, 2, 3]) AS h, last([1, 2, 3]) AS l, tail([1, 2, 3]) AS t, \
         head([]) AS eh, tail([]) AS et, head(null) AS nh",
    );
    assert_eq!(rows, vec![vec!["1", "3", "[2, 3]", "null", "[]", "null"]]);
}

#[test]
fn reverse_strings_and_lists() {
    let mut conn = connection();
    let rows = query(
        &mut conn,
        "RETURN reverse('raksO') AS s, reverse([1, 2, 3]) AS l, reverse(null) AS n",
    );
    assert_eq!(rows, vec![vec!["Oskar", "[3, 2, 1]", "null"]]);
}

#[test]
fn split_and_substring() {
    let mut conn = connection();
    let rows = query(
        &mut conn,
        "RETURN split('one1two', '1') AS a, split('abc', '') AS b, split('', ',') AS c, \
         substring('0123456789', 1) AS d, substring('hello', 1, 3) AS e, substring('hi', 9) AS f",
    );
    assert_eq!(
        rows,
        vec![vec![
            "[one, two]",
            "[a, b, c]",
            "[]",
            "123456789",
            "ell",
            ""
        ]]
    );
}

#[test]
fn split_empty_input_keeps_one_empty_part() {
    let mut conn = connection();
    let rows = query(
        &mut conn,
        "RETURN size(split('', ',')) AS a, substring('hello', null) AS b",
    );
    assert_eq!(rows, vec![vec!["1", "null"]]);
}

#[test]
fn string_case_and_trimming_helpers() {
    let mut conn = connection();
    let rows = query(
        &mut conn,
        "RETURN toUpper('aBc') AS u, toLower('AbC') AS l, trim('  x  ') AS t, \
         ltrim('  x  ') AS lt, rtrim('  x  ') AS rt, left('hello', 2) AS le, \
         right('hello', 2) AS ri, left('hi', 9) AS lc, replace('hello', 'l', 'L') AS rp",
    );
    assert_eq!(
        rows,
        vec![vec![
            "ABC", "abc", "x", "x  ", "  x", "he", "lo", "hi", "heLLo"
        ]]
    );
}

#[test]
fn math_rounding_and_signs() {
    let mut conn = connection();
    let rows = query(
        &mut conn,
        "RETURN abs(-1) AS a, abs(-2.5) AS b, sign(-3) AS c, sign(0.0) AS d, \
         ceil(0.1) AS e, floor(-0.1) AS f, round(2.5) AS g, round(-2.5) AS h",
    );
    assert_eq!(
        rows,
        vec![vec!["1", "2.5", "-1", "0", "1", "-1", "3", "-3"]]
    );
}

#[test]
fn math_logarithms_and_roots() {
    let mut conn = connection();
    let rows = query(
        &mut conn,
        "RETURN sqrt(12.96) AS a, exp(0.0) AS b, log(1.0) AS c, log10(100.0) AS d, \
         log(null) AS e",
    );
    assert_eq!(rows, vec![vec!["3.6", "1", "0", "2", "null"]]);
}

#[test]
fn math_trigonometry_and_constants() {
    let mut conn = connection();
    let rows = query(
        &mut conn,
        "RETURN degrees(pi()) AS a, radians(180.0) AS b, sin(0.0) AS c, cos(0.0) AS d, \
         atan2(1.0, 1.0) AS e, haversin(pi()) AS f, e() > 2.7 AS g",
    );
    assert_eq!(
        rows,
        vec![vec![
            "180",
            "3.141592653589793",
            "0",
            "1",
            "0.7853981633974483",
            "1",
            "true"
        ]]
    );
}

#[test]
fn math_functions_reject_non_numbers() {
    let mut conn = connection();
    let err = conn
        .query("RETURN abs('x')", &Params::new())
        .expect_err("string argument");
    assert_eq!(err.code(), grafile::ErrorCode::Type);
}

#[test]
fn conversions_reject_incompatible_types() {
    let mut conn = connection();
    for source in [
        "RETURN toInteger([1])",
        "RETURN toFloat(true)",
        "RETURN toBoolean(1.0)",
        "RETURN toString([1, 2])",
        "RETURN toString({a: 1})",
    ] {
        let err = conn
            .query(source, &Params::new())
            .expect_err("conversion must reject the type");
        assert_eq!(err.code(), grafile::ErrorCode::Type, "{source}");
    }
}
