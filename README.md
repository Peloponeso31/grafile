# grafile

A graph database that lives in a single file, queried with Cypher.

grafile borrows SQLite's architecture wholesale — VFS, pager, B+tree, code
generator, bytecode VM (VDBE) — and adds a **chunked incidence matrix** as the
adjacency index: the graph is stored as sparse 256x256-bit bitmap fragments in
a B+tree, so neighbor iteration, edge-existence checks and bitmap set
operations are cheap, while node and edge records stay in ordinary table
B+trees. No server, no sidecars at rest, embeddable from Rust or any language
with a C FFI.

```console
$ cargo run --release -- graph.grafile -c "CREATE (:Person {name: 'alice'})-[:KNOWS]->(:Person {name: 'bob'})"
OK
$ cargo run --release -- graph.grafile -c "MATCH (a:Person)-[:KNOWS]->(b) RETURN a.name AS from_name, b.name AS to_name"
+-----------+---------+
| from_name | to_name |
+-----------+---------+
| alice     | bob     |
+-----------+---------+
```

## Build and run

The project targets stable Rust with edition 2024 and builds as a Cargo
workspace: the `grafile` library and CLI, `grafile-ffi` (C ABI) and
`tck-runner` (conformance harness).

```console
$ cargo build --release              # library + grafile CLI
$ cargo test --workspace             # all test suites
$ cargo run --release -- graph.grafile          # interactive REPL
$ cargo run --release -- graph.grafile -c "QUERY"  # one query, then exit
$ cargo run --release -- graph.grafile script.cypher
```

The REPL supports dot commands: `.help`, `.schema`, `.indexes`, `.meta
[raw|human]`, `.read FILE`, `.timer on|off`, `.mode [NAME]`, `.headers`,
`.separators`, `.colors`, `.pretty on|off`, `.clear`, `.quit`. Input is
syntax-highlighted while typing (`NO_COLOR` disables it). Results print in the
modes SQLite users know — `table` (default), `box`, `qbox`, `psql`,
`markdown`, `column`, `list`, `tabs`, `csv`, `quote`, `line`, `json` — and
`.pretty on` renders nodes and edges as indented JSON inside table cells.
`.meta` prints the interpreted database header (page size, UUID, freelist,
tree roots, allocated counts); inside an open transaction it shows uncommitted
changes, because the header is the in-memory authoritative copy.

## Using the library

```rust
use grafile::api::{Connection, Params};

let mut conn = Connection::open("graph.grafile")?;
conn.execute("CREATE (a:Person {name: 'alice'})-[:KNOWS]->(b:Person {name: 'bob'})")?;

let mut params = Params::new();
params.insert("name", "alice");
let rows = conn.query(
    "MATCH (a:Person {name: $name})-[:KNOWS]->(b) RETURN b.name AS friend",
    &params,
)?;
for row in rows.iter() {
    println!("{row:?}");
}
# Ok::<(), grafile::Error>(())
```

Property indexes are a DDL extension (openCypher has no DDL):

```rust
conn.create_node_index("name", Some("Person"), true)?; // unique index
conn.create_edge_index("since", Some("KNOWS"), false)?;
```

`Connection::query_with_phase` reports whether a failure came from
compilation or execution; `explain` prints the logical plan and
`explain_program` the VDBE bytecode. `database_mut` exposes the engine for
bulk loading and introspection. Errors are SQLite-shaped: every failure
carries an `ErrorCode` (`Syntax`, `Constraint`, `Busy`, `Corrupt`, …) plus a
message and optional source.

## Project layout

```
src/            library + CLI (main.rs)
  cypher/       lexer (logos), recursive-descent parser, AST
  planner/      anchor selection, pattern expansion, pipeline operators
  codegen/      plan -> VDBE bytecode
  vdbe/         register VM: cursor loops, sort/aggregate row cursors
  engine/       node/edge operations, schema, indexes, fragment store
  matrix/       chunked incidence matrix (sparse/dense bitmap fragments)
  record/       varint/typed record codec, order-preserving index keys
  btree/        slotted pages, overflow chains, cursors, integrity checker
  catalog/      labels, edge types, property keys, index definitions
  pager/        page cache, rollback journal, freelist, CRC32C checksums
  vfs/          StdVfs + fault-injecting MemVfs for crash tests
  api.rs        Connection, Params, Rows
benches/        criterion: storage, queries, insert (SQLite baselines)
examples/       profile_insert.rs: insert-path profiling workload
tests/          end-to-end Cypher integration tests
ffi/            C ABI crate, header and language examples
tck-runner/     openCypher TCK harness (see docs/tck.md)
third_party/    vendored openCypher TCK (submodule)
docs/           format.md, ffi.md, tck.md, roadmap.md
```

## Architecture

A query flows top to bottom:

```
Cypher text -> cypher::parse -> planner::plan_query -> codegen::compile
            -> vdbe::execute -> cursors -> btree -> pager -> vfs
```

| Layer | Module | Responsibility |
|-------|--------|----------------|
| OS interface | `vfs` | `StdVfs` plus an in-memory VFS with fault injection |
| Pager | `pager` | Page cache, rollback journal, freelist, checksums |
| B+tree | `btree` | Slotted pages, overflow chains, cursors, integrity checks |
| Records | `record` | Record codec and order-preserving index keys |
| Catalog | `catalog` | Labels, edge types, property keys, index definitions |
| Incidence matrix | `matrix` | Bitmap fragments per direction/type/label plane |
| Engine | `engine` | Node/edge operations, index maintenance, transactions |
| Front end | `cypher` | Lexer, parser, AST |
| Planner | `planner` | Index seek > label scan > full scan, BFS expansion |
| Code generator | `codegen` | Continuation-passing compilation to bytecode |
| VDBE | `vdbe` | Register machine with cursor loops, sort and aggregation |
| API / CLI | `api`, `main` | `Connection`, `Params`, `Rows`; REPL and scripts |

Design points worth knowing before changing storage code:

- **Single file.** Pages are 4 KiB by default and every page carries a CRC32C,
  computed once when dirty pages are written at commit (not on every in-memory
  mutation). The database header holds the format version, page size, UUID,
  creation time, freelist head and tree roots. The layout is specified in
  [docs/format.md](docs/format.md).
- **Transactions.** A SQLite-style rollback journal: a transient
  `<db>-journal` sidecar is created per write transaction and removed on
  commit, so the database file is self-contained at rest. Readers take a
  shared file lock, writers an exclusive one. The journal is flushed with a
  full sync; database pages and journal invalidation use a normal `fsync`.
- **The adjacency index.** `matrix` keeps fixed-size fragments (256 nodes x
  256 edges of bits) as records in a B+tree: an out-plane and an in-plane per
  edge type, plus label and edge-type bitmaps. Fragments are stored sparse
  (sorted bit offsets) while they are small and switch to dense word arrays as
  they fill. A write-back cache keeps touched fragments in memory for the
  duration of a transaction.
- **Determinism.** Query results do not depend on hash-map iteration order:
  the planner scope is insertion-ordered and records keep properties sorted by
  key.

## Conformance: the openCypher TCK

Conformance is measured, not claimed. The vendored openCypher TCK (pinned
submodule, tag `2024.3`) is executed scenario by scenario through the public
API:

```console
$ git submodule update --init
$ cargo run -p tck-runner --release      # exits 0 when the gate passes
```

The gate fails on unexpected failures, harness errors (steps or cells the
runner cannot evaluate) and **stale allow-list entries**, so a fix must prune
its allow-list entries in the same commit. Current baseline: 3897 scenarios,
948 passing (24.3% of the suite, 100% of applicable scenarios), 2949
allow-listed, zero harness errors. Run instructions, error mapping and
comparison semantics are documented in [docs/tck.md](docs/tck.md).

## Testing

```console
$ cargo test --workspace   # unit + property + crash recovery + integration + FFI + harness
$ cargo bench              # criterion: storage, queries, insert
```

- Unit tests live beside the code; B+tree and fragment codecs are also checked
  against reference models with `proptest` (`proptest-regressions/` keeps seed
  corpora for past failures).
- Crash recovery is exercised with the fault-injecting `MemVfs`, which fails
  or tears writes at every operation boundary; reopened databases are verified
  for integrity.
- `tests/cypher_engine.rs` drives end-to-end Cypher through the public API.
- The TCK harness has fixture-based tests for the gate itself plus a parse and
  classification test over every vendored feature file.
- CI builds and tests the FFI crate on macOS, Linux and Windows (with C and
  Python smoke tests on macOS and Linux) and runs the TCK gate on every pull
  request.

## Performance

10 000 inserts in one transaction, Apple silicon, release build
(`cargo bench --bench insert`); the SQLite column uses `rusqlite` with bundled
SQLite as a baseline:

| Workload | grafile | SQLite |
|---|---:|---:|
| nodes, in-memory | 2.9 ms | 1.2 ms |
| nodes + unique index, in-memory | 10.5 ms | 2.3 ms |
| chain edges, in-memory | 6.7 ms | — |
| `UNWIND … CREATE` (20 batches) | 6.4 ms | — |
| nodes, real file (2 transactions) | 13.0 ms | 1.8 ms (1 transaction) |

Before the insert-path work these workloads took 52 ms, 73 ms, 145 ms, 80 ms
and 58 ms (4-22x slower). The remaining gap is dominated by the richer record
format and the fragment bitmap index, and by transaction syncs on real files.
`examples/profile_insert.rs` is a workload used to profile the insert path.

## Embedding: C ABI

`grafile-ffi` builds `libgrafile.so` / `libgrafile.dylib` / `grafile.dll` plus
a hand-written `grafile.h`, so any language with C FFI can embed the engine
in-process: C/C++, Go (cgo), Python (ctypes), Ruby, Node, Java, C#, Dart,
Swift, PHP.

```console
$ cargo build -p grafile-ffi --release   # or scripts/build-ffi.sh
$ bash scripts/ffi-smoke.sh              # C and Python smoke tests
```

```c
GrafileDb *db = NULL;
char *err = NULL;
grafile_open("graph.grafile", &db, &err);
char *json = NULL;
grafile_query_json(db, "MATCH (n:Person) RETURN n.name AS name", NULL, &json);
puts(json);
grafile_free(json);
grafile_close(db);
```

The API follows SQLite's shape (`prepare`/`step`/`column`, JSON helpers,
SQLite-compatible result codes). Ownership, threading and error handling are
documented in [docs/ffi.md](docs/ffi.md), with C, Go, Python and Dart examples
in `ffi/examples/`.

## Cypher support

Implemented: `MATCH` (node and relationship patterns, directions, inline
property filters), `WHERE`, `RETURN`/`WITH` (`DISTINCT`, `ORDER BY`, `SKIP`,
`LIMIT`), `UNWIND`, `CREATE`, `MERGE` (`ON CREATE SET` / `ON MATCH SET`),
`SET`, `REMOVE`, `DELETE` / `DETACH DELETE`, parameters (`$name`), list/map
literals, `CASE`, `IS [NOT] NULL`, string predicates (`STARTS WITH`,
`ENDS WITH`, `CONTAINS`, `=~`), label predicates, three-valued logic, and the
aggregations `count`, `sum`, `avg`, `min`, `max`, `collect`. Scalar functions:
`id`, `labels`, `type`, `startNode`, `endNode`, `coalesce`, `toString`,
`toInteger`, `toFloat`, `size`, `properties`, `keys`.

Explicit gaps fail with clear `Unsupported` errors instead of wrong results:
`OPTIONAL MATCH`, `UNION`, variable-length paths, named paths and path values,
`DISTINCT` inside aggregations, multiple relationships in `MERGE`, pattern
predicates, list/pattern comprehensions, map projections, `FOREACH`, `CALL`,
most of the function library, and temporal/spatial types. The plan to close
them, ordered by TCK scenario share, is in
[docs/roadmap.md](docs/roadmap.md). Storage-side work tracked alongside it:
WAL mode for concurrent readers, delete rebalancing and `VACUUM`-style
compaction, bulk loading, and streaming scans.

## Contributing

Before a commit: `cargo fmt --all --check`,
`cargo clippy --workspace --all-targets -- -D warnings`,
`cargo test --workspace`, and — when behavior changes — the TCK gate.
Commits follow conventional commits.

Rules of the house:

- No feature ships without TCK coverage. Fixing a gap means removing its
  allow-list entries in the same commit; the gate fails on stale entries.
- Harness errors (unrecognized steps, cells, setups) are never allow-listed:
  fix the runner first.
- Storage changes follow [docs/format.md](docs/format.md) and bump the format
  minor version; readers stay backward compatible.
- Panics on user input are treated as bugs: no `unwrap` in library paths, and
  `unsafe` only with a documented safety invariant.

## License

MIT, see [LICENSE](LICENSE).
