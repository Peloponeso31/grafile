# openCypher TCK harness

grafile measures conformance against the
[openCypher TCK](https://github.com/opencypher/openCypher/tree/main/tck) with a
Rust step-runner that maps every scenario onto the public API. The vendored
TCK lives in `third_party/opencypher` as a git submodule pinned to the
`2024.3` release tag and is licensed under Apache-2.0 (see
`third_party/opencypher/LICENSE` and `NOTICE`).

## Running

```console
$ git submodule update --init          # once, fetches the TCK
$ cargo run -p tck-runner --release    # run the gate; exits 0 when it passes
```

Useful options:

| Option | Meaning |
| --- | --- |
| `--filter <text>` | run only scenarios whose `file: name` contains `text` |
| `--write-allowlist` | regenerate `tck/allowlist.json` from the current results |
| `--quiet` | suppress the progress bar |
| `--report <path>` | report output (default `tck/report.json`, git-ignored) |

A filtered run still fails on unexpected failures and harness errors, but it
cannot detect stale allow-list entries, so it skips that check.

## The gate

The gate is strict; `tck-runner` exits non-zero when any of the following
happens:

- **Unexpected failure** — a scenario fails and is not on the allow-list.
- **Harness error** — a step, table cell or setup the runner cannot evaluate.
  Harness errors are never allow-listable: the harness must be fixed first.
- **Stale allow-list entry** — an allow-listed scenario passes. Any commit
  that makes scenarios pass must prune their allow-list entries in the same
  commit.

The pass rate in the report counts passing scenarios over *applicable*
scenarios (all scenarios minus allow-listed ones).

## Allow-list

`tck/allowlist.json` maps feature files to scenario names:

```json
{
  "files": {
    "clauses/call/Call1.feature": {
      "*": { "reason": "not-implemented", "note": "CALL procedures are not implemented" }
    },
    "clauses/match/Match1.feature": {
      "[5] Matching a node by label": { "reason": "not-implemented" }
    }
  }
}
```

The special scenario name `*` covers every scenario in the file; the
bootstrap collapses files where every scenario fails into a single `*` entry
to keep the list reviewable. Reasons:

- `not-implemented` — valid openCypher that grafile does not support yet.
- `neo4j-extension` — the scenario depends on Neo4j-specific behavior.
- `out-of-scope` — an explicit non-goal.

`--write-allowlist` preserves existing reasons and notes and refuses to write
while any harness error exists.

## Semantics

**Comparison.** Result tables are compared structurally. Nodes and
relationships are matched by labels, relationship type and properties, with
internal ids ignored; property and label order never matters. Row order
matters only for `the result should be, in order`; lists inside cells are
compared as multisets for `(ignoring element order for lists)`. Column names
are compared exactly, because the TCK treats them as part of the result.

**Errors.** Scenarios specify an error type and a phase. grafile's error
codes map onto the TCK taxonomy as follows:

| grafile `ErrorCode` | phase | TCK type |
| --- | --- | --- |
| `Syntax` | compile | `SyntaxError` |
| `NotFound` | compile | `SyntaxError` (undefined variable or label) |
| `NotFound` | runtime | `EntityNotFound` |
| `Unsupported` | compile | `SemanticError` |
| `Type` | compile/runtime | `TypeError` |
| `Constraint` | compile/runtime | `ConstraintVerificationFailed` |
| `Exists` | compile/runtime | `ConstraintValidationFailed` |
| `Range`, `Misuse` | compile/runtime | `ArgumentError` |

The `*NotFound` and `Constraint*` families are accepted interchangeably,
since the TCK distinguishes finer granularity than grafile. The TCK's error
*detail* (for example `InvalidArgumentType`) is Neo4j-specific and only
recorded as an informational note in the report, never as a failure.

**Side effects.** The four observable metrics (nodes, relationships,
properties as entity/key/value triples, distinct labels) are computed by
scanning the engine directly rather than by running the TCK's defining
queries, so the harness itself does not depend on query features grafile may
lack. A query's side effects are the metric deltas across its execution.

**Graphs.** `Given an empty graph` uses a fresh in-memory database per
scenario. `Given any graph` loads `binary-tree-1`: the TCK guarantees that
scenarios using it do not depend on graph content. Named graph scripts are
read from `third_party/opencypher/tck/graphs`.

**Table cells.** Gherkin table cells only allow the escapes `\\`, `\|` and
`\n`, while the TCK also writes Cypher string escapes such as `\'` inside
cells. The runner rewrites those backslashes before parsing and applies
Cypher escape rules when reading expected string values.

## Report

`tck/report.json` (uploaded as a CI artifact) contains the totals, the pass
rate, per-file counts and the full lists of unexpected failures, harness
errors, stale entries and detail notes. Current baseline: 3897 scenarios,
1195 passing, 2702 allow-listed (30.7% of the whole suite, 100% of
applicable scenarios).
