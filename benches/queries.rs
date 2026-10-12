//! End-to-end Cypher benchmarks: traversal, indexed lookup and aggregation.

use std::path::Path;
use std::sync::Arc;

use criterion::{Criterion, criterion_group, criterion_main};
use grafile::api::{Connection, Params};
use grafile::vfs::mem::MemVfs;
use std::hint::black_box;

fn seeded_connection(nodes: u64) -> Connection {
    let mut conn =
        Connection::create_with_vfs(Arc::new(MemVfs::new()), Path::new("bench-query.db"), 8192)
            .expect("connection");
    {
        let db = conn.database_mut();
        db.write(|db| {
            let person = db.ensure_label("Person")?;
            let knows = db.ensure_edge_type("KNOWS")?;
            let seed_key = db.ensure_property_key("seed")?;
            let name_key = db.ensure_property_key("name")?;
            let mut ids = Vec::with_capacity(nodes as usize);
            for index in 0..nodes {
                let id = db.create_node(
                    &[person],
                    vec![
                        (seed_key, grafile::record::Value::Int(index as i64)),
                        (
                            name_key,
                            grafile::record::Value::from(format!("node{index}").as_str()),
                        ),
                    ],
                )?;
                ids.push(id);
            }
            for pair in ids.windows(2) {
                db.create_edge(knows, pair[0], pair[1], vec![])?;
            }
            Ok(())
        })
        .expect("seed");
    }
    conn.create_node_index("seed", Some("Person"), true)
        .expect("index");
    conn
}

fn bench_point_lookup(c: &mut Criterion) {
    let mut conn = seeded_connection(5_000);
    c.bench_function("cypher_indexed_point_lookup", |b| {
        let mut probe = 0u64;
        b.iter(|| {
            probe = probe.wrapping_mul(6364136223846793005).wrapping_add(1);
            let id = probe % 5_000;
            let query = format!("MATCH (n:Person {{seed: {id}}}) RETURN n.name");
            let rows = conn.query(&query, &Params::new()).expect("query");
            black_box(rows.len());
        });
    });
}

fn bench_one_hop(c: &mut Criterion) {
    let mut conn = seeded_connection(5_000);
    c.bench_function("cypher_one_hop", |b| {
        let mut probe = 0u64;
        b.iter(|| {
            probe = probe.wrapping_mul(6364136223846793005).wrapping_add(1);
            let id = probe % 4_000;
            let query = format!("MATCH (a:Person {{seed: {id}}})-[:KNOWS]->(b) RETURN b.name");
            let rows = conn.query(&query, &Params::new()).expect("query");
            black_box(rows.len());
        });
    });
}

fn bench_aggregate(c: &mut Criterion) {
    let mut conn = seeded_connection(5_000);
    c.bench_function("cypher_label_count", |b| {
        b.iter(|| {
            let rows = conn
                .query("MATCH (n:Person) RETURN count(*) AS n", &Params::new())
                .expect("query");
            black_box(rows.len());
        });
    });
}

criterion_group!(benches, bench_point_lookup, bench_one_hop, bench_aggregate);
criterion_main!(benches);
