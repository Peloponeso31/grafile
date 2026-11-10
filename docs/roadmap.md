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
  `reduce()`, `CASE`, `IS [NOT] NULL`, `IS [NOT] :: TYPE` / `IS TYPED`
  (GQL types with `NOT NULL`, `LIST<T>` and unions), `STARTS WITH` /
  `ENDS WITH` / `CONTAINS` / `=~`, `IN`, `||` string and list
  concatenation, map projections (`n{.name, .*, key: expr, var}`),
  `COUNT { … }` and `EXISTS { … }` subqueries, SQL-style
  `trim(BOTH|LEADING|TRAILING … FROM s)`, label predicates, three-valued
  logic, numeric cross-type comparison.
- Aggregations: `count`, `sum`, `avg`, `min`, `max`, `collect`,
  `percentileCont`, `percentileDisc`, `stDev`, `stDevP`, with `DISTINCT`,
  multiple arguments and expressions over aggregations.
- Scalar functions: `id`, `labels`, `type`, `startNode`, `endNode`,
  `coalesce`, `toString`, `toInteger`, `toFloat`, `toBoolean`, `size`,
  `properties`, `keys`, `nodes`, `relationships`, `length`, `randomUUID`,
  `elementId`, `valueType`, `nullIf`, `isEmpty`, `isNaN`, the `…OrNull`
  conversions and `to…List` list conversions;
  list functions (`range`, `head`, `last`, `tail`, `reverse`, `split`,
  `substring`); string functions (`toUpper`/`upper`, `toLower`/`lower`,
  `trim`/`btrim`/`ltrim`/`rtrim` with optional character sets,
  `char_length`, `left`, `right`, `replace`); math functions (`abs`, `sign`, `ceil`,
  `floor`, `round`, `sqrt`, `exp`, `log`, `log10`, trigonometry,
  `degrees`/`radians`, `haversin`, `pi`, `e`, `rand`).
- Built-in procedures: `db.labels()`, `db.propertyKeys()`,
  `db.relationshipTypes()`, `db.indexes()`, `db.constraints()`,
  `db.schema.nodeTypeProperties()`, `db.schema.relTypeProperties()` and the
  `db.awaitIndexes([timeout])` no-op; `CALL … YIELD … WHERE` filters the
  yielded columns, and `SHOW [ALL] INDEXES|CONSTRAINTS [YIELD …] [WHERE …]`
  calls the first two.
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
- Statement prefixes: `EXPLAIN` returns the plan as rows (one `plan`
  column) without running the statement; `PROFILE` is rejected by name.
  Unknown functions and procedures suggest the closest names.
- Execution: index seeks on bound values and pushed-down `WHERE`
  equalities, label-bitmap intersection for multi-label scans, streaming
  aggregation, `OPTIONAL MATCH` and variable-length expansion, bounded
  top-k sorts, spilling sorts, bidirectional shortest paths and bulk-built
  index backfill
  (see Phase 8).

Explicit gaps (currently clear `Unsupported` errors):

| Gap | Where rejected |
|-----|----------------|
| Unknown functions | codegen (by design) |

Silently absent areas: none of the openCypher value types.

Known limitations:

- A pattern comprehension inside a list comprehension is rejected (one
  allow-listed scenario; a general fix needs nested-plan evaluation, so it is
  not scheduled).

## Phases

Status markers: `[done]` implemented and gated, `[partial]` some items
finished, `[pending]` not started, `[declined]` closed without building it
(the reason is given). Items the TCK does not exercise are
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
    message pointing at the modern syntax. `SHOW INDEXES`/`SHOW CONSTRAINTS`
    came later, as calls of `db.indexes()`/`db.constraints()`.
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
  by default and CI runs it with `--release --ignored` (Phase 8 brought it
  to 1.6 s).

The TCK does not exercise this surface; unit and integration tests are the
gate, and the report must not regress. Non-goals: Lucene query syntax, a
general user-procedure registry (TCK non-goal), and `CALL {} IN
TRANSACTIONS` inside explicit transactions.

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
  written by 0.2.0, 0.2.1, 0.2.2, 0.3.0 and 0.4.0 are checked in, and
  `tests/compat.rs` requires a clean report and identical snapshot answers from each. The
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

### Phase 8 — Query performance, vectorization and SIMD (large) [done]

This phase absorbs the former Phase 6 items: streaming `OPTIONAL MATCH` and
variable-length expansion, bidirectional shortest-path search, and
memory-bounded sort and aggregation. Every storage or bitmap fast path is
verified against the Phase 7 reference model and checker before it ships.
Every benchmark target below is met; numbers are in `docs/benchmarks.md`.

- [done] Runtime index seeks: `IndexSeek` carries an expression evaluated
  once per input row, so `{id: uid}` seeks the index, and `WHERE n.key =
  expr` moves into the pattern when `n` is introduced by the clause and
  `expr` reads only earlier variables (non-deterministic and pattern
  expressions stay in `WHERE`). Lookups follow Cypher equality: numeric
  twins (both float zeros, every integer that rounds to a large float),
  nothing for null and NaN, and a re-checked superset for lists. B+tree
  descents now binary-search interior pages in place instead of copying
  every divider, halving point lookups.
- [done] Bulk write path: the full-size seed script loads (it used to
  panic: eviction could drop the page just fetched) in 460 s, and the
  1/100 fixture in 1.6 s instead of 140–153 s. Page eviction uses a
  clean-page LRU instead of scanning dirty frames, which cost 92% of the
  full load. The run is now I/O-bound. Literal lists and maps compile to a
  single constant, so `UNWIND [...] AS x CREATE ...` no longer copies the
  list's item registers into every row (76.1 ms to 7.3 ms for 10 000
  rows). Batched node and edge creation and group commits were not needed
  for the target and are not built.
- [declined] Batch/vectorized execution: the targets were met without a
  columnar pipeline. The VM borrows instructions and operands instead of
  cloning them each step, aggregation folds rows into per-group
  accumulators as they arrive (memory bounded by the number of groups, no
  per-row allocation), and `ORDER BY … [SKIP] LIMIT` keeps only the best
  `skip + limit` rows, rejecting candidates before copying them. A
  1024-row columnar pipeline with `wide` kernels is not built: per-row
  cost is interpretation and record access, not arithmetic.
- [done] Bitmap set operations: `Fragment::intersect_with` (sorted merge,
  bit tests or word AND by representation) and band-wise label
  intersection serve multi-label scans, so `(n:A:B)` reads no records
  (33 ms to 93 µs at 50k nodes). The word loops already compile to NEON; a
  `wide` version tied on AND and lost 30% on cardinality, so no SIMD
  dependency ships. OR, XOR, rank/select and batched expansion have no
  caller yet and are not built.
- [done] Streaming `OPTIONAL MATCH`: inner matches feed one copy of the
  downstream code through a `Gosub`/`Return` subroutine, and the
  null-extended row takes the same path, so no rows are buffered.
- [done] Streaming variable-length expansion: an explicit depth-first
  stack (`Database::var_expansion`/`next_trail`) yields trails in the
  former order, with memory bounded by depth times degree; a property test
  compares it with the recursive enumeration.
- [done] Bidirectional shortest paths: with a bound target, a
  bidirectional breadth-first search over node distances finds the
  distance, and a depth-first walk over nodes on shortest paths lists the
  trails in the order the old path enumeration did (that enumeration, which
  kept every simple path in its frontier, remains for an unbound target and
  for a minimum above the distance, and is the property-test oracle).
  `shortestPath` across a 10x10 grid: 87.4 ms to 102 µs.
- [done] External merge sort: an unbounded `ORDER BY` or a `DISTINCT`
  buffers rows up to a sort budget (`Connection::set_sort_memory`, 256 MiB
  by default), then writes stably sorted runs to `<db>-sort-<pid>-<n>`
  files through the database's VFS, and a k-way merge streams them back.
  Ties go to the earlier run, so the output equals the in-memory stable
  sort; `DISTINCT` also deduplicates each run. Spill files are removed when
  the cursor drops, on success, error or an early stop. Runs sort with
  `rayon`'s stable parallel sort: one million rows in 280 ms in memory
  (450 ms before) and 309 ms with a 16 MiB budget.
- [done] Parallel index backfill: backfills gather keys into one buffer,
  sort them with `rayon`, detect unique violations as adjacent equal
  values and fill the empty tree with `Btree::build_sorted` (packed leaves,
  then interior levels). 50k nodes: 25.0 ms to 7.0 ms (unique 35.0 ms to
  7.1 ms); the parallel sort is about 10% of the gain. Record decoding
  stays serial because every page read goes through the single pager.
  Fulltext backfill reads records serially, then analyzes texts and sorts
  postings on every core and bulk-builds the tree: 50k texts, 171 ms to
  34 ms.
- [done] Benchmark gates: Criterion groups for seek versus scan,
  variable-key lookups, multi-label counts, one-million-row aggregates and
  filters, and dense fragment kernels; results recorded in
  `docs/benchmarks.md`.

| Workload | Baseline | Target | Result |
|----------|----------|--------|--------|
| Variable-key point lookup, 50k nodes | 17.2 ms (label scan) | ≤ 10 µs (seek) | 4.97 µs |
| Seed load, 1/100 scale | 140–153 s (release CLI) | ≥ 3x throughput | 1.6 s (~90x) |
| Numeric filter, 1M rows | 90.8 ms | ≥ 1.5x | 55.2 ms (1.64x) |
| `sum`/`count`/`avg`, 1M values | 138 ms | ≥ 2x | 36.1 ms (3.8x) |
| Dense 8 KiB bitmap AND/OR/cardinality | scalar words | ≥ 1.5x | not met by SIMD; scalar already vectorized |
| Unbounded sort, 1M rows | 450 ms | spills within budget | 280 ms in memory, 309 ms spilled at 16 MiB |

Declined, with the reasons the phase closed on:

- A columnar pipeline (1024-row batches with `wide` kernels): per-row cost
  is interpretation and record access, not arithmetic.
- Parallel scans and parallel record decoding: every page read goes through
  one `&mut` pager; this needs a shared, `Sync` page cache first.
- Bitmap OR, XOR, rank/select and batched expansion: no caller needs them.
- Batched node and edge creation and group commits: the seed load is
  I/O-bound and met its target without them.

`std::simd` is still unstable on rustc 1.99.1, so kernels would use `wide`.
Any `std::arch` specialisation is isolated, runtime-detected and
documented; `-C target-cpu=native` is never required. Benchmarks run
serially, and a SIMD path ships only when it beats the scalar reference on
the recorded machine, with the scalar path retained as the differential
oracle.

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
| After Phase 8 | Bound equality lookups use index seeks; unbounded sorts spill; benchmark targets met (bitmap SIMD target withdrawn: scalar is already vectorized) |

Phase 4 achieved 3844 of 3897 scenarios (98.6%); the allow-list holds the
52 CALL procedure scenarios (non-goal) and one nested pattern
comprehension.

## Non-goals

- User-defined procedures and functions beyond built-ins.
- Remote `LOAD CSV` over HTTP (local files first; revisit on demand).
- GQL-only syntax beyond what openCypher 9 specifies.
- `db.schema.visualization()`: it returns virtual nodes and relationships,
  and grafile values can only reference stored entities.
- The Phase 8 items declined for lack of a workload: a columnar pipeline,
  pager-parallel scans, bitmap OR/XOR/rank/select, batched expansion and
  group commits. A workload that needs one reopens it with a benchmark.

### Quality-of-life backlog (delivered)

The conveniences listed here against 0.3.0 have all shipped: shell history,
completion, `.param` and `--param`, `~/.grafilerc`, `--readonly`,
`EXPLAIN`, the null-safe and list conversions and other Neo4j 5 helpers,
map projections, `COUNT { }`, `||`, `IS :: TYPE`, SQL-style `trim`,
`SHOW INDEXES`/`SHOW CONSTRAINTS`, the schema property procedures, did-you-mean
suggestions and the named `PROFILE` error. New small items go here.

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
