/*
** grafile.h - C ABI for grafile, the single-file graph database.
**
** Link against libgrafile.so / libgrafile.dylib / grafile.dll (or the static
** library) and query with Cypher. The API follows SQLite's shape:
** open/close, prepare/step/column, JSON helpers, result codes and a free
** function for library-allocated strings.
**
** Threading: a GrafileDb handle serializes its own calls and may be shared
** between threads. Statements are single-threaded. One writer at a time is
** enforced across processes (file locks) and within a process (writer slot).
**
** Ownership: every char** output must be released with grafile_free().
** grafile_errmsg() returns a borrowed pointer valid until the next error.
*/
#ifndef GRAFILE_H
#define GRAFILE_H

#ifdef __cplusplus
extern "C" {
#endif

/* Version of this header and of the linked library. */
#define GRAFILE_VERSION_MAJOR 0
#define GRAFILE_VERSION_MINOR 1
#define GRAFILE_VERSION_PATCH 0
#define GRAFILE_VERSION_NUMBER 1000 /* major*1000000 + minor*1000 + patch */

/* Result codes (SQLite-compatible numbering where the classes overlap). */
#define GRAFILE_OK          0
#define GRAFILE_ERROR       1  /* syntax errors, unsupported features */
#define GRAFILE_INTERNAL    2  /* invariant failure or panic */
#define GRAFILE_BUSY        5  /* database locked by another writer */
#define GRAFILE_LOCKED      6  /* conflicting lock */
#define GRAFILE_READONLY    8  /* write attempted on a read-only database */
#define GRAFILE_IOERR      10  /* operating-system I/O failure */
#define GRAFILE_CORRUPT    11  /* on-disk structure failed validation */
#define GRAFILE_NOTFOUND   12  /* referenced entity does not exist */
#define GRAFILE_TOOBIG     18  /* value out of range */
#define GRAFILE_CONSTRAINT 19  /* uniqueness constraint violated */
#define GRAFILE_MISMATCH   20  /* type mismatch */
#define GRAFILE_MISUSE     21  /* API misuse (null pointers, wrong state) */
#define GRAFILE_NOTADB     26  /* not a grafile database */
#define GRAFILE_ROW       100  /* grafile_step() produced a row */
#define GRAFILE_DONE      101  /* grafile_step() finished */

/* Open flags for grafile_open_v2(). */
#define GRAFILE_OPEN_READONLY  0x1
#define GRAFILE_OPEN_READWRITE 0x2
#define GRAFILE_OPEN_CREATE    0x4

/* Column kinds returned by grafile_column_kind(). */
#define GRAFILE_NULL   0
#define GRAFILE_INT    1
#define GRAFILE_FLOAT  2
#define GRAFILE_TEXT   3
#define GRAFILE_BLOB   4
#define GRAFILE_BOOL   5
#define GRAFILE_LIST   6
#define GRAFILE_MAP    7
#define GRAFILE_NODE   8
#define GRAFILE_EDGE   9

typedef struct GrafileDb GrafileDb;
typedef struct GrafileStmt GrafileStmt;

/* --- library information ------------------------------------------------ */

/* Library version as an integer (see GRAFILE_VERSION_NUMBER). */
int grafile_libversion(void);

/* Library version as a static string. */
const char *grafile_version(void);

/* Release a string allocated by this library (error messages, JSON output). */
void grafile_free(void *ptr);

/* --- connections -------------------------------------------------------- */

/*
** Open or create a database (equivalent to open_v2 with
** GRAFILE_OPEN_READWRITE | GRAFILE_OPEN_CREATE).
**
** On failure, *out_err (when non-NULL) receives an allocated message that
** must be released with grafile_free(); on success it is set to NULL.
*/
int grafile_open(const char *path, GrafileDb **out_db, char **out_err);

/* Open a database with explicit flags (see GRAFILE_OPEN_*). */
int grafile_open_v2(const char *path, int flags, GrafileDb **out_db, char **out_err);

/* Close a handle. Statements obtained from it remain usable. */
void grafile_close(GrafileDb *db);

/* Last result code on this handle. */
int grafile_errcode(GrafileDb *db);

/* Last error message (borrowed; valid until the next error or close). */
const char *grafile_errmsg(GrafileDb *db);

/* --- queries ------------------------------------------------------------ */

/*
** Execute a statement, discarding rows. params_json is an optional JSON
** object mapping $name parameters, e.g. {"min": 30}.
*/
int grafile_exec(GrafileDb *db, const char *cypher, const char *params_json);

/*
** Execute a query and return all rows as a JSON array of objects. Nodes and
** edges expand to {"id", "labels", "properties"} / {"id", "type", "start",
** "end", "properties"}. *out_json must be released with grafile_free().
*/
int grafile_query_json(GrafileDb *db, const char *cypher, const char *params_json,
                       char **out_json);

/*
** Return the database header as JSON. raw != 0 emits the exact stored values
** (epoch seconds, hex UUID); otherwise a human-readable interpretation.
*/
int grafile_meta_json(GrafileDb *db, int raw, char **out_json);

/* --- transactions ------------------------------------------------------- */

/* Begin a write transaction (acquires the exclusive writer slot). */
int grafile_begin(GrafileDb *db);

/* Commit the active write transaction. */
int grafile_commit(GrafileDb *db);

/* Roll back the active write transaction. */
int grafile_rollback(GrafileDb *db);

/* --- prepared statements ------------------------------------------------ */

/*
** Compile a statement. Cypher parsing and planning happen on the first
** grafile_step() so that parameters can be bound first; syntax and planning
** errors therefore surface from step (or errcode/errmsg).
*/
int grafile_prepare(GrafileDb *db, const char *cypher, GrafileStmt **out_stmt,
                    char **out_err);

/* Bind parameters from a JSON object; clears the current result. */
int grafile_bind_params_json(GrafileStmt *stmt, const char *params_json);

/* Advance the statement: GRAFILE_ROW, GRAFILE_DONE or an error code. */
int grafile_step(GrafileStmt *stmt);

/* Reset to the start of the (already materialized) result. */
int grafile_reset(GrafileStmt *stmt);

/* Destroy a statement. */
void grafile_finalize(GrafileStmt *stmt);

/* Number of output columns. */
int grafile_column_count(GrafileStmt *stmt);

/* Column name (borrowed; valid until finalize). */
const char *grafile_column_name(GrafileStmt *stmt, int index);

/* Kind of the value in the current row (see GRAFILE_* column kinds). */
int grafile_column_kind(GrafileStmt *stmt, int index);

/* Serialize the current value as JSON (allocated; release with grafile_free). */
int grafile_column_json(GrafileStmt *stmt, int index, char **out_json);

/* Current value as int64 (null yields 0; non-numeric yields GRAFILE_MISMATCH). */
int grafile_column_int64(GrafileStmt *stmt, int index, long long *out_value);

/* Current value as double (null yields 0.0; non-numeric is GRAFILE_MISMATCH). */
int grafile_column_double(GrafileStmt *stmt, int index, double *out_value);

/*
** Borrow the current value as a C string (NULL for null values; valid until
** the next step/reset/finalize; non-text yields GRAFILE_MISMATCH).
*/
const char *grafile_column_text(GrafileStmt *stmt, int index);

#ifdef __cplusplus
}
#endif

#endif /* GRAFILE_H */
