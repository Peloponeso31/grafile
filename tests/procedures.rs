//! Built-in CALL procedures and YIELD handling.

mod support;

use grafile::api::Params;
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

#[test]
fn await_indexes_is_a_unit_call() {
    let mut conn = connection();
    query(&mut conn, "CALL db.awaitIndexes(600)");
    let rows = query(&mut conn, "CALL db.awaitIndexes() RETURN 1 AS ok");
    assert_eq!(rows, vec![vec!["1"]]);
    let rows = query(
        &mut conn,
        "UNWIND [1, 2] AS x CALL db.awaitIndexes(10) RETURN x ORDER BY x",
    );
    assert_eq!(rows, vec![vec!["1"], vec!["2"]]);
}

#[test]
fn await_indexes_validates_its_argument() {
    let mut conn = connection();
    let err = conn
        .query("CALL db.awaitIndexes('soon')", &Params::new())
        .expect_err("non-integer timeout");
    assert!(err.to_string().contains("integer timeout"), "{err}");
    let err = conn
        .query("CALL db.awaitIndexes(1, 2)", &Params::new())
        .expect_err("too many arguments");
    assert!(err.to_string().contains("between 0 and 1"), "{err}");
    let err = conn
        .query("CALL db.labels(1)", &Params::new())
        .expect_err("labels takes no arguments");
    assert!(err.to_string().contains("takes no arguments"), "{err}");
}

#[test]
fn random_uuid_returns_distinct_v4_strings() {
    let mut conn = connection();
    let rows = query(&mut conn, "RETURN randomUUID() AS a, randomUUID() AS b");
    let first = &rows[0][0];
    let second = &rows[0][1];
    assert_ne!(first, second);
    for uuid in [first, second] {
        assert_eq!(uuid.len(), 36, "{uuid}");
        assert_eq!(uuid.chars().filter(|ch| *ch == '-').count(), 4, "{uuid}");
        assert_eq!(uuid.chars().nth(14), Some('4'), "{uuid}");
    }
    let err = conn
        .query("RETURN randomUUID(1)", &Params::new())
        .expect_err("randomUUID takes no arguments");
    assert!(err.to_string().contains("takes no arguments"), "{err}");
}
