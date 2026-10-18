//! Projection, UNION, aggregation, subscripts/slices and comprehensions.

mod support;

use grafile::api::Params;
use grafile::record::Value;
use support::{connection, query, rows_to_strings, seed};

#[test]
fn aggregation_and_grouping() {
    let mut conn = connection();
    seed(&mut conn);
    let rows = query(
        &mut conn,
        "MATCH (a:Person)-[:KNOWS]->(b) RETURN a.name, count(b) AS friends ORDER BY a.name",
    );
    assert_eq!(rows, vec![vec!["alice", "1"], vec!["bob", "1"]]);
    let rows = query(
        &mut conn,
        "MATCH (n:Person) RETURN count(*) AS n, sum(n.age) AS total, avg(n.age) AS mean",
    );
    assert_eq!(rows, vec![vec!["3", "90", "30"]]);
    let rows = query(
        &mut conn,
        "MATCH (n:Person) RETURN collect(n.name) AS names",
    );
    assert_eq!(rows[0][0], "[alice, bob, carol]");
}

#[test]
fn with_pipeline_and_distinct() {
    let mut conn = connection();
    seed(&mut conn);
    let rows = query(
        &mut conn,
        "MATCH (a:Person)-[:KNOWS]->(b) WITH a.name AS name, b.age AS age WHERE age > 20 RETURN DISTINCT name ORDER BY name",
    );
    assert_eq!(rows, vec![vec!["alice"], vec!["bob"]]);
}

#[test]
fn unwind_and_parameters() {
    let mut conn = connection();
    let params = Params::new().with(
        "names",
        Value::List(vec![Value::from("x"), Value::from("y")].into()),
    );
    let mut rows = conn
        .query("UNWIND $names AS name RETURN name", &params)
        .expect("unwind");
    assert_eq!(rows_to_strings(&mut rows), vec![vec!["x"], vec!["y"]]);
}

#[test]
fn default_column_names_render_the_expression() {
    let mut conn = connection();
    seed(&mut conn);
    let mut rows = conn
        .query("MATCH (n:Person) RETURN n.name, count(*)", &Params::new())
        .expect("query");
    let columns: Vec<&str> = rows.columns().iter().map(String::as_str).collect();
    assert_eq!(columns, vec!["n.name", "count(*)"]);
    assert_eq!(rows_to_strings(&mut rows).len(), 3);
}

#[test]
fn union_deduplicates_rows() {
    let mut conn = connection();
    let rows = query(
        &mut conn,
        "RETURN 1 AS x UNION RETURN 2 AS x UNION RETURN 1 AS x",
    );
    assert_eq!(rows, vec![vec!["1"], vec!["2"]]);
}

#[test]
fn union_all_keeps_duplicates() {
    let mut conn = connection();
    let rows = query(
        &mut conn,
        "UNWIND [2, 1, 2] AS x RETURN x UNION ALL UNWIND [2, 3] AS x RETURN x",
    );
    assert_eq!(rows.len(), 5);
}

#[test]
fn union_over_matches() {
    let mut conn = connection();
    conn.execute("CREATE (:A), (:B)").expect("create");
    let rows = query(
        &mut conn,
        "MATCH (a:A) RETURN a AS a UNION MATCH (b:B) RETURN b AS a",
    );
    assert_eq!(rows.len(), 2);
}

#[test]
fn union_uses_first_part_column_names() {
    let mut conn = connection();
    let mut rows = conn
        .query("RETURN 1 AS x UNION RETURN 2 AS x", &Params::new())
        .expect("query");
    assert_eq!(rows.columns(), &["x".to_owned()]);
    assert_eq!(rows_to_strings(&mut rows).len(), 2);
}

#[test]
fn default_column_names_keep_necessary_parentheses() {
    let mut conn = connection();
    let mut rows = conn
        .query(
            "RETURN 12 / 4 * (3 - 2 * 4), 1 + 2 + 3, 2 ^ 3 ^ 2, -(3 * 4)",
            &Params::new(),
        )
        .expect("query");
    assert_eq!(
        rows.columns(),
        &[
            "12 / 4 * (3 - 2 * 4)".to_owned(),
            "1 + 2 + 3".to_owned(),
            "2 ^ 3 ^ 2".to_owned(),
            "-(3 * 4)".to_owned(),
        ]
    );
    let values = rows_to_strings(&mut rows);
    assert_eq!(values, vec![vec!["-15", "6", "64", "-12"]]);
}

#[test]
fn distinct_inside_aggregations() {
    let mut conn = connection();
    let rows = query(
        &mut conn,
        "UNWIND [1, 1, 2, null] AS x RETURN count(DISTINCT x) AS c, collect(DISTINCT x) AS l",
    );
    assert_eq!(rows, vec![vec!["2", "[1, 2]"]]);
}

#[test]
fn post_aggregation_expressions() {
    let mut conn = connection();
    conn.execute("CREATE (:A), (:X), (:X)").expect("create");
    let rows = query(
        &mut conn,
        "MATCH (a:A), (b:X) RETURN count(a) * 10 + count(b) * 5 AS x",
    );
    assert_eq!(rows, vec![vec!["30"]]);
}

#[test]
fn post_aggregation_with_grouping_keys() {
    let mut conn = connection();
    conn.execute("CREATE ({id: 42})").expect("create");
    let rows = query(&mut conn, "MATCH (a) RETURN a, count(a) + 3");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][1], "4");
}

#[test]
fn aggregation_inside_map_projection() {
    let mut conn = connection();
    conn.execute("CREATE (:A)-[:T]->(:B), (:A)-[:T]->(:B2)")
        .expect("create");
    let rows = query(
        &mut conn,
        "MATCH (a:A)-[:T]->(b) RETURN {name: count(b)} AS m",
    );
    assert_eq!(rows, vec![vec!["{name: 2}"]]);
}

#[test]
fn map_field_access_and_subscript() {
    let mut conn = connection();
    let rows = query(
        &mut conn,
        "WITH {name: 'Alice', age: 38} AS m RETURN m.name AS a, m['age'] AS b, m.missing AS c",
    );
    assert_eq!(rows, vec![vec!["Alice", "38", "null"]]);
}

#[test]
fn map_keys_include_null_values() {
    let mut conn = connection();
    let rows = query(
        &mut conn,
        "RETURN size(keys({exists: 42, notMissing: null})) AS n",
    );
    assert_eq!(rows, vec![vec!["2"]]);
}

#[test]
fn list_slices() {
    let mut conn = connection();
    let rows = query(
        &mut conn,
        "WITH [1, 2, 3, 4, 5] AS list RETURN list[1..3] AS a, list[1..] AS b, list[..2] AS c, list[-3..-1] AS d, list[3..1] AS e, list[-5..5] AS f",
    );
    assert_eq!(
        rows,
        vec![vec![
            "[2, 3]",
            "[2, 3, 4, 5]",
            "[1, 2]",
            "[3, 4]",
            "[]",
            "[1, 2, 3, 4, 5]"
        ]]
    );
}

#[test]
fn list_slice_with_null_bound_is_null() {
    let mut conn = connection();
    let rows = query(
        &mut conn,
        "WITH [1, 2, 3] AS list, null AS bound RETURN list[bound..2] IS NULL",
    );
    assert_eq!(rows, vec![vec!["true"]]);
}

#[test]
fn list_concatenation_with_scalars() {
    let mut conn = connection();
    let rows = query(&mut conn, "RETURN [1, 2] + 3 AS a, 0 + [1, 2] AS b");
    assert_eq!(rows, vec![vec!["[1, 2, 3]", "[0, 1, 2]"]]);
}

#[test]
fn list_comprehensions() {
    let mut conn = connection();
    let rows = query(
        &mut conn,
        "WITH [1, 2, 3, 4] AS list RETURN [x IN list WHERE x > 2] AS a, [x IN list | x * 10] AS b, [x IN list WHERE x < 3 | x + 1] AS c",
    );
    assert_eq!(rows, vec![vec!["[3, 4]", "[10, 20, 30, 40]", "[2, 3]"]]);
}

#[test]
fn comprehension_over_inline_aggregation() {
    let mut conn = connection();
    seed(&mut conn);
    let rows = query(
        &mut conn,
        "MATCH (n:Person) RETURN [x IN collect(n) WHERE x.age > 28 | x.name] AS names",
    );
    assert_eq!(rows, vec![vec!["[alice, carol]"]]);
}

#[test]
fn comprehension_body_hoists_free_variables() {
    let mut conn = connection();
    seed(&mut conn);
    let rows = query(
        &mut conn,
        "MATCH (n:Person) WITH n, 10 AS extra \
         RETURN size([x IN collect(n) | x.age + extra]) AS s, extra",
    );
    assert_eq!(rows, vec![vec!["3", "10"]]);
}

#[test]
fn quantifiers() {
    let mut conn = connection();
    let rows = query(
        &mut conn,
        "WITH [1, 2, 3] AS list RETURN all(x IN list WHERE x > 0) AS a, any(x IN list WHERE x > 2) AS b, none(x IN list WHERE x > 5) AS c, single(x IN list WHERE x = 2) AS d",
    );
    assert_eq!(rows, vec![vec!["true", "true", "true", "true"]]);
}

#[test]
fn quantifiers_with_nulls() {
    let mut conn = connection();
    let rows = query(
        &mut conn,
        "WITH [1, null] AS list RETURN all(x IN list WHERE x > 0) AS a, any(x IN list WHERE x > 0) AS b, none(x IN list WHERE x > 5) AS c",
    );
    assert_eq!(rows, vec![vec!["null", "true", "null"]]);
}

#[test]
fn reduce_folds_a_list() {
    let mut conn = connection();
    let rows = query(
        &mut conn,
        "RETURN reduce(total = 0, x IN [1, 2, 3] | total + x) AS total",
    );
    assert_eq!(rows, vec![vec!["6"]]);
}

#[test]
fn percentile_aggregations() {
    let mut conn = connection();
    let rows = query(
        &mut conn,
        "UNWIND [10.0, 20.0, 30.0] AS price \
         RETURN percentileDisc(price, 0.5) AS d, percentileCont(price, 0.5) AS c, \
         percentileCont(price, 0.0) AS lo, percentileDisc(price, 1.0) AS hi",
    );
    assert_eq!(rows, vec![vec!["20", "20", "10", "30"]]);
}

#[test]
fn percentile_disc_preserves_value_types() {
    let mut conn = connection();
    let rows = query(
        &mut conn,
        "UNWIND [10, 20, 30] AS x \
         RETURN percentileDisc(x, 0.5) AS d, percentileCont(x, 0.5) AS c",
    );
    assert_eq!(rows, vec![vec!["20", "20"]]);
}

#[test]
fn percentile_ignores_null_values() {
    let mut conn = connection();
    let rows = query(
        &mut conn,
        "UNWIND [null, 4.0, null, 8.0] AS x \
         RETURN percentileDisc(x, 0.5) AS d, percentileCont(x, 1.0) AS c",
    );
    assert_eq!(rows, vec![vec!["4", "8"]]);
}

#[test]
fn percentile_out_of_range_is_an_error() {
    let mut conn = connection();
    for source in [
        "UNWIND [1.0] AS x RETURN percentileCont(x, 2.0)",
        "UNWIND [1.0] AS x RETURN percentileDisc(x, -0.5)",
    ] {
        let err = conn
            .query(source, &Params::new())
            .expect_err("out of range");
        assert_eq!(err.code(), grafile::ErrorCode::Range, "{source}");
    }
}

#[test]
fn standard_deviations() {
    let mut conn = connection();
    let rows = query(
        &mut conn,
        "UNWIND [1, 2, 3, 4] AS x RETURN stDev(x) AS s, stDevP(x) AS p, stDev(null) AS n",
    );
    assert_eq!(
        rows,
        vec![vec!["1.2909944487358056", "1.118033988749895", "null"]]
    );
}
