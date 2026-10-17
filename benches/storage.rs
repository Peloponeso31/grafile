//! Storage micro-benchmarks: B+tree operations, page cache behavior,
//! rollback-journal commits, freelist churn and overflow chains.

use std::path::Path;
use std::sync::Arc;

use criterion::{Criterion, criterion_group, criterion_main};
use grafile::btree::{Btree, TreeKind};
use grafile::pager::Pager;
use grafile::vfs::mem::MemVfs;
use std::hint::black_box;

const PAGE_SIZE: u32 = 8192;
const KEYS: u64 = 10_000;

fn open_pager(name: &str) -> Pager {
    Pager::create(Arc::new(MemVfs::new()), Path::new(name), PAGE_SIZE).expect("pager")
}

/// A committed table tree of `KEYS` small entries.
fn seeded_tree(name: &str) -> (Pager, Btree) {
    let mut pager = open_pager(name);
    pager.begin_write().expect("begin");
    let mut tree = Btree::create(&mut pager, TreeKind::Table).expect("tree");
    for key in 0..KEYS {
        tree.insert(&mut pager, &key.to_be_bytes(), b"value")
            .expect("insert");
    }
    pager.commit().expect("commit");
    (pager, tree)
}

fn bench_btree_insert(c: &mut Criterion) {
    c.bench_function("btree_insert_10k", |b| {
        b.iter(|| {
            let mut pager = open_pager("bench-insert.db");
            pager.begin_write().expect("begin");
            let mut tree = Btree::create(&mut pager, TreeKind::Table).expect("tree");
            for key in 0..KEYS {
                tree.insert(&mut pager, &key.to_be_bytes(), b"value")
                    .expect("insert");
            }
            pager.commit().expect("commit");
            black_box(tree.root());
        });
    });
}

fn bench_btree_lookup(c: &mut Criterion) {
    let (mut pager, tree) = seeded_tree("bench-lookup.db");

    c.bench_function("btree_point_lookup", |b| {
        let mut probe = 0u64;
        b.iter(|| {
            probe = probe.wrapping_mul(6364136223846793005).wrapping_add(1);
            let key = (probe % KEYS).to_be_bytes();
            let value = tree.get(&mut pager, &key).expect("get");
            black_box(value);
        });
    });
}

fn bench_btree_lookup_cold_cache(c: &mut Criterion) {
    let (mut pager, tree) = seeded_tree("bench-lookup-cold.db");
    // The tree spans many more than eight pages, so random probes miss.
    pager.set_cache_capacity(8);

    c.bench_function("btree_point_lookup_cold_cache", |b| {
        let mut probe = 0u64;
        b.iter(|| {
            probe = probe.wrapping_mul(6364136223846793005).wrapping_add(1);
            let key = (probe % KEYS).to_be_bytes();
            let value = tree.get(&mut pager, &key).expect("get");
            black_box(value);
        });
    });
}

fn bench_btree_scan(c: &mut Criterion) {
    let (mut pager, tree) = seeded_tree("bench-scan.db");

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

fn bench_pager_commit(c: &mut Criterion) {
    let mut pager = open_pager("bench-commit.db");

    c.bench_function("pager_commit_small_transaction", |b| {
        let mut version = 0u32;
        b.iter(|| {
            pager.begin_write().expect("begin");
            version = version.wrapping_add(1);
            pager.update_header(|header| header.user_version = version);
            pager.commit().expect("commit");
        });
    });
}

fn bench_pager_freelist(c: &mut Criterion) {
    const PAGES: u32 = 512;
    let mut pager = open_pager("bench-freelist.db");
    pager.begin_write().expect("begin");
    let pages: Vec<u32> = (0..PAGES)
        .map(|_| pager.alloc_page().expect("alloc"))
        .collect();
    for page in pages {
        pager.free_page(page).expect("free");
    }
    pager.commit().expect("commit");

    c.bench_function("pager_freelist_alloc_free_512", |b| {
        b.iter(|| {
            pager.begin_write().expect("begin");
            let mut pages = Vec::with_capacity(PAGES as usize);
            for _ in 0..PAGES {
                pages.push(pager.alloc_page().expect("alloc"));
            }
            for page in pages {
                pager.free_page(page).expect("free");
            }
            pager.commit().expect("commit");
        });
    });
}

fn bench_btree_overflow(c: &mut Criterion) {
    const KEY: u64 = 1;
    let big = vec![0xA5u8; 30_000];
    let mut pager = open_pager("bench-overflow.db");
    pager.begin_write().expect("begin");
    let mut tree = Btree::create(&mut pager, TreeKind::Table).expect("tree");
    tree.insert(&mut pager, &KEY.to_be_bytes(), &big)
        .expect("insert big");
    tree.insert(&mut pager, &2u64.to_be_bytes(), b"small")
        .expect("insert small");
    pager.commit().expect("commit");

    c.bench_function("btree_overflow_value_read", |b| {
        b.iter(|| {
            let value = tree.get(&mut pager, &KEY.to_be_bytes()).expect("get");
            black_box(value);
        });
    });

    c.bench_function("btree_overflow_value_rewrite", |b| {
        let mut flip = 0u8;
        b.iter(|| {
            pager.begin_write().expect("begin");
            let mut value = big.clone();
            value[0] = flip;
            flip = flip.wrapping_add(1);
            tree.replace(&mut pager, &KEY.to_be_bytes(), &value)
                .expect("replace");
            pager.commit().expect("commit");
        });
    });
}

criterion_group!(
    benches,
    bench_btree_insert,
    bench_btree_lookup,
    bench_btree_lookup_cold_cache,
    bench_btree_scan,
    bench_pager_commit,
    bench_pager_freelist,
    bench_btree_overflow
);
criterion_main!(benches);
