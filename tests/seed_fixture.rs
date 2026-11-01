//! Phase 6 acceptance: the audited Neo4j 5 seed script at 1/100 scale runs
//! end to end through the statement splitter.
//!
//! The script takes about 20 s in a debug build (under 2 s in release), so it
//! is ignored by default and CI runs it with
//! `cargo test --release --test seed_fixture -- --ignored`.

mod support;

use grafile::cypher::split_statements;
use support::{connection, query};

const SEED: &str = include_str!("fixtures/seed_1_100.cypher");

fn count(conn: &mut grafile::api::Connection, source: &str) -> i64 {
    query(conn, source)[0][0].parse().expect("integer count")
}

#[test]
#[ignore = "slow in debug builds; CI runs it in release mode with --ignored"]
fn seed_script_loads_at_one_hundredth_scale() {
    let mut conn = connection();
    for (index, statement) in split_statements(SEED).iter().enumerate() {
        conn.query(statement, &grafile::api::Params::new())
            .unwrap_or_else(|err| panic!("statement {} failed: {err}\n{statement}", index + 1));
    }

    let nodes = [
        ("Country", 20),
        ("City", 20),
        ("Category", 1332),
        ("Brand", 20),
        ("Company", 50),
        ("Tag", 50),
        ("Skill", 3),
        ("User", 2000),
        ("Product", 1000),
        ("Order", 6000),
        ("Review", 4000),
        ("Post", 3000),
        ("Comment", 6000),
        ("Group", 50),
        ("Event", 300),
    ];
    for (label, expected) in nodes {
        let actual = count(&mut conn, &format!("MATCH (n:{label}) RETURN count(n)"));
        assert_eq!(actual, expected, "{label} nodes");
    }
    let total: i64 = nodes.iter().map(|(_, expected)| expected).sum();
    assert_eq!(count(&mut conn, "MATCH (n) RETURN count(n)"), total);

    let fixed = [
        ("IN_COUNTRY", 20),
        ("SUBCATEGORY_OF", 1320),
        ("LOCATED_IN", 50),
        ("LIVES_IN", 2000),
        ("IN_CATEGORY", 1000),
        ("MADE_BY", 1000),
        ("SOLD_BY", 1000),
        ("PLACED", 6000),
        ("SHIPPED_TO", 6000),
        ("WROTE", 10000),
        ("REVIEWS", 4000),
        ("AUTHORED", 3000),
        ("ON", 6000),
        ("FOUNDED", 50),
        ("BASED_IN", 50),
        ("HOSTS", 300),
        ("HELD_IN", 300),
        ("FOLLOWS", 15000),
        ("FRIEND_OF", 6000),
        ("LIKES", 15000),
        ("WISHLISTED", 4000),
        ("SIMILAR_TO", 3000),
        ("MEMBER_OF", 5000),
        ("RSVP", 4000),
    ];
    for (edge_type, expected) in fixed {
        let actual = count(
            &mut conn,
            &format!("MATCH ()-[r:{edge_type}]->() RETURN count(r)"),
        );
        assert_eq!(actual, expected, "{edge_type} relationships");
    }
    // Fan-out drawn with rand(): bounds follow from the script's ranges.
    let random = [
        ("CONTAINS", 6000, 30000),
        ("TAGGED", 3000, 9000),
        ("HAS_SKILL", 2000, 10000),
        ("INTERESTED_IN", 0, 10000),
        ("WORKS_AT", 1, 1999),
        ("REPLY_TO", 1, 5998),
    ];
    for (edge_type, low, high) in random {
        let actual = count(
            &mut conn,
            &format!("MATCH ()-[r:{edge_type}]->() RETURN count(r)"),
        );
        assert!((low..=high).contains(&actual), "{edge_type}: {actual}");
    }

    assert_eq!(
        count(&mut conn, "MATCH (u:User) RETURN count(DISTINCT u.email)"),
        2000
    );
    assert_eq!(
        count(
            &mut conn,
            "MATCH (u:User) WHERE u.orderCount > 0 RETURN count(u)"
        ),
        count(
            &mut conn,
            "MATCH (u:User)-[:PLACED]->() RETURN count(DISTINCT u)"
        )
    );

    let rows = query(
        &mut conn,
        "CALL db.index.fulltext.queryNodes('post_text_ft', 'graphs') YIELD node, score \
         RETURN node.text, score LIMIT 10",
    );
    assert_eq!(rows.len(), 10);
    let scores: Vec<f64> = rows
        .iter()
        .map(|row| {
            assert!(row[0].contains("graphs"), "{row:?}");
            row[1].parse().expect("float score")
        })
        .collect();
    assert!(
        scores.windows(2).all(|pair| pair[0] >= pair[1]),
        "{scores:?}"
    );
    assert_eq!(
        count(
            &mut conn,
            "CALL db.index.fulltext.queryNodes('product_text_ft', '\"synthetic product\"') \
             YIELD node RETURN count(node)"
        ),
        1000
    );

    let report = conn.database_mut().check_integrity().expect("check");
    assert!(report.is_clean(), "{report}");
    assert_eq!(report.stats().nodes, total as u64);
}
