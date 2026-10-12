/*
** C smoke test for the grafile C ABI.
**
** Build (from the repository root):
**   cargo build -p grafile-ffi
**   cc -I ffi/include ffi/examples/smoke.c -L target/debug -lgrafile -o /tmp/grafile-smoke
**   DYLD_LIBRARY_PATH=target/debug /tmp/grafile-smoke        # macOS
**   LD_LIBRARY_PATH=target/debug /tmp/grafile-smoke          # Linux
**
** Or run scripts/ffi-smoke.sh, which does all of the above.
*/
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "grafile.h"

static int fail(GrafileDb *db, const char *what, int rc) {
    fprintf(stderr, "%s failed (code %d): %s\n", what, rc,
            db ? grafile_errmsg(db) : "(no handle)");
    return 1;
}

int main(int argc, char **argv) {
    const char *path = argc > 1 ? argv[1] : "ffi-smoke.grafile";
    remove(path);
    {
        char journal[1024];
        snprintf(journal, sizeof(journal), "%s-journal", path);
        remove(journal);
    }

    GrafileDb *db = NULL;
    char *err = NULL;
    int rc = grafile_open(path, &db, &err);
    if (rc != GRAFILE_OK) {
        fprintf(stderr, "grafile_open: %s\n", err ? err : "?");
        grafile_free(err);
        return 1;
    }

    rc = grafile_exec(db, "CREATE (a:Person {name: 'alice', age: 30})"
                          "-[:KNOWS {since: 2020}]->"
                          "(b:Person {name: 'bob', age: 25})",
                      NULL);
    if (rc != GRAFILE_OK) {
        return fail(db, "grafile_exec", rc);
    }

    char *json = NULL;
    rc = grafile_query_json(
        db,
        "MATCH (a:Person)-[r:KNOWS]->(b) RETURN a.name AS a, b.name AS b, r",
        NULL, &json);
    if (rc != GRAFILE_OK) {
        return fail(db, "grafile_query_json", rc);
    }
    printf("query_json: %s\n", json);
    grafile_free(json);

    GrafileStmt *stmt = NULL;
    rc = grafile_prepare(
        db, "MATCH (n:Person) WHERE n.age > $min RETURN n.name AS name ORDER BY name",
        &stmt, &err);
    if (rc != GRAFILE_OK) {
        fprintf(stderr, "grafile_prepare: %s\n", err ? err : "?");
        grafile_free(err);
        return 1;
    }
    rc = grafile_bind_params_json(stmt, "{\"min\": 20}");
    if (rc != GRAFILE_OK) {
        return fail(db, "grafile_bind_params_json", rc);
    }

    int rows = 0;
    while ((rc = grafile_step(stmt)) == GRAFILE_ROW) {
        const char *name = grafile_column_text(stmt, 0);
        printf("row %d: kind=%d name=%s\n", rows, grafile_column_kind(stmt, 0),
               name ? name : "NULL");
        rows++;
    }
    if (rc != GRAFILE_DONE) {
        return fail(db, "grafile_step", rc);
    }
    grafile_finalize(stmt);

    char *meta = NULL;
    rc = grafile_meta_json(db, 0, &meta);
    if (rc != GRAFILE_OK) {
        return fail(db, "grafile_meta_json", rc);
    }
    printf("meta: %s\n", meta);
    grafile_free(meta);

    grafile_close(db);

    if (rows != 2) {
        fprintf(stderr, "expected 2 rows, got %d\n", rows);
        return 1;
    }
    printf("smoke ok (%d rows, library %s)\n", rows, grafile_version());
    return 0;
}
