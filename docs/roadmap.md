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
- Aggregations: `count`, `sum`, `avg`, `min`, `max`, `collect`, with
  `DISTINCT` and expressions over aggregations.
- Scalar functions: `id`, `labels`, `type`, `startNode`, `endNode`,
  `coalesce`, `toString`, `toInteger`, `toFloat`, `size`, `properties`,
  `keys`, `nodes`, `relationships`, `length`.

Explicit gaps (currently clear `Unsupported` errors):

| Gap | Where rejected |
|-----|----------------|
| Non-literal property values in `MATCH` patterns (`{key: expr}`) | planner |
| Pattern predicates and pattern comprehensions | parser |
| `MERGE` with multiple relationships | planner |
| `REMOVE n = {…}` | codegen |
| Map projections (`n{.name}`) | parser |
| Unknown functions | codegen |

Silently absent areas: most of the built-in function library and
temporal/spatial value types.

Known semantic deviations to close:

- A write clause followed by a read clause streams rows instead of
  buffering: `MATCH (a:A) DELETE a MERGE (a2:A)` can still match a node
  deleted by an earlier row (Phase 4).
- `ORDER BY` after aggregation may only reference projected values.
- Aggregations with multiple arguments are rejected (needed for
  `percentileCont`/`percentileDisc`, Phase 2).
- Errors that Neo4j detects statically (for example `NOT {true: true}`) are
  raised at runtime instead, so their phase differs from the TCK.
- `+` does not coerce the non-string operand to text, so `'p-' + 1` errors.

## Phases

Status markers: `[done]` implemented and gated, `[partial]` some items
finished, `[pending]` not started. Items the TCK does not exercise are
verified by unit and integration tests and must not regress the report.

Current baseline: 2084 of 3897 scenarios passing (53.5%), 1813 allow-listed.
The remaining list is dominated by temporal types (~1000, Phase 3), the
error-kind/phase and ordering/comparison sweeps (~690, Phase 4), the function
library (Phase 2), eager evaluation after writes (Phase 4) and CALL procedure
registration (52, unreachable: the TCK registers test procedures).

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
  expressions inside list comprehensions are rejected: their bodies are
  evaluated per element without plan nodes (one scenario, Phase 4).
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

### Phase 2 — Function library (medium) [pending]

- String: `substring`, `replace`, `split`, `trim`/`ltrim`/`rtrim`,
  `left`/`right`, `toUpper`/`toLower`, `reverse`.
- Math: `abs`, `ceil`, `floor`, `round`, `sign`, `sqrt`, `exp`, `log`,
  `log10`, trigonometry, `degrees`/`radians`, `pi`, `e`, `rand`.
- List: `head`, `last`, `tail`, `range`, `size`, `reverse`, `nodes`,
  `relationships`.
- Conversions: `toBoolean`, `toString` for every type.
- Aggregation: `stDev`, `stDevP`, `percentileCont`, `percentileDisc`.

### Phase 3 — Temporal and spatial types (large, cross-cutting) [pending]

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

### Phase 4 — Semantic conformance sweep (medium, iterative) [pending]

TCK-driven fixes: null semantics corners, cross-type comparison and
ordering, `IN` with nulls, list equality, error-vs-null decisions, scope
boundaries across `WITH`, `SKIP`/`LIMIT` expression rules, and removal of
the remaining documented deviations.

- Operator coercion: `+` with a string operand converts the other side to
  text (`'p-' + 1`), matching openCypher; list concatenation follows the
  same rules. TCK-blind: unit and integration tests only.
- Statically detectable errors (for example `NOT {true: true}`,
  `labels(path)`) are reported at compile time instead of runtime.
- Eager evaluation between a write clause and a later read clause: rows are
  buffered so that `MATCH … DELETE a MERGE (a2:A)` cannot match a node
  deleted by an earlier row, matching Neo4j's eager operators.

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
- `EXISTS { … }` subqueries.

These stay allow-listed as `neo4j-extension` until this phase.

### Phase 6 — Performance for the new surface (medium, interleaved) [pending]

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
4. Features the TCK does not exercise (DDL, non-literal pattern properties,
   operator coercions, `FOREACH`) are planned here explicitly and verified by
   unit and integration tests; they must not regress the TCK report either.
