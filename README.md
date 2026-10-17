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
$ grafile social.grafile
grafile> CREATE (alice:Person {name: 'alice', age: 30}),
         (bob:Person {name: 'bob', age: 25}),
         (alice)-[:KNOWS {since: 2020}]->(bob);
OK
grafile> MATCH (a:Person)-[:KNOWS]->(b)
         RETURN a.name AS from_name, b.name AS to_name;
+-----------+---------+
| from_name | to_name |
+-----------+---------+
| alice     | bob     |
+-----------+---------+
```

## Why grafile

- **One file, zero setup.** The database is a single file. A transient
  `-journal` sidecar exists only while a write transaction is in flight, so a
  grafile database is self-contained at rest.
- **Graph-native storage.** Adjacency is kept in a bitmap index, so walking
  relationships and checking whether an edge exists do not scan tables.
- **Crash-safe.** Writes use a rollback journal and every page is checksummed;
  recovery is tested by tearing writes at every operation boundary.
- **Embeddable.** Use it as a Rust library or through a C ABI from Go, Python,
  Dart and anything else with C FFI.
- **Measured, not claimed.** Conformance is checked against the openCypher TCK
  on every change, with a published pass rate (see below).

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

### Project layout

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

## Installation

macOS and Linux, any distro. The script picks the right static binary,
verifies its SHA-256, and installs to `~/.local/bin` without sudo:

```console
$ curl -LsSf https://github.com/Peloponeso31/grafile/releases/latest/download/install.sh | sh
```

Set `GRAFILE_PREFIX` to install elsewhere, or `GRAFILE_VERSION` to pin a
release. Alongside the binary the script installs the FFI library
(`lib/libgrafile.*`), the C header (`include/grafile.h`) and pkg-config
metadata (`lib/pkgconfig/grafile.pc`), so programs can link with:

```console
$ export PKG_CONFIG_PATH="$HOME/.local/lib/pkgconfig:$PKG_CONFIG_PATH"
$ cc app.c $(pkg-config --cflags --libs grafile)
```

See [docs/ffi.md](docs/ffi.md) for the C ABI.

Debian/Ubuntu users can install the `.deb`; Fedora/RHEL users the `.rpm`
(x86_64 and aarch64 packages are released):

```console
$ sudo dpkg -i grafile_0.1.0-1_amd64.deb
$ sudo rpm -i grafile-0.1.0-1.x86_64.rpm
```

Windows, once the first manifest is merged into the winget community
repository:

```console
$ winget install Peloponeso31.Grafile
```

From source (stable Rust, edition 2024):

```console
$ cargo install --path .        # installs the `grafile` binary
```

Every release also ships tarballs, packages and `SHA256SUMS.txt` on the
[releases page](https://github.com/Peloponeso31/grafile/releases).

## Using the shell

```console
$ grafile social.grafile        # start the shell on a new or existing file
```

From a checkout, `cargo run --release -- social.grafile` does the same without
installing.

Run one query and exit:

```console
$ grafile social.grafile -c "MATCH (n:Person) RETURN count(n) AS people"
+--------+
| people |
+--------+
| 2      |
+--------+
```

`-c` runs a single query. For several statements, put them in a script file
(statements are separated by `;`) or type them in the shell:

```console
$ grafile social.grafile import.cypher
```

Errors carry a code (and a position for syntax errors), and the process exits
with status 1, so scripts and CI jobs can rely on it:

```console
$ grafile social.grafile -c "MATCH (n"
error: Syntax: expected ')' to close a node pattern at line 1 column 9 (found end of input)
$ grafile social.grafile -c "RETURN 1 + 'x'"
error: Type: cannot apply arithmetic to integer and string
```

The interactive shell remembers a few conveniences:

| Command | Effect |
| --- | --- |
| `.schema` | labels, relationship types and property keys |
| `.indexes` | index definitions |
| `.meta` | database header: page size, UUID, freelist, tree roots |
| `.mode NAME` | output mode: `table`, `box`, `csv`, `json`, `markdown`, … |
| `.headers on\|off` | column titles |
| `.pretty on\|off` | expand nodes and edges to highlighted JSON |
| `.timer on\|off` | per-query timing |
| `.read FILE` | run a script |
| `.help` / `.quit` | help and exit |

`.meta` prints the interpreted database header (page size, UUID, freelist,
tree roots, allocated counts); inside an open transaction it shows uncommitted
changes, because the header is the in-memory authoritative copy. Input is
syntax-highlighted while typing (`NO_COLOR` disables it). `.mode json` emits
one JSON object per row, which makes piping into `jq` or another program
straightforward:

```console
grafile> .mode json
grafile> MATCH (n:Person) RETURN n;
{"n":{"id":1,"labels":["Person"],"properties":{"name":"alice","age":30}}}
```

### A Cypher tour

```cypher
// Create nodes and a relationship in one statement.
CREATE (alice:Person {name: 'alice', age: 30}),
       (bob:Person {name: 'bob', age: 25}),
       (alice)-[:KNOWS {since: 2020}]->(bob)

// Traverse relationships.
MATCH (a:Person)-[:KNOWS]->(b)
RETURN a.name AS from_name, b.name AS to_name

// Filter, sort and paginate.
MATCH (p:Person)
WHERE p.age > 24
RETURN p.name AS name
ORDER BY p.name
SKIP 0 LIMIT 10

// Aggregate.
MATCH (p:Person)
RETURN count(p) AS people, avg(p.age) AS avg_age

// Chain clauses with WITH and build rows with UNWIND.
UNWIND [1, 2, 3] AS x
WITH x * 2 AS doubled
RETURN sum(doubled) AS total

// Create if missing, update if present.
MERGE (carol:Person {name: 'carol'})
ON CREATE SET carol.age = 40
ON MATCH SET carol.seen = true
RETURN carol.name AS name

// Update and delete.
MATCH (p:Person {name: 'bob'}) SET p.age = 26
MATCH (a)-[r:KNOWS]->(b) DELETE r
MATCH (p:Person) DETACH DELETE p
```

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

Property indexes are a DDL extension (openCypher has no DDL, and the shell
does not expose index creation yet; Cypher `CREATE INDEX` / `CREATE
CONSTRAINT` statements are scheduled in [docs/roadmap.md](docs/roadmap.md)
Phase 5):

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

Implemented: `MATCH` (node and relationship patterns, directions, type
disjunction, inline property filters including non-literal values),
`OPTIONAL MATCH`, `WHERE`, `RETURN` and `WITH` (with
`DISTINCT`, `ORDER BY`, `SKIP`, `LIMIT`), `UNWIND`, `CREATE`, `MERGE` (with
`ON CREATE SET` / `ON MATCH SET`), `SET`, `REMOVE`, `DELETE` /
`DETACH DELETE`, `FOREACH`, `CALL` for built-in `db.*` procedures,
`UNION` / `UNION ALL`, `SET n += {…}`, parameters
(`$name`), list and map literals, map field access and subscript
(`m.k`, `m['k']`), list indexing and slicing (`list[1..3]`), list
comprehensions, pattern predicates and pattern comprehensions,
quantifiers (`all`, `any`, `none`, `single`), `reduce()`,
variable-length paths (`*1..3`), `shortestPath()` / `allShortestPaths()`,
named paths (`p = …`) with `nodes(p)`,
`relationships(p)` and `length(p)`, `CASE`,
`IS [NOT] NULL`, string predicates (`STARTS WITH`, `ENDS WITH`, `CONTAINS`,
`=~`), label predicates, three-valued logic, and the aggregations `count`,
`sum`, `avg`, `min`, `max`, `collect`, `percentileCont`, `percentileDisc`,
`stDev`, `stDevP` (with `DISTINCT` and expressions over aggregations).
Scalar functions: the graph accessors (`id`, `labels`, `type`, `startNode`,
`endNode`, `properties`, `keys`, `nodes`, `relationships`, `size`,
`length`), conversions (`toString`, `toInteger`, `toFloat`, `toBoolean`),
list functions (`range` with two or three arguments, `head`, `last`, `tail`,
`reverse`, `split`, `substring`, `coalesce`), string functions (`toUpper`,
`toLower`, `trim`, `ltrim`, `rtrim`, `left`, `right`, `replace`) and the
math library (`abs`, `sign`, `ceil`, `floor`, `round`, `sqrt`, `exp`, `log`,
`log10`, `sin`, `cos`, `tan`, `cot`, `asin`, `acos`, `atan`, `atan2`,
`haversin`, `degrees`, `radians`, `pi`, `e`, `rand`).

Temporal and spatial values are first class: `date`, `localtime`, `time`,
`localdatetime`, `datetime` (fixed offsets and IANA zones like
`Europe/Stockholm`), `duration` and `point`, built from strings, component
maps or other temporal values, comparable and storable, with ISO 8601
serialization, component accessors (`d.year`, `t.offset`, `du.months`,
`p.longitude`), calendar and clock arithmetic (`date + duration`, `duration
* 2`), `duration.between`/`inMonths`/`inDays`/`inSeconds`,
`date.truncate`/`datetime.truncate` and friends (millennium through
microsecond), plus `distance()` for cartesian and WGS-84 points.

Explicit gaps fail with clear `Unsupported` errors instead of wrong results:
`REMOVE n = {…}` and map projections (`n{.name}`). The plan to close them,
ordered by TCK scenario share, is in [docs/roadmap.md](docs/roadmap.md).
Storage-side work tracked alongside it: WAL mode for concurrent readers,
delete rebalancing and `VACUUM`-style compaction, bulk loading, and
streaming scans.

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
3844 passing (98.6% of the suite, 100% of applicable scenarios), 53
allow-listed (the CALL procedure family, a non-goal, and one nested pattern
comprehension), zero harness errors. Run instructions, error mapping and
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

Reads are served from the bitmap adjacency index rather than by scanning.
End-to-end Cypher latency on a 5 000-node chain graph (in-memory, release
build, `cargo bench --bench queries`; each measurement includes parsing,
planning, compilation and execution):

| Query | Time |
|---|---:|
| indexed point lookup (`MATCH (n:Person {seed: $id})`) | 2.8 µs |
| indexed one-hop traversal (`MATCH (a:Person {seed: $id})-[:KNOWS]->(b)`) | 4.5 µs |
| label count over 5 000 nodes (`count(*)`) | 1.6 µs |

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
- AI use is fine, work will be anaylized and compared with the description of
  your pull request.

## License

MIT, see [LICENSE](LICENSE).
