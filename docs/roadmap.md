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
  `DELETE`, `DETACH DELETE`, `UNION` / `UNION ALL`.
- Patterns: node and relationship patterns, directions, type disjunction
  (`:A|B`), inline literal property filters, multiple labels, pre-bound
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
  `properties`, `keys`, `nodes`, `relationships`, `length`; list functions
  (`range`, `head`, `last`, `tail`, `reverse`, `split`, `substring`); string
  functions (`toUpper`, `toLower`, `trim`/`ltrim`/`rtrim`, `left`, `right`,
  `replace`); math functions (`abs`, `sign`, `ceil`, `floor`, `round`,
  `sqrt`, `exp`, `log`, `log10`, trigonometry, `degrees`/`radians`,
  `haversin`, `pi`, `e`, `rand`).
- Temporal and spatial values: `date`, `localtime`, `time`, `localdatetime`,
  `datetime` (fixed offsets and IANA zones), `duration` and `point`, built
  from strings, component maps or other temporal values; ISO 8601
  serialization and parsing (week, ordinal and compact dates, named zones,
  the alternative duration form); component accessors; calendar and clock
  arithmetic; `duration.between`/`inMonths`/`inDays`/`inSeconds`;
  `*.truncate` from millennium to microsecond; `distance()` for cartesian
  and WGS-84 points; statement clock functions
  (`date.transaction`/`realtime`/`statement` and friends).

Explicit gaps (currently clear `Unsupported` errors):

| Gap | Where rejected |
|-----|----------------|
| `REMOVE n = {…}` | codegen |
| Map projections (`n{.name}`) | parser |
| Unknown functions | codegen (by design) |

Silently absent areas: none of the openCypher value types.

Known semantic deviations to close:

- Comparing values of different temporal types (a date with a datetime, for
  example) yields null instead of Neo4j's error; the Phase 4 comparison
  sweep pins the exact rules.
- `+` does not coerce the non-string operand to text, so `'p-' + 1` errors.
- A pattern comprehension inside a list comprehension is rejected (one
  allow-listed scenario).

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

### Phase 5 — Neo4j extensions (medium) [pending]

- Cypher DDL for indexes and constraints (today they exist only through the
  Rust API, and the shell cannot create or drop them):
  - `CREATE [CONSTRAINT|INDEX] name [IF NOT EXISTS] FOR (n:Label) …` and the
    edge form `FOR ()-[r:TYPE]-() …`, plus `DROP … name [IF EXISTS]`.
  - Constraint kinds: `IS UNIQUE`, `IS NOT NULL` (existence, enforced on
    create, property removal, label addition and whole-map replacement) and
    `REQUIRE (n.a, n.b) IS NODE KEY` (existence plus tuple uniqueness over
    composite index keys). Composite indexes stay non-seekable initially.
  - Reject the Neo4j 3.x forms (`CREATE INDEX ON :L(p)`, `… ASSERT …`) with a
    message pointing at the modern syntax; `SHOW INDEXES`/`SHOW CONSTRAINTS`
    remain unsupported.
  - Storage: index definitions gain a name, key list and kind; the catalog
    entry encoding changes with a format minor bump and stays readable in
    both directions. Dropping an index frees its B+tree pages.
  - No TCK coverage (openCypher defines no DDL): verified by unit and
    integration tests, and the conformance gate must not regress.
- `LOAD CSV [WITH HEADERS] FROM …` for local file paths.
- Label expressions in patterns: `:A|B`, `:!A`.

These stay allow-listed as `neo4j-extension` until this phase.

### Phase 6 — Performance for the new surface (medium, interleaved) [pending]

- Streaming cursors for `OPTIONAL MATCH` and variable-length expansion
  (avoid full materialization).
- Bidirectional shortest-path search with early exit.
- Memory-bounded sort and aggregation (spill to temporary B-trees).

## Milestones

The gate keeps the applicable pass rate at 100% by construction (allow-listed
scenarios are excluded), so milestones are stated against the whole suite.

| Milestone | Target |
|-----------|--------|
| After Phases 1–2 | 2301 of 3897 scenarios pass (59.0%) |
| After Phase 3 | 3370 of 3897 scenarios pass (86.5%) |
| After Phase 4 | ≥ 93% of the whole suite; deviations closed |
| After Phase 5 | Allow-list contains only non-goals (e.g. registered procedures) |

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
4. Features the TCK does not exercise (DDL, operator coercions, `stDev`) are
   planned here explicitly and verified by unit and integration tests; they
   must not regress the TCK report either.
