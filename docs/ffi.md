# Embedding grafile: the C ABI

`grafile-ffi` builds a shared library — `libgrafile.so`, `libgrafile.dylib`
or `grafile.dll` — plus a static library and a hand-written C header
(`ffi/include/grafile.h`). Any language with C FFI can embed grafile
in-process: C/C++, Go (cgo), Python (ctypes), Ruby, Node, Java, C#, Dart,
Swift, PHP.

The API follows SQLite's shape: `open`/`close`, `prepare`/`step`/`column`,
JSON helpers, SQLite-compatible numeric result codes, and `grafile_free` for
library-allocated strings.

## Build

```console
$ cargo build -p grafile-ffi --release     # or scripts/build-ffi.sh
$ ls target/release/libgrafile.*
target/release/libgrafile.dylib
target/release/libgrafile.a
```

The CI workflow (`.github/workflows/ffi.yml`) builds and smoke-tests the
library on macOS, Linux and Windows, and uploads the artifacts.

## Quick start

### C

```c
#include "grafile.h"

GrafileDb *db = NULL;
char *err = NULL;
if (grafile_open("graph.grafile", &db, &err) != GRAFILE_OK) {
    fprintf(stderr, "%s\n", err);
    grafile_free(err);
    return 1;
}

char *json = NULL;
grafile_query_json(db, "MATCH (n:Person) RETURN n.name AS name", NULL, &json);
puts(json);
grafile_free(json);

GrafileStmt *stmt = NULL;
grafile_prepare(db, "MATCH (n:Person) WHERE n.age > $min RETURN n.name AS name",
                &stmt, NULL);
grafile_bind_params_json(stmt, "{\"min\": 30}");
while (grafile_step(stmt) == GRAFILE_ROW) {
    puts(grafile_column_text(stmt, 0));
}
grafile_finalize(stmt);
grafile_close(db);
```

A complete program lives in `ffi/examples/smoke.c`; `scripts/ffi-smoke.sh`
compiles and runs it against the built library.

### Python (ctypes)

```python
import ctypes, json

lib = ctypes.CDLL("libgrafile.dylib")  # libgrafile.so on Linux
lib.grafile_open.argtypes = [ctypes.c_char_p, ctypes.POINTER(ctypes.c_void_p),
                             ctypes.POINTER(ctypes.c_char_p)]
lib.grafile_open.restype = ctypes.c_int
lib.grafile_query_json.argtypes = [ctypes.c_void_p, ctypes.c_char_p,
                                   ctypes.c_char_p, ctypes.POINTER(ctypes.c_char_p)]

db, err, out = ctypes.c_void_p(), ctypes.c_char_p(), ctypes.c_char_p()
assert lib.grafile_open(b"graph.grafile", ctypes.byref(db), ctypes.byref(err)) == 0
assert lib.grafile_query_json(db, b"MATCH (n:Person) RETURN n.name AS name",
                              None, ctypes.byref(out)) == 0
rows = json.loads(out.value)
```

See `ffi/examples/smoke.py` (run by `scripts/ffi-smoke.sh`).

### Go (cgo)

```go
/*
#cgo CFLAGS: -I${SRCDIR}/../include
#cgo LDFLAGS: -L${SRCDIR}/../../target/debug -lgrafile
#include <stdlib.h>
#include "grafile.h"
*/
import "C"
```

See `ffi/examples/example.go` for a complete program; build with
`CGO_LDFLAGS="-L$(pwd)/target/debug" go run ffi/examples/example.go` after
building the library.

### Dart (dart:ffi)

```dart
final lib = DynamicLibrary.open('libgrafile.dylib');
final open = lib.lookupFunction<
    Int32 Function(Pointer<Utf8>, Pointer<Pointer<Void>>, Pointer<Pointer<Utf8>>),
    int Function(Pointer<Utf8>, Pointer<Pointer<Void>>, Pointer<Pointer<Utf8>>)>(
  'grafile_open');
```

See `ffi/examples/example.dart` (requires `dart pub add ffi`).

## API reference

| Function | Purpose |
|---|---|
| `grafile_libversion` / `grafile_version` | runtime version as int / string |
| `grafile_free` | release strings allocated by the library |
| `grafile_open` | open or create a database |
| `grafile_open_v2` | open with `GRAFILE_OPEN_*` flags |
| `grafile_close` | close a handle (statements stay usable) |
| `grafile_errcode` / `grafile_errmsg` | last error on a handle |
| `grafile_exec` | run a statement, discard rows |
| `grafile_query_json` | run a query, JSON array of row objects |
| `grafile_meta_json` | database header as JSON (`raw` flag) |
| `grafile_begin` / `grafile_commit` / `grafile_rollback` | transactions |
| `grafile_prepare` | compile a statement |
| `grafile_bind_params_json` | bind `$params` from a JSON object |
| `grafile_step` | advance: `GRAFILE_ROW` / `GRAFILE_DONE` / error |
| `grafile_reset` / `grafile_finalize` | rewind / destroy a statement |
| `grafile_column_count` / `_name` / `_kind` | result shape |
| `grafile_column_json` | value as JSON (nodes/edges expand to objects) |
| `grafile_column_int64` / `_double` / `_text` | typed accessors |

### Result codes

`GRAFILE_OK` (0), `GRAFILE_ERROR` (1), `GRAFILE_INTERNAL` (2),
`GRAFILE_BUSY` (5), `GRAFILE_LOCKED` (6), `GRAFILE_READONLY` (8),
`GRAFILE_IOERR` (10), `GRAFILE_CORRUPT` (11), `GRAFILE_NOTFOUND` (12),
`GRAFILE_TOOBIG` (18), `GRAFILE_CONSTRAINT` (19), `GRAFILE_MISMATCH` (20),
`GRAFILE_MISUSE` (21), `GRAFILE_NOTADB` (26), plus `GRAFILE_ROW` (100) and
`GRAFILE_DONE` (101). The numbering reuses SQLite's code space where the
classes overlap.

### Column kinds

`GRAFILE_NULL`, `GRAFILE_INT`, `GRAFILE_FLOAT`, `GRAFILE_TEXT`,
`GRAFILE_BLOB`, `GRAFILE_BOOL`, `GRAFILE_LIST`, `GRAFILE_MAP`,
`GRAFILE_NODE`, `GRAFILE_EDGE`.

### JSON conventions

- **Parameters in**: one JSON object mapping names to values
  (`{"min": 30, "names": ["a", "b"]}`); objects become Cypher maps, arrays
  become lists.
- **Rows out**: an array of objects keyed by column name. Node values expand
  to `{"id", "labels", "properties"}`; edges to
  `{"id", "type", "start", "end", "properties"}`. Bytes render as `"0x…"`
  hex strings; non-finite floats become `null`.

## Ownership and lifetimes

- Every `char**` output is allocated by the library and must be released
  with `grafile_free`.
- `grafile_errmsg` and `grafile_column_name` return borrowed pointers; they
  stay valid until the next error / `grafile_finalize`.
- `grafile_column_text` points into per-statement scratch space, valid until
  the next `step`, `reset` or `finalize`.
- Statements keep their connection alive: closing the `GrafileDb` while
  statements are outstanding is safe; the statement must still be
  finalized.

## Threading and concurrency

- A `GrafileDb` handle serializes its own calls and may be shared across
  threads. Statements are single-threaded.
- One writer at a time: enforced across processes with file locks and within
  a process with a writer slot keyed by path. A second writer gets
  `GRAFILE_BUSY`; readers run concurrently and always observe committed
  state (a hot journal left by a crashed writer is replayed automatically
  before reading).
- Write transactions are explicit: `grafile_begin` … `grafile_commit` /
  `grafile_rollback`. Without them each statement auto-commits.

## Limitations

- `grafile_prepare` defers parsing and planning to the first
  `grafile_step`, so syntax/planning errors surface from `step` (or from
  `errcode`/`errmsg` afterwards).
- Result rows are materialized before the first `ROW`; very large results
  use memory proportional to their size. (Streaming cursors are on the
  roadmap.)
- No callbacks, user-defined functions or virtual tables in the ABI.

## ABI stability

Within 0.x the ABI is additive-only: existing functions keep their
signatures and codes; new functions and constants may be added. Check
`GRAFILE_VERSION_NUMBER` / `grafile_libversion()` at runtime and compile
against the matching `grafile.h`.
