//! MATCH semantics: expansion, direction, OPTIONAL MATCH, pre-bound
//! elements, label expressions and matching-level filters.

mod support;

use support::{connection, query, seed};

#[test]
fn create_and_match_roundtrip() {
    let mut conn = connection();
    seed(&mut conn);
    let rows = query(&mut conn, "MATCH (n:Person) RETURN n.name ORDER BY n.name");
    assert_eq!(rows, vec![vec!["alice"], vec!["bob"], vec!["carol"]]);
}

#[test]
fn pattern_expansion_and_where() {
    let mut conn = connection();
    seed(&mut conn);
    let rows = query(
        &mut conn,
        "MATCH (a:Person)-[:KNOWS]->(b:Person) WHERE a.age > 24 RETURN a.name, b.name ORDER BY a.name",
    );
    assert_eq!(rows, vec![vec!["alice", "bob"], vec!["bob", "carol"]]);
}

#[test]
fn direction_and_undirected() {
    let mut conn = connection();
    seed(&mut conn);
    let rows = query(
        &mut conn,
        "MATCH (a:Person)<-[:KNOWS]-(b:Person) RETURN a.name, b.name ORDER BY a.name",
    );
    assert_eq!(rows, vec![vec!["bob", "alice"], vec!["carol", "bob"]]);
    let rows = query(
        &mut conn,
        "MATCH (a:Person {name: 'alice'})-[:KNOWS]-(b) RETURN b.name",
    );
    assert_eq!(rows, vec![vec!["bob"]]);
}

#[test]
fn relationship_properties_and_type_filter() {
    let mut conn = connection();
    seed(&mut conn);
    let rows = query(
        &mut conn,
        "MATCH (a)-[r:KNOWS {since: 2020}]->(b) RETURN a.name, type(r), r.since",
    );
    assert_eq!(rows, vec![vec!["alice", "KNOWS", "2020"]]);
}

#[test]
fn skip_limit_and_order() {
    let mut conn = connection();
    seed(&mut conn);
    let rows = query(
        &mut conn,
        "MATCH (n:Person) RETURN n.name ORDER BY n.age DESC SKIP 1 LIMIT 1",
    );
    assert_eq!(rows, vec![vec!["alice"]]);
}

#[test]
fn multi_hop_path() {
    let mut conn = connection();
    seed(&mut conn);
    let rows = query(
        &mut conn,
        "MATCH (a:Person {name: 'alice'})-[:KNOWS]->()-[:KNOWS]->(c) RETURN c.name",
    );
    assert_eq!(rows, vec![vec!["carol"]]);
}

#[test]
fn label_predicate_in_where() {
    let mut conn = connection();
    seed(&mut conn);
    let rows = query(
        &mut conn,
        "MATCH (n) WHERE n:Person AND n.age > 30 RETURN n.name",
    );
    assert_eq!(rows, vec![vec!["carol"]]);
}

#[test]
fn optional_match_returns_null_row_on_empty_graph() {
    let mut conn = connection();
    let rows = query(&mut conn, "OPTIONAL MATCH (n) RETURN n");
    assert_eq!(rows, vec![vec!["null"]]);
}

#[test]
fn optional_match_extends_with_nulls_when_missing() {
    let mut conn = connection();
    conn.execute("CREATE ()").expect("create");
    let rows = query(
        &mut conn,
        "MATCH (n) OPTIONAL MATCH (n)-[:NOT_EXIST]->(x) RETURN n, x",
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][1], "null");
}

#[test]
fn optional_match_keeps_matches() {
    let mut conn = connection();
    conn.execute("CREATE (:A)-[:T]->(:B)").expect("create");
    let rows = query(
        &mut conn,
        "MATCH (a:A) OPTIONAL MATCH (a)-[:T]->(b) RETURN b",
    );
    assert_eq!(rows, vec![vec!["(node 2)"]]);
}

#[test]
fn optional_match_with_where_filters_before_null_extension() {
    let mut conn = connection();
    conn.execute("CREATE (:A)-[:T]->(:B {name: 'b'})")
        .expect("create");
    let rows = query(
        &mut conn,
        "MATCH (a:A) OPTIONAL MATCH (a)-[:T]->(b) WHERE b.name = 'nope' RETURN b",
    );
    assert_eq!(rows, vec![vec!["null"]]);
}

#[test]
fn match_after_optional_match_from_null_returns_nothing() {
    let mut conn = connection();
    conn.execute("CREATE ()-[:T]->()").expect("create");
    let rows = query(
        &mut conn,
        "OPTIONAL MATCH (a:Missing) WITH a MATCH (a)-->(b) RETURN b",
    );
    assert!(rows.is_empty());
}

#[test]
fn optional_match_after_optional_match() {
    let mut conn = connection();
    conn.execute("CREATE (:A)-[:T]->(:B)").expect("create");
    let rows = query(
        &mut conn,
        "MATCH (a:A) OPTIONAL MATCH (a)-[:T]->(b) OPTIONAL MATCH (b)-[:T]->(c) RETURN b, c",
    );
    assert_eq!(rows, vec![vec!["(node 2)", "null"]]);
}

#[test]
fn with_where_can_reference_incoming_scope() {
    let mut conn = connection();
    conn.execute("CREATE (:A)-[:T]->(:B)").expect("create");
    let rows = query(
        &mut conn,
        "MATCH (a:A) OPTIONAL MATCH (a)-[:T]->(b) WITH a WHERE b IS NULL RETURN a",
    );
    assert!(rows.is_empty());
}

#[test]
fn null_bound_variables_are_safe_in_reads_and_writes() {
    let mut conn = connection();
    let rows = query(
        &mut conn,
        "OPTIONAL MATCH (n:Missing) SET n.x = 1 REMOVE n.x DELETE n RETURN n.x IS NULL",
    );
    assert_eq!(rows, vec![vec!["true"]]);
}

#[test]
fn pattern_verifies_a_bound_relationship() {
    let mut conn = connection();
    conn.execute("CREATE (:A)-[:T]->(:B), (:C)-[:T]->(:D)")
        .expect("create");
    let rows = query(
        &mut conn,
        "MATCH ()-[r]->() WITH r LIMIT 1 MATCH (a)-[r]->(b) RETURN a, b",
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], "(node 1)");
    assert_eq!(rows[0][1], "(node 2)");
}

#[test]
fn pattern_verifies_labels_on_bound_nodes() {
    let mut conn = connection();
    conn.execute("CREATE (:A)-[:T]->(:B)").expect("create");
    let rows = query(
        &mut conn,
        "MATCH (a:A)-[:T]->(b) WHERE b:B MATCH (a)-[:T]->(c:Missing) RETURN c",
    );
    assert!(rows.is_empty());
}

#[test]
fn pattern_checks_every_label_on_scanned_nodes() {
    let mut conn = connection();
    conn.execute("CREATE (:A), (:A:B)").expect("create");
    let rows = query(&mut conn, "MATCH (n:A:B) RETURN count(*)");
    assert_eq!(rows, vec![vec!["1"]]);
}

#[test]
fn label_expressions_match_nodes() {
    let mut conn = connection();
    conn.execute("CREATE (:A), (:B), (:C), (:A:B), (:D)")
        .expect("create");
    let rows = query(&mut conn, "MATCH (n:A|B) RETURN count(*)");
    assert_eq!(rows, vec![vec!["3"]]);
    let rows = query(&mut conn, "MATCH (n:A&B) RETURN count(*)");
    assert_eq!(rows, vec![vec!["1"]]);
    let rows = query(&mut conn, "MATCH (n:!A) RETURN count(*)");
    assert_eq!(rows, vec![vec!["3"]]);
    let rows = query(&mut conn, "MATCH (n:(A|B)&!D) RETURN count(*)");
    assert_eq!(rows, vec![vec!["3"]]);
    // A lone node pattern in a predicate evaluates its label expression.
    let rows = query(&mut conn, "MATCH (n) WHERE (n:A|B) RETURN count(*)");
    assert_eq!(rows, vec![vec!["3"]]);
}

#[test]
fn label_expressions_on_relationships() {
    let mut conn = connection();
    conn.execute("CREATE (a:N)-[:X]->(b:N), (a)-[:Y]->(b), (a)-[:Z]->(b)")
        .expect("create");
    let rows = query(&mut conn, "MATCH ()-[r:!X]->() RETURN count(*)");
    assert_eq!(rows, vec![vec!["2"]]);
    let rows = query(&mut conn, "MATCH ()-[r:X|:Y]->() RETURN count(*)");
    assert_eq!(rows, vec![vec!["2"]]);
    // Variable-length traversals accept plain disjunctions...
    let rows = query(&mut conn, "MATCH (a:N)-[:Y|:Z*1..1]->(b) RETURN count(*)");
    assert_eq!(rows, vec![vec!["2"]]);
    // ...but negation and conjunction need the single-hop form.
    let err = conn
        .query(
            "MATCH (a:N)-[:!X*1..2]->(b) RETURN count(*)",
            &grafile::api::Params::new(),
        )
        .expect_err("variable-length negation");
    assert_eq!(err.code(), grafile::ErrorCode::Unsupported);
}

#[test]
fn relationships_are_distinct_within_a_pattern() {
    let mut conn = connection();
    conn.execute("CREATE (:A)-[:T1]->(l:Looper), (l)-[:LOOP]->(l), (l)-[:T2]->(:B)")
        .expect("seed");
    let rows = query(
        &mut conn,
        "MATCH (x:A)-[r1]->(y)-[r2]-(z) RETURN type(r2), labels(z) ORDER BY type(r2)",
    );
    assert_eq!(rows, vec![vec!["LOOP", "[Looper]"], vec!["T2", "[B]"]]);
    // A self-loop matches once.
    let rows = query(&mut conn, "MATCH (n)-[r:LOOP]->(n) RETURN count(r)");
    assert_eq!(rows, vec![vec!["1"]]);
}

#[test]
fn keywords_and_repeated_types_in_patterns() {
    let mut conn = connection();
    conn.execute("CREATE (a:A)-[:CONTAINS]->(b:B)")
        .expect("seed");
    let rows = query(
        &mut conn,
        "MATCH (a)-[:CONTAINS|:CONTAINS]->(b) RETURN count(b)",
    );
    assert_eq!(rows, vec![vec!["1"]]);
}

#[test]
fn bidirectional_arrow_is_undirected() {
    let mut conn = connection();
    conn.execute("CREATE (a:A)-[:T]->(b:B)").expect("seed");
    let rows = query(&mut conn, "MATCH (a:A)<-->(b:B) RETURN count(*)");
    assert_eq!(rows, vec![vec!["1"]]);
    let rows = query(&mut conn, "MATCH (b:B)<-->(a:A) RETURN count(*)");
    assert_eq!(rows, vec![vec!["1"]]);
}

#[test]
fn non_literal_pattern_properties_filter_at_runtime() {
    let mut conn = connection();
    conn.execute("CREATE (:A {num: 1})-[:T {w: 5}]->(:B {num: 2})")
        .expect("create");
    let rows = query(
        &mut conn,
        "WITH 1 AS x MATCH (n {num: x}) RETURN n.num AS n",
    );
    assert_eq!(rows, vec![vec!["1"]]);
    let rows = query(
        &mut conn,
        "WITH 5 AS w MATCH (a)-[r:T {w: w}]->(b) RETURN a.num AS a, b.num AS b",
    );
    assert_eq!(rows, vec![vec!["1", "2"]]);
    let rows = query(
        &mut conn,
        "UNWIND [1, 3] AS x MATCH (n {num: x}) RETURN count(*) AS c",
    );
    assert_eq!(rows, vec![vec!["1"]]);
    let rows = query(
        &mut conn,
        "OPTIONAL MATCH (n {num: 99}) RETURN n IS NULL AS missing",
    );
    assert_eq!(rows, vec![vec!["true"]]);
}
