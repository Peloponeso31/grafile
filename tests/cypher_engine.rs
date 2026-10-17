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

#[test]
fn second_connection_sees_committed_changes() {
    let store = grafile::vfs::mem::new_store();
    let mut writer = Connection::create_with_vfs(
        Arc::new(MemVfs::from_store(store.clone())),
        Path::new("two-conn.db"),
        4096,
    )
    .expect("create");
    writer
        .execute("CREATE (:Person {name: 'alice'})")
        .expect("seed");

    let mut reader = Connection::open_with_vfs(
        Arc::new(MemVfs::from_store(store)),
        Path::new("two-conn.db"),
        grafile::vfs::OpenMode::ReadWrite,
    )
    .expect("open");
    let rows = query(&mut reader, "MATCH (n:Person) RETURN count(*) AS n");
    assert_eq!(rows, vec![vec!["1"]]);

    // The writer commits another node; the reader's next statement sees it
    // (its page cache is invalidated by the change counter).
    writer
        .execute("CREATE (:Person {name: 'bob'})")
        .expect("seed");
    let rows = query(&mut reader, "MATCH (n:Person) RETURN count(*) AS n");
    assert_eq!(rows, vec![vec!["2"]]);
}

// ---- OPTIONAL MATCH ----

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

// ---- pre-bound pattern elements ----

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

// ---- column naming ----

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

// ---- UNION ----

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
    assert_eq!(values, vec![vec!["-15", "6", "512", "-12"]]);
}

#[test]
fn union_rejects_different_columns() {
    let mut conn = connection();
    let err = conn
        .query("RETURN 1 AS a UNION RETURN 2 AS b", &Params::new())
        .expect_err("different columns");
    assert_eq!(err.code(), grafile::ErrorCode::Syntax);
}

#[test]
fn union_rejects_mixed_kinds() {
    let mut conn = connection();
    let err = conn
        .query(
            "RETURN 1 AS a UNION RETURN 2 AS a UNION ALL RETURN 3 AS a",
            &Params::new(),
        )
        .expect_err("mixed kinds");
    assert_eq!(err.code(), grafile::ErrorCode::Syntax);
}

// ---- SET += ----

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

// ---- aggregation completeness ----

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

// ---- maps, subscripts and slices ----

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
fn indexing_errors_are_type_errors() {
    let mut conn = connection();
    for query_text in ["RETURN 1[0]", "RETURN [1]['x']", "RETURN {a: 1}[0]"] {
        let err = conn
            .query(query_text, &Params::new())
            .expect_err("type error");
        assert_eq!(err.code(), grafile::ErrorCode::Type, "{query_text}");
    }
}

#[test]
fn list_concatenation_with_scalars() {
    let mut conn = connection();
    let rows = query(&mut conn, "RETURN [1, 2] + 3 AS a, 0 + [1, 2] AS b");
    assert_eq!(rows, vec![vec!["[1, 2, 3]", "[0, 1, 2]"]]);
}

// ---- comprehensions, quantifiers and reduce ----

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
fn aggregations_inside_comprehensions_are_rejected() {
    let mut conn = connection();
    let err = conn
        .query("RETURN [x IN [1] | count(*)]", &Params::new())
        .expect_err("aggregation in comprehension");
    assert_eq!(err.code(), grafile::ErrorCode::Syntax);
}

// ---- type conversions ----

#[test]
fn to_integer_truncates_and_parses() {
    let mut conn = connection();
    let rows = query(
        &mut conn,
        "WITH [82.9, 2, true, '2.9', 'foo', '', null] AS xs \
         RETURN [x IN xs | toInteger(x)] AS ints",
    );
    assert_eq!(rows, vec![vec!["[82, 2, 1, 2, null, null, null]"]]);
}

#[test]
fn to_float_converts_numbers_and_strings() {
    let mut conn = connection();
    let rows = query(
        &mut conn,
        "WITH [3.4, 3, '5', 'foo', null] AS xs RETURN [x IN xs | toFloat(x)] AS floats",
    );
    assert_eq!(rows, vec![vec!["[3.4, 3.0, 5.0, null, null]"]]);
}

#[test]
fn to_boolean_accepts_boolean_and_string() {
    let mut conn = connection();
    let rows = query(
        &mut conn,
        "WITH [true, 'TRUE', 'False', ' tru ', 'x', null] AS xs \
         RETURN [x IN xs | toBoolean(x)] AS bools",
    );
    assert_eq!(rows, vec![vec!["[true, true, false, null, null, null]"]]);
}

#[test]
fn to_string_renders_scalars() {
    let mut conn = connection();
    let rows = query(
        &mut conn,
        "WITH [42, 2.3, true, 'apa', null] AS xs RETURN [x IN xs | toString(x)] AS texts",
    );
    assert_eq!(rows, vec![vec!["[42, 2.3, true, apa, null]"]]);
}

#[test]
fn conversions_reject_incompatible_types() {
    let mut conn = connection();
    for source in [
        "RETURN toInteger([1])",
        "RETURN toFloat(true)",
        "RETURN toBoolean(1.0)",
        "RETURN toString([1, 2])",
        "RETURN toString({a: 1})",
    ] {
        let err = conn
            .query(source, &Params::new())
            .expect_err("conversion must reject the type");
        assert_eq!(err.code(), grafile::ErrorCode::Type, "{source}");
    }
}

// ---- variable-length paths ----

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

// ---- named paths ----

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

#[test]
fn rebinding_a_path_variable_is_rejected() {
    let mut conn = connection();
    let err = conn
        .query("MATCH p = ()-->() MATCH (p) RETURN p", &Params::new())
        .expect_err("path variable rebound");
    assert_eq!(err.code(), grafile::ErrorCode::Syntax);
}

#[test]
fn rebinding_a_relationship_in_the_same_section_is_rejected() {
    let mut conn = connection();
    let err = conn
        .query("MATCH ()-[r]->() MATCH ()-[r]->() RETURN r", &Params::new())
        .expect_err("relationship rebound");
    assert_eq!(err.code(), grafile::ErrorCode::Syntax);
    // Through WITH it constrains the pattern instead.
    conn.execute("CREATE (:A)-[:T]->(:B)").expect("create");
    let rows = query(
        &mut conn,
        "MATCH ()-[r]->() WITH r LIMIT 1 MATCH (a)-[r]->(b) RETURN a IS NOT NULL AS ok",
    );
    assert_eq!(rows, vec![vec!["true"]]);
}

// ---- non-literal pattern properties ----

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
fn merge_rejects_null_property_values() {
    let mut conn = connection();
    let err = conn
        .query("MERGE (n:Person {name: null})", &Params::new())
        .expect_err("null property must be rejected");
    assert!(err.to_string().contains("null"), "{err}");
}

#[test]
fn merge_rejects_bound_node_with_labels() {
    let mut conn = connection();
    seed(&mut conn);
    let err = conn
        .query(
            "MATCH (n:Person {name: 'alice'}) MERGE (n:Robot)",
            &Params::new(),
        )
        .expect_err("labels on a bound node must be rejected");
    assert!(err.to_string().contains("labels"), "{err}");
}

#[test]
fn merge_rejects_variable_length_pattern() {
    let mut conn = connection();
    let err = conn
        .query(
            "MERGE (:Person {name: 'quinn'})-[:KNOWS*1..2]->()",
            &Params::new(),
        )
        .expect_err("variable-length MERGE must be rejected");
    assert!(err.to_string().contains("variable-length"), "{err}");
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
fn bare_pattern_in_projection_is_rejected() {
    let mut conn = connection();
    seed(&mut conn);
    let err = conn
        .query("MATCH (n) RETURN (n)-[:KNOWS]->()", &Params::new())
        .expect_err("bare pattern projection must be rejected");
    assert!(err.to_string().contains("predicates"), "{err}");
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

#[test]
fn nested_pattern_expression_is_rejected() {
    let mut conn = connection();
    seed(&mut conn);
    let err = conn
        .query(
            "MATCH p = (n:Person)-[:KNOWS]->() \
             RETURN [x IN nodes(p) | size([(x)-->(:Person) | 1])]",
            &Params::new(),
        )
        .expect_err("nested pattern expressions must be rejected");
    assert!(err.to_string().contains("list comprehensions"), "{err}");
}

fn shortest_graph(conn: &mut Connection) {
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
fn call_rejects_unknown_procedures() {
    let mut conn = connection();
    let err = conn
        .query("CALL db.nope() YIELD x RETURN x", &Params::new())
        .expect_err("unknown procedure must be rejected");
    assert!(err.to_string().contains("not supported"), "{err}");
    let err = conn
        .query(
            "CALL db.labels() YIELD missing RETURN missing",
            &Params::new(),
        )
        .expect_err("unknown yield column must be rejected");
    assert!(err.to_string().contains("no column"), "{err}");
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

#[test]
fn foreach_rejects_non_updating_clauses() {
    let mut conn = connection();
    seed(&mut conn);
    let err = conn
        .query("FOREACH (x IN [1] | MATCH (n) RETURN n)", &Params::new())
        .expect_err("non-updating clause must be rejected");
    assert!(err.to_string().contains("updating clauses"), "{err}");
}
