//! Variable-length paths, named paths and shortestPath/allShortestPaths.

mod support;

use grafile::api::Params;
use support::{connection, query};

#[test]
fn named_variable_length_paths_include_intermediate_nodes() {
    let mut conn = connection();
    conn.execute("CREATE (a {n: 'a'})-[:K]->(b {n: 'b'})-[:K]->(c {n: 'c'})")
        .expect("seed");
    let rows = query(
        &mut conn,
        "MATCH p = (a {n: 'a'})-[:K*2..2]->(c) RETURN size(nodes(p)), size(relationships(p))",
    );
    assert_eq!(rows, vec![vec!["3", "2"]]);
}

#[test]
fn variable_length_paths() {
    let mut conn = connection();
    conn.execute("CREATE (a:A)-[:T]->(b:B)-[:T]->(c:C)-[:T]->(d:D)")
        .expect("create");
    let rows = query(&mut conn, "MATCH (a:A)-[*1..2]->(x) RETURN x.name");
    assert_eq!(rows.len(), 2);
    let rows = query(&mut conn, "MATCH (a:A)-[*]->(x) RETURN count(*)");
    assert_eq!(rows, vec![vec!["3"]]);
    let rows = query(&mut conn, "MATCH (a:A)-[*2]->(x) RETURN x.name");
    assert_eq!(rows, vec![vec!["null"]]);
}

#[test]
fn zero_length_variable_paths_include_the_start() {
    let mut conn = connection();
    conn.execute("CREATE (a:A)-[:T]->(b:B)").expect("create");
    let rows = query(&mut conn, "MATCH (a:A)-[*0..1]->(x) RETURN count(*)");
    assert_eq!(rows, vec![vec!["2"]]);
}

#[test]
fn variable_length_binds_a_relationship_list() {
    let mut conn = connection();
    conn.execute("CREATE (a:A)-[:T]->(:B)-[:T]->(:C)")
        .expect("create");
    let rows = query(
        &mut conn,
        "MATCH (a:A)-[r*2]->(x) RETURN size(r) AS n, labels(x) AS t",
    );
    assert_eq!(rows, vec![vec!["2", "[C]"]]);
}

#[test]
fn variable_length_trails_do_not_repeat_relationships() {
    let mut conn = connection();
    conn.execute("CREATE (a:A)-[:T]->(b:B)-[:T]->(a)")
        .expect("create");
    // The trail may not reuse the edge back to the start, so the only
    // trails are one and two hops.
    let rows = query(&mut conn, "MATCH (a:A)-[*]->(x) RETURN count(*)");
    assert_eq!(rows, vec![vec!["2"]]);
}

#[test]
fn optional_variable_length_from_null_is_null() {
    let mut conn = connection();
    let rows = query(&mut conn, "OPTIONAL MATCH (a:Missing)-[*]->(x) RETURN x");
    assert_eq!(rows, vec![vec!["null"]]);
}

#[test]
fn named_paths_bind_alternating_values() {
    let mut conn = connection();
    conn.execute("CREATE (:A {name: 'A'})-[:KNOWS]->(:B {name: 'B'})")
        .expect("create");
    let rows = query(
        &mut conn,
        "MATCH p = (a {name: 'A'})-[:KNOWS]->(b) RETURN length(p) AS l, size(nodes(p)) AS n, size(relationships(p)) AS r, p IS NULL AS nil",
    );
    assert_eq!(rows, vec![vec!["1", "2", "1", "false"]]);
}

#[test]
fn named_paths_with_variable_length_splice_edges() {
    let mut conn = connection();
    conn.execute("CREATE (:A)-[:T]->(:B)-[:T]->(:C)")
        .expect("create");
    let rows = query(&mut conn, "MATCH p = (a:A)-[*2]->(c) RETURN length(p) AS l");
    assert_eq!(rows, vec![vec!["2"]]);
    let rows = query(
        &mut conn,
        "MATCH p = (a:A)-[*0..1]->(x) RETURN length(p) AS l",
    );
    assert_eq!(rows.len(), 2);
}

#[test]
fn optional_named_path_miss_is_null() {
    let mut conn = connection();
    let rows = query(
        &mut conn,
        "OPTIONAL MATCH p = (a:Missing)-[:T]->(b) RETURN p IS NULL AS missing",
    );
    assert_eq!(rows, vec![vec!["true"]]);
}

fn shortest_graph(conn: &mut grafile::api::Connection) {
    conn.execute(
        "CREATE (a:N {name: 'a'})-[:R]->(b:N {name: 'b'}), \
         (b)-[:R]->(c:N {name: 'c'}), \
         (a)-[:R]->(c), \
         (c)-[:R]->(d:N {name: 'd'}), \
         (b)-[:R]->(d), \
         (a)-[:S]->(d)",
    )
    .expect("seed");
}

#[test]
fn shortest_path_prefers_fewer_hops() {
    let mut conn = connection();
    shortest_graph(&mut conn);
    let rows = query(
        &mut conn,
        "MATCH (a:N {name: 'a'}), (d:N {name: 'd'}) \
         MATCH p = shortestPath((a)-[:R*]->(d)) RETURN length(p)",
    );
    assert_eq!(rows, vec![vec!["2"]]);
    let rows = query(
        &mut conn,
        "MATCH (a:N {name: 'a'}), (d:N {name: 'd'}) \
         MATCH p = shortestPath((a)-[:S*]->(d)) RETURN length(p)",
    );
    assert_eq!(rows, vec![vec!["1"]]);
}

#[test]
fn all_shortest_paths_returns_every_minimal_path() {
    let mut conn = connection();
    shortest_graph(&mut conn);
    let rows = query(
        &mut conn,
        "MATCH (a:N {name: 'a'}), (d:N {name: 'd'}) \
         MATCH p = allShortestPaths((a)-[:R*]->(d)) RETURN length(p) ORDER BY length(p)",
    );
    assert_eq!(rows, vec![vec!["2"], vec!["2"]]);
}

#[test]
fn shortest_path_honours_bounds_and_direction() {
    let mut conn = connection();
    shortest_graph(&mut conn);
    let rows = query(
        &mut conn,
        "MATCH (a:N {name: 'a'}), (d:N {name: 'd'}) \
         MATCH p = shortestPath((a)-[:R*3..4]->(d)) RETURN length(p)",
    );
    assert_eq!(rows, vec![vec!["3"]]);
    let rows = query(
        &mut conn,
        "MATCH (a:N {name: 'a'}), (d:N {name: 'd'}) \
         MATCH p = shortestPath((d)-[:R*]-(a)) RETURN length(p)",
    );
    assert_eq!(rows, vec![vec!["2"]]);
}

#[test]
fn shortest_path_binds_unbound_target_per_node() {
    let mut conn = connection();
    shortest_graph(&mut conn);
    let rows = query(
        &mut conn,
        "MATCH (a:N {name: 'a'}) MATCH p = shortestPath((a)-[:R*]->(x)) \
         RETURN x.name ORDER BY x.name",
    );
    assert_eq!(rows, vec![vec!["b"], vec!["c"], vec!["d"]]);
}

#[test]
fn shortest_path_expression_is_null_when_absent() {
    let mut conn = connection();
    shortest_graph(&mut conn);
    let rows = query(
        &mut conn,
        "MATCH (d:N {name: 'd'}), (b:N {name: 'b'}) \
         RETURN shortestPath((d)-[:R*]->(b)) IS NULL AS missing, \
         length(shortestPath((b)-[:R*]->(d))) AS len",
    );
    assert_eq!(rows, vec![vec!["true", "1"]]);
}

#[test]
fn shortest_path_requires_variable_length_single_segment() {
    let mut conn = connection();
    shortest_graph(&mut conn);
    let err = conn
        .query(
            "MATCH p = shortestPath((a)-[:R]->(b)) RETURN p",
            &Params::new(),
        )
        .expect_err("fixed-length shortestPath must be rejected");
    assert!(err.to_string().contains("variable-length"), "{err}");
    let err = conn
        .query(
            "MATCH p = shortestPath((a)-[:R*]->(b)-[:R*]->(c)) RETURN p",
            &Params::new(),
        )
        .expect_err("multi-segment shortestPath must be rejected");
    assert!(err.to_string().contains("exactly one"), "{err}");
}
