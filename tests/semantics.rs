//! Comparison, membership and ordering semantics.

mod support;

use support::{connection, query};

#[test]
fn equality_is_false_across_types() {
    let mut conn = connection();
    let rows = query(
        &mut conn,
        "RETURN 1 = '1' AS a, [1] = 1 AS b, {k: null} = {} AS c, \
         {k: 1, l: 2} = {k: 1, l: 2} AS d, {k: null} = {k: null} AS e, \
         {k: 1} = {k: null} AS f",
    );
    assert_eq!(
        rows,
        vec![vec!["false", "false", "false", "true", "null", "null"]]
    );
}

#[test]
fn nan_is_not_equal_or_ordered() {
    let mut conn = connection();
    let rows = query(
        &mut conn,
        "RETURN (0.0 / 0.0 = 0.0 / 0.0) AS a, (0.0 / 0.0 <> 0.0 / 0.0) AS b, \
         (0.0 / 0.0 > 1) AS c, (0.0 / 0.0 = 1) AS d",
    );
    assert_eq!(rows, vec![vec!["false", "true", "false", "false"]]);
}

#[test]
fn list_comparisons_are_lexicographic_with_nulls_unknown() {
    let mut conn = connection();
    let rows = query(
        &mut conn,
        "RETURN [1, 0] >= [1] AS a, [1, null] >= [1] AS b, [1, 2] >= [1, null] AS c, \
         [1, 2] < [3, 4] AS d, [1] < [1, 2] AS e",
    );
    assert_eq!(rows, vec![vec!["true", "true", "null", "true", "true"]]);
}

#[test]
fn membership_type_mismatches_are_false() {
    let mut conn = connection();
    let rows = query(
        &mut conn,
        "RETURN 5 IN ['a'] AS a, null IN [] AS b, [1, 2] IN [[2, 1]] AS c, \
         2 IN [null, 1] AS d, [1, 2] IN [[1, 2]] AS e",
    );
    assert_eq!(rows, vec![vec!["false", "false", "false", "null", "true"]]);
}

#[test]
fn ordering_uses_the_neo4j_type_order_with_nulls_last() {
    let mut conn = connection();
    let rows = query(
        &mut conn,
        "UNWIND [1.5, ['list'], 'text', null, false, {a: 'map'}] AS types \
         WITH types ORDER BY types RETURN types",
    );
    assert_eq!(
        rows,
        vec![
            vec!["{a: map}"],
            vec!["[list]"],
            vec!["text"],
            vec!["false"],
            vec!["1.5"],
            vec!["null"],
        ]
    );
}
