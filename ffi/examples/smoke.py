#!/usr/bin/env python3
"""ctypes smoke test for libgrafile.

Run from the repository root after building the library:

    cargo build -p grafile-ffi
    DYLD_LIBRARY_PATH=target/debug python3 ffi/examples/smoke.py   # macOS
    LD_LIBRARY_PATH=target/debug python3 ffi/examples/smoke.py     # Linux
"""

import ctypes
import json
import os
import sys
import tempfile

LIBRARY = os.environ.get("GRAFILE_LIB", "libgrafile.dylib"
                         if sys.platform == "darwin" else "libgrafile.so")
lib = ctypes.CDLL(LIBRARY)

lib.grafile_open.argtypes = [ctypes.c_char_p, ctypes.POINTER(ctypes.c_void_p),
                             ctypes.POINTER(ctypes.c_char_p)]
lib.grafile_open.restype = ctypes.c_int
lib.grafile_close.argtypes = [ctypes.c_void_p]
lib.grafile_free.argtypes = [ctypes.c_void_p]
lib.grafile_exec.argtypes = [ctypes.c_void_p, ctypes.c_char_p, ctypes.c_char_p]
lib.grafile_exec.restype = ctypes.c_int
lib.grafile_query_json.argtypes = [ctypes.c_void_p, ctypes.c_char_p,
                                   ctypes.c_char_p, ctypes.POINTER(ctypes.c_char_p)]
lib.grafile_query_json.restype = ctypes.c_int
lib.grafile_prepare.argtypes = [ctypes.c_void_p, ctypes.c_char_p,
                                ctypes.POINTER(ctypes.c_void_p),
                                ctypes.POINTER(ctypes.c_char_p)]
lib.grafile_prepare.restype = ctypes.c_int
lib.grafile_bind_params_json.argtypes = [ctypes.c_void_p, ctypes.c_char_p]
lib.grafile_bind_params_json.restype = ctypes.c_int
lib.grafile_step.argtypes = [ctypes.c_void_p]
lib.grafile_step.restype = ctypes.c_int
lib.grafile_column_text.argtypes = [ctypes.c_void_p, ctypes.c_int]
lib.grafile_column_text.restype = ctypes.c_char_p
lib.grafile_finalize.argtypes = [ctypes.c_void_p]

GRAFILE_OK = 0
GRAFILE_ROW = 100
GRAFILE_DONE = 101


def check(rc, what):
    if rc != GRAFILE_OK:
        raise SystemExit(f"{what} failed with code {rc}")


def main():
    path = os.path.join(tempfile.mkdtemp(), "ctypes-smoke.grafile")
    db = ctypes.c_void_p()
    err = ctypes.c_char_p()
    check(lib.grafile_open(path.encode(), ctypes.byref(db), ctypes.byref(err)),
          "grafile_open")

    check(lib.grafile_exec(
        db,
        b"CREATE (:Person {name: 'alice'}), (:Person {name: 'bob'})", None),
        "grafile_exec")

    out = ctypes.c_char_p()
    check(lib.grafile_query_json(
        db, b"MATCH (n:Person) RETURN n.name AS name ORDER BY name", None,
        ctypes.byref(out)), "grafile_query_json")
    rows = json.loads(out.value)
    assert rows == [{"name": "alice"}, {"name": "bob"}], rows
    lib.grafile_free(out)

    stmt = ctypes.c_void_p()
    check(lib.grafile_prepare(
        db, b"MATCH (n:Person) WHERE n.name STARTS WITH $prefix RETURN n.name AS name",
        ctypes.byref(stmt), ctypes.byref(err)), "grafile_prepare")
    check(lib.grafile_bind_params_json(stmt, b'{"prefix": "a"}'),
          "grafile_bind_params_json")
    assert lib.grafile_step(stmt) == GRAFILE_ROW
    assert lib.grafile_column_text(stmt, 0) == b"alice"
    assert lib.grafile_step(stmt) == GRAFILE_DONE
    lib.grafile_finalize(stmt)

    lib.grafile_close(db)
    print("ctypes smoke ok")


if __name__ == "__main__":
    main()
