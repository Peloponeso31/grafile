# grafile

A graph database that lives in a file, heavily inspired by SQLite. You query it
with Cypher.

grafile keeps SQLite's proven decomposition — VFS, pager, B+tree, code
generator, bytecode VM (VDBE) — and adds a chunked incidence matrix as the
adjacency index: the graph is stored as sparse 256x256-bit bitmap fragments in
a B+tree, so neighbor iteration, edge-existence checks and bitmap set
operations are cheap, while node and edge records stay in ordinary table
B+trees.

## Status

Working v1: single-file storage with crash-safe transactions, a Cypher subset
(see below), property indexes, a CLI shell, and an embedded Rust API. The
on-disk format is documented in [docs/format.md](docs/format.md).

## Quick start

```console
$ cargo run -- graph.grafile -c "CREATE (:Person {name: 'alice', age: 30})"
OK
$ cargo run -- graph.grafile -c "MATCH (n:Person) RETURN n.name AS name, n.age AS age"
name    | age
--------+----
"alice" | 30
(1 row)
$ cargo run -- graph.grafile          # interactive REPL
grafile> MATCH (n:Person) RETURN n.name;
```

REPL dot commands: `.help`, `.schema`, `.indexes`, `.meta [raw|human]`,
`.read FILE`, `.timer on|off`, `.mode [NAME]`, `.headers on|off`,
`.separators on|off`, `.colors on|off`, `.pretty on|off`, `.clear`
(alias `.cls`), `.quit`. Scripts can also be passed as a file argument.

`.meta` prints the database header — interpreted by default (page size,
creation time, UUID, freelist, tree roots, allocated counts, file path and
size) or with `.meta raw` as the exact stored values. Because the header is
the in-memory authoritative copy, `.meta` inside an open transaction shows
uncommitted changes.

Input is syntax-highlighted while typing (disabled with `.colors off` or the
`NO_COLOR` environment variable). Results can be printed in the modes SQLite
users know: `table` (default), `box`, `qbox`, `psql`, `markdown`, `column`,
`list`, `tabs`, `csv`, `quote`, `line`, and `json` (NDJSON, one object per
row):

```console
grafile> .mode json
grafile> MATCH (n:Person) RETURN n;
{"n":{"id":1,"labels":["Person"],"properties":{"name":"alice","age":30}}}
```

With `.pretty on`, node and edge values expand to indented, highlighted JSON
with their attributes (labels, type, endpoints, properties), also inside
table cells:

```console
grafile> .pretty on
grafile> MATCH (n:Person) RETURN n;
+----------------------+
| n                    |
+----------------------+
| {                    |
|   "id": 1,           |
|   "labels": [        |
|     "Person"         |
|   ],                 |
|   "properties": {    |
|     "name": "alice", |
|     "age": 30        |
|   }                  |
| }                    |
+----------------------+
```


## Library

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

Property indexes (a DDL extension — openCypher has no DDL):

```rust
conn.create_node_index("name", Some("Person"), true)?; // unique index
```

`conn.explain(query)` prints the logical plan; `conn.explain_program(query)`
prints the VDBE bytecode.

## Embedding (C ABI)

`grafile-ffi` builds `libgrafile.so` / `libgrafile.dylib` / `grafile.dll` plus
a hand-written `grafile.h`, so any language with C FFI can embed the engine
in-process: C/C++, Go (cgo), Python (ctypes), Ruby, Node, Java, C#, Dart,
Swift, PHP.

```console
$ cargo build -p grafile-ffi --release   # or scripts/build-ffi.sh
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

The API follows SQLite's shape (`prepare`/`step`/`column`, JSON in/out
helpers, SQLite-compatible result codes). See [docs/ffi.md](docs/ffi.md) for
the full reference, ownership and threading rules, and C/Go/Python/Dart
examples.

## Cypher v1

Implemented: `MATCH` (node and relationship patterns, directions, inline
property filters), `WHERE`, `RETURN`/`WITH` (with `DISTINCT`, `ORDER BY`,
`SKIP`, `LIMIT`), `UNWIND`, `CREATE`, `MERGE` (with `ON CREATE SET` /
`ON MATCH SET`), `SET`, `REMOVE`, `DELETE` / `DETACH DELETE`, parameters
(`$name`), list/map literals, `CASE`, `IS NULL`, string predicates
(`STARTS WITH`, `ENDS WITH`, `CONTAINS`, `=~`), label predicates, and the
aggregations `count`, `sum`, `avg`, `min`, `max`, `collect`. Scalar functions:
`id`, `labels`, `type`, `startNode`, `endNode`, `coalesce`, `toString`,
`toInteger`, `toFloat`, `size`, `properties`, `keys`.

Not yet implemented (clear `Unsupported` errors): `OPTIONAL MATCH`,
variable-length paths, `UNION`, `DISTINCT` inside aggregations, multiple
relationships in `MERGE`, `ORDER BY` over non-projected values after
aggregation. MERGE on a path with unbound endpoints merges the node(s) first
and then the relationship.

## Architecture

| Layer | Module | Notes |
|-------|--------|-------|
| OS interface | `vfs` | `StdVfs` plus an in-memory VFS with fault injection for crash tests |
| Pager | `pager` | page cache, rollback journal, freelist, CRC32C checksums |
| B+tree | `btree` | slotted pages, overflow chains, cursors, integrity checker |
| Records | `record` | varint/typed codec plus order-preserving index keys |
| Catalog | `catalog` | labels, edge types, property keys, index definitions |
| Incidence matrix | `matrix` | 65536-bit fragments in a B+tree; out/in planes per type plus label/type bitmaps |
| Cypher front end | `cypher` | `logos` lexer, recursive-descent parser, AST |
| Planner | `planner` | anchor selection (index seek > label scan > full scan), BFS expansion, pipeline operators |
| Code generator | `codegen` | plan to VDBE bytecode (continuation-passing) |
| VDBE | `vdbe` | register machine with cursor loops, sort/aggregate row cursors |
| API | `api` | `Connection`, `Params`, `Rows` |
| Shell | `bin` | REPL, dot commands, script runner |
| C ABI | `ffi` | `libgrafile` cdylib/staticlib plus `grafile.h` for any language |

Transactions use a SQLite-style rollback journal: a transient
`<db>-journal` sidecar is created for each write transaction and removed on
commit; the database file is self-contained at rest. Readers take a shared
file lock, writers an exclusive one.

## Testing and benchmarks

```console
$ cargo test          # unit + property + crash-recovery + Cypher integration tests
$ cargo bench         # criterion: storage, insert and end-to-end query benchmarks
```

Crash recovery is exercised with a fault-injecting in-memory VFS that fails
(or tears) writes at every operation boundary; reopened databases are checked
for integrity. B+tree and fragment codecs are verified against reference
models with `proptest`.

Conformance is measured against the vendored openCypher TCK (pinned submodule
plus an allow-list of known gaps): `cargo run -p tck-runner --release`. The
gate fails on unexpected failures, harness errors and stale allow-list
entries; see `docs/tck.md` for the semantics and the current pass rate.

Insert throughput (10 000 inserts in one transaction, Apple silicon, release
build), `cargo bench --bench insert`:

| Workload | grafile | SQLite (rusqlite, bundled) |
|---|---:|---:|
| nodes, in-memory | 2.9 ms | 1.2 ms |
| nodes + unique index, in-memory | 10.5 ms | 2.3 ms |
| chain edges, in-memory | 6.7 ms | — |
| `UNWIND … CREATE` (20 batches) | 6.4 ms | — |
| nodes, real file (2 transactions) | 13.0 ms | 1.8 ms (1 transaction) |

Before the insert-path work these workloads took 52 ms, 73 ms, 145 ms, 80 ms
and 58 ms respectively (4–22× slower); the remaining gap is dominated by the
richer record format and the fragment bitmap index, and by transaction syncs
on real files.

## Roadmap

The openCypher completeness plan — TCK harness first (now live, see
[docs/tck.md](docs/tck.md)), then core language, function library,
temporal/spatial types, a semantic sweep and Neo4j extensions — lives in
[docs/roadmap.md](docs/roadmap.md).

Storage-side work tracked alongside it: WAL mode for concurrent readers,
delete rebalancing and `VACUUM`-style compaction, bulk loading, and
streaming scans.
