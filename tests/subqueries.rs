//! `CALL { ... }` subqueries: correlated imports, returning joins and writes.

mod support;

use grafile::api::Params;
use grafile::record::Value;
use support::{connection, query, rows_to_strings};

#[test]
fn unit_subquery_runs_once_per_input_row() {
    let mut conn = connection();
    let rows = query(
        &mut conn,
        "UNWIND [1, 2, 3] AS i \
         CALL { WITH i CREATE (:N {id: i}) } \
         RETURN count(*) AS c",
    );
    assert_eq!(rows, vec![vec!["3"]]);
    let rows = query(&mut conn, "MATCH (n:N) RETURN n.id ORDER BY n.id");
    assert_eq!(rows, vec![vec!["1"], vec!["2"], vec!["3"]]);
}

#[test]
fn returning_subquery_joins_its_rows() {
    let mut conn = connection();
    let rows = query(
        &mut conn,
        "UNWIND [1, 2] AS i \
         CALL { WITH i UNWIND [10, 20] AS j RETURN i, i * 100 + j AS v } \
         RETURN i, v ORDER BY i, v",
    );
    assert_eq!(
        rows,
        vec![
            vec!["1", "110"],
            vec!["1", "120"],
            vec!["2", "210"],
            vec!["2", "220"],
        ]
    );
}

#[test]
fn uncorrelated_subquery_runs_once() {
    let mut conn = connection();
    let rows = query(&mut conn, "CALL { RETURN 1 AS x } RETURN x");
    assert_eq!(rows, vec![vec!["1"]]);
    let rows = query(&mut conn, "CALL { RETURN 42 AS answer }");
    assert_eq!(rows, vec![vec!["42"]]);
}

#[test]
fn unit_subquery_updates_after_an_aggregation() {
    let mut conn = connection();
    conn.execute(
        "CREATE (p1:Product {id: 1}), (p2:Product {id: 2}), \
         (r1:Review {rating: 5}), (r2:Review {rating: 3}), \
         (r1)-[:REVIEWS]->(p1), (r2)-[:REVIEWS]->(p1), (r2)-[:REVIEWS]->(p2)",
    )
    .expect("seed");
    let rows = query(
        &mut conn,
        "MATCH (p:Product)<-[:REVIEWS]-(r:Review) \
         WITH p, avg(r.rating) AS avgRating, count(r) AS nReviews \
         CALL { WITH p, avgRating, nReviews \
                SET p.avgRating = round(avgRating * 100) / 100, p.reviewCount = nReviews } \
         RETURN p.id AS id, p.avgRating AS rating, p.reviewCount AS n ORDER BY id",
    );
    assert_eq!(rows, vec![vec!["1", "4", "2"], vec!["2", "3", "1"]]);
}

#[test]
fn unit_subquery_can_filter_its_input() {
    let mut conn = connection();
    let rows = query(
        &mut conn,
        "UNWIND [1, 2, 3] AS i \
         CALL { WITH i WHERE i > 1 CREATE (:Filtered {id: i}) } \
         RETURN count(*) AS c",
    );
    assert_eq!(rows, vec![vec!["3"]]);
    let rows = query(&mut conn, "MATCH (n:Filtered) RETURN n.id ORDER BY n.id");
    assert_eq!(rows, vec![vec!["2"], vec!["3"]]);
}

#[test]
fn scope_clause_imports_only_listed_variables() {
    let mut conn = connection();
    let rows = query(
        &mut conn,
        "WITH 1 AS i, 2 AS hidden \
         CALL (i) { RETURN i * 10 AS x } \
         RETURN x",
    );
    assert_eq!(rows, vec![vec!["10"]]);
    let err = conn
        .query(
            "WITH 1 AS i, 2 AS hidden CALL (i) { RETURN hidden AS x } RETURN x",
            &Params::new(),
        )
        .expect_err("hidden must not be visible");
    assert!(err.to_string().contains("not defined"), "{err}");
}

#[test]
fn unit_subquery_passes_rows_when_inner_has_no_rows() {
    let mut conn = connection();
    let rows = query(
        &mut conn,
        "UNWIND [1, 2] AS i \
         CALL { WITH i MATCH (missing:Never) CREATE (:Never) } \
         RETURN count(*) AS c",
    );
    assert_eq!(rows, vec![vec!["2"]]);
}

#[test]
fn unit_subquery_can_delete_correlated_entities() {
    let mut conn = connection();
    conn.execute("CREATE (:X {id: 1}), (:X {id: 2})")
        .expect("seed");
    let rows = query(
        &mut conn,
        "MATCH (n:X) CALL { WITH n DETACH DELETE n } RETURN count(*) AS c",
    );
    assert_eq!(rows, vec![vec!["2"]]);
    let rows = query(&mut conn, "MATCH (n:X) RETURN count(*) AS c");
    assert_eq!(rows, vec![vec!["0"]]);
}

#[test]
fn returning_subquery_exports_node_values() {
    let mut conn = connection();
    let rows = query(
        &mut conn,
        "UNWIND [1, 2] AS i \
         CALL { WITH i CREATE (n:N {id: i}) RETURN n } \
         RETURN n.id ORDER BY n.id",
    );
    assert_eq!(rows, vec![vec!["1"], vec!["2"]]);
}

#[test]
fn returning_subquery_aggregates_per_input_row() {
    let mut conn = connection();
    let rows = query(
        &mut conn,
        "UNWIND [1, 2] AS i \
         CALL { WITH i UNWIND [1, 2, 3] AS j RETURN i, count(j) AS c } \
         RETURN i, c ORDER BY i",
    );
    assert_eq!(rows, vec![vec!["1", "3"], vec!["2", "3"]]);
}

#[test]
fn parameters_inside_subqueries_are_substituted() {
    let mut conn = connection();
    let mut params = Params::new();
    params.insert("x", Value::Int(10));
    let mut rows = conn
        .query(
            "UNWIND [1, 2] AS i CALL { WITH i RETURN $x + i AS v } RETURN v ORDER BY v",
            &params,
        )
        .expect("query");
    assert_eq!(rows_to_strings(&mut rows), vec![vec!["11"], vec!["12"]]);
}

#[test]
fn unit_subquery_must_end_with_an_update() {
    let mut conn = connection();
    let err = conn
        .query("UNWIND [1] AS i CALL { WITH i } RETURN i", &Params::new())
        .expect_err("read-only unit subquery");
    assert!(err.to_string().contains("updating clause"), "{err}");
}

#[test]
fn nested_call_subqueries_are_rejected() {
    let mut conn = connection();
    let err = conn
        .query(
            "CALL { CALL { RETURN 1 AS x } RETURN x } RETURN x",
            &Params::new(),
        )
        .expect_err("nested subquery");
    assert!(err.to_string().contains("nested"), "{err}");
}

#[test]
fn batch_transactions_keep_earlier_batches_on_failure() {
    let mut conn = connection();
    conn.execute("CREATE CONSTRAINT batch_id IF NOT EXISTS FOR (n:Batch) REQUIRE n.id IS UNIQUE")
        .expect("constraint");
    conn.execute("CREATE (:Batch {id: 3})").expect("seed");
    let err = conn
        .query(
            "UNWIND [1, 2, 3] AS i \
             CALL { WITH i CREATE (:Batch {id: i}) } IN TRANSACTIONS OF 1 ROWS",
            &Params::new(),
        )
        .expect_err("third batch violates the constraint");
    assert!(err.to_string().contains("Constraint"), "{err}");
    let rows = query(&mut conn, "MATCH (n:Batch) RETURN n.id ORDER BY n.id");
    assert_eq!(rows, vec![vec!["1"], vec!["2"], vec!["3"]]);
}

#[test]
fn batch_transactions_finish_the_last_partial_batch() {
    let mut conn = connection();
    let rows = query(
        &mut conn,
        "UNWIND [1, 2, 3, 4, 5] AS i \
         CALL { WITH i CREATE (:B {id: i}) } IN TRANSACTIONS OF 2 ROWS \
         RETURN count(*) AS c",
    );
    assert_eq!(rows, vec![vec!["5"]]);
    let rows = query(&mut conn, "MATCH (n:B) RETURN n.id ORDER BY n.id");
    assert_eq!(
        rows,
        vec![vec!["1"], vec!["2"], vec!["3"], vec!["4"], vec!["5"],]
    );
}

#[test]
fn batch_transactions_accept_the_default_batch_size() {
    let mut conn = connection();
    let rows = query(
        &mut conn,
        "UNWIND [1, 2] AS i \
         CALL { WITH i CREATE (:D {id: i}) } IN TRANSACTIONS \
         RETURN count(*) AS c",
    );
    assert_eq!(rows, vec![vec!["2"]]);
    let rows = query(&mut conn, "MATCH (n:D) RETURN count(*) AS c");
    assert_eq!(rows, vec![vec!["2"]]);
}

#[test]
fn batch_transactions_reject_explicit_transactions() {
    let mut conn = connection();
    conn.begin().expect("begin");
    let err = conn
        .query(
            "UNWIND [1] AS i CALL { WITH i CREATE (:X) } IN TRANSACTIONS OF 1 ROWS",
            &Params::new(),
        )
        .expect_err("explicit transaction");
    assert!(err.to_string().contains("explicit transaction"), "{err}");
    conn.rollback().expect("rollback");
    let rows = query(&mut conn, "MATCH (n:X) RETURN count(*) AS c");
    assert_eq!(rows, vec![vec!["0"]]);
}

#[test]
fn batch_transactions_validate_their_shape() {
    let mut conn = connection();
    let err = conn
        .query(
            "UNWIND [1] AS i CALL { WITH i RETURN i } IN TRANSACTIONS OF 1 ROWS",
            &Params::new(),
        )
        .expect_err("returning subquery");
    assert!(err.to_string().contains("cannot return rows"), "{err}");
    let err = conn
        .query(
            "UNWIND [1] AS i CALL { WITH i CREATE (:X) } IN TRANSACTIONS OF 0 ROWS",
            &Params::new(),
        )
        .expect_err("zero batch size");
    assert!(err.to_string().contains("positive row count"), "{err}");
    let err = conn
        .query(
            "UNWIND [1] AS i \
             CALL { WITH i CREATE (:A {id: i}) } IN TRANSACTIONS OF 1 ROWS \
             CALL { WITH i CREATE (:B {id: i}) } IN TRANSACTIONS OF 1 ROWS",
            &Params::new(),
        )
        .expect_err("two batched subqueries");
    assert!(err.to_string().contains("only one"), "{err}");
}

#[test]
fn batch_transactions_delete_while_scanning() {
    let mut conn = connection();
    conn.execute("CREATE (:W {id: 1}), (:W {id: 2}), (:W {id: 3}), (:W {id: 4}), (:W {id: 5})")
        .expect("seed");
    let rows = query(
        &mut conn,
        "MATCH (n:W) CALL { WITH n DETACH DELETE n } IN TRANSACTIONS OF 2 ROWS \
         RETURN count(*) AS c",
    );
    assert_eq!(rows, vec![vec!["5"]]);
    let rows = query(&mut conn, "MATCH (n:W) RETURN count(*) AS c");
    assert_eq!(rows, vec![vec!["0"]]);
}

#[test]
fn batch_transactions_run_the_seed_shape() {
    let mut conn = connection();
    conn.execute("CREATE (:Country {id: 1}), (:Country {id: 2})")
        .expect("seed");
    let rows = query(
        &mut conn,
        "UNWIND range(1, 5) AS i \
         CALL { \
           WITH i \
           MATCH (co:Country {id: 1 + (i % 2)}) \
           CREATE (c:City {id: i}) \
           CREATE (c)-[:IN_COUNTRY]->(co) \
         } IN TRANSACTIONS OF 2 ROWS \
         RETURN count(*) AS c",
    );
    assert_eq!(rows, vec![vec!["5"]]);
    let rows = query(
        &mut conn,
        "MATCH (c:City)-[:IN_COUNTRY]->(co:Country) RETURN count(*) AS c",
    );
    assert_eq!(rows, vec![vec!["5"]]);
}

#[test]
fn batch_transactions_update_after_an_aggregation() {
    let mut conn = connection();
    conn.execute(
        "CREATE (p1:Product {id: 1}), (p2:Product {id: 2}), \
         (r1:Review {rating: 5}), (r2:Review {rating: 3}), \
         (r1)-[:REVIEWS]->(p1), (r2)-[:REVIEWS]->(p1), (r2)-[:REVIEWS]->(p2)",
    )
    .expect("seed");
    let rows = query(
        &mut conn,
        "MATCH (p:Product)<-[:REVIEWS]-(r:Review) \
         WITH p, avg(r.rating) AS avgRating, count(r) AS nReviews \
         CALL { WITH p, avgRating, nReviews \
                SET p.avgRating = round(avgRating * 100) / 100, p.reviewCount = nReviews } \
         IN TRANSACTIONS OF 1 ROWS \
         RETURN p.id AS id, p.avgRating AS rating ORDER BY id",
    );
    assert_eq!(rows, vec![vec!["1", "4"], vec!["2", "3"]]);
    let rows = query(
        &mut conn,
        "MATCH (p:Product) RETURN p.id AS id, p.reviewCount AS n ORDER BY id",
    );
    assert_eq!(rows, vec![vec!["1", "2"], vec!["2", "1"]]);
}
