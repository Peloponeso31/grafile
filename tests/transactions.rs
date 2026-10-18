//! Transactions, persistence across reopen and cross-connection visibility.

mod support;

use std::path::Path;
use std::sync::Arc;

use grafile::api::Connection;
use grafile::vfs::OpenMode;
use grafile::vfs::mem::MemVfs;
use support::{query, seed};

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
    let mut conn = Connection::open_with_vfs(vfs, Path::new("persist.db"), OpenMode::ReadWrite)
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
    let mut conn = support::connection();
    conn.begin().expect("begin");
    conn.execute("CREATE (:Person {name: 'temp'})")
        .expect("create");
    conn.rollback().expect("rollback");
    let rows = query(&mut conn, "MATCH (n:Person) RETURN count(*)");
    assert_eq!(rows, vec![vec!["0"]]);
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
        OpenMode::ReadWrite,
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
