//! Write clauses: CREATE, SET, SET +=, REMOVE, DELETE, MERGE and FOREACH.

mod support;

use support::{connection, query, seed};

#[test]
fn set_and_delete() {
    let mut conn = connection();
    seed(&mut conn);
    conn.execute("MATCH (n:Person {name: 'bob'}) SET n.age = 26")
        .expect("set");
    let rows = query(&mut conn, "MATCH (n:Person {name: 'bob'}) RETURN n.age");
    assert_eq!(rows, vec![vec!["26"]]);

    // Deleting a connected node requires DETACH.
    let err = conn
        .execute("MATCH (n:Person {name: 'bob'}) DELETE n")
        .expect_err("constraint");
    assert_eq!(err.code(), grafile::ErrorCode::Constraint);
    conn.execute("MATCH (n:Person {name: 'bob'}) DETACH DELETE n")
        .expect("detach delete");
    let rows = query(&mut conn, "MATCH (n:Person) RETURN count(*)");
    assert_eq!(rows, vec![vec!["2"]]);
}

#[test]
fn merge_is_idempotent() {
    let mut conn = connection();
    conn.execute("MERGE (n:Person {name: 'dave'})")
        .expect("merge");
    conn.execute("MERGE (n:Person {name: 'dave'})")
        .expect("merge");
    let rows = query(&mut conn, "MATCH (n:Person) RETURN count(*)");
    assert_eq!(rows, vec![vec!["1"]]);
    conn.execute("MATCH (n:Person {name: 'dave'}) MERGE (n)-[:KNOWS]->(m:Person {name: 'eve'})")
        .expect("merge edge");
    let rows = query(
        &mut conn,
        "MATCH (:Person)-[r:KNOWS]->(:Person) RETURN count(r)",
    );
    assert_eq!(rows, vec![vec!["1"]]);
}

#[test]
fn merge_on_create_and_on_match() {
    let mut conn = connection();
    conn.execute("MERGE (n:Person {name: 'frank'}) ON CREATE SET n.created = true")
        .expect("merge create");
    conn.execute("MERGE (n:Person {name: 'frank'}) ON MATCH SET n.seen = true")
        .expect("merge match");
    let rows = query(
        &mut conn,
        "MATCH (n:Person {name: 'frank'}) RETURN n.created, n.seen",
    );
    assert_eq!(rows, vec![vec!["true", "true"]]);
}

#[test]
fn create_with_bound_variables() {
    let mut conn = connection();
    seed(&mut conn);
    conn.execute(
        "MATCH (a:Person {name: 'alice'}) CREATE (a)-[:WORKS_AT]->(c:Company {name: 'acme'})",
    )
    .expect("create with bound");
    let rows = query(
        &mut conn,
        "MATCH (:Person)-[:WORKS_AT]->(c:Company) RETURN c.name",
    );
    assert_eq!(rows, vec![vec!["acme"]]);
}

#[test]
fn set_append_merges_properties() {
    let mut conn = connection();
    conn.execute("CREATE (:A {name: 'a', keep: 1})")
        .expect("create");
    let rows = query(
        &mut conn,
        "MATCH (n:A) SET n += {name: 'b', extra: 2, gone: null} RETURN n.name AS name, n.keep AS keep, n.extra AS extra",
    );
    assert_eq!(rows, vec![vec!["b", "1", "2"]]);
    let rows = query(&mut conn, "MATCH (n:A) RETURN n.gone IS NULL");
    assert_eq!(rows, vec![vec!["true"]]);
}

#[test]
fn set_append_on_null_is_a_no_op() {
    let mut conn = connection();
    let rows = query(
        &mut conn,
        "OPTIONAL MATCH (n:Missing) SET n += {num: 1} RETURN n",
    );
    assert_eq!(rows, vec![vec!["null"]]);
}

#[test]
fn set_append_merges_relationship_properties() {
    let mut conn = connection();
    conn.execute("CREATE (:A)-[:T {since: 1}]->(:B)")
        .expect("create");
    let rows = query(
        &mut conn,
        "MATCH ()-[r:T]->() SET r += {since: 2, extra: 3} RETURN r.since AS since, r.extra AS extra",
    );
    assert_eq!(rows, vec![vec!["2", "3"]]);
}

#[test]
fn merge_matches_whole_path_atomically() {
    let mut conn = connection();
    conn.execute("CREATE (:Person {name: 'gina'})-[:KNOWS]->(:Person {name: 'hank'})")
        .expect("seed");
    conn.execute("MERGE (:Person {name: 'gina'})-[:KNOWS]->(:Person {name: 'hank'})")
        .expect("merge path");
    // The whole path exists, so the MERGE matches it without creating.
    let rows = query(&mut conn, "MATCH (n:Person) RETURN count(*)");
    assert_eq!(rows, vec![vec!["2"]]);
    let rows = query(&mut conn, "MATCH ()-[r:KNOWS]->() RETURN count(r)");
    assert_eq!(rows, vec![vec!["1"]]);
}

#[test]
fn merge_creates_fresh_endpoints_when_path_is_absent() {
    let mut conn = connection();
    conn.execute("CREATE (:Person {name: 'iris'})")
        .expect("seed");
    conn.execute("MERGE (:Person {name: 'iris'})-[:KNOWS]->(:Person {name: 'jack'})")
        .expect("merge path");
    // The whole path is absent, so the existing `iris` node is not reused.
    let rows = query(&mut conn, "MATCH (n:Person) RETURN count(*)");
    assert_eq!(rows, vec![vec!["3"]]);
}

#[test]
fn merge_respects_relationship_direction_when_matching() {
    let mut conn = connection();
    conn.execute("CREATE (:Person {name: 'kim'})-[:KNOWS]->(:Person {name: 'lena'})")
        .expect("seed");
    conn.execute("MERGE (:Person {name: 'lena'})-[:KNOWS]->(:Person {name: 'kim'})")
        .expect("merge path");
    // The reverse edge is created; the existing edge is not reused.
    let rows = query(&mut conn, "MATCH ()-[r:KNOWS]->() RETURN count(r)");
    assert_eq!(rows, vec![vec!["2"]]);
    // An undirected pattern matches edges in both directions.
    conn.execute("MERGE (:Person {name: 'lena'})-[:KNOWS]-(:Person {name: 'kim'})")
        .expect("merge undirected");
    let rows = query(&mut conn, "MATCH ()-[r:KNOWS]->() RETURN count(r)");
    assert_eq!(rows, vec![vec!["2"]]);
}

#[test]
fn merge_binds_matched_path() {
    let mut conn = connection();
    conn.execute("CREATE (:Person {name: 'mia'})-[:KNOWS]->(:Person {name: 'nate'})")
        .expect("seed");
    let rows = query(
        &mut conn,
        "MERGE p = (:Person {name: 'mia'})-[:KNOWS]->(:Person {name: 'nate'}) RETURN length(p)",
    );
    assert_eq!(rows, vec![vec!["1"]]);
}

#[test]
fn merge_emits_one_row_per_match() {
    let mut conn = connection();
    conn.execute("CREATE (:Person {name: 'olga'})")
        .expect("seed");
    conn.execute("MATCH (a:Person) MERGE (a)-[:KNOWS]->(:Person {name: 'pete'})")
        .expect("merge edge");
    conn.execute("MATCH (a:Person) MERGE (a)-[:KNOWS]->(:Person {name: 'pete'})")
        .expect("merge edge again");
    let rows = query(
        &mut conn,
        "MATCH (:Person {name: 'olga'})-[r:KNOWS]->(:Person {name: 'pete'}) RETURN count(r)",
    );
    assert_eq!(rows, vec![vec!["1"]]);
}

#[test]
fn set_entity_copies_properties() {
    let mut conn = connection();
    conn.execute("CREATE (:Person {name: 'ravi', age: 40, city: 'oslo'})")
        .expect("seed");
    conn.execute("CREATE (:Person {name: 'sara'})")
        .expect("seed");
    conn.execute("MATCH (a:Person {name: 'ravi'}), (b:Person {name: 'sara'}) SET b = a")
        .expect("copy properties");
    // The copy replaces every property, including the name.
    let rows = query(
        &mut conn,
        "MATCH (n:Person {name: 'ravi'}) RETURN count(*), max(n.age)",
    );
    assert_eq!(rows, vec![vec!["2", "40"]]);
}

#[test]
fn writes_on_entities_unwound_from_lists() {
    let mut conn = connection();
    conn.execute("CREATE (:Person {name: 'ravi', age: 40})")
        .expect("seed");
    let rows = query(
        &mut conn,
        "MATCH (a:Person) WITH collect(a) AS people UNWIND people AS n \
         SET n.age = 41, n.city = 'oslo' RETURN n.age, n.city",
    );
    assert_eq!(rows, vec![vec!["41", "oslo"]]);
    let rows = query(
        &mut conn,
        "MATCH (a:Person) WITH collect(a) AS people UNWIND people AS n \
         REMOVE n.city RETURN n.city",
    );
    assert_eq!(rows, vec![vec!["null"]]);
    let rows = query(
        &mut conn,
        "MATCH (a:Person) WITH collect(a) AS people UNWIND people AS n \
         DETACH DELETE n RETURN count(*)",
    );
    assert_eq!(rows, vec![vec!["1"]]);
}

#[test]
fn dynamic_property_access_on_entities() {
    let mut conn = connection();
    conn.execute(
        "CREATE (:Person {name: 'tara'})-[:KNOWS {since: 2019}]->(:Person {name: 'umar'})",
    )
    .expect("seed");
    let rows = query(
        &mut conn,
        "MATCH (n:Person {name: 'tara'})-[r:KNOWS]->() RETURN n['name'], r['since'], n['missing']",
    );
    assert_eq!(rows, vec![vec!["tara", "2019", "null"]]);
}

#[test]
fn create_respects_relationship_direction() {
    let mut conn = connection();
    conn.execute("CREATE (:A)<-[:R]-(:B), (:C)-[:S]->(:D)")
        .expect("create");
    let rows = query(
        &mut conn,
        "MATCH (b:B)-[:R]->(a:A) RETURN labels(a), labels(b)",
    );
    assert_eq!(rows, vec![vec!["[A]", "[B]"]]);
    let rows = query(
        &mut conn,
        "MATCH (c:C)-[:S]->(d:D) RETURN labels(c), labels(d)",
    );
    assert_eq!(rows, vec![vec!["[C]", "[D]"]]);
}

#[test]
fn foreach_creates_per_element() {
    let mut conn = connection();
    seed(&mut conn);
    conn.execute(
        "MATCH (n:Person {name: 'alice'}) FOREACH (x IN [1, 2, 3] | CREATE (:Tag {v: x}))",
    )
    .expect("foreach create");
    let rows = query(&mut conn, "MATCH (t:Tag) RETURN count(*)");
    assert_eq!(rows, vec![vec!["3"]]);
    let rows = query(&mut conn, "MATCH (t:Tag) RETURN count(*) ORDER BY count(*)");
    assert_eq!(rows, vec![vec!["3"]]);
}

#[test]
fn foreach_updates_and_nests() {
    let mut conn = connection();
    seed(&mut conn);
    conn.execute("MATCH (n:Person {name: 'alice'}) FOREACH (x IN [1, 2] | SET n.step = x)")
        .expect("foreach set");
    let rows = query(&mut conn, "MATCH (n:Person {name: 'alice'}) RETURN n.step");
    assert_eq!(rows, vec![vec!["2"]]);
    conn.execute(
        "FOREACH (x IN [1, 2] | FOREACH (y IN [10, 20] | CREATE (:Pair {v: x * 100 + y})))",
    )
    .expect("nested foreach");
    let rows = query(&mut conn, "MATCH (p:Pair) RETURN count(*)");
    assert_eq!(rows, vec![vec!["4"]]);
}
