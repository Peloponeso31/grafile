# grafile roadmap: openCypher completeness

This document is the execution plan for taking grafile from its v1 Cypher
subset to full conformance with openCypher 9, plus the Neo4j extensions the
project has chosen to support. The [openCypher TCK](https://github.com/opencypher/openCypher/tree/main/tck)
is the conformance contract: a feature is "done" when the applicable TCK
scenarios pass without allow-list entries.

## Current state (v1)

Implemented:

- Clauses: `MATCH`, `WHERE`, `RETURN`, `WITH` (with `DISTINCT`, `ORDER BY`,
  `SKIP`, `LIMIT`, trailing `WHERE`), `UNWIND`, `CREATE`, `MERGE` (with
  `ON CREATE SET` / `ON MATCH SET`), `SET`, `REMOVE`, `DELETE`,
  `DETACH DELETE`.
- Patterns: node and relationship patterns, directions, type disjunction
  (`:A|B`), inline literal property filters, multiple labels.
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
| `UNION` / `UNION ALL` | parser |
| `OPTIONAL MATCH` | parser |
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
- Aggregations with multiple arguments are rejected.

## Phases

### Phase 1 — Core language completion (large)

Highest TCK scenario share; ordered roughly by dependency:

1. `OPTIONAL MATCH`: compile the inner plan into a collected row cursor
   (the existing `Collect`/`CtxData` machinery) and emit one null row when
   the inner plan is empty; merge scoping rules with subsequent clauses.
2. `UNION` / `UNION ALL`: statement-level concatenation plus deduplication
   for `UNION`.
3. Variable-length paths: bounded and unbounded expansion with a per-path
   visited/trail set, driven by the incidence matrix.
4. Named paths and path values: `p = (a)-[…]→(b)` plus `nodes(p)`,
   `relationships(p)`, `length(p)`; new value representation across record,
   JSON and display layers.
5. Pattern predicates in `WHERE`, `exists()`, and `shortestPath()` /
   `allShortestPaths()` (bidirectional BFS over the incidence planes).
6. List comprehensions, pattern comprehensions, quantifiers
   (`all`, `any`, `none`, `single`), `reduce()`.
7. Map property access (`m.k`), subscript (`m['k']`), list slicing
   (`list[1..3]`), and map projections (`n{.name}`).
8. `FOREACH`, `SET n += {…}`, and `CALL … YIELD` for built-in procedures
   (`db.labels()`, `db.propertyKeys()`, …).
9. MERGE conformance: whole-path match-or-create and multiple
   relationships; removes the documented deviation.
10. Aggregation completeness: `DISTINCT` inside aggregations,
    multi-argument aggregations, and grouping-key semantics for
    expressions that mix aggregation and non-aggregated values.

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
3. Storage-format changes (Phase 3) follow `docs/format.md` and bump the
   format minor version; readers remain backward compatible.
