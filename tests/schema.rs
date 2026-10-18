//! Indexes, named DDL and constraints: creation, enforcement, backfill,
//! dropping and planner use.

mod support;

use support::{connection, query, seed};

#[test]
fn index_seek_is_used() {
    let mut conn = connection();
    seed(&mut conn);
    conn.create_node_index("name", Some("Person"), true)
        .expect("index");
    let plan = conn
        .explain("MATCH (n:Person {name: 'alice'}) RETURN n.age")
        .expect("explain");
    assert!(plan.contains("seek="), "{plan}");
    let rows = query(&mut conn, "MATCH (n:Person {name: 'alice'}) RETURN n.age");
    assert_eq!(rows, vec![vec!["30"]]);
}

#[test]
fn unique_constraint_via_cypher() {
    let mut conn = connection();
    conn.execute("CREATE (:Person {email: 'a@example.com'})")
        .expect("first");
    conn.create_node_index("email", Some("Person"), true)
        .expect("index");
    let err = conn
        .execute("CREATE (:Person {email: 'a@example.com'})")
        .expect_err("duplicate");
    assert_eq!(err.code(), grafile::ErrorCode::Constraint);
}

#[test]
fn edge_unique_constraint_survives_map_replacement() {
    let mut conn = connection();
    conn.execute("CREATE (:Person)-[:KNOWS {weight: 1}]->(:Person)")
        .expect("first edge");
    conn.execute("CREATE (:Person)-[:KNOWS {weight: 2}]->(:Person)")
        .expect("second edge");
    conn.create_edge_index("weight", Some("KNOWS"), true)
        .expect("index");
    conn.execute("MATCH ()-[r:KNOWS {weight: 2}]->() SET r = {weight: 3}")
        .expect("replace");
    let rows = query(
        &mut conn,
        "MATCH ()-[r:KNOWS]->() RETURN r.weight ORDER BY r.weight",
    );
    assert_eq!(rows, vec![vec!["1"], vec!["3"]]);
    let err = conn
        .execute("MATCH ()-[r:KNOWS {weight: 3}]->() SET r = {weight: 1}")
        .expect_err("duplicate");
    assert_eq!(err.code(), grafile::ErrorCode::Constraint);
}

#[test]
fn ddl_creates_and_drops_indexes() {
    let mut conn = connection();
    conn.execute("CREATE (:Person {name: 'alice'})")
        .expect("seed");
    conn.execute("CREATE INDEX person_name IF NOT EXISTS FOR (n:Person) ON (n.name)")
        .expect("create");
    let plan = conn
        .explain("MATCH (n:Person {name: 'alice'}) RETURN n")
        .expect("explain");
    assert!(!plan.contains("seek=-"), "{plan}");
    let schema = conn.schema();
    assert_eq!(schema.node_indexes.len(), 1);
    assert_eq!(schema.node_indexes[0].name.as_deref(), Some("person_name"));
    // IF NOT EXISTS tolerates the existing name; a plain CREATE does not.
    conn.execute("CREATE INDEX person_name IF NOT EXISTS FOR (n:Person) ON (n.name)")
        .expect("again");
    let err = conn
        .execute("CREATE INDEX person_name FOR (n:Person) ON (n.name)")
        .expect_err("duplicate");
    assert_eq!(err.code(), grafile::ErrorCode::Exists);
    conn.execute("DROP INDEX person_name").expect("drop");
    assert!(conn.schema().node_indexes.is_empty());
    let err = conn.execute("DROP INDEX person_name").expect_err("missing");
    assert_eq!(err.code(), grafile::ErrorCode::NotFound);
    conn.execute("DROP INDEX person_name IF EXISTS")
        .expect("tolerated");
}

#[test]
fn ddl_unique_constraint_is_enforced_and_backfill_is_atomic() {
    let mut conn = connection();
    conn.execute("CREATE (:Person {email: 'a@x'}), (:Person {email: 'a@x'})")
        .expect("seed");
    // Backfilling a unique constraint over duplicates fails and leaves no
    // schema change behind.
    let err = conn
        .execute("CREATE CONSTRAINT person_email FOR (n:Person) REQUIRE n.email IS UNIQUE")
        .expect_err("duplicate");
    assert_eq!(err.code(), grafile::ErrorCode::Constraint);
    assert!(conn.schema().node_indexes.is_empty());
    conn.execute("MATCH (n:Person) DETACH DELETE n")
        .expect("clean");
    conn.execute("CREATE CONSTRAINT person_email FOR (n:Person) REQUIRE n.email IS UNIQUE")
        .expect("constraint");
    conn.execute("CREATE (:Person {email: 'b@x'})").expect("ok");
    let err = conn
        .execute("CREATE (:Person {email: 'b@x'})")
        .expect_err("duplicate");
    assert_eq!(err.code(), grafile::ErrorCode::Constraint);
    // Constraint-backed names are dropped through DROP CONSTRAINT.
    let err = conn
        .execute("DROP INDEX person_email")
        .expect_err("is constraint");
    assert_eq!(err.code(), grafile::ErrorCode::Misuse);
    conn.execute("DROP CONSTRAINT person_email").expect("drop");
    assert!(conn.schema().node_indexes.is_empty());
}

#[test]
fn ddl_composite_and_edge_forms() {
    let mut conn = connection();
    conn.execute("CREATE (:Person {first: 'a', last: 'b'})")
        .expect("seed");
    conn.execute("CREATE INDEX person_name FOR (n:Person) ON (n.first, n.last)")
        .expect("composite");
    // Composite indexes are not seekable yet.
    let plan = conn
        .explain("MATCH (n:Person {first: 'a'}) RETURN n")
        .expect("explain");
    assert!(plan.contains("seek=-"), "{plan}");
    conn.execute("CREATE INDEX knows_weight FOR ()-[r:KNOWS]-() ON (r.weight)")
        .expect("edge index");
    let schema = conn.schema();
    assert_eq!(schema.edge_indexes.len(), 1);
    assert_eq!(schema.edge_indexes[0].name.as_deref(), Some("knows_weight"));
    conn.execute("DROP INDEX knows_weight").expect("drop edge");
    assert!(conn.schema().edge_indexes.is_empty());
    let err = conn
        .execute("DROP CONSTRAINT person_name")
        .expect_err("is index");
    assert_eq!(err.code(), grafile::ErrorCode::Misuse);
    conn.execute("DROP INDEX person_name")
        .expect("drop composite");
    assert!(conn.schema().node_indexes.is_empty());
}

#[test]
fn not_null_constraints_are_enforced() {
    let mut conn = connection();
    conn.execute("CREATE CONSTRAINT person_name FOR (n:Person) REQUIRE n.name IS NOT NULL")
        .expect("constraint");
    conn.execute("CREATE (:Person {name: 'alice'})")
        .expect("create ok");
    let err = conn.execute("CREATE (:Person)").expect_err("missing");
    assert_eq!(err.code(), grafile::ErrorCode::Constraint);
    // Property removal in all its spellings.
    let err = conn
        .execute("MATCH (n:Person {name: 'alice'}) SET n.name = null")
        .expect_err("set null");
    assert_eq!(err.code(), grafile::ErrorCode::Constraint);
    let err = conn
        .execute("MATCH (n:Person {name: 'alice'}) REMOVE n.name")
        .expect_err("remove");
    assert_eq!(err.code(), grafile::ErrorCode::Constraint);
    let err = conn
        .execute("MATCH (n:Person {name: 'alice'}) SET n = {other: 1}")
        .expect_err("replace");
    assert_eq!(err.code(), grafile::ErrorCode::Constraint);
    let err = conn
        .execute("MATCH (n:Person {name: 'alice'}) SET n += {name: null}")
        .expect_err("merge null");
    assert_eq!(err.code(), grafile::ErrorCode::Constraint);
    // Adding the label later still enforces existence.
    conn.execute("CREATE (:Other)").expect("other");
    let err = conn
        .execute("MATCH (n:Other) SET n:Person")
        .expect_err("label add");
    assert_eq!(err.code(), grafile::ErrorCode::Constraint);
    conn.execute("CREATE (:Other {name: 'bob'})")
        .expect("other with name");
    conn.execute("MATCH (n:Other {name: 'bob'}) SET n:Person")
        .expect("label add ok");
    // Dropping the constraint lifts the rule.
    conn.execute("DROP CONSTRAINT person_name").expect("drop");
    conn.execute("CREATE (:Person)").expect("now allowed");
}

#[test]
fn edge_not_null_constraints_are_enforced() {
    let mut conn = connection();
    conn.execute("CREATE CONSTRAINT knows_weight FOR ()-[r:KNOWS]-() REQUIRE r.weight IS NOT NULL")
        .expect("constraint");
    conn.execute("CREATE (a:N)-[:KNOWS {weight: 1}]->(b:N)")
        .expect("create ok");
    let err = conn
        .execute("CREATE (a:N)-[:KNOWS]->(b:N)")
        .expect_err("missing");
    assert_eq!(err.code(), grafile::ErrorCode::Constraint);
    let err = conn
        .execute("MATCH ()-[r:KNOWS]->() SET r.weight = null")
        .expect_err("set null");
    assert_eq!(err.code(), grafile::ErrorCode::Constraint);
    let err = conn
        .execute("MATCH ()-[r:KNOWS]->() SET r = {}")
        .expect_err("replace");
    assert_eq!(err.code(), grafile::ErrorCode::Constraint);
}

#[test]
fn node_key_constraints_check_existence_and_uniqueness() {
    let mut conn = connection();
    conn.execute(
        "CREATE CONSTRAINT person_key FOR (n:Person) REQUIRE (n.first, n.last) IS NODE KEY",
    )
    .expect("constraint");
    conn.execute("CREATE (:Person {first: 'a', last: 'b'})")
        .expect("create ok");
    let err = conn
        .execute("CREATE (:Person {first: 'a'})")
        .expect_err("missing key");
    assert_eq!(err.code(), grafile::ErrorCode::Constraint);
    let err = conn
        .execute("CREATE (:Person {first: 'a', last: 'b'})")
        .expect_err("duplicate");
    assert_eq!(err.code(), grafile::ErrorCode::Constraint);
    conn.execute("CREATE (:Person {first: 'a', last: 'c'})")
        .expect("distinct ok");
    let err = conn
        .execute("MATCH (n:Person {last: 'c'}) SET n.last = 'b'")
        .expect_err("collision");
    assert_eq!(err.code(), grafile::ErrorCode::Constraint);
    let err = conn
        .execute("MATCH (n:Person {last: 'c'}) SET n.first = null")
        .expect_err("removal");
    assert_eq!(err.code(), grafile::ErrorCode::Constraint);
}

#[test]
fn label_expressions_do_not_seek_indexes() {
    let mut conn = connection();
    conn.execute("CREATE (:A {name: 'x'}), (:B {name: 'x'})")
        .expect("create");
    conn.create_node_index("name", Some("A"), false)
        .expect("index");
    // No label is guaranteed, so the literal property cannot use the index.
    let plan = conn
        .explain("MATCH (n:A|B {name: 'x'}) RETURN n")
        .expect("explain");
    assert!(plan.contains("seek=-"), "{plan}");
    // A conjunction guarantees both labels, so the index still applies.
    let plan = conn
        .explain("MATCH (n:A&:B {name: 'x'}) RETURN n")
        .expect("explain");
    assert!(!plan.contains("seek=-"), "{plan}");
}

#[test]
fn literal_pattern_properties_still_seek_indexes() {
    let mut conn = connection();
    conn.execute("CREATE (:Person {seed: 1})").expect("create");
    conn.create_node_index("seed", Some("Person"), false)
        .expect("index");
    let plan = conn
        .explain("MATCH (n:Person {seed: 1}) RETURN n")
        .expect("explain");
    assert!(!plan.contains("seek=-"), "{plan}");
    let plan = conn
        .explain("WITH 1 AS s MATCH (n:Person {seed: s}) RETURN n")
        .expect("explain");
    assert!(plan.contains("seek=-"), "{plan}");
}
