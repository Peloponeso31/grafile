# grafile roadmap: openCypher completeness

This document is the execution plan for taking grafile from its v1 Cypher
subset to full conformance with openCypher 9, plus the Neo4j extensions the
project has chosen to support. The [openCypher TCK](https://github.com/opencypher/openCypher/tree/main/tck)
is the conformance contract: a feature is "done" when the applicable TCK
scenarios pass without allow-list entries. Features the TCK does not exercise
are done when their unit and integration tests pass and the report does not
regress.

## Current state

Implemented and gated (see `docs/tck.md` for the pass rate):

- Clauses: `MATCH`, `OPTIONAL MATCH`, `WHERE`, `RETURN`, `WITH` (with
  `DISTINCT`, `ORDER BY`, `SKIP`, `LIMIT`, trailing `WHERE`), `UNWIND`,
  `CREATE`, `MERGE` (with `ON CREATE SET` / `ON MATCH SET`), `SET`, `REMOVE`,
  `DELETE`, `DETACH DELETE`, `CALL` for built-ins, `CALL { ... }` unit and
  returning subqueries, `CALL { ... } IN TRANSACTIONS OF n ROWS`,
  `UNION` / `UNION ALL`.
- Patterns: node and relationship patterns, directions, type disjunction
  (`:A|B`) and label expressions (`:A|B`, `:A&B`, `:!A`), inline literal
  property filters, multiple labels, pre-bound
  elements (verified instead of re-bound), variable-length paths (`*`, `*2`,
  `*1..3`, `*0..`) with trail semantics, named paths (`p = …`) with
  `nodes()`, `relationships()` and `length()`.
- Expressions: parameters, list/map literals, map access, list indexing and
  slicing, list comprehensions, quantifiers (`all`, `any`, `none`, `single`),
  `reduce()`, `CASE`, `IS [NOT] NULL`, `STARTS WITH` / `ENDS WITH` /
  `CONTAINS` / `=~`, `IN`, label predicates, three-valued logic, numeric
  cross-type comparison.
- Aggregations: `count`, `sum`, `avg`, `min`, `max`, `collect`,
  `percentileCont`, `percentileDisc`, `stDev`, `stDevP`, with `DISTINCT`,
  multiple arguments and expressions over aggregations.
- Scalar functions: `id`, `labels`, `type`, `startNode`, `endNode`,
  `coalesce`, `toString`, `toInteger`, `toFloat`, `toBoolean`, `size`,
  `properties`, `keys`, `nodes`, `relationships`, `length`, `randomUUID`;
  list functions (`range`, `head`, `last`, `tail`, `reverse`, `split`,
  `substring`); string functions (`toUpper`, `toLower`, `trim`/`ltrim`/
  `rtrim`, `left`, `right`, `replace`); math functions (`abs`, `sign`, `ceil`,
  `floor`, `round`, `sqrt`, `exp`, `log`, `log10`, trigonometry,
  `degrees`/`radians`, `haversin`, `pi`, `e`, `rand`).
- Built-in procedures: `db.labels()`, `db.propertyKeys()`,
  `db.relationshipTypes()` and the `db.awaitIndexes([timeout])` no-op.
- Temporal and spatial values: `date`, `localtime`, `time`, `localdatetime`,
  `datetime` (fixed offsets and IANA zones), `duration` and `point`, built
  from strings, component maps or other temporal values; ISO 8601
  serialization and parsing (week, ordinal and compact dates, named zones,
  the alternative duration form); component accessors; calendar and clock
  arithmetic; `duration.between`/`inMonths`/`inDays`/`inSeconds`;
  `*.truncate` from millennium to microsecond; `distance()` for cartesian
  and WGS-84 points; statement clock functions
  (`date.transaction`/`realtime`/`statement` and friends).
- Schema DDL: `CREATE INDEX`/`CREATE CONSTRAINT` with `IS UNIQUE`,
  `IS NOT NULL` and `IS NODE KEY`, `DROP INDEX`/`DROP CONSTRAINT` with
  `IF [NOT] EXISTS`, and the edge forms; constraints are enforced on every
  write path (see Phase 5). `CREATE FULLTEXT INDEX … ON EACH [...]` with
  `db.index.fulltext.queryNodes`. `LOAD CSV [WITH HEADERS] FROM file AS row`
  reads local CSVs into the row pipeline.
- Integrity: `grafile check FILE [--rebuild]` verifies every derived
  structure against the records and the page layout (see Phase 7).

Explicit gaps (currently clear `Unsupported` errors):

| Gap | Where rejected |
|-----|----------------|
| `REMOVE n = {…}` | codegen |
| Map projections (`n{.name}`) | parser |
| Unknown functions | codegen (by design) |

Silently absent areas: none of the openCypher value types.

Known limitations:

- A pattern comprehension inside a list comprehension is rejected (one
  allow-listed scenario; a general fix needs nested-plan evaluation, so it is
  not scheduled).

## Phases

Status markers: `[done]` implemented and gated, `[partial]` some items
finished, `[pending]` not started. Items the TCK does not exercise are
verified by unit and integration tests and must not regress the report.

Current baseline: 3844 of 3897 scenarios passing (98.6%), 53 allow-listed.
The remaining entries are the CALL procedure registration family (52: the
TCK registers test procedures, a non-goal) and one nested pattern
comprehension scenario.

### Phase 1 — Core language completion (large) [done]

#### 1A — Statement and expression surface [done]

- `UNION` / `UNION ALL` (10 scenarios), `SET n += {…}` and runtime property
  key resolution (55), aggregation completeness with `DISTINCT` and
  post-aggregation expressions (21), map access, subscript, list slicing and
  list concatenation (64).

#### 1B — Patterns and paths [done]

- [done] Variable-length paths: trails with `*`, `*2`, `*1..3`, `*0..`; the
  relationship variable binds a list (41 scenarios).
- [done] Named paths and path values: `p = …`, `nodes(p)`,
  `relationships(p)`, `length(p)`, plus the variable re-binding validations
  (93 scenarios).
- [done] Pattern predicates in `WHERE` (including `NOT`, `AND`, `OR`) and
  `exists()`, plus pattern comprehensions (`[p = (a)-->(b) WHERE … | …]`)
  anywhere an expression is allowed (36 scenarios). Nested pattern
  expressions inside list comprehensions remain rejected: their bodies are
  evaluated per element without plan nodes (one allow-listed scenario).
- [done] `shortestPath()` / `allShortestPaths()`: a breadth-first search over
  simple paths, both as a `MATCH` pattern and as an expression (null when no
  path exists), honouring direction, type filters, bounds and unbound
  endpoints. TCK-blind: unit and integration tests only.
- [done] Non-literal property values in `MATCH` patterns (`{key: expr}`)
  compile to runtime filters while literals keep index seeks; `MERGE` already
  supports expressions. TCK-blind: unit and integration tests only.

#### 1C — Comprehensions, quantifiers, reduce [done]

- List comprehensions, quantifiers with three-valued logic and `reduce()`
  (556 scenarios).

#### 1D — Write-side completeness [done]

- [done] MERGE conformance: the whole path matches or is created atomically
  (including multiple relationships), matching honours the written
  direction, `p = …` binds the path, bound nodes cannot gain labels,
  variable-length patterns and relationship re-binding are rejected,
  null property values are rejected, `n[key]`/`r[key]` dynamic property
  access works and `SET x = entity` copies properties.
- [done] `FOREACH (x IN list | updating clauses)` with nested loops, and
  `CALL … YIELD` for the built-in procedures `db.labels()`,
  `db.propertyKeys()` and `db.relationshipTypes()`. TCK-blind: unit and
  integration tests only.

### Phase 2 — Function library (medium) [done]

- [done] String: `substring`, `replace`, `split`, `trim`/`ltrim`/`rtrim`,
  `left`/`right`, `toUpper`/`toLower`, `reverse`.
- [done] Math: `abs`, `ceil`, `floor`, `round`, `sign`, `sqrt`, `exp`, `log`,
  `log10`, trigonometry (`sin`, `cos`, `tan`, `cot`, `asin`, `acos`, `atan`,
  `atan2`, `haversin`), `degrees`/`radians`, `pi`, `e`, `rand` (fastrand).
- [done] List: `head`, `last`, `tail`, `range` (capped at ten million
  elements), `reverse` for strings and lists.
- [done] Conversions: `toBoolean`, and `toInteger`/`toFloat`/`toString`
  follow the TCK matrix (float strings truncate, invalid types raise a
  runtime `TypeError`).
- [done] Aggregation: `stDev`, `stDevP` (Welford, TCK-blind) and
  `percentileCont`/`percentileDisc`, which required multi-argument
  aggregation plumbing (value plus percentile in the aggregate window).

### Phase 3 — Temporal and spatial types (large, cross-cutting) [done]

- [done] New `Value` variants `Date`, `LocalTime`, `Time`, `LocalDateTime`,
  `DateTime` (offset plus optional IANA zone name), `Duration` (Neo4j's
  months/days/seconds/nanoseconds that never fold into each other, with the
  nanosecond field in `[0, 1e9)`) and `Point`; total ordering and equality.
- [done] Record encode/decode tags, order-preserving index-key encodings,
  JSON rendering, FFI column kinds, CLI output; storage stays backward
  compatible (new tags only) with the format minor bumped to 2.
- [done] Function surface: `date`, `localtime`, `time`, `localdatetime`,
  `datetime`, `duration` and `point` constructors (strings, field maps with
  calendar/week/ordinal/quarter keys, whole-value composition and timezone
  resolution through the bundled IANA database), `distance`, component
  accessors, `toString`, statement clocks, `*.truncate` (millennium through
  microsecond), calendar/clock arithmetic and the
  `duration.between`/`inMonths`/`inDays`/`inSeconds` family.

### Phase 4 — Semantic conformance sweep (medium, iterative) [done]

TCK-driven fixes: null semantics corners, cross-type comparison and
ordering, `IN` with nulls, list equality, error-vs-null decisions, scope
boundaries across `WITH`, `SKIP`/`LIMIT` expression rules, and removal of
the remaining documented deviations.

Delivered: comparison and ordering semantics (cross-type equality, map
equality, NaN, list lexicographic order, Neo4j's type order in `ORDER BY`),
operator precedence and chained comparisons, hex/octal and extreme number
literals, relationship distinctness and variable-length pattern rules,
static kind checks with compile-time type errors, pattern binding and
aggregation scope rules (`AmbiguousAggregationExpression`, ORDER BY over
projections), runtime argument checks, deleted-entity semantics, eager
evaluation of updating clauses, `EXISTS { … }` subqueries and source-text
column names.

Not delivered: pattern comprehensions nested inside list comprehensions
(evaluated per element without plan nodes; one scenario stays allow-listed).

### Phase 5 — Neo4j extensions (medium) [done]

- [done] Cypher DDL for indexes and constraints:
  - `CREATE [CONSTRAINT|INDEX] name [IF NOT EXISTS] FOR (n:Label) …` and the
    edge form `FOR ()-[r:TYPE]-() …`, plus `DROP … name [IF EXISTS]`.
  - Constraint kinds: `IS UNIQUE`, `IS NOT NULL` (existence, enforced on
    create, property removal, label addition and whole-map replacement) and
    `REQUIRE (n.a, n.b) IS NODE KEY` (existence plus tuple uniqueness over
    composite index keys). Composite indexes stay non-seekable initially.
  - Reject the Neo4j 3.x forms (`CREATE INDEX ON :L(p)`, `… ASSERT …`) with a
    message pointing at the modern syntax; `SHOW INDEXES`/`SHOW CONSTRAINTS`
    remain unsupported.
  - Storage: index definitions gained a name, key list and kind. The catalog
    format minor is 3: named objects use new catalog entry kinds and
    `IS NOT NULL` constraints take entries of their own, so builds from
    minor 2 reject files that contain them while minor-3 builds read both
    encodings. Dropping an index frees its B+tree pages.
  - No TCK coverage (openCypher defines no DDL): verified by unit and
    integration tests, and the conformance gate must not regress.
- [done] `LOAD CSV [WITH HEADERS] FROM expr AS var [FIELDTERMINATOR char]`
  for local paths and `file://` URIs (the `csv` crate parses RFC 4180);
  HTTP stays a non-goal. Headers bind a string map per row, plain files a
  list of strings, and short rows fill nulls.
- [done] Label expressions in patterns: `:A|B`, `:A&B`, `:!A` (negation and
  conjunction are rejected on variable-length and shortest-path patterns).

The vendored TCK has no scenarios for these extensions, so none of them is
allow-listed; the allow-list only holds the CALL procedure non-goal and one
nested pattern comprehension.

### Phase 6 — Neo4j bulk-load surface (large) [done]

The acceptance fixture is the audited `seed_big.cypher` (Neo4j 5 style,
e-commerce/social seed). It must run end to end through the script runner at
1/100 scale; full-size loading is a Phase 8 performance target, not a Phase 6
gate.

- [done] Statement runner: make `split_statements` skip `//` line comments and
  `/* ... */` block comments when finding top-level semicolons, and drop
  comment-only statements. The audited script used to abort before its first
  statement because a commented-out wipe ends in `;`. The runner reports the
  failing statement number and shows progress with `indicatif`.
- [done] Procedure ABI and scalar functions: accept typed arguments through a
  procedure table, implement `db.awaitIndexes([timeout])` as a no-op (indexes
  backfill synchronously), and add `randomUUID()`.
- [done] `CALL { ... }` subqueries: parse and plan unit and returning forms,
  correlated through the leading `WITH` (and optionally `CALL (x) { ... }`).
  Unit subqueries compile inline per input row; returning subqueries collect
  the inner rows and join them to the outer row. Nested `CALL {}` stays
  explicitly unsupported until required.
- [done] `IN TRANSACTIONS OF n ROWS`: allowed only after a unit subquery.
  Each input row counts toward the batch; the VM commits and reopens the write
  transaction at every boundary, so earlier batches survive a later failure.
  Explicit transactions are rejected, and the parser defaults the batch size
  to 1000 rows.
- [done] Fulltext indexes: `CREATE FULLTEXT INDEX name [IF NOT EXISTS] FOR
  (n:Label) ON EACH [n.prop, ...]` and `DROP INDEX`; a new catalog entry kind
  (format minor 4) whose inverted B+tree maps `term 0x00 node id` to token
  positions; maintenance on create, property set and replacement, label
  change and delete; backfill with a progress bar;
  `db.index.fulltext.queryNodes(index, query)` yielding `node, score`
  (BM25-style idf with square-root term frequency). Queries cover terms,
  phrases and `AND`/`OR`; the analyzer (split on non-alphanumerics,
  lowercase) is explicitly not Lucene-compatible. Relationship fulltext
  indexes are rejected as unsupported.
- [done] Acceptance fixture: `tests/fixtures/seed_1_100.cypher` is the
  hardcoded-size seed script divided by 100, and `tests/seed_fixture.rs`
  asserts every node count, the fixed relationship counts, bounds on the
  random fan-out and the sample fulltext query. It takes about 150 s in a
  release build (variable-key lookups still scan labels), so it is ignored
  by default and CI runs it with `--release --ignored`.

The TCK does not exercise this surface; unit and integration tests are the
gate, and the report must not regress. Non-goals: Lucene query syntax, a
general user-procedure registry (TCK non-goal), `SHOW INDEXES`/`SHOW
CONSTRAINTS`, and `CALL {} IN TRANSACTIONS` inside explicit transactions.

### Phase 7 — Data integrity verification (medium) [done]

The 0.2.1 band-boundary bug (expanding node ids divisible by 256 returned
neighbors' relationships) went unnoticed because no test compared the derived
structures with the records they derive from. The stored bitmaps were
correct and only the reads were wrong, but `DETACH DELETE` read through the
same path and deleted relationships that belonged to other nodes. Incidence,
label and type fragments, property indexes and constraint state are all
derived from node and edge records, so the records serve as the oracle.

- [done] Integrity checker: read-only `Database::check_integrity()` returns
  an `IntegrityReport` (findings by kind, capped at 100 per kind, plus
  sizes), exposed as `grafile check FILE` (read-only open, `indicatif`
  progress, exit status 1 on findings) and `grafile_check_json()` in the
  FFI. Every implied bit and entry must be present and every stored total
  must equal the record-implied total, which proves set equality without
  materialising the expected sets; a mismatch triggers a targeted scan
  that names the stray bits or stale entries. Covered: out/in bits in the
  typed and any-type planes, edge-type and label bitmaps, node and edge
  property indexes, uniqueness (adjacent equal values), `IS NOT NULL` and
  NODE KEY existence, fulltext postings and positions, record decoding and
  dangling endpoints, header id counters, B+tree ordering and dividers,
  overflow chains, page ownership across all trees (a page claimed twice,
  leaked, or both free and owned) and freelist structure. The 1/100 seed
  (23 845 nodes, 131 470 relationships) checks clean in 0.09 s.
- [done] Derived-structure rebuild: `grafile check --rebuild` /
  `Database::rebuild_derived()` regenerates fragments, property indexes and
  fulltext indexes from records in one write transaction, then re-checks.
  It refuses when the catalog, a record table or the freelist is damaged,
  and abandons (rather than frees) a derived tree whose pages fail
  verification, so a shared page is never freed under a live tree.
- [done] Model-based property tests (`src/engine/model_tests.rs`): create,
  delete, detach delete, set/remove label and property, each committed or
  rolled back, mirrored in a reference graph. After every step outgoing,
  incoming and per-type expansion, label and type scans, index seeks,
  `edge_exists` over every ordered pair, and a clean checker report must
  hold. Start ids are drawn from band boundaries (254, 511, 65 534,
  131 070, 2^24 - 2).
- [done] Boundary sweeps: ids straddling 256, 512, 65 536 + 256, 65 536,
  131 072 and 2^24; label and edge-type fragments crossing `SPARSE_MAX`
  into dense form and back; multi-level trees with deletes that leave stale
  dividers and empty leaves. B+tree merges do not exist yet (deletes never
  rebalance), so there is nothing to sweep there.
- [done] Durability: an engine-level proptest interrupts a commit at a
  random I/O operation (optionally tearing the write), reopens, and requires
  the old or the new state exactly with a clean checker; `tests/integrity.rs`
  checks after a failed `IN TRANSACTIONS` batch, after explicit rollback of
  index, label and detach-delete changes, and after reopening a file with
  freed record, index and overflow pages. The pager-level interrupted-commit
  test writes raw pages outside any tree, so the checker runs at engine
  level instead.
- [done] Cross-version fixtures: `scripts/version-fixture.sh` runs
  `tests/fixtures/versions/fixture.cypher` with a release CLI; the files
  written by 0.2.0, 0.2.1 and 0.2.2 are checked in, and `tests/compat.rs`
  requires a clean report and identical snapshot answers from each. The
  0.2.0 and 0.2.1 binaries themselves answer two snapshot queries wrongly
  on their own clean files (750 instead of 742 relationships): the band
  bug, reproduced. Each release adds its fixture.
- [done] Suite and CI wiring: `tests/support::connection()` returns a guard
  that asserts a clean report when each test ends, the seed acceptance test
  asserts a clean report, the model tests run with a fixed case count (48)
  under `cargo test --workspace`, and `proptest-regressions/` stays checked
  in.

Non-goals: online or incremental checking during normal writes, and repairing
primary records.

### Phase 8 — Query performance, vectorization and SIMD (large) [pending]

This phase absorbs the former Phase 6 items: streaming `OPTIONAL MATCH` and
variable-length expansion, bidirectional shortest-path search, and
memory-bounded sort and aggregation. Every storage or bitmap fast path is
verified against the Phase 7 reference model and checker before it ships.

- Runtime index seeks (highest ROI): plan a seek for equality values bound
  before the scan, not just literals (`IndexSeek` currently stores a constant
  `Value`), and push `WHERE n.key = x` down to the scan. This turns the bulk
  script's `MATCH (u:User {id: uid})` into a lookup: `docs/benchmarks.md`
  records 15.52 ms for the label scan versus 6.82 µs for the indexed seek at
  50k nodes.
- Bulk write path: batched node and edge creation, amortised constraint,
  index and matrix updates, bulk index backfill, and group commits. Report
  nodes/s and edges/s on the 1/100 fixture.
- Batch/vectorized execution: a 1024-row pipeline for scan, filter, project
  and aggregate; selection vectors, columnar `Int`/`Float`/`Bool`, null
  bitmaps, and stable-Rust SIMD kernels (`wide`) for comparison, arithmetic
  and `sum`/`count`/`avg`/`min`/`max`. Heap values fall back to the scalar
  VM; differential `proptest` tests must prove identical results before a
  fast path is enabled.
- SIMD bitmap engine: bulk `Fragment` `and`/`or`/`and_not`/`xor`,
  intersection tests, cardinality, rank/select and batch row operations.
  Dense fragments use `wide`; sparse fragments use sorted merges. Use them
  for multi-label scans, type intersections and batched expansion.
  `count_ones` already lowers to hardware `popcnt`, so only claim measured
  wins.
- Parallel CPU phases: `rayon` for index and fulltext backfill, decoding and
  large sorts/aggregations, with a single writer. Morsel-driven parallel read
  scans are a stretch goal.
- Benchmark gates: Criterion groups for seek versus scan, bitmap operations,
  numeric filters and aggregates, expansion, bulk load and fulltext, with
  before/after numbers recorded in `docs/benchmarks.md`.

| Workload | Baseline | Target |
|----------|----------|--------|
| Variable-key point lookup, 50k nodes | 15.5 ms (label scan) | ≤ 10 µs (seek) |
| Seed load, 1/100 scale | 140–153 s (release CLI) | ≥ 3x throughput |
| Numeric filter, 1M rows | scalar VM | ≥ 1.5x |
| `sum`/`count`/`avg`, 1M values | scalar VM | ≥ 2x |
| Dense 8 KiB bitmap AND/OR/cardinality | scalar words | ≥ 1.5x |

`std::simd` is still unstable on rustc 1.99.1, so kernels use `wide`. Any
`std::arch` specialisation is isolated, runtime-detected and documented;
`-C target-cpu=native` is never required. Benchmarks run serially, and a SIMD
path ships only when it beats the scalar reference on the recorded machine,
with the scalar path retained as the differential oracle.

## Milestones

The gate keeps the applicable pass rate at 100% by construction (allow-listed
scenarios are excluded), so milestones are stated against the whole suite.

| Milestone | Target |
|-----------|--------|
| After Phases 1–2 | 2301 of 3897 scenarios pass (59.0%) |
| After Phase 3 | 3370 of 3897 scenarios pass (86.5%) |
| After Phase 4 | ≥ 93% of the whole suite; deviations closed |
| After Phase 5 | Allow-list contains only non-goals (e.g. registered procedures) |
| After Phase 6 | 1/100 seed script runs end to end; TCK report unchanged |
| After Phase 7 | Checker clean on every suite, fixture and release file |
| After Phase 8 | Bound equality lookups use index seeks; benchmark targets met |

Phase 4 achieved 3844 of 3897 scenarios (98.6%); the allow-list holds the
52 CALL procedure scenarios (non-goal) and one nested pattern
comprehension.

## Non-goals

- User-defined procedures and functions beyond built-ins.
- Remote `LOAD CSV` over HTTP (local files first; revisit on demand).
- GQL-only syntax beyond what openCypher 9 specifies.

## Sequencing rules

1. No feature ships without TCK coverage: a feature is done when its
   applicable scenarios pass and its allow-list entries are pruned in the
   same commit.
2. Each phase lands as conventional commits with tests; the TCK report must
   not regress between phases.
3. Storage-format changes follow `docs/format.md` and bump the format minor
   version; readers remain backward compatible.
4. Features the TCK does not exercise (DDL, fulltext, `CALL {}` subqueries,
   operator coercions, `stDev`) are planned here explicitly and verified by
   unit and integration tests; they must not regress the TCK report either.
5. Performance changes land with Criterion benchmark evidence: a vectorized or
   SIMD path ships only when it beats the scalar reference on the recorded
   machine, and the scalar path stays as the differential oracle.
6. Changes to storage, the incidence matrix or write paths land with
   checker coverage and a boundary case in the Phase 7 model tests. A
   release is cut only when every cross-version fixture opens with a clean
   checker report, and it adds its own fixture.
