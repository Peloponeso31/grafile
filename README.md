# grafile

**A graph database in a single file, queried with Cypher.**

[![CI](https://github.com/Peloponeso31/grafile/actions/workflows/ci.yml/badge.svg)](https://github.com/Peloponeso31/grafile/actions/workflows/ci.yml)
[![openCypher TCK](https://github.com/Peloponeso31/grafile/actions/workflows/tck.yml/badge.svg)](https://github.com/Peloponeso31/grafile/actions/workflows/tck.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

grafile is to graphs what SQLite is to tables: no server, no setup, one
portable file. Open it from the shell, from Rust, or from any language with a
C FFI, and query it with the openCypher dialect you already know from Neo4j.

```console
$ grafile social.grafile -c "CREATE (alice:Person {name: 'alice', age: 30}),
    (bob:Person {name: 'bob', age: 25}), (carol:Person {name: 'carol', age: 41}),
    (alice)-[:KNOWS {since: 2020}]->(bob), (bob)-[:KNOWS {since: 2023}]->(carol)"
OK
$ grafile social.grafile -c "MATCH p = shortestPath((a:Person {name: 'alice'})-[:KNOWS*]->(c:Person {name: 'carol'}))
    RETURN [n IN nodes(p) | n.name] AS route, length(p) AS hops"
+---------------------+------+
| route               | hops |
+---------------------+------+
| [alice, bob, carol] | 2    |
+---------------------+------+
```

## Highlights

- **One file, zero configuration.** The database is a single file; a
  `-journal` sidecar exists only while a write transaction is in flight, and
  `-sort-*` files only while a large sort spills.
- **Real Cypher.** 3844 of 3844 applicable openCypher TCK scenarios pass,
  plus the Neo4j extensions people actually use: schema DDL, constraints,
  fulltext indexes, `LOAD CSV` and `CALL { ... } IN TRANSACTIONS`.
- **Graph-native storage.** Relationships live in a bitmap incidence index,
  so traversals and edge-existence checks never scan tables.
- **Crash-safe and self-checking.** A rollback journal and per-page checksums
  protect every commit, and `grafile check` verifies every index and bitmap
  against the records they derive from.
- **Fast where it matters.** Indexed lookups take about 3.5 µs, and the
  Neo4j seed script with 2.3 million nodes and 13 million relationships loads
  in under eight minutes on an Apple M5.
- **Embeddable anywhere.** A Rust crate and a stable, SQLite-shaped C ABI,
  with examples for C, Go, Python and Dart.

## Contents

- [Install](#install)
- [Using the shell](#using-the-shell)
- [Cypher at a glance](#cypher-at-a-glance)
- [Embedding grafile](#embedding-grafile)
- [Performance](#performance)
- [Reliability](#reliability)
- [Developing grafile](#developing-grafile)
- [License](#license)

## Install

**macOS and Linux.** The script downloads the right static binary, verifies
its SHA-256 and installs it to `~/.local/bin`, without sudo:

```console
$ curl -LsSf https://github.com/Peloponeso31/grafile/releases/latest/download/install.sh | sh
```

Set `GRAFILE_PREFIX` to install elsewhere or `GRAFILE_VERSION` to pin a
release. The script also installs the C library (`lib/libgrafile.*`), its
header (`include/grafile.h`) and pkg-config metadata.

**Debian/Ubuntu and Fedora/RHEL** (x86_64 and aarch64 packages are released):

```console
$ sudo dpkg -i grafile_0.4.0-1_amd64.deb
$ sudo rpm -i grafile-0.4.0-1.x86_64.rpm
```

**Windows**, once the manifest is merged into the winget community
repository:

```console
$ winget install Peloponeso31.Grafile
```

**From source** (stable Rust, edition 2024):

```console
$ cargo install --path .
```

Every release ships tarballs, packages and `SHA256SUMS.txt` on the
[releases page](https://github.com/Peloponeso31/grafile/releases).

## Using the shell

```console
$ grafile social.grafile                    # interactive shell (creates the file if needed)
$ grafile social.grafile -c "MATCH (n) RETURN count(n)"   # one query, then exit
$ grafile social.grafile import.cypher      # run a script of ;-separated statements
$ grafile check social.grafile              # verify integrity, read-only
```

Statements in the shell end with `;` or an empty line. Input is
syntax-highlighted (`NO_COLOR` turns colors off), and errors carry a code and,
for syntax errors, a position. Failed commands exit with status 1, so scripts
and CI jobs can rely on them:

```console
$ grafile social.grafile -c "MATCH (n"
error: Syntax: expected ')' to close a node pattern at line 1 column 9 (found end of input)
```

Scripts may contain `//` and `/* */` comments and dot commands between
statements, and long loads show a progress bar. The script runner accepts Neo4j-style seed files unchanged; the
1/100-scale audited seed script in `tests/fixtures/seed_1_100.cypher` loads in
under two seconds.

### Shell commands

| Command | Effect |
| --- | --- |
| `.schema` | labels, relationship types and property keys |
| `.indexes` | `SHOW INDEXES` and `SHOW CONSTRAINTS` |
| `.param set NAME EXPR`, `.param unset NAME`, `.param list`, `.param clear` | bind `$NAME` to the value of any Cypher expression |
| `.meta [raw\|human]` | database header: page size, UUID, freelist, tree roots |
| `.mode NAME` | `table`, `box`, `qbox`, `psql`, `markdown`, `column`, `list`, `tabs`, `csv`, `quote`, `line`, `json` |
| `.headers on\|off`, `.separators on\|off` | table layout |
| `.pretty on\|off` | render nodes and relationships as highlighted JSON |
| `.colors on\|off`, `.timer on\|off` | highlighting and per-query timing |
| `.read FILE` | run a script |
| `.clear`, `.help`, `.quit` | housekeeping |

Tab completes dot commands, keywords, functions and procedures, labels and
relationship types after `:`, property keys after `n.` and parameters after
`$`. Interactive history persists in `~/.grafile_history` (`GRAFILE_HISTORY`
moves it; an empty value disables it). Like sqlite's `~/.sqliterc`,
`~/.grafilerc` runs before every session, including `-c` and scripts:

```text
.mode box
.param set tenant 'acme'
```

| Option | Effect |
| --- | --- |
| `--readonly` | open an existing file read-only; writes fail |
| `--param NAME=EXPR` (`-p`) | bind a parameter before running (repeatable) |
| `--init FILE`, `--no-init` | run another startup file, or none |

Prefix a statement with `EXPLAIN` to see its plan without running it.

`.mode json` prints one JSON object per row, ready for `jq`:

```console
grafile> .mode json
grafile> MATCH (n:Person {name: 'alice'}) RETURN n;
{"n":{"id":1,"labels":["Person"],"properties":{"name":"alice","age":30}}}
```

### Checking integrity

`grafile check FILE` opens the database read-only and compares every derived
structure (relationship and label bitmaps, property and fulltext indexes,
uniqueness and existence constraints) with the node and relationship records,
alongside B+tree ordering, page ownership and the freelist. It prints any
findings and a summary, and exits with status 1 when something is wrong:

```console
$ grafile check social.grafile
0 findings: 5 pages (0 free), 3 nodes, 2 edges, 6 fragments, 0 index entries, 0 postings
```

`grafile check FILE --rebuild` regenerates bitmaps and indexes from the
records in one transaction and checks again. Records themselves are never
rewritten: damaged primary data is reported, not guessed at.

## Cypher at a glance

```cypher
// Create, match and traverse.
CREATE (a:Person {name: 'alice'})-[:KNOWS {since: 2020}]->(b:Person {name: 'bob'});
MATCH (a:Person)-[:KNOWS*1..3]->(b) WHERE a.name = 'alice' RETURN DISTINCT b.name;

// Aggregate, sort and paginate.
MATCH (p:Person)-[:KNOWS]->(friend)
RETURN p.name AS person, count(friend) AS friends
ORDER BY friends DESC LIMIT 10;

// Upsert.
MERGE (c:Person {name: 'carol'})
ON CREATE SET c.created = datetime()
ON MATCH SET c.seen = true;

// Optional patterns, subqueries and comprehensions.
MATCH (p:Person)
OPTIONAL MATCH (p)-[:KNOWS]->(f)
CALL (p) { MATCH (p)<-[:KNOWS]-(fan) RETURN count(fan) AS fans }
RETURN p.name, [x IN collect(f.name) WHERE x IS NOT NULL] AS knows, fans;

// Bulk loads in batches that commit as they go.
LOAD CSV WITH HEADERS FROM 'people.csv' AS row
CALL (row) { CREATE (:Person {name: row.name, age: toInteger(row.age)}) }
IN TRANSACTIONS OF 10000 ROWS;
```

### Schema, indexes and fulltext search

Schema DDL follows Neo4j 5:

```cypher
CREATE CONSTRAINT person_email IF NOT EXISTS FOR (n:Person) REQUIRE n.email IS UNIQUE;
CREATE INDEX person_name FOR (n:Person) ON (n.name);
CREATE FULLTEXT INDEX post_text FOR (n:Post) ON EACH [n.title, n.body];

CALL db.index.fulltext.queryNodes('post_text', '"graph database" OR sqlite')
YIELD node, score
RETURN node.title, score;
```

`IS UNIQUE`, `IS NOT NULL` and `IS NODE KEY` are enforced on every write, and
the relationship form `FOR ()-[r:TYPE]-()` covers unique and not-null
constraints. Equality lookups use an index whether the value is a literal, a
parameter or a variable bound earlier in the query (`{id: uid}`,
`WHERE n.id = uid`). Fulltext queries take terms, `"phrases"`, `AND` and
`OR`. The analyzer splits on non-alphanumeric characters and lowercases, and
is deliberately not Lucene-compatible.

### What is supported

| Area | Coverage |
| --- | --- |
| Clauses | `MATCH`, `OPTIONAL MATCH`, `WHERE`, `RETURN`, `WITH`, `UNWIND`, `CREATE`, `MERGE` (`ON CREATE`/`ON MATCH`), `SET`, `REMOVE`, `DELETE`, `DETACH DELETE`, `FOREACH`, `UNION [ALL]`, `LOAD CSV`, `CALL { ... }` (unit, returning, correlated, `IN TRANSACTIONS`), `CALL ... YIELD ... WHERE`, `SHOW INDEXES`/`SHOW CONSTRAINTS`, `EXPLAIN` |
| Patterns | directions, label expressions (`:A\|B`, `:A&B`, `:!A`), inline property maps, variable-length paths (`*1..3`), named paths, `shortestPath`, `allShortestPaths`, pattern predicates and comprehensions |
| Expressions | parameters, list and map literals, map projections (`n{.name, .*}`), indexing and slicing, list comprehensions, `all`/`any`/`none`/`single`, `reduce`, `CASE`, `EXISTS { }` and `COUNT { }`, string predicates and `=~`, `\|\|` concatenation, `IS :: TYPE`, three-valued logic |
| Aggregation | `count`, `sum`, `avg`, `min`, `max`, `collect`, `percentileCont`, `percentileDisc`, `stDev`, `stDevP`, with `DISTINCT` |
| Functions | graph accessors (`elementId`), conversions including `toXOrNull` and `toXList`, `valueType`, `isEmpty`, `nullIf`, list, string (`btrim`, `trim(BOTH ... FROM ...)`) and math libraries, `randomUUID`, `coalesce`; unknown names suggest close matches |
| Types | `date`, `time`, `localtime`, `datetime`, `localdatetime` (IANA zones), `duration`, `point` (cartesian and WGS-84) with ISO 8601 parsing, arithmetic, truncation and `distance()` |
| Procedures | `db.labels`, `db.propertyKeys`, `db.relationshipTypes`, `db.indexes`, `db.constraints`, `db.schema.nodeTypeProperties`, `db.schema.relTypeProperties`, `db.awaitIndexes`, `db.index.fulltext.queryNodes` |

Unsupported features fail with a clear `Unsupported` error instead of a wrong
answer: `PROFILE`, nested `CALL { }`, `db.schema.visualization()` and
user-defined procedures. What is planned next is in
[docs/roadmap.md](docs/roadmap.md).

## Embedding grafile

### Rust

grafile is not on crates.io yet; depend on the repository (pin a release
with `tag = "vX.Y.Z"`):

```toml
[dependencies]
grafile = { git = "https://github.com/Peloponeso31/grafile" }
```

```rust
use grafile::api::{Connection, Params};

fn main() -> Result<(), grafile::Error> {
    let mut conn = Connection::create("graph.grafile")?; // `Connection::open` for an existing file
    conn.execute("CREATE (:Person {name: 'alice'})-[:KNOWS]->(:Person {name: 'bob'})")?;

    let mut params = Params::new();
    params.insert("name", "alice");
    let rows = conn.query(
        "MATCH (:Person {name: $name})-[:KNOWS]->(friend) RETURN friend.name AS friend",
        &params,
    )?;
    for row in rows.iter() {
        println!("{row:?}"); // [Text("bob")]
    }

    // Statements autocommit; group them explicitly when needed.
    conn.begin()?;
    conn.execute("CREATE (:Person {name: 'carol'})")?;
    conn.commit()?;
    Ok(())
}
```

Every error carries an `ErrorCode` (`Syntax`, `Constraint`, `Busy`,
`Corrupt`, ...) and a message; `query_with_phase` also tells compile-time
from runtime failures. `explain` prints the logical plan and
`explain_program` the bytecode. `database_mut` exposes the engine for bulk
loading, and `Database::check_integrity` / `rebuild_derived` back the
`check` command.

### C and other languages

`grafile-ffi` builds `libgrafile.so` / `libgrafile.dylib` / `grafile.dll` and
ships a hand-written `grafile.h`. The API follows SQLite's shape:
`prepare`/`step`/`column`, JSON helpers and SQLite-compatible result codes.

```c
GrafileDb *db = NULL;
char *err = NULL;
if (grafile_open("graph.grafile", &db, &err) != GRAFILE_OK) { /* report err */ }

char *json = NULL;
grafile_query_json(db, "MATCH (n:Person) RETURN n.name AS name", NULL, &json);
puts(json);
grafile_free(json);
grafile_close(db);
```

After the install script, link with pkg-config:

```console
$ export PKG_CONFIG_PATH="$HOME/.local/lib/pkgconfig:$PKG_CONFIG_PATH"
$ cc app.c $(pkg-config --cflags --libs grafile)
```

Ownership, threading and error handling are documented in
[docs/ffi.md](docs/ffi.md); runnable C, Go, Python and Dart examples live in
[`ffi/examples/`](ffi/examples). Within 0.x the ABI only grows: check
`grafile_libversion()` (currently 2000) against `GRAFILE_VERSION_NUMBER`.

## Performance

Measured with Criterion on an Apple M5, in memory, release build:

| Workload | Time |
| --- | ---: |
| Indexed point lookup, 50 000 nodes | 3.5 µs |
| Lookup by a variable key (`{id: uid}`), 50 000 nodes | 5.0 µs |
| One-hop traversal | 5.2 µs |
| Count a label, 50 000 nodes | 0.89 ms (18 ns per node) |
| Count a two-label conjunction, 50 000 nodes | 93 µs |
| `sum`/`count`/`avg` over 1 million rows | 36 ms |
| `shortestPath` across a 10x10 grid | 102 µs |
| `CREATE INDEX` over 50 000 existing nodes | 7.0 ms |
| Seed script, 23 845 nodes and 131k relationships (1/100 scale, real file) | 1.6 s |
| Seed script, 2.25 M nodes and 13 M relationships (full size, real file) | 460 s |

Write workloads are compared with SQLite and, together with the
methodology and the before/after history of every optimization, live in
[docs/benchmarks.md](docs/benchmarks.md).

## Reliability

- **Transactions.** A SQLite-style rollback journal: readers take a shared
  lock, writers an exclusive one, and a crash at any point leaves either the
  old or the new state. `IN TRANSACTIONS` batches commit as they go, so
  earlier batches survive a later failure.
- **Bounded sorts.** An `ORDER BY` without `LIMIT`, or a `DISTINCT`, buffers
  up to 256 MiB of rows (`Connection::set_sort_memory`) and then spills
  sorted runs to `<db>-sort-*` files, merged and removed when the query
  ends.
- **Checksums.** Every page carries a CRC32C, verified on every read.
- **Fault injection.** An in-memory VFS fails or tears writes at every I/O
  operation; property tests then reopen the database and require the old
  or new state exactly, with a clean integrity report.
- **The checker as an oracle.** Every integration test ends by asserting a
  clean `check` report, and model-based tests mirror random write sequences
  in a reference graph.
- **Compatibility.** Files written by every release since 0.2.0 are checked
  in, and each build must open them, check them clean and answer the same
  queries identically.
- **Conformance.** The openCypher TCK runs on every pull request; the format
  is specified in [docs/format.md](docs/format.md) and stays backward
  compatible.

## Developing grafile

### Architecture

grafile follows SQLite's layering, with a bitmap incidence matrix as the
adjacency index:

```
Cypher text -> cypher (lexer, parser) -> planner -> codegen -> vdbe (bytecode VM)
            -> engine -> btree / matrix -> pager -> vfs
```

| Layer | Module | Responsibility |
| --- | --- | --- |
| Front end | `cypher` | logos lexer, recursive-descent parser, AST |
| Planner | `planner` | anchor choice (index seek over label scan over full scan), `WHERE` push-down, expansion order |
| Code generator | `codegen` | continuation-passing compilation of plans to bytecode |
| VM | `vdbe` | register machine: cursor loops, streaming aggregation, bounded and spilling sorts, subroutines |
| Engine | `engine` | node and relationship operations, index maintenance, shortest paths, integrity checker |
| Incidence matrix | `matrix` | 256 x 256-bit sparse/dense bitmap fragments per direction, type and label |
| Records | `record` | record codec and order-preserving index keys |
| Catalog | `catalog` | labels, types, property keys, index and constraint definitions |
| Fulltext | `fulltext` | analyzer, inverted index, BM25-style scoring |
| B+tree | `btree` | slotted pages, overflow chains, cursors, bulk build |
| Pager | `pager` | page cache with a clean-page LRU, rollback journal, freelist, checksums |
| OS interface | `vfs` | `StdVfs` and a fault-injecting in-memory VFS |
| API and CLI | `api`, `main` | `Connection`, `Params`, `Rows`; shell and script runner |

Worth knowing before changing storage code:

- **Adjacency.** Relationships are bits in fixed-size fragments (256 nodes by
  256 relationships) stored as B+tree records: an out-plane and an in-plane
  per relationship type, plus label and type bitmaps. Fragments stay sparse
  (sorted offsets) until they fill, then switch to dense words.
- **Pages.** 4 KiB by default, each with a CRC32C computed once at commit.
  The header holds the format version, UUID, freelist and tree roots; the
  layout is in [docs/format.md](docs/format.md).
- **Determinism.** Results never depend on hash-map iteration order: planner
  scopes are insertion-ordered and records keep properties sorted by key.

### Repository layout

```
src/          library and CLI (main.rs); one directory per layer above
tests/        end-to-end suites by feature area, CLI tests, shared helpers in support/
benches/      Criterion: storage, queries, insert (with SQLite baselines)
ffi/          C ABI crate, grafile.h and language examples
tck-runner/   openCypher TCK harness (see docs/tck.md)
third_party/  vendored openCypher TCK (git submodule)
scripts/      FFI build and smoke tests, release fixture generator
docs/         format.md, ffi.md, tck.md, roadmap.md, benchmarks.md
```

### Build, test and benchmark

```console
$ cargo build --release
$ cargo test --workspace                                   # unit, property, crash, integration, FFI
$ cargo test --release --test seed_fixture -- --ignored    # 1/100 seed acceptance test
$ git submodule update --init && cargo run -p tck-runner --release   # openCypher TCK gate
$ cargo bench --bench queries                              # or storage, insert
$ bash scripts/ffi-smoke.sh                                # C and Python against the shared library
```

The TCK gate fails on unexpected failures, on harness errors and on stale
allow-list entries, so a fix must prune its allow-list entries in the same
commit. The current allow-list holds the 52 procedure-registration scenarios
(a non-goal) and one nested pattern comprehension; see
[docs/tck.md](docs/tck.md).

To compare performance before and after a change, save a Criterion baseline
(`cargo bench -- --save-baseline main`) and compare against it
(`cargo bench -- --baseline main`). Run benchmarks one at a time on an idle
machine.

### Contributing

Before a commit, run what CI runs:

```console
$ cargo fmt --all --check
$ cargo clippy --workspace --all-targets -- -D warnings
$ RUSTDOCFLAGS="-D warnings" cargo doc -p grafile --no-deps
$ cargo test --workspace
```

Plus the TCK gate whenever query behavior changes. Commits follow
[Conventional Commits](https://www.conventionalcommits.org/).

House rules:

- **No feature without TCK coverage.** Features the TCK does not exercise
  (DDL, fulltext, subqueries) need unit and integration tests and must not
  regress the TCK report.
- **Harness errors are never allow-listed.** Fix the runner instead.
- **Storage changes follow [docs/format.md](docs/format.md)**, bump the
  format minor version and keep readers backward compatible. Changes to
  storage, the incidence matrix or write paths come with checker coverage and
  a boundary case in `src/engine/model_tests.rs`.
- **Performance changes come with benchmark evidence.** A fast path ships
  only when it beats the code it replaces on the recorded machine; the
  simpler path stays as the test oracle.
- **No panics on user input.** No `unwrap` in library code, and `unsafe` only
  with a documented safety invariant.
- **Releases.** Bump the version in `Cargo.toml` and `ffi/Cargo.toml`, run
  `scripts/version-fixture.sh` and commit
  `tests/fixtures/versions/<version>.grafile`.
- **AI-assisted work is welcome.** It is reviewed against the description in
  your pull request like any other contribution.

## License

MIT; see [LICENSE](LICENSE).
