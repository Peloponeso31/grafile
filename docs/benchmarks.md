# Benchmarks

Criterion benchmarks cover the storage layer (`benches/storage.rs`), the
end-to-end Cypher pipeline (`benches/queries.rs`) and write workloads with
SQLite baselines (`benches/insert.rs`). This document records how results are
produced and the current numbers.

## Methodology

- Apple M5, 16 GiB, macOS 27.0.1; rustc 1.98.1; criterion 0.8.2, release
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

## Results (2026-10-17)

### End-to-end Cypher (in-memory, 8 KiB pages, unique index on `seed`)

| Benchmark | 1 000 nodes | 10 000 nodes | 50 000 nodes |
|---|---:|---:|---:|
| point lookup (`MATCH (n:Person {seed: $id})`) | 3.03 µs | 3.74 µs | 6.90 µs |
| one hop (`-[:KNOWS]->`) | 4.86 µs | 5.91 µs | 10.55 µs |
| label count (`count(*)`) | 52.2 µs | 496 µs | 2.46 ms |
| per row | 52.2 ns | 49.6 ns | 49.1 ns |

| Benchmark | Time |
|---|---:|
| two hops, 10 000 nodes | 8.61 µs |
| top 10 by `seed`, 50 000 nodes (`ORDER BY … LIMIT 10`) | 34.8 ms |
| equality lookup, 50 000 nodes, indexed | 6.82 µs |
| equality lookup, 50 000 nodes, label scan | 15.52 ms |
| indexed update + autocommit, 10 000 nodes | 21.1 µs |

### Writes (10 000 rows)

| Workload | grafile | SQLite |
|---|---:|---:|
| nodes, in-memory | 2.90 ms | 1.26 ms |
| nodes + unique index, in-memory | 10.52 ms | 2.27 ms |
| chain edges, in-memory | 6.67 ms | — |
| `UNWIND … CREATE` (20 batches) | 79.6 ms | — |
| nodes, real file | 12.46 ms | 1.80 ms |
| `SET n.seed = n.seed` | 11.83 ms | 345 µs |
| `DETACH DELETE` all nodes | 8.26 ms | 22.5 µs |

### Storage

| Benchmark | Time |
|---|---:|
| B+tree insert 10 000 keys | 1.84 ms |
| B+tree point lookup (warm cache) | 210 ns |
| B+tree point lookup (8-page cache) | 700 ns |
| B+tree full scan 10 000 keys | 277.7 µs (27.8 ns/key) |
| pager commit, header-only transaction | 10.6 µs |
| freelist alloc/free 512 pages | 1.04 ms |
| 30 KB overflow value read | 992 ns |
| 30 KB overflow value rewrite | 25.3 µs |

## Notes

- Reads go through the bitmap adjacency index and the B+tree, never scans;
  the indexed equality lookup is ~2 300× faster than the label-scan fallback
  at 50 000 nodes.
- `ORDER BY … LIMIT 10` fully sorts before truncating, so it pays for all
  50 000 rows; a top-k cursor is the obvious optimization if needed.
- `UNWIND … CREATE` is dominated by per-row VM overhead (7.6 µs/row versus
  0.29 µs/row for the engine-level create loop) and drifted ~34% between the
  benchmark's introduction (59.2 ms) and today (79.6 ms). Profiling this path
  is pending.
- The `SET n.seed = n.seed` workload compares grafile's Cypher update against
  SQLite's `UPDATE`; the DETACH DELETE pairing compares Cypher `DETACH DELETE`
  against `DELETE FROM`, which is a generous SQLite baseline: it skips
  indexing and per-entity bookkeeping.
