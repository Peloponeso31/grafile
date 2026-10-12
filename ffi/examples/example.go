// Go (cgo) example for libgrafile.
//
// Build the library first, then run from the repository root:
//
//	cargo build -p grafile-ffi
//	CGO_LDFLAGS="-L$(pwd)/target/debug" go run ffi/examples/example.go
//
// On macOS the dynamic loader also needs DYLD_LIBRARY_PATH=target/debug; on
// Linux use LD_LIBRARY_PATH=target/debug (or install the library system-wide).
package main

/*
#cgo CFLAGS: -I${SRCDIR}/../include
#include <stdlib.h>
#include "grafile.h"
*/
import "C"

import (
	"encoding/json"
	"fmt"
	"os"
	"unsafe"
)

func check(db *C.GrafileDb, rc C.int, what string) {
	if rc != C.GRAFILE_OK {
		msg := C.GoString(C.grafile_errmsg(db))
		fmt.Fprintf(os.Stderr, "%s failed (%d): %s\n", what, int(rc), msg)
		os.Exit(1)
	}
}

func main() {
	path := C.CString("go-example.grafile")
	defer C.free(unsafe.Pointer(path))

	var db *C.GrafileDb
	var err *C.char
	if rc := C.grafile_open(path, &db, &err); rc != C.GRAFILE_OK {
		fmt.Fprintln(os.Stderr, C.GoString(err))
		C.grafile_free(unsafe.Pointer(err))
		os.Exit(1)
	}
	defer C.grafile_close(db)

	create := C.CString("CREATE (:Person {name: 'alice'})-[:KNOWS]->(:Person {name: 'bob'})")
	defer C.free(unsafe.Pointer(create))
	check(db, C.grafile_exec(db, create, nil), "grafile_exec")

	query := C.CString("MATCH (a:Person)-[:KNOWS]->(b) RETURN a.name AS a, b.name AS b")
	defer C.free(unsafe.Pointer(query))
	var out *C.char
	check(db, C.grafile_query_json(db, query, nil, &out), "grafile_query_json")
	defer C.grafile_free(unsafe.Pointer(out))

	var rows []map[string]any
	if err := json.Unmarshal([]byte(C.GoString(out)), &rows); err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
	fmt.Println(rows)
}
