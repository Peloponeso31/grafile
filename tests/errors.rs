//! Language-level rejections: undefined names, invalid clauses, unsupported
//! DDL and malformed patterns.

mod support;

use grafile::api::Params;
use support::{connection, query, seed};

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
fn ddl_is_rejected_where_unsupported() {
    let mut conn = connection();
    let err = conn.execute("SHOW INDEXES").expect_err("show");
    assert_eq!(err.code(), grafile::ErrorCode::Unsupported);
    let err = conn
        .execute("CREATE INDEX ON :Person(name)")
        .expect_err("old form");
    assert_eq!(err.code(), grafile::ErrorCode::Syntax);
    let err = conn
        .execute("CREATE CONSTRAINT ON (n:Person) ASSERT n.name IS UNIQUE")
        .expect_err("old form");
    assert_eq!(err.code(), grafile::ErrorCode::Syntax);
}

#[test]
fn label_expressions_are_rejected_in_writes() {
    let mut conn = connection();
    let err = conn.execute("CREATE (n:A|B)").expect_err("create");
    assert_eq!(err.code(), grafile::ErrorCode::Syntax);
    let err = conn
        .execute("CREATE ()-[:X|Y]->()")
        .expect_err("create edge");
    assert_eq!(err.code(), grafile::ErrorCode::Syntax);
    let err = conn.execute("MERGE (n:A|B)").expect_err("merge");
    assert_eq!(err.code(), grafile::ErrorCode::Syntax);
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
fn aggregations_inside_comprehensions_are_rejected() {
    let mut conn = connection();
    let err = conn
        .query("RETURN [x IN [1] | count(*)]", &Params::new())
        .expect_err("aggregation in comprehension");
    assert_eq!(err.code(), grafile::ErrorCode::Syntax);
}

#[test]
fn static_type_errors_are_compile_time() {
    let mut conn = connection();
    for (source, code) in [
        ("RETURN 1 AND true", grafile::ErrorCode::Syntax),
        ("RETURN NOT 'x'", grafile::ErrorCode::Syntax),
        ("RETURN 1 IN 3", grafile::ErrorCode::Syntax),
        ("RETURN labels('x')", grafile::ErrorCode::Syntax),
        ("RETURN type(1)", grafile::ErrorCode::Syntax),
        ("RETURN type(2)", grafile::ErrorCode::Syntax),
        ("RETURN properties(1)", grafile::ErrorCode::Syntax),
        ("RETURN length(1)", grafile::ErrorCode::Syntax),
        ("RETURN size(1)", grafile::ErrorCode::Syntax),
        ("RETURN 'x'.name", grafile::ErrorCode::Type),
        ("RETURN [1, 2].name", grafile::ErrorCode::Type),
        (
            "RETURN all(x IN [1] WHERE x + 1)",
            grafile::ErrorCode::Syntax,
        ),
    ] {
        let err = conn.query(source, &Params::new()).expect_err(source);
        assert_eq!(err.code(), code, "{source}");
    }
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
fn rebinding_a_relationship_in_the_same_pattern_is_rejected() {
    let mut conn = connection();
    // The same relationship variable cannot appear twice in one pattern.
    let err = conn
        .query("MATCH (a)-[r]->(b)-[r]->(c) RETURN r", &Params::new())
        .expect_err("relationship rebound");
    assert_eq!(err.code(), grafile::ErrorCode::Syntax);
    // Across MATCH clauses (or through WITH) it constrains the pattern.
    conn.execute("CREATE (:A)-[:T]->(:B)").expect("create");
    let rows = query(
        &mut conn,
        "MATCH ()-[r]->() WITH r MATCH (a)-[r]->(b) RETURN a IS NOT NULL AS ok",
    );
    assert_eq!(rows, vec![vec!["true"]]);
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
fn bare_pattern_in_projection_is_rejected() {
    let mut conn = connection();
    seed(&mut conn);
    let err = conn
        .query("MATCH (n) RETURN (n)-[:KNOWS]->()", &Params::new())
        .expect_err("bare pattern projection must be rejected");
    assert!(err.to_string().contains("predicates"), "{err}");
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
fn foreach_rejects_non_updating_clauses() {
    let mut conn = connection();
    seed(&mut conn);
    let err = conn
        .query("FOREACH (x IN [1] | MATCH (n) RETURN n)", &Params::new())
        .expect_err("non-updating clause must be rejected");
    assert!(err.to_string().contains("updating clauses"), "{err}");
}
