//! End-to-end Cypher tests: parser -> planner -> codegen -> VDBE -> engine.

use std::path::Path;
use std::sync::Arc;

use grafile::api::{Connection, Params};
use grafile::record::Value;
use grafile::vfs::mem::MemVfs;

fn connection() -> Connection {
    Connection::create_with_vfs(Arc::new(MemVfs::new()), Path::new("test.db"), 4096)
        .expect("create connection")
}

fn rows_to_strings(rows: &mut grafile::api::Rows) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    while let Some(row) = rows.next_row() {
        out.push(row.iter().map(render).collect());
    }
    out
}

fn render(value: &Value) -> String {
    match value {
        Value::Null => "null".to_owned(),
        Value::Text(text) => text.to_string(),
        Value::Int(value) => value.to_string(),
        Value::Float(value) => format!("{value}"),
        Value::Bool(value) => value.to_string(),
        other => grafile::vdbe::format_value(other),
    }
}

fn query(conn: &mut Connection, source: &str) -> Vec<Vec<String>> {
    let mut rows = conn
        .query(source, &Params::new())
        .unwrap_or_else(|err| panic!("query failed: {err}\n{source}"));
    rows_to_strings(&mut rows)
}

fn seed(conn: &mut Connection) {
    conn.execute(
        "CREATE (alice:Person {name: 'alice', age: 30}), \
         (bob:Person {name: 'bob', age: 25}), \
         (carol:Person {name: 'carol', age: 35}), \
         (alice)-[:KNOWS {since: 2020}]->(bob), \
         (bob)-[:KNOWS {since: 2021}]->(carol), \
         (alice)-[:LIKES]->(carol)",
    )
    .expect("seed");
}

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
fn undefined_variable_is_rejected() {
    let mut conn = connection();
    let err = conn
        .query("MATCH (n) RETURN m", &Params::new())
        .expect_err("undefined");
    assert!(err.message().contains("not defined"), "{err}");
}

#[test]
fn missing_parameter_is_rejected() {
    let mut conn = connection();
    let err = conn
        .query("MATCH (n) WHERE n.age > $age RETURN n", &Params::new())
        .expect_err("parameter");
    assert!(err.message().contains("$age"), "{err}");
}

#[test]
fn persistence_across_reopen() {
    let store = grafile::vfs::mem::new_store();
    {
        let vfs = Arc::new(MemVfs::from_store(store.clone()));
        let mut conn =
            Connection::create_with_vfs(vfs, Path::new("persist.db"), 4096).expect("create");
        seed(&mut conn);
    }
    let vfs = Arc::new(MemVfs::from_store(store));
    let mut conn = Connection::open_with_vfs(
        vfs,
        Path::new("persist.db"),
        grafile::vfs::OpenMode::ReadWrite,
    )
    .expect("reopen");
    let rows = query(&mut conn, "MATCH (n:Person) RETURN count(*)");
    assert_eq!(rows, vec![vec!["3"]]);
    let rows = query(
        &mut conn,
        "MATCH (:Person)-[:KNOWS]->(:Person) RETURN count(*)",
    );
    assert_eq!(rows, vec![vec!["2"]]);
}

#[test]
fn explicit_transaction_rollback() {
    let mut conn = connection();
    conn.begin().expect("begin");
    conn.execute("CREATE (:Person {name: 'temp'})")
        .expect("create");
    conn.rollback().expect("rollback");
    let rows = query(&mut conn, "MATCH (n:Person) RETURN count(*)");
    assert_eq!(rows, vec![vec!["0"]]);
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
