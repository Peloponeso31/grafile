# Benchmarks

Criterion benchmarks cover the storage layer (`benches/storage.rs`), the
end-to-end Cypher pipeline (`benches/queries.rs`) and write workloads with
SQLite baselines (`benches/insert.rs`). This document records how results are
produced and the current numbers.

## Methodology

- Apple M5, 16 GiB, macOS 27.2; rustc 1.99.1; criterion 0.8.2, release
  profile.
- In-memory VFS (`MemVfs`) unless a workload says `file`.
- Each measurement includes parsing, planning, compilation and execution; the
  query string is formatted inside the loop, as in real usage.
- Benchmarks never run in parallel; results are medians from criterion's
  default 3 s warm-up and 5 s measurement.

## Running

```console
$ cargo bench                     # all targets
$ cargo bench --bench queries     # one target
$ cargo bench --bench queries -- cypher_one_hop   # filter by name
```

Compare against a saved baseline (for example before and after a change):

```console
$ cargo bench -- --save-baseline main
# ... change code ...
$ cargo bench -- --baseline main
```

`critcmp` renders the two runs side by side:
`cargo install critcmp && critcmp main changed`.

CI compiles every benchmark and runs each once (`cargo bench -- --test`) so a
benchmark that stops compiling or panics fails the build; CI does not gate on
timings.

## Results (2026-11-01, after Phase 8)

"Before" is the Phase 7 tree (`b49eb50`) measured on the same machine and
toolchain in the same session; the 2026-10-17 numbers drifted by a few
percent with the compiler upgrade.

### End-to-end Cypher (in-memory, 8 KiB pages, unique index on `seed`)

| Benchmark | 1 000 nodes | 10 000 nodes | 50 000 nodes |
|---|---:|---:|---:|
| point lookup (`MATCH (n:Person {seed: $id})`) | 3.20 µs | 3.37 µs | 3.53 µs |
| one hop (`-[:KNOWS]->`) | 4.71 µs | 5.00 µs | 5.25 µs |
| label count (`count(*)`) | 20.6 µs | 179 µs | 889 µs |
| per row | 20.6 ns | 17.9 ns | 17.8 ns |

| Benchmark | Before | After |
|---|---:|---:|
| equality lookup, 50 000 nodes, indexed | 6.90 µs | 3.50 µs |
| variable-key lookup, pattern (`{seed: id}`) | 17.2 ms (label scan) | 4.97 µs |
| variable-key lookup, `WHERE n.seed = id` | 17.2 ms (label scan) | 5.26 µs |
| equality lookup, 50 000 nodes, no index | 17.3 ms | 14.8 ms |
| label count, 50 000 nodes | 2.68 ms | 889 µs |
| two-label count (`:Person:Tenth`), 50 000 nodes | 33.0 ms | 92.9 µs |
| top 10 by `seed`, 50 000 nodes | 35.5 ms | 29.7 ms |
| two hops, 10 000 nodes | 8.61 µs | 6.90 µs |
| indexed update + autocommit, 10 000 nodes | 21.1 µs | 19.4 µs |

### One million generated rows (`UNWIND range(1, 1000000)`)

| Workload | Before | After | Speedup |
|---|---:|---:|---:|
| `sum(x), count(x), avg(x)` | 138 ms | 36.1 ms | 3.8x |
| `x % 16 AS k, sum(x)` | 166 ms | 55.1 ms | 3.0x |
| `WHERE x % 7 = 3 RETURN count(*)` | 90.8 ms | 55.2 ms | 1.64x |

### Writes (10 000 rows)

| Workload | grafile | SQLite |
|---|---:|---:|
| nodes, in-memory | 2.83 ms | 1.22 ms |
| nodes + unique index, in-memory | 6.97 ms | 2.20 ms |
| chain edges, in-memory | 6.62 ms | — |
| `UNWIND … CREATE` (20 batches) | 7.30 ms | — |
| nodes, real file | 15.0 ms | 1.75 ms |
| `SET n.seed = n.seed` | 11.2 ms | 335 µs |
| `DETACH DELETE` all nodes | 7.86 ms | 20.7 µs |

### Index backfill and shortest paths

| Workload | Before | After | SQLite |
|---|---:|---:|---:|
| `CREATE INDEX` over 50 000 nodes | 25.0 ms | 7.02 ms | 3.65 ms |
| unique index over 50 000 nodes | 35.0 ms | 7.07 ms | — |
| `CREATE INDEX` over 3 000 000 nodes (release CLI) | 2.64 s | 0.73 s | — |
| `shortestPath`, 10x10 directed grid, corner to corner | 87.4 ms | 102 µs | — |
| `allShortestPaths`, same grid (48 620 paths) | 106.9 ms | 91.0 ms | — |
| `UNWIND … CREATE`, 10 000 nodes in 500-row batches | 76.1 ms | 7.30 ms | — |

Backfill gathers keys, sorts them with `rayon` and bulk-builds the index
tree. A serial sort measured 7.75 ms and 0.81 s on the two sizes, so the
parallel sort is worth about 10%; packing the tree without descents does
the rest. `allShortestPaths` is now dominated by producing its 48 620 rows.

### Storage

| Benchmark | Time |
|---|---:|
| B+tree insert 10 000 keys | 1.74 ms |
| B+tree point lookup (warm cache) | 205 ns |
| B+tree point lookup (8-page cache) | 708 ns |
| B+tree full scan 10 000 keys | 246 µs (24.6 ns/key) |
| pager commit, header-only transaction | 10.6 µs |
| freelist alloc/free 512 pages | 987 µs |
| 30 KB overflow value read | 992 ns |
| 30 KB overflow value rewrite | 25.4 µs |
| dense 8 KiB fragment AND (incl. 90 ns clone) | 183 ns |
| dense 8 KiB fragment cardinality | 119 ns |

The dense kernels are plain word loops: LLVM already lowers them to NEON
(about 88 GB/s for the AND). A `wide::u64x4` version measured the same
for AND and 30% slower for cardinality (no vector popcount), so the scalar
code ships and `wide` is not a dependency.

### Bulk load: `benches/fixtures/seed_big_hardcoded.cypher`

The audited Neo4j 5 seed script (e-commerce, social, jobs, groups and
events), kept verbatim as the bulk-load workload. At full size it creates
about 2.3 million nodes and 13 million relationships; the sizes are
hardcoded literals listed in its header. `tests/fixtures/seed_1_100.cypher`
is the same script at 1/100 scale and is the CI acceptance fixture.

```sh
cargo build --release
./target/release/grafile seed.db benches/fixtures/seed_big_hardcoded.cypher
./target/release/grafile check seed.db
```

| Scale | Nodes | Relationships | Load before | Load after | Throughput | `grafile check` |
|---|---:|---:|---:|---:|---:|---:|
| 1/100 | 23 845 | ~131 400 | 140–153 s | 1.6 s | 15k nodes/s, 82k rels/s | 0.09 s, 0 findings |
| 1/1 | 2 250 652 | 13 008 741 | panicked | 460 s, 290 MB RSS | 4.9k nodes/s, 28k rels/s | 36.8 s, 0 findings |

Relationship counts vary between runs because the script uses `rand()`.
Throughput divides the entity counts by the whole script's wall time,
including its reporting queries. The 1/100 speedup comes from bound-value
index seeks (`MATCH (u:User {id: uid})` no longer scans the label) and
in-place B+tree descents. Before the pager fix the full script panicked:
an `IN TRANSACTIONS` batch that dirtied the whole cache evicted the page
it had just read. With that fixed, the full script spent 92% of its time
scanning dirty frames to find an eviction victim. Eviction now uses a
clean-page LRU, and the full run is I/O-bound (`pwrite`, `fsync`,
`pread`): it commits every 10 000 rows into a 2.9 GB file through a
16 MiB page cache.

## Notes

- Reads go through the bitmap adjacency index and the B+tree, never scans;
  the indexed equality lookup is ~4 200× faster than the label-scan fallback
  at 50 000 nodes, and bound values (`{id: x}`, `WHERE n.id = x`) seek too.
- `ORDER BY … [SKIP s] LIMIT k` keeps the best `s + k` rows and rejects
  rows against the current cutoff before copying them; the remaining top-10
  cost is reading two properties from 50 000 node records.
- `UNWIND … CREATE` used to compile each item of a literal list into its
  own register, and those registers widened every following row window, so
  a 500-item batch copied ~500 registers per row (76.1 ms for 10 000 rows).
  Literal lists and maps now compile to one constant (7.30 ms).
- The `SET n.seed = n.seed` workload compares grafile's Cypher update against
  SQLite's `UPDATE`; the DETACH DELETE pairing compares Cypher `DETACH DELETE`
  against `DELETE FROM`, which is a generous SQLite baseline: it skips
  indexing and per-entity bookkeeping.
