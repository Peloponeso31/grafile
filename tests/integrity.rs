//! Integrity after failure paths: interrupted batches, rollback and reopen.

mod support;

use std::path::Path;
use std::sync::Arc;

use grafile::api::{Connection, Params};
use grafile::vfs::OpenMode;
use grafile::vfs::mem::{MemVfs, new_store};
use support::{connection, query};

fn assert_clean(conn: &mut Connection) {
    let report = conn.database_mut().check_integrity().expect("check");
    assert!(report.is_clean(), "{report}");
}

#[test]
fn failed_batch_leaves_committed_batches_consistent() {
    let mut conn = connection();
    conn.execute("CREATE CONSTRAINT b_id FOR (n:B) REQUIRE n.id IS UNIQUE")
        .expect("constraint");
    conn.execute("CREATE FULLTEXT INDEX b_text FOR (n:B) ON EACH [n.text]")
        .expect("fulltext");
    let batched = conn.query(
        "UNWIND [1, 2, 3, 3, 4] AS i \
         CALL { WITH i CREATE (b:B {id: i, text: 'row ' + toString(i)}) \
                CREATE (b)-[:NEXT]->(b) } IN TRANSACTIONS OF 2 ROWS",
        &Params::new(),
    );
    let Err(err) = batched else {
        panic!("the duplicate id must fail the second batch");
    };
    assert_eq!(err.code(), grafile::ErrorCode::Constraint);
    assert_clean(&mut conn);
    assert_eq!(
        query(
            &mut conn,
            "MATCH (b:B)-[:NEXT]->(b) RETURN b.id ORDER BY b.id"
        ),
        vec![vec!["1"], vec!["2"]]
    );
}

#[test]
fn rollback_discards_every_derived_change() {
    let mut conn = connection();
    conn.execute("CREATE INDEX p_name FOR (n:P) ON (n.name)")
        .expect("index");
    conn.execute("UNWIND range(1, 600) AS i CREATE (:P {name: 'p' + toString(i % 10)})")
        .expect("seed");
    conn.begin().expect("begin");
    conn.execute("MATCH (a:P), (b:P) WHERE id(b) = id(a) + 1 CREATE (a)-[:NEXT]->(b)")
        .expect("edges");
    conn.execute("MATCH (n:P) WHERE id(n) % 256 = 0 DETACH DELETE n")
        .expect("delete");
    conn.execute("MATCH (n:P) WHERE id(n) % 3 = 0 SET n:Q, n.name = 'renamed'")
        .expect("update");
    assert_clean(&mut conn);
    conn.rollback().expect("rollback");
    assert_clean(&mut conn);
    assert_eq!(
        query(&mut conn, "MATCH ()-[r]->() RETURN count(r)"),
        vec![vec!["0"]]
    );
    assert_eq!(
        query(&mut conn, "MATCH (n:P {name: 'p0'}) RETURN count(n)"),
        vec![vec!["60"]]
    );
}

#[test]
fn reopened_files_with_freed_pages_are_clean() {
    let store = new_store();
    let path = Path::new("reopen.db");
    {
        let vfs = Arc::new(MemVfs::from_store(store.clone()));
        let mut conn = Connection::create_with_vfs(vfs, path, 4096).expect("create");
        conn.execute("CREATE INDEX n_v FOR (n:N) ON (n.v)")
            .expect("index");
        conn.execute(
            "UNWIND range(1, 600) AS i \
             CREATE (n:N {v: i, blob: reduce(s = '', x IN range(1, 1200) | s + 'x')})",
        )
        .expect("seed with overflow values");
        conn.execute("MATCH (a:N), (b:N) WHERE b.v = a.v + 256 CREATE (a)-[:SKIP]->(b)")
            .expect("edges");
        // Freed record, index and overflow pages land on the freelist.
        conn.execute("MATCH (n:N) WHERE n.v % 2 = 0 DETACH DELETE n")
            .expect("delete");
        conn.execute("DROP INDEX n_v").expect("drop");
        assert_clean(&mut conn);
    }
    let vfs = Arc::new(MemVfs::from_store(store));
    let mut conn = Connection::open_with_vfs(vfs, path, OpenMode::ReadOnly).expect("reopen");
    let report = conn.database_mut().check_integrity().expect("check");
    assert!(report.is_clean(), "{report}");
    assert!(report.stats().free_pages > 0, "{report}");
    assert_eq!(
        query(&mut conn, "MATCH (n:N) RETURN count(n), sum(size(n.blob))"),
        vec![vec!["300", "360000"]]
    );
}
