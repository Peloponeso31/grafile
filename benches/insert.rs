//! Insert benchmarks: node/edge creation and end-to-end `CREATE`, with an
//! equivalent SQLite workload as a baseline.

use std::path::Path;
use std::sync::Arc;

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use grafile::api::{Connection, Params};
use grafile::engine::Database;
use grafile::record::Value;
use grafile::vfs::StdVfs;
use grafile::vfs::mem::MemVfs;
use std::hint::black_box;

const NODES: u64 = 10_000;

fn create_database(conn: &mut Connection) -> (u32, u32, u32, u32) {
    conn.begin().expect("begin");
    let db = conn.database_mut();
    let person = db.ensure_label("Person").expect("label");
    let knows = db.ensure_edge_type("KNOWS").expect("type");
    let seed = db.ensure_property_key("seed").expect("key");
    let name = db.ensure_property_key("name").expect("key");
    conn.commit().expect("commit");
    (person, knows, seed, name)
}

fn insert_nodes(db: &mut Database, ids: (u32, u32, u32), count: u64) {
    let (person, seed, name) = ids;
    for index in 0..count {
        db.create_node(
            &[person],
            vec![
                (seed, Value::Int(index as i64)),
                (name, Value::from(format!("node{index}").as_str())),
            ],
        )
        .expect("create node");
    }
}

fn mem_connection(name: &str) -> Connection {
    Connection::create_with_vfs(Arc::new(MemVfs::new()), Path::new(name), 8192).expect("connection")
}

fn bench_create_nodes(c: &mut Criterion) {
    c.bench_function("grafile_create_nodes_10k", |b| {
        b.iter_batched(
            || mem_connection("bench-insert-nodes.db"),
            |mut conn| {
                let ids = create_database(&mut conn);
                conn.begin().expect("begin");
                insert_nodes(conn.database_mut(), (ids.0, ids.2, ids.3), NODES);
                conn.commit().expect("commit");
                black_box(conn);
            },
            BatchSize::PerIteration,
        );
    });

    c.bench_function("grafile_create_nodes_indexed_10k", |b| {
        b.iter_batched(
            || {
                let mut conn = mem_connection("bench-insert-indexed.db");
                let ids = create_database(&mut conn);
                conn.create_node_index("seed", Some("Person"), true)
                    .expect("index");
                (conn, ids)
            },
            |(mut conn, ids)| {
                conn.begin().expect("begin");
                insert_nodes(conn.database_mut(), (ids.0, ids.2, ids.3), NODES);
                conn.commit().expect("commit");
                black_box(conn);
            },
            BatchSize::PerIteration,
        );
    });
}

fn bench_create_edges(c: &mut Criterion) {
    c.bench_function("grafile_create_edges_10k", |b| {
        b.iter_batched(
            || {
                let mut conn = mem_connection("bench-insert-edges.db");
                let ids = create_database(&mut conn);
                let mut node_ids = Vec::with_capacity(NODES as usize);
                conn.begin().expect("begin");
                insert_nodes(conn.database_mut(), (ids.0, ids.2, ids.3), NODES);
                let db = conn.database_mut();
                let mut cursor = db.all_node_ids().expect("ids");
                node_ids.append(&mut cursor);
                conn.commit().expect("commit");
                (conn, ids.1, node_ids)
            },
            |(mut conn, knows, node_ids)| {
                conn.begin().expect("begin");
                let db = conn.database_mut();
                for pair in node_ids.windows(2) {
                    db.create_edge(knows, pair[0], pair[1], vec![])
                        .expect("create edge");
                }
                conn.commit().expect("commit");
                black_box(conn);
            },
            BatchSize::PerIteration,
        );
    });
}

fn bench_cypher_unwind_create(c: &mut Criterion) {
    c.bench_function("grafile_cypher_unwind_create_10k", |b| {
        b.iter_batched(
            || mem_connection("bench-insert-cypher.db"),
            |mut conn| {
                let mut created = 0u64;
                while created < NODES {
                    let batch = 500.min(NODES - created);
                    let mut query = String::from("UNWIND [");
                    for index in 0..batch {
                        if index > 0 {
                            query.push(',');
                        }
                        query.push_str(&format!("{}", created + index));
                    }
                    query.push_str("] AS seed CREATE (:Person {seed: seed, name: toString(seed)})");
                    conn.query(&query, &Params::new()).expect("create");
                    created += batch;
                }
                black_box(conn);
            },
            BatchSize::PerIteration,
        );
    });
}

fn bench_create_nodes_file(c: &mut Criterion) {
    c.bench_function("grafile_create_nodes_10k_file", |b| {
        b.iter_batched(
            || {
                let dir = tempfile::tempdir().expect("tempdir");
                let path = dir.path().join("insert.grafile");
                let conn =
                    Connection::create_with_vfs(Arc::new(StdVfs), &path, 8192).expect("connection");
                (dir, conn)
            },
            |(_dir, mut conn)| {
                let ids = create_database(&mut conn);
                conn.begin().expect("begin");
                insert_nodes(conn.database_mut(), (ids.0, ids.2, ids.3), NODES);
                conn.commit().expect("commit");
                black_box(conn);
            },
            BatchSize::PerIteration,
        );
    });
}

fn bench_sqlite_baseline(c: &mut Criterion) {
    c.bench_function("sqlite_insert_10k_memory", |b| {
        b.iter_batched(
            || rusqlite::Connection::open_in_memory().expect("sqlite"),
            |conn| {
                conn.execute(
                    "CREATE TABLE person(id INTEGER PRIMARY KEY, seed INTEGER, name TEXT)",
                    [],
                )
                .expect("table");
                let tx = conn.unchecked_transaction().expect("txn");
                {
                    let mut stmt = tx
                        .prepare("INSERT INTO person(seed, name) VALUES (?1, ?2)")
                        .expect("prepare");
                    for index in 0..NODES {
                        stmt.execute(rusqlite::params![index as i64, format!("node{index}")])
                            .expect("insert");
                    }
                }
                tx.commit().expect("commit");
                black_box(conn);
            },
            BatchSize::PerIteration,
        );
    });

    c.bench_function("sqlite_insert_10k_indexed_memory", |b| {
        b.iter_batched(
            || rusqlite::Connection::open_in_memory().expect("sqlite"),
            |conn| {
                conn.execute(
                    "CREATE TABLE person(id INTEGER PRIMARY KEY, seed INTEGER UNIQUE, name TEXT)",
                    [],
                )
                .expect("table");
                let tx = conn.unchecked_transaction().expect("txn");
                {
                    let mut stmt = tx
                        .prepare("INSERT INTO person(seed, name) VALUES (?1, ?2)")
                        .expect("prepare");
                    for index in 0..NODES {
                        stmt.execute(rusqlite::params![index as i64, format!("node{index}")])
                            .expect("insert");
                    }
                }
                tx.commit().expect("commit");
                black_box(conn);
            },
            BatchSize::PerIteration,
        );
    });

    c.bench_function("sqlite_insert_10k_file", |b| {
        b.iter_batched(
            || {
                let dir = tempfile::tempdir().expect("tempdir");
                let conn = rusqlite::Connection::open(dir.path().join("bench.db")).expect("sqlite");
                (dir, conn)
            },
            |(_dir, conn)| {
                conn.execute(
                    "CREATE TABLE person(id INTEGER PRIMARY KEY, seed INTEGER, name TEXT)",
                    [],
                )
                .expect("table");
                let tx = conn.unchecked_transaction().expect("txn");
                {
                    let mut stmt = tx
                        .prepare("INSERT INTO person(seed, name) VALUES (?1, ?2)")
                        .expect("prepare");
                    for index in 0..NODES {
                        stmt.execute(rusqlite::params![index as i64, format!("node{index}")])
                            .expect("insert");
                    }
                }
                tx.commit().expect("commit");
                black_box(conn);
            },
            BatchSize::PerIteration,
        );
    });
}

fn bench_update_nodes(c: &mut Criterion) {
    c.bench_function("grafile_update_nodes_10k", |b| {
        let mut conn = mem_connection("bench-update.db");
        let ids = create_database(&mut conn);
        conn.begin().expect("begin");
        insert_nodes(conn.database_mut(), (ids.0, ids.2, ids.3), NODES);
        conn.commit().expect("commit");

        b.iter(|| {
            conn.execute("MATCH (n:Person) SET n.seed = n.seed")
                .expect("update");
        });
    });
}

fn bench_delete_nodes(c: &mut Criterion) {
    c.bench_function("grafile_delete_nodes_10k", |b| {
        b.iter_batched(
            || {
                let mut conn = mem_connection("bench-delete.db");
                let ids = create_database(&mut conn);
                conn.begin().expect("begin");
                insert_nodes(conn.database_mut(), (ids.0, ids.2, ids.3), NODES);
                conn.commit().expect("commit");
                conn
            },
            |mut conn| {
                conn.execute("MATCH (n:Person) DETACH DELETE n")
                    .expect("delete");
                black_box(conn);
            },
            BatchSize::PerIteration,
        );
    });
}

/// An in-memory SQLite table holding the same 10k rows as the grafile
/// workloads.
fn sqlite_seeded() -> rusqlite::Connection {
    let conn = rusqlite::Connection::open_in_memory().expect("sqlite");
    conn.execute(
        "CREATE TABLE person(id INTEGER PRIMARY KEY, seed INTEGER, name TEXT)",
        [],
    )
    .expect("table");
    let tx = conn.unchecked_transaction().expect("txn");
    {
        let mut stmt = tx
            .prepare("INSERT INTO person(seed, name) VALUES (?1, ?2)")
            .expect("prepare");
        for index in 0..NODES {
            stmt.execute(rusqlite::params![index as i64, format!("node{index}")])
                .expect("insert");
        }
    }
    tx.commit().expect("commit");
    conn
}

fn bench_sqlite_update_delete(c: &mut Criterion) {
    c.bench_function("sqlite_update_10k_memory", |b| {
        b.iter_batched(
            sqlite_seeded,
            |conn| {
                let tx = conn.unchecked_transaction().expect("txn");
                tx.execute("UPDATE person SET seed = seed", [])
                    .expect("update");
                tx.commit().expect("commit");
                black_box(conn);
            },
            BatchSize::PerIteration,
        );
    });

    c.bench_function("sqlite_delete_10k_memory", |b| {
        b.iter_batched(
            sqlite_seeded,
            |conn| {
                let tx = conn.unchecked_transaction().expect("txn");
                tx.execute("DELETE FROM person", []).expect("delete");
                tx.commit().expect("commit");
                black_box(conn);
            },
            BatchSize::PerIteration,
        );
    });
}

criterion_group!(
    benches,
    bench_create_nodes,
    bench_create_edges,
    bench_cypher_unwind_create,
    bench_create_nodes_file,
    bench_update_nodes,
    bench_delete_nodes,
    bench_sqlite_baseline,
    bench_sqlite_update_delete
);
criterion_main!(benches);
