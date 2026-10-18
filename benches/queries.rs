//! End-to-end Cypher benchmarks: lookup, traversal, aggregation, sorting
//! and updates, parameterized by graph size.

use std::path::Path;
use std::sync::Arc;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use grafile::api::{Connection, Params};
use grafile::vfs::mem::MemVfs;
use std::hint::black_box;

const SIZES: [u64; 3] = [1_000, 10_000, 50_000];

/// A `Person` chain of `nodes` nodes with sequential `seed` and `name`
/// properties, optionally backed by a unique index on `seed`.
fn seeded_connection(nodes: u64, indexed: bool) -> Connection {
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
    if indexed {
        conn.create_node_index("seed", Some("Person"), true)
            .expect("index");
    }
    conn
}

fn next_probe(state: &mut u64) -> u64 {
    *state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
    *state
}

fn bench_point_lookup(c: &mut Criterion) {
    let mut group = c.benchmark_group("cypher_point_lookup");
    for &nodes in &SIZES {
        let mut conn = seeded_connection(nodes, true);
        group.bench_with_input(BenchmarkId::from_parameter(nodes), &nodes, |b, &nodes| {
            let mut state = 0u64;
            b.iter(|| {
                let id = next_probe(&mut state) % nodes;
                let query = format!("MATCH (n:Person {{seed: {id}}}) RETURN n.name");
                let rows = conn.query(&query, &Params::new()).expect("query");
                black_box(rows.len());
            });
        });
    }
    group.finish();
}

fn bench_one_hop(c: &mut Criterion) {
    let mut group = c.benchmark_group("cypher_one_hop");
    for &nodes in &SIZES {
        let mut conn = seeded_connection(nodes, true);
        group.bench_with_input(BenchmarkId::from_parameter(nodes), &nodes, |b, &nodes| {
            let mut state = 0u64;
            b.iter(|| {
                let id = next_probe(&mut state) % (nodes - 1);
                let query = format!("MATCH (a:Person {{seed: {id}}})-[:KNOWS]->(b) RETURN b.name");
                let rows = conn.query(&query, &Params::new()).expect("query");
                black_box(rows.len());
            });
        });
    }
    group.finish();
}

fn bench_two_hop(c: &mut Criterion) {
    let nodes = 10_000;
    let mut conn = seeded_connection(nodes, true);
    c.bench_function("cypher_two_hop_10k", |b| {
        let mut state = 0u64;
        b.iter(|| {
            let id = next_probe(&mut state) % (nodes - 2);
            let query =
                format!("MATCH (a:Person {{seed: {id}}})-[:KNOWS]->()-[:KNOWS]->(c) RETURN c.name");
            let rows = conn.query(&query, &Params::new()).expect("query");
            black_box(rows.len());
        });
    });
}

fn bench_label_count(c: &mut Criterion) {
    let mut group = c.benchmark_group("cypher_label_count");
    for &nodes in &SIZES {
        let mut conn = seeded_connection(nodes, true);
        group.throughput(Throughput::Elements(nodes));
        group.bench_with_input(BenchmarkId::from_parameter(nodes), &nodes, |b, _| {
            b.iter(|| {
                let rows = conn
                    .query("MATCH (n:Person) RETURN count(*) AS n", &Params::new())
                    .expect("query");
                black_box(rows.len());
            });
        });
    }
    group.finish();
}

fn bench_top_k(c: &mut Criterion) {
    let nodes = 50_000;
    let mut conn = seeded_connection(nodes, true);
    let mut group = c.benchmark_group("cypher_top_k");
    group.throughput(Throughput::Elements(nodes));
    group.bench_function("50k", |b| {
        b.iter(|| {
            let rows = conn
                .query(
                    "MATCH (n:Person) RETURN n.name ORDER BY n.seed LIMIT 10",
                    &Params::new(),
                )
                .expect("query");
            black_box(rows.len());
        });
    });
    group.finish();
}

/// Equality lookup with and without an index: the indexed path plans an
/// index seek, the other scans the label and filters.
fn bench_index_vs_scan(c: &mut Criterion) {
    let nodes = 50_000;
    let mut group = c.benchmark_group("cypher_equality_50k");
    for (label, indexed) in [("indexed_seek", true), ("label_scan", false)] {
        let mut conn = seeded_connection(nodes, indexed);
        group.bench_function(label, |b| {
            let mut state = 0u64;
            b.iter(|| {
                let id = next_probe(&mut state) % nodes;
                let query = format!("MATCH (n:Person {{seed: {id}}}) RETURN n.name");
                let rows = conn.query(&query, &Params::new()).expect("query");
                black_box(rows.len());
            });
        });
    }
    group.finish();
}

/// A single-property update through the unique index, autocommitted.
fn bench_update(c: &mut Criterion) {
    let nodes = 10_000;
    let mut conn = seeded_connection(nodes, true);
    c.bench_function("cypher_update_via_index_10k", |b| {
        let mut state = 0u64;
        b.iter(|| {
            let id = next_probe(&mut state) % nodes;
            let query = format!("MATCH (n:Person {{seed: {id}}}) SET n.seed = {id}");
            conn.execute(&query).expect("update");
        });
    });
}

criterion_group!(
    benches,
    bench_point_lookup,
    bench_one_hop,
    bench_two_hop,
    bench_label_count,
    bench_top_k,
    bench_index_vs_scan,
    bench_update
);
criterion_main!(benches);
