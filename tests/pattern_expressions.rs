//! Pattern predicates, pattern comprehensions and exists().

mod support;

use grafile::api::Params;
use support::{connection, query, seed};

#[test]
fn pattern_predicate_filters_rows() {
    let mut conn = connection();
    seed(&mut conn);
    let rows = query(
        &mut conn,
        "MATCH (n) WHERE (n)-[:KNOWS]->() RETURN n.name ORDER BY n.name",
    );
    assert_eq!(rows, vec![vec!["alice"], vec!["bob"]]);
}

#[test]
fn pattern_predicate_with_bound_endpoints() {
    let mut conn = connection();
    seed(&mut conn);
    let rows = query(
        &mut conn,
        "MATCH (a), (b) WHERE (a)-[:KNOWS]->(b) RETURN a.name, b.name ORDER BY a.name",
    );
    assert_eq!(rows, vec![vec!["alice", "bob"], vec!["bob", "carol"]]);
}

#[test]
fn pattern_predicate_negation_and_conjunction() {
    let mut conn = connection();
    seed(&mut conn);
    let rows = query(
        &mut conn,
        "MATCH (n:Person) WHERE NOT (n)-[:LIKES]->() RETURN n.name ORDER BY n.name",
    );
    assert_eq!(rows, vec![vec!["bob"], vec!["carol"]]);
    let rows = query(
        &mut conn,
        "MATCH (n:Person) WHERE (n)-[:KNOWS]->() AND (n)-[:LIKES]->() RETURN n.name",
    );
    assert_eq!(rows, vec![vec!["alice"]]);
}

#[test]
fn pattern_predicate_requires_bound_variables() {
    let mut conn = connection();
    seed(&mut conn);
    let err = conn
        .query(
            "MATCH (n) WHERE (n)-[:KNOWS]->(m) RETURN n.name",
            &Params::new(),
        )
        .expect_err("unbound pattern variable must be rejected");
    assert!(err.to_string().contains("not defined"), "{err}");
}

#[test]
fn pattern_comprehension_lists_matches() {
    let mut conn = connection();
    seed(&mut conn);
    let rows = query(
        &mut conn,
        "MATCH (n:Person) RETURN [(n)-[:KNOWS]->(m) | m.name] AS names ORDER BY n.name",
    );
    assert_eq!(rows, vec![vec!["[bob]"], vec!["[carol]"], vec!["[]"]]);
}

#[test]
fn pattern_comprehension_binds_paths_and_filters() {
    let mut conn = connection();
    seed(&mut conn);
    let rows = query(
        &mut conn,
        "MATCH (n:Person {name: 'alice'}) \
         RETURN [p = (n)-[:KNOWS]->() WHERE n.age > 20 | length(p)] AS lens",
    );
    assert_eq!(rows, vec![vec!["[1]"]]);
    let rows = query(
        &mut conn,
        "MATCH (n:Person {name: 'carol'}) RETURN [(n)-[:KNOWS]->(m) | m.name] AS names",
    );
    assert_eq!(rows, vec![vec!["[]"]]);
}

#[test]
fn pattern_comprehension_in_where() {
    let mut conn = connection();
    seed(&mut conn);
    let rows = query(
        &mut conn,
        "MATCH (n:Person) WHERE size([(n)-[:KNOWS]->(m) | m.name]) > 0 \
         RETURN n.name ORDER BY n.name",
    );
    assert_eq!(rows, vec![vec!["alice"], vec!["bob"]]);
}

#[test]
fn exists_with_pattern_and_property() {
    let mut conn = connection();
    seed(&mut conn);
    let rows = query(
        &mut conn,
        "MATCH (n:Person) RETURN n.name AS name, exists((n)-[:LIKES]->()) AS likes, \
         exists(n.age) AS aged ORDER BY name",
    );
    assert_eq!(
        rows,
        vec![
            vec!["alice", "true", "true"],
            vec!["bob", "false", "true"],
            vec!["carol", "false", "true"],
        ]
    );
}
