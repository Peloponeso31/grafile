//! Built-in CALL procedures and YIELD handling.

mod support;

use support::{connection, query, seed};

#[test]
fn call_builtin_procedures() {
    let mut conn = connection();
    seed(&mut conn);
    let rows = query(
        &mut conn,
        "CALL db.labels() YIELD label RETURN label ORDER BY label",
    );
    assert_eq!(rows, vec![vec!["Person"]]);
    let rows = query(
        &mut conn,
        "CALL db.relationshipTypes() YIELD relationshipType RETURN relationshipType ORDER BY relationshipType",
    );
    assert_eq!(rows, vec![vec!["KNOWS"], vec!["LIKES"]]);
    let rows = query(
        &mut conn,
        "CALL db.propertyKeys() YIELD propertyKey RETURN propertyKey ORDER BY propertyKey",
    );
    assert_eq!(rows, vec![vec!["age"], vec!["name"], vec!["since"]]);
}

#[test]
fn call_yields_aliases_and_aggregates() {
    let mut conn = connection();
    seed(&mut conn);
    let rows = query(
        &mut conn,
        "CALL db.labels() YIELD label AS l RETURN count(l) AS c",
    );
    assert_eq!(rows, vec![vec!["1"]]);
}
