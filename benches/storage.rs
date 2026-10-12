//! Storage micro-benchmarks: B+tree operations and page cache behavior.

use std::path::Path;
use std::sync::Arc;

use criterion::{Criterion, criterion_group, criterion_main};
use grafile::btree::{Btree, TreeKind};
use grafile::pager::Pager;
use grafile::vfs::mem::MemVfs;
use std::hint::black_box;

fn open_pager(name: &str) -> Pager {
    Pager::create(Arc::new(MemVfs::new()), Path::new(name), 8192).expect("pager")
}

fn bench_btree_insert(c: &mut Criterion) {
    c.bench_function("btree_insert_10k", |b| {
        b.iter(|| {
            let mut pager = open_pager("bench-insert.db");
            pager.begin_write().expect("begin");
            let mut tree = Btree::create(&mut pager, TreeKind::Table).expect("tree");
            for key in 0..10_000u64 {
                tree.insert(&mut pager, &key.to_be_bytes(), b"value")
                    .expect("insert");
            }
            pager.commit().expect("commit");
            black_box(tree.root());
        });
    });
}

fn bench_btree_lookup(c: &mut Criterion) {
    let mut pager = open_pager("bench-lookup.db");
    pager.begin_write().expect("begin");
    let mut tree = Btree::create(&mut pager, TreeKind::Table).expect("tree");
    for key in 0..10_000u64 {
        tree.insert(&mut pager, &key.to_be_bytes(), b"value")
            .expect("insert");
    }
    pager.commit().expect("commit");

    c.bench_function("btree_point_lookup", |b| {
        let mut probe = 0u64;
        b.iter(|| {
            probe = probe.wrapping_mul(6364136223846793005).wrapping_add(1);
            let key = (probe % 10_000).to_be_bytes();
            let value = tree.get(&mut pager, &key).expect("get");
            black_box(value);
        });
    });
}

fn bench_btree_scan(c: &mut Criterion) {
    let mut pager = open_pager("bench-scan.db");
    pager.begin_write().expect("begin");
    let mut tree = Btree::create(&mut pager, TreeKind::Table).expect("tree");
    for key in 0..10_000u64 {
        tree.insert(&mut pager, &key.to_be_bytes(), b"value")
            .expect("insert");
    }
    pager.commit().expect("commit");

    c.bench_function("btree_full_scan_10k", |b| {
        b.iter(|| {
            let mut cursor = tree.cursor();
            let mut count = 0u64;
            let mut ok = cursor.first(&mut pager).expect("first");
            while ok {
                count += 1;
                ok = cursor.next(&mut pager).expect("next");
            }
            black_box(count);
        });
    });
}

criterion_group!(
    benches,
    bench_btree_insert,
    bench_btree_lookup,
    bench_btree_scan
);
criterion_main!(benches);
