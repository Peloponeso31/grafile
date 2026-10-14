# grafile roadmap: openCypher completeness

This document is the execution plan for taking grafile from its v1 Cypher
subset to full conformance with openCypher 9, plus the Neo4j extensions the
project has chosen to support. The [openCypher TCK](https://github.com/opencypher/openCypher/tree/main/tck)
is the conformance contract: a feature is "done" when the applicable TCK
scenarios pass without allow-list entries.

## Current state (v1)

Implemented:

- Clauses: `MATCH`, `OPTIONAL MATCH`, `WHERE`, `RETURN`, `WITH` (with
  `DISTINCT`, `ORDER BY`, `SKIP`, `LIMIT`, trailing `WHERE`), `UNWIND`,
  `CREATE`, `MERGE` (with `ON CREATE SET` / `ON MATCH SET`), `SET`, `REMOVE`,
  `DELETE`, `DETACH DELETE`, `UNION` / `UNION ALL`.
- Patterns: node and relationship patterns, directions, type disjunction
  (`:A|B`), inline literal property filters, multiple labels, pre-bound
  elements (nodes and relationships verified instead of re-bound).
- Expressions: parameters, list/map literals, `CASE`, `IS [NOT] NULL`,
  `STARTS WITH` / `ENDS WITH` / `CONTAINS` / `=~`, `IN`, label predicates,
  three-valued logic, numeric cross-type comparison.
- Aggregations: `count`, `sum`, `avg`, `min`, `max`, `collect`.
- Scalar functions: `id`, `labels`, `type`, `startNode`, `endNode`,
  `coalesce`, `toString`, `toInteger`, `toFloat`, `size`, `properties`,
  `keys`.

Explicit gaps (currently clear `Unsupported` errors):

| Gap | Where rejected |
|-----|----------------|
| Variable-length paths (`*min..max`) | planner |
| `MERGE` with multiple relationships | planner |
| Expressions mixing aggregation and grouping keys | planner |
| `DISTINCT` inside aggregations | planner |
| `REMOVE n = {…}` | codegen |
| Unknown functions | codegen |

Silently absent areas: named paths and path values (`nodes()`,
`relationships()`, `length()`), shortest paths, pattern predicates and
`exists()`, list/pattern comprehensions, quantifiers, `reduce()`, map
property access / subscript / slicing / projections, `FOREACH`, `CALL`,
`SET n += {…}`, most of the built-in function library, and temporal/spatial
value types.

Known semantic deviations to close:

- `MERGE` on a path with unbound endpoints merges the node(s) first and then
  the relationship, instead of matching the whole path atomically.
- `ORDER BY` after aggregation may only reference projected values.
- Aggregations with multiple arguments are rejected (needed for
  `percentileCont`/`percentileDisc`, Phase 2).
- Errors that Neo4j detects statically (for example `NOT {true: true}`) are
  raised at runtime instead, so their phase differs from the TCK.

## Phases

Where the allow-list stood after 1A (2024.3, 1849 scenarios):
temporal types 1004 (Phase 3), error-kind and validation mismatches ~340
(Phase 4), ordering/comparison/precedence semantics ~350 (Phase 4), paths ~250 (Phase 1B), CALL
procedure registration 52 (unreachable: the TCK registers test procedures),
and the rest spread over functions (Phase 2), error phases (Phase 4) and
clause corner cases.

### Phase 1 — Core language completion (large)

Four sub-phases, each verified by the TCK gate; scenario counts are the
allow-list sizes measured after `OPTIONAL MATCH`.

#### 1A — Statement and expression surface

- Map property access (`m.k`), subscript (`m['k']`) and list slicing
  (`list[1..3]`) are done; map projections (`n{.name}`) have no TCK coverage
  and stay unimplemented.

#### 1B — Paths

- List comprehensions, quantifiers (`all`, `any`, `none`, `single`),
  `reduce()`, variable-length paths and named paths (`p = …`, `nodes(p)`,
  `relationships(p)`, `length(p)`) are done.
- Pattern predicates in `WHERE`, `exists()`, and `shortestPath()` /
  `allShortestPaths()` (early-exit BFS over the incidence planes) (~50);
  pattern comprehensions depend on them.

#### 1D — Write-side completeness

- MERGE conformance: whole-path match-or-create and multiple relationships;
  removes the documented deviation (~21).
- `FOREACH` and `CALL … YIELD` for built-in procedures (`db.labels()`,
  `db.propertyKeys()`, …); no TCK coverage, verified by unit tests only.

### Phase 2 — Function library (medium)

- String: `substring`, `replace`, `split`, `trim`/`ltrim`/`rtrim`,
  `left`/`right`, `toUpper`/`toLower`, `reverse`.
- Math: `abs`, `ceil`, `floor`, `round`, `sign`, `sqrt`, `exp`, `log`,
  `log10`, trigonometry, `degrees`/`radians`, `pi`, `e`, `rand`.
- List: `head`, `last`, `tail`, `range`, `size`, `reverse`, `nodes`,
  `relationships`.
- Conversions: `toBoolean`, `toString` for every type.
- Aggregation: `stDev`, `stDevP`, `percentileCont`, `percentileDisc`.

### Phase 3 — Temporal and spatial types (large, cross-cutting)

The biggest value-system change:

- New `Value` variants: `Date`, `Time`, `LocalTime`, `DateTime`,
  `LocalDateTime`, `Duration`, `Point`.
- Record encode/decode tags, order-preserving index-key encodings,
  comparison and arithmetic rules.
- Function surface: `date()`, `datetime()`, `duration()`, `point()`,
  `distance()`, component accessors.
- CLI/JSON rendering.

Storage stays backward compatible (new tags only); the format minor version
is bumped.

### Phase 4 — Semantic conformance sweep (medium, iterative)

TCK-driven fixes: null semantics corners, cross-type comparison and
ordering, `IN` with nulls, list equality, error-vs-null decisions, scope
boundaries across `WITH`, `SKIP`/`LIMIT` expression rules, and removal of
the remaining documented deviations.

### Phase 5 — Neo4j extensions (medium)

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
- `EXISTS { … }` subqueries.

These stay allow-listed as `neo4j-extension` until this phase.

### Phase 6 — Performance for the new surface (medium, interleaved)

- Streaming cursors for `OPTIONAL MATCH` and variable-length expansion
  (avoid full materialization).
- Bidirectional shortest-path search with early exit.
- Memory-bounded sort and aggregation (spill to temporary B-trees).

## Milestones

| Milestone | Target |
|-----------|--------|
| After Phases 1–2 | ≥ 80% of applicable TCK scenarios pass |
| After Phase 4 | ≥ 95% of applicable TCK scenarios pass; deviations closed |
| After Phase 5 | Allow-list contains only non-goals |

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
