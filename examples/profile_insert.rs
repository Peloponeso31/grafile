//! Profiling workload for the insert path (used with `sample`/`instruments`).
//!
//! Run with: cargo build --release --example profile_insert
use std::path::Path;
use std::sync::Arc;

use grafile::api::Connection;
use grafile::record::Value;
use grafile::vfs::mem::MemVfs;

fn main() {
    let rounds: u64 = std::env::args()
        .nth(1)
        .and_then(|value| value.parse().ok())
        .unwrap_or(10);
    for round in 0..rounds {
        let mut conn =
            Connection::create_with_vfs(Arc::new(MemVfs::new()), Path::new("profile.db"), 8192)
                .expect("connection");
        conn.begin().expect("begin");
        let db = conn.database_mut();
        let person = db.ensure_label("Person").expect("label");
        let knows = db.ensure_edge_type("KNOWS").expect("type");
        let seed = db.ensure_property_key("seed").expect("key");
        let name = db.ensure_property_key("name").expect("key");
        let mut ids = Vec::with_capacity(10_000);
        for index in 0..10_000u64 {
            ids.push(
                db.create_node(
                    &[person],
                    vec![
                        (seed, Value::Int(index as i64)),
                        (name, Value::from(format!("node{index}").as_str())),
                    ],
                )
                .expect("node"),
            );
        }
        for pair in ids.windows(2) {
            db.create_edge(knows, pair[0], pair[1], vec![])
                .expect("edge");
        }
        conn.commit().expect("commit");
        println!("round {round} done");
    }
}
