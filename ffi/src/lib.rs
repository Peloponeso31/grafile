//! C ABI for grafile, the single-file graph database.
//!
//! Builds `libgrafile.so` / `libgrafile.dylib` / `grafile.dll` (plus a static
//! library) so any language with C FFI can embed grafile: C/C++, Go (cgo),
//! Python (ctypes), Ruby, Node, Java, C#, Dart, Swift, PHP.
//!
//! The surface follows SQLite's shape: `open`/`close`, `prepare`/`step`/
//! `column`, JSON in/out helpers, SQLite-compatible numeric result codes and
//! `free` for library-allocated strings. See `docs/ffi.md`.
//!
//! # Threading
//!
//! A `GrafileDb` handle serializes its calls internally and may be shared
//! between threads. Statements are single-threaded. The file itself allows
//! one writer at a time (across processes via file locks, within a process
//! via a global writer slot), with concurrent readers.
//!
//! # Ownership
//!
//! Every `char**` output is heap-allocated and must be released with
//! [`grafile_free`]. [`grafile_errmsg`] returns a borrowed pointer that stays
//! valid until the next error on the same handle.

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex};

use grafile::api::{Connection, Params};
use grafile::json::{JsonOptions, header_json, rows_json_array, value_json};
use grafile::record::Value;
use grafile::vfs::{OpenMode, StdVfs};

/// Success.
pub const GRAFILE_OK: c_int = 0;
/// Generic error (syntax errors, unsupported features).
pub const GRAFILE_ERROR: c_int = 1;
/// Internal invariant failure or panic.
pub const GRAFILE_INTERNAL: c_int = 2;
/// The database is locked by another writer.
pub const GRAFILE_BUSY: c_int = 5;
/// A lock request conflicts with locks held elsewhere.
pub const GRAFILE_LOCKED: c_int = 6;
/// Write attempted on a read-only database.
pub const GRAFILE_READONLY: c_int = 8;
/// Operating-system I/O failure.
pub const GRAFILE_IOERR: c_int = 10;
/// On-disk structure failed validation.
pub const GRAFILE_CORRUPT: c_int = 11;
/// A referenced entity does not exist.
pub const GRAFILE_NOTFOUND: c_int = 12;
/// Value out of range.
pub const GRAFILE_TOOBIG: c_int = 18;
/// Uniqueness constraint violated.
pub const GRAFILE_CONSTRAINT: c_int = 19;
/// Type mismatch.
pub const GRAFILE_MISMATCH: c_int = 20;
/// API misuse (null pointers, wrong state).
pub const GRAFILE_MISUSE: c_int = 21;
/// The file is not a grafile database.
pub const GRAFILE_NOTADB: c_int = 26;
/// [`grafile_step`] produced a row.
pub const GRAFILE_ROW: c_int = 100;
/// [`grafile_step`] finished.
pub const GRAFILE_DONE: c_int = 101;

/// Open an existing database read/write.
pub const GRAFILE_OPEN_READONLY: c_int = 0x1;
/// Open read/write (the file must exist).
pub const GRAFILE_OPEN_READWRITE: c_int = 0x2;
/// Create the file when missing.
pub const GRAFILE_OPEN_CREATE: c_int = 0x4;

/// Column kind: null.
pub const GRAFILE_NULL: c_int = 0;
/// Column kind: integer.
pub const GRAFILE_INT: c_int = 1;
/// Column kind: float.
pub const GRAFILE_FLOAT: c_int = 2;
/// Column kind: string.
pub const GRAFILE_TEXT: c_int = 3;
/// Column kind: bytes.
pub const GRAFILE_BLOB: c_int = 4;
/// Column kind: boolean.
pub const GRAFILE_BOOL: c_int = 5;
/// Column kind: list.
pub const GRAFILE_LIST: c_int = 6;
/// Column kind: map.
pub const GRAFILE_MAP: c_int = 7;
/// Column kind: node reference.
pub const GRAFILE_NODE: c_int = 8;
/// Column kind: edge reference.
pub const GRAFILE_EDGE: c_int = 9;
/// Column kind: date, exposed as ISO 8601 text.
pub const GRAFILE_DATE: c_int = 10;
/// Column kind: local time.
pub const GRAFILE_LOCALTIME: c_int = 11;
/// Column kind: time with a fixed UTC offset.
pub const GRAFILE_TIME: c_int = 12;
/// Column kind: local datetime.
pub const GRAFILE_LOCALDATETIME: c_int = 13;
/// Column kind: datetime with an offset and optional IANA zone.
pub const GRAFILE_DATETIME: c_int = 14;
/// Column kind: duration.
pub const GRAFILE_DURATION: c_int = 15;
/// Column kind: point, exposed as a JSON object.
pub const GRAFILE_POINT: c_int = 16;

const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), "\0");

struct DbInner {
    conn: Mutex<Connection>,
    last_error: Mutex<CString>,
    last_code: AtomicI32,
}

/// Database handle.
pub struct GrafileDb {
    inner: Arc<DbInner>,
}

struct StmtResult {
    columns: Vec<String>,
    column_names: Vec<CString>,
    rows: Vec<Vec<Value>>,
}

/// Prepared statement handle.
pub struct GrafileStmt {
    inner: Arc<DbInner>,
    source: String,
    params_json: Option<String>,
    result: Option<StmtResult>,
    position: usize,
    scratch: Vec<CString>,
}

fn code_of(err: &grafile::Error) -> c_int {
    match err.code() {
        grafile::ErrorCode::Internal => GRAFILE_INTERNAL,
        grafile::ErrorCode::Corrupt => GRAFILE_CORRUPT,
        grafile::ErrorCode::NotADb => GRAFILE_NOTADB,
        grafile::ErrorCode::Io => GRAFILE_IOERR,
        grafile::ErrorCode::Busy => GRAFILE_BUSY,
        grafile::ErrorCode::Locked => GRAFILE_LOCKED,
        grafile::ErrorCode::ReadOnly => GRAFILE_READONLY,
        grafile::ErrorCode::Misuse => GRAFILE_MISUSE,
        grafile::ErrorCode::Range => GRAFILE_TOOBIG,
        grafile::ErrorCode::Constraint | grafile::ErrorCode::Exists => GRAFILE_CONSTRAINT,
        grafile::ErrorCode::Type => GRAFILE_MISMATCH,
        grafile::ErrorCode::Syntax | grafile::ErrorCode::Unsupported => GRAFILE_ERROR,
        grafile::ErrorCode::NotFound => GRAFILE_NOTFOUND,
    }
}

fn sanitize(message: &str) -> CString {
    CString::new(message).unwrap_or_else(|_| {
        CString::new("error message contained a NUL byte").expect("static message")
    })
}

fn set_error(inner: &DbInner, code: c_int, message: &str) -> c_int {
    inner.last_code.store(code, Ordering::Relaxed);
    if let Ok(mut slot) = inner.last_error.lock() {
        *slot = sanitize(message);
    }
    code
}

fn report(inner: &DbInner, err: &grafile::Error) -> c_int {
    set_error(inner, code_of(err), &err.to_string())
}

fn clear_error(inner: &DbInner) {
    inner.last_code.store(GRAFILE_OK, Ordering::Relaxed);
}

fn guarded(inner: Option<&DbInner>, body: impl FnOnce() -> c_int) -> c_int {
    match catch_unwind(AssertUnwindSafe(body)) {
        Ok(code) => code,
        Err(_) => match inner {
            Some(inner) => set_error(inner, GRAFILE_INTERNAL, "internal panic"),
            None => GRAFILE_INTERNAL,
        },
    }
}

/// Read a UTF-8 C string argument.
///
/// # Safety
///
/// `ptr` must be null or point to a NUL-terminated string.
unsafe fn c_string(ptr: *const c_char, what: &str) -> Result<String, String> {
    if ptr.is_null() {
        return Err(format!("{what} is NULL"));
    }
    let text = unsafe { CStr::from_ptr(ptr) }
        .to_str()
        .map_err(|_| format!("{what} is not valid UTF-8"))?;
    Ok(text.to_owned())
}

/// Read an optional UTF-8 C string argument.
///
/// # Safety
///
/// `ptr` must be null or point to a NUL-terminated string.
unsafe fn c_string_opt(ptr: *const c_char, what: &str) -> Result<Option<String>, String> {
    if ptr.is_null() {
        return Ok(None);
    }
    unsafe { c_string(ptr, what) }.map(Some)
}

/// Write an optional error message to a `char**` out parameter.
///
/// # Safety
///
/// `out` must be null or point to a writable pointer slot.
unsafe fn write_error(out: *mut *mut c_char, message: Option<&str>) -> Result<(), String> {
    if out.is_null() {
        return Ok(());
    }
    let raw = message
        .map(|message| sanitize(message).into_raw())
        .unwrap_or(std::ptr::null_mut());
    unsafe { *out = raw };
    Ok(())
}

/// Write an allocated string to a `char**` out parameter.
///
/// # Safety
///
/// `out` must point to a writable pointer slot.
unsafe fn write_string(out: *mut *mut c_char, text: &str) -> Result<(), String> {
    if out.is_null() {
        return Err("output pointer is NULL".to_owned());
    }
    unsafe { *out = sanitize(text).into_raw() };
    Ok(())
}

fn json_to_value(value: &serde_json::Value) -> Result<Value, String> {
    Ok(match value {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(value) => Value::Bool(*value),
        serde_json::Value::Number(number) => {
            if let Some(int) = number.as_i64() {
                Value::Int(int)
            } else if let Some(float) = number.as_f64() {
                Value::Float(float)
            } else {
                return Err(format!("number out of range: {number}"));
            }
        }
        serde_json::Value::String(text) => Value::Text(text.as_str().into()),
        serde_json::Value::Array(items) => {
            let values = items
                .iter()
                .map(json_to_value)
                .collect::<Result<Vec<_>, _>>()?;
            Value::List(values.into())
        }
        serde_json::Value::Object(entries) => {
            let pairs = entries
                .iter()
                .map(|(key, value)| Ok((Arc::<str>::from(key.as_str()), json_to_value(value)?)))
                .collect::<Result<Vec<_>, String>>()?;
            Value::Map(pairs.into())
        }
    })
}

fn params_from_json(json: Option<&str>) -> Result<Params, String> {
    let Some(text) = json else {
        return Ok(Params::new());
    };
    if text.trim().is_empty() {
        return Ok(Params::new());
    }
    let parsed: serde_json::Value =
        serde_json::from_str(text).map_err(|err| format!("invalid params JSON: {err}"))?;
    let object = parsed
        .as_object()
        .ok_or_else(|| "params must be a JSON object".to_owned())?;
    let mut params = Params::new();
    for (key, value) in object {
        params.insert(key.clone(), json_to_value(value)?);
    }
    Ok(params)
}

fn kind_of(value: &Value) -> c_int {
    match value {
        Value::Null => GRAFILE_NULL,
        Value::Bool(_) => GRAFILE_BOOL,
        Value::Int(_) => GRAFILE_INT,
        Value::Float(_) => GRAFILE_FLOAT,
        Value::Text(_) => GRAFILE_TEXT,
        Value::Bytes(_) => GRAFILE_BLOB,
        Value::List(_) => GRAFILE_LIST,
        Value::Map(_) => GRAFILE_MAP,
        Value::Node(_) => GRAFILE_NODE,
        Value::Edge(_) => GRAFILE_EDGE,
        Value::Date(_) => GRAFILE_DATE,
        Value::LocalTime(_) => GRAFILE_LOCALTIME,
        Value::Time(_) => GRAFILE_TIME,
        Value::LocalDateTime(_) => GRAFILE_LOCALDATETIME,
        Value::DateTime(_) => GRAFILE_DATETIME,
        Value::Duration(_) => GRAFILE_DURATION,
        Value::Point(_) => GRAFILE_POINT,
    }
}

fn current_value(stmt: &GrafileStmt, index: c_int) -> Option<&Value> {
    if index < 0 || stmt.position == 0 {
        return None;
    }
    stmt.result
        .as_ref()?
        .rows
        .get(stmt.position - 1)?
        .get(index as usize)
}

/// Library version as an integer: major * 1_000_000 + minor * 1_000 + patch.
#[unsafe(no_mangle)]
pub extern "C" fn grafile_libversion() -> c_int {
    1_000
}

/// Library version as a static string.
#[unsafe(no_mangle)]
pub extern "C" fn grafile_version() -> *const c_char {
    VERSION.as_ptr() as *const c_char
}

/// Release a string allocated by this library.
///
/// # Safety
///
/// `ptr` must be null or a pointer previously returned in a `char**` output.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn grafile_free(ptr: *mut c_void) {
    if ptr.is_null() {
        return;
    }
    unsafe { drop(CString::from_raw(ptr as *mut c_char)) };
}

/// Open or create a database with `READWRITE | CREATE` flags.
///
/// # Safety
///
/// Pointer arguments must be valid; see [`grafile_open_v2`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn grafile_open(
    path: *const c_char,
    out_db: *mut *mut GrafileDb,
    out_err: *mut *mut c_char,
) -> c_int {
    unsafe {
        grafile_open_v2(
            path,
            GRAFILE_OPEN_READWRITE | GRAFILE_OPEN_CREATE,
            out_db,
            out_err,
        )
    }
}

/// Open a database.
///
/// `out_err`, when non-null, receives an allocated message on failure (free
/// with [`grafile_free`]) and NULL on success.
///
/// # Safety
///
/// `path` must be a valid UTF-8 C string; `out_db` and `out_err` must be
/// null or writable pointer slots.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn grafile_open_v2(
    path: *const c_char,
    flags: c_int,
    out_db: *mut *mut GrafileDb,
    out_err: *mut *mut c_char,
) -> c_int {
    guarded(None, || {
        let path = match unsafe { c_string(path, "path") } {
            Ok(path) => path,
            Err(message) => {
                let _ = unsafe { write_error(out_err, Some(&message)) };
                return GRAFILE_MISUSE;
            }
        };
        if out_db.is_null() {
            let _ = unsafe { write_error(out_err, Some("out_db is NULL")) };
            return GRAFILE_MISUSE;
        }
        let exists = Path::new(&path).exists();
        let open_mode = if flags & GRAFILE_OPEN_READONLY != 0 {
            OpenMode::ReadOnly
        } else {
            OpenMode::ReadWrite
        };
        let result = if flags & GRAFILE_OPEN_CREATE != 0 && !exists {
            Connection::create(&path)
        } else {
            Connection::open_with_vfs(Arc::new(StdVfs), Path::new(&path), open_mode)
        };
        match result {
            Ok(conn) => {
                let db = Box::new(GrafileDb {
                    inner: Arc::new(DbInner {
                        conn: Mutex::new(conn),
                        last_error: Mutex::new(sanitize("")),
                        last_code: AtomicI32::new(GRAFILE_OK),
                    }),
                });
                unsafe { *out_db = Box::into_raw(db) };
                let _ = unsafe { write_error(out_err, None) };
                GRAFILE_OK
            }
            Err(err) => {
                let _ = unsafe { write_error(out_err, Some(&err.to_string())) };
                code_of(&err)
            }
        }
    })
}

/// Close a database handle. Statements obtained from it remain usable.
///
/// # Safety
///
/// `db` must be null or a handle from [`grafile_open`]; it must not be used
/// afterwards.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn grafile_close(db: *mut GrafileDb) {
    if db.is_null() {
        return;
    }
    unsafe { drop(Box::from_raw(db)) };
}

/// Last result code on this handle.
///
/// # Safety
///
/// `db` must be null or a valid handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn grafile_errcode(db: *mut GrafileDb) -> c_int {
    match unsafe { db.as_ref() } {
        Some(db) => db.inner.last_code.load(Ordering::Relaxed),
        None => GRAFILE_MISUSE,
    }
}

/// Last error message (borrowed; valid until the next error or close).
///
/// # Safety
///
/// `db` must be null or a valid handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn grafile_errmsg(db: *mut GrafileDb) -> *const c_char {
    match unsafe { db.as_ref() } {
        Some(db) => match db.inner.last_error.lock() {
            Ok(slot) => slot.as_ptr(),
            Err(_) => std::ptr::null(),
        },
        None => std::ptr::null(),
    }
}

unsafe fn with_connection(
    db: *mut GrafileDb,
    f: impl FnOnce(&mut Connection) -> grafile::Result<()>,
) -> c_int {
    let Some(db) = (unsafe { db.as_ref() }) else {
        return GRAFILE_MISUSE;
    };
    let inner = &db.inner;
    guarded(Some(inner), || {
        let mut conn = match inner.conn.lock() {
            Ok(conn) => conn,
            Err(_) => return set_error(inner, GRAFILE_INTERNAL, "connection mutex poisoned"),
        };
        match f(&mut conn) {
            Ok(()) => {
                clear_error(inner);
                GRAFILE_OK
            }
            Err(err) => report(inner, &err),
        }
    })
}

/// Execute a statement, discarding any rows.
///
/// `params_json` is an optional JSON object mapping `$name` parameters.
///
/// # Safety
///
/// Pointer arguments must be valid; see [`grafile_open_v2`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn grafile_exec(
    db: *mut GrafileDb,
    cypher: *const c_char,
    params_json: *const c_char,
) -> c_int {
    let Some(db_ref) = (unsafe { db.as_ref() }) else {
        return GRAFILE_MISUSE;
    };
    let inner = &db_ref.inner;
    guarded(Some(inner), || {
        let cypher = match unsafe { c_string(cypher, "cypher") } {
            Ok(cypher) => cypher,
            Err(message) => return set_error(inner, GRAFILE_MISUSE, &message),
        };
        let params = match unsafe { c_string_opt(params_json, "params_json") } {
            Ok(params) => match params_from_json(params.as_deref()) {
                Ok(params) => params,
                Err(message) => return set_error(inner, GRAFILE_MISUSE, &message),
            },
            Err(message) => return set_error(inner, GRAFILE_MISUSE, &message),
        };
        let mut conn = match inner.conn.lock() {
            Ok(conn) => conn,
            Err(_) => return set_error(inner, GRAFILE_INTERNAL, "connection mutex poisoned"),
        };
        match conn.query(&cypher, &params) {
            Ok(_) => {
                clear_error(inner);
                GRAFILE_OK
            }
            Err(err) => report(inner, &err),
        }
    })
}

/// Execute a query and return all rows as a JSON array of objects.
///
/// # Safety
///
/// Pointer arguments must be valid; `out_json` receives an allocated string
/// on success (free with [`grafile_free`]).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn grafile_query_json(
    db: *mut GrafileDb,
    cypher: *const c_char,
    params_json: *const c_char,
    out_json: *mut *mut c_char,
) -> c_int {
    let Some(db_ref) = (unsafe { db.as_ref() }) else {
        return GRAFILE_MISUSE;
    };
    let inner = &db_ref.inner;
    guarded(Some(inner), || {
        if out_json.is_null() {
            return set_error(inner, GRAFILE_MISUSE, "out_json is NULL");
        }
        let cypher = match unsafe { c_string(cypher, "cypher") } {
            Ok(cypher) => cypher,
            Err(message) => return set_error(inner, GRAFILE_MISUSE, &message),
        };
        let params = match unsafe { c_string_opt(params_json, "params_json") } {
            Ok(params) => match params_from_json(params.as_deref()) {
                Ok(params) => params,
                Err(message) => return set_error(inner, GRAFILE_MISUSE, &message),
            },
            Err(message) => return set_error(inner, GRAFILE_MISUSE, &message),
        };
        let mut conn = match inner.conn.lock() {
            Ok(conn) => conn,
            Err(_) => return set_error(inner, GRAFILE_INTERNAL, "connection mutex poisoned"),
        };
        match conn.query(&cypher, &params) {
            Ok(rows) => {
                match rows_json_array(conn.database_mut(), &rows, JsonOptions::plain(false)) {
                    Ok(json) => {
                        if let Err(message) = unsafe { write_string(out_json, &json) } {
                            return set_error(inner, GRAFILE_MISUSE, &message);
                        }
                        clear_error(inner);
                        GRAFILE_OK
                    }
                    Err(err) => report(inner, &err),
                }
            }
            Err(err) => report(inner, &err),
        }
    })
}

/// Return the database header as JSON (`raw != 0` for stored values).
///
/// # Safety
///
/// Pointer arguments must be valid; `out_json` receives an allocated string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn grafile_meta_json(
    db: *mut GrafileDb,
    raw: c_int,
    out_json: *mut *mut c_char,
) -> c_int {
    let Some(db_ref) = (unsafe { db.as_ref() }) else {
        return GRAFILE_MISUSE;
    };
    let inner = &db_ref.inner;
    guarded(Some(inner), || {
        if out_json.is_null() {
            return set_error(inner, GRAFILE_MISUSE, "out_json is NULL");
        }
        let mut conn = match inner.conn.lock() {
            Ok(conn) => conn,
            Err(_) => return set_error(inner, GRAFILE_INTERNAL, "connection mutex poisoned"),
        };
        match header_json(conn.database_mut(), raw != 0) {
            Ok(json) => {
                if let Err(message) = unsafe { write_string(out_json, &json) } {
                    return set_error(inner, GRAFILE_MISUSE, &message);
                }
                clear_error(inner);
                GRAFILE_OK
            }
            Err(err) => report(inner, &err),
        }
    })
}

/// Begin a write transaction.
///
/// # Safety
///
/// `db` must be null or a valid handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn grafile_begin(db: *mut GrafileDb) -> c_int {
    unsafe { with_connection(db, |conn| conn.begin()) }
}

/// Commit the active write transaction.
///
/// # Safety
///
/// `db` must be null or a valid handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn grafile_commit(db: *mut GrafileDb) -> c_int {
    unsafe { with_connection(db, |conn| conn.commit()) }
}

/// Roll back the active write transaction.
///
/// # Safety
///
/// `db` must be null or a valid handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn grafile_rollback(db: *mut GrafileDb) -> c_int {
    unsafe { with_connection(db, |conn| conn.rollback()) }
}

/// Compile a statement. Planning and parameter binding happen on the first
/// [`grafile_step`].
///
/// # Safety
///
/// Pointer arguments must be valid; `out_stmt` receives the statement and
/// must be released with [`grafile_finalize`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn grafile_prepare(
    db: *mut GrafileDb,
    cypher: *const c_char,
    out_stmt: *mut *mut GrafileStmt,
    out_err: *mut *mut c_char,
) -> c_int {
    guarded(None, || {
        let Some(db_ref) = (unsafe { db.as_ref() }) else {
            return GRAFILE_MISUSE;
        };
        let cypher = match unsafe { c_string(cypher, "cypher") } {
            Ok(cypher) => cypher,
            Err(message) => {
                let _ = unsafe { write_error(out_err, Some(&message)) };
                return GRAFILE_MISUSE;
            }
        };
        if out_stmt.is_null() {
            let _ = unsafe { write_error(out_err, Some("out_stmt is NULL")) };
            return GRAFILE_MISUSE;
        }
        let stmt = Box::new(GrafileStmt {
            inner: db_ref.inner.clone(),
            source: cypher,
            params_json: None,
            result: None,
            position: 0,
            scratch: Vec::new(),
        });
        unsafe { *out_stmt = Box::into_raw(stmt) };
        let _ = unsafe { write_error(out_err, None) };
        GRAFILE_OK
    })
}

/// Bind parameters from a JSON object; requires re-stepping afterwards.
///
/// # Safety
///
/// `stmt` must be a statement from [`grafile_prepare`]; `params_json` must be
/// null or a valid UTF-8 C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn grafile_bind_params_json(
    stmt: *mut GrafileStmt,
    params_json: *const c_char,
) -> c_int {
    let Some(stmt) = (unsafe { stmt.as_mut() }) else {
        return GRAFILE_MISUSE;
    };
    let inner = stmt.inner.clone();
    guarded(Some(&inner), || {
        let params = match unsafe { c_string_opt(params_json, "params_json") } {
            Ok(params) => params,
            Err(message) => return set_error(&inner, GRAFILE_MISUSE, &message),
        };
        if let Some(text) = &params
            && !text.trim().is_empty()
            && let Err(message) = params_from_json(Some(text))
        {
            return set_error(&inner, GRAFILE_MISUSE, &message);
        }
        stmt.params_json = params;
        stmt.result = None;
        stmt.position = 0;
        stmt.scratch.clear();
        clear_error(&inner);
        GRAFILE_OK
    })
}

/// Advance the statement: [`GRAFILE_ROW`] on a row, [`GRAFILE_DONE`] at the
/// end, or an error code.
///
/// # Safety
///
/// `stmt` must be a statement from [`grafile_prepare`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn grafile_step(stmt: *mut GrafileStmt) -> c_int {
    let Some(stmt) = (unsafe { stmt.as_mut() }) else {
        return GRAFILE_MISUSE;
    };
    let inner = stmt.inner.clone();
    guarded(Some(&inner), || {
        stmt.scratch.clear();
        if stmt.result.is_none() {
            let params = match params_from_json(stmt.params_json.as_deref()) {
                Ok(params) => params,
                Err(message) => return set_error(&inner, GRAFILE_MISUSE, &message),
            };
            let mut conn = match inner.conn.lock() {
                Ok(conn) => conn,
                Err(_) => return set_error(&inner, GRAFILE_INTERNAL, "connection mutex poisoned"),
            };
            match conn.query(&stmt.source, &params) {
                Ok(rows) => {
                    let columns = rows.columns().to_vec();
                    let column_names = columns.iter().map(|name| sanitize(name)).collect();
                    stmt.result = Some(StmtResult {
                        columns,
                        column_names,
                        rows: rows.rows().to_vec(),
                    });
                    stmt.position = 0;
                }
                Err(err) => return report(&inner, &err),
            }
        }
        let result = stmt.result.as_ref().expect("result was just materialized");
        if stmt.position < result.rows.len() {
            stmt.position += 1;
            clear_error(&inner);
            GRAFILE_ROW
        } else {
            clear_error(&inner);
            GRAFILE_DONE
        }
    })
}

/// Reset the statement to the start of its result.
///
/// # Safety
///
/// `stmt` must be a statement from [`grafile_prepare`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn grafile_reset(stmt: *mut GrafileStmt) -> c_int {
    let Some(stmt) = (unsafe { stmt.as_mut() }) else {
        return GRAFILE_MISUSE;
    };
    stmt.position = 0;
    stmt.scratch.clear();
    clear_error(&stmt.inner);
    GRAFILE_OK
}

/// Destroy a statement.
///
/// # Safety
///
/// `stmt` must be null or a statement from [`grafile_prepare`]; it must not
/// be used afterwards.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn grafile_finalize(stmt: *mut GrafileStmt) {
    if stmt.is_null() {
        return;
    }
    unsafe { drop(Box::from_raw(stmt)) };
}

/// Number of output columns.
///
/// # Safety
///
/// `stmt` must be null or a valid statement.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn grafile_column_count(stmt: *mut GrafileStmt) -> c_int {
    match unsafe { stmt.as_ref() }.and_then(|stmt| stmt.result.as_ref()) {
        Some(result) => result.columns.len() as c_int,
        None => 0,
    }
}

/// Name of a column (borrowed; valid until finalize).
///
/// # Safety
///
/// `stmt` must be null or a valid statement.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn grafile_column_name(
    stmt: *mut GrafileStmt,
    index: c_int,
) -> *const c_char {
    if index < 0 {
        return std::ptr::null();
    }
    match unsafe { stmt.as_ref() }.and_then(|stmt| stmt.result.as_ref()) {
        Some(result) => match result.column_names.get(index as usize) {
            Some(name) => name.as_ptr(),
            None => std::ptr::null(),
        },
        None => std::ptr::null(),
    }
}

/// Kind of the value in the current row (see `GRAFILE_*` column kinds).
///
/// # Safety
///
/// `stmt` must be null or a valid statement.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn grafile_column_kind(stmt: *mut GrafileStmt, index: c_int) -> c_int {
    match unsafe { stmt.as_ref() }.and_then(|stmt| current_value(stmt, index)) {
        Some(value) => kind_of(value),
        None => GRAFILE_NULL,
    }
}

/// Serialize the current row's value as JSON (nodes/edges expand to objects).
///
/// # Safety
///
/// `stmt` must be a valid statement; `out` receives an allocated string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn grafile_column_json(
    stmt: *mut GrafileStmt,
    index: c_int,
    out: *mut *mut c_char,
) -> c_int {
    let Some(stmt) = (unsafe { stmt.as_mut() }) else {
        return GRAFILE_MISUSE;
    };
    let inner = stmt.inner.clone();
    guarded(Some(&inner), || {
        if out.is_null() {
            return set_error(&inner, GRAFILE_MISUSE, "out is NULL");
        }
        let Some(value) = current_value(stmt, index).cloned() else {
            return set_error(
                &inner,
                GRAFILE_MISUSE,
                "no current row or column index out of range",
            );
        };
        let mut conn = match inner.conn.lock() {
            Ok(conn) => conn,
            Err(_) => return set_error(&inner, GRAFILE_INTERNAL, "connection mutex poisoned"),
        };
        match value_json(conn.database_mut(), &value, JsonOptions::plain(false), 0) {
            Ok(json) => {
                if let Err(message) = unsafe { write_string(out, &json) } {
                    return set_error(&inner, GRAFILE_MISUSE, &message);
                }
                clear_error(&inner);
                GRAFILE_OK
            }
            Err(err) => report(&inner, &err),
        }
    })
}

/// Read the current row's value as a 64-bit integer (null yields 0).
///
/// # Safety
///
/// `stmt` must be a valid statement; `out` must be a writable `int64_t`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn grafile_column_int64(
    stmt: *mut GrafileStmt,
    index: c_int,
    out: *mut i64,
) -> c_int {
    let Some(stmt) = (unsafe { stmt.as_ref() }) else {
        return GRAFILE_MISUSE;
    };
    let inner = stmt.inner.clone();
    guarded(Some(&inner), || {
        if out.is_null() {
            return set_error(&inner, GRAFILE_MISUSE, "out is NULL");
        }
        match current_value(stmt, index) {
            Some(Value::Int(value)) => {
                unsafe { *out = *value };
                clear_error(&inner);
                GRAFILE_OK
            }
            Some(Value::Bool(value)) => {
                unsafe { *out = i64::from(*value) };
                clear_error(&inner);
                GRAFILE_OK
            }
            Some(Value::Null) => {
                unsafe { *out = 0 };
                clear_error(&inner);
                GRAFILE_OK
            }
            Some(other) => set_error(
                &inner,
                GRAFILE_MISMATCH,
                &format!("column is {}, not an integer", other.type_name()),
            ),
            None => set_error(
                &inner,
                GRAFILE_MISUSE,
                "no current row or column index out of range",
            ),
        }
    })
}

/// Read the current row's value as a double (null yields 0.0).
///
/// # Safety
///
/// `stmt` must be a valid statement; `out` must be a writable `double`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn grafile_column_double(
    stmt: *mut GrafileStmt,
    index: c_int,
    out: *mut f64,
) -> c_int {
    let Some(stmt) = (unsafe { stmt.as_ref() }) else {
        return GRAFILE_MISUSE;
    };
    let inner = stmt.inner.clone();
    guarded(Some(&inner), || {
        if out.is_null() {
            return set_error(&inner, GRAFILE_MISUSE, "out is NULL");
        }
        match current_value(stmt, index) {
            Some(Value::Int(value)) => {
                unsafe { *out = *value as f64 };
                clear_error(&inner);
                GRAFILE_OK
            }
            Some(Value::Float(value)) => {
                unsafe { *out = *value };
                clear_error(&inner);
                GRAFILE_OK
            }
            Some(Value::Null) => {
                unsafe { *out = 0.0 };
                clear_error(&inner);
                GRAFILE_OK
            }
            Some(other) => set_error(
                &inner,
                GRAFILE_MISMATCH,
                &format!("column is {}, not a number", other.type_name()),
            ),
            None => set_error(
                &inner,
                GRAFILE_MISUSE,
                "no current row or column index out of range",
            ),
        }
    })
}

/// Borrow the current row's value as a C string (valid until the next step,
/// reset or finalize; NULL for null values).
///
/// # Safety
///
/// `stmt` must be a valid statement.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn grafile_column_text(
    stmt: *mut GrafileStmt,
    index: c_int,
) -> *const c_char {
    let Some(stmt) = (unsafe { stmt.as_mut() }) else {
        return std::ptr::null();
    };
    let inner = stmt.inner.clone();
    match current_value(stmt, index) {
        Some(Value::Text(text)) => match CString::new(text.as_bytes()) {
            Ok(cstring) => {
                stmt.scratch.push(cstring);
                stmt.scratch.last().expect("just pushed").as_ptr()
            }
            Err(_) => {
                set_error(&inner, GRAFILE_MISMATCH, "text contains a NUL byte");
                std::ptr::null()
            }
        },
        Some(Value::Null) | None => std::ptr::null(),
        Some(other) => {
            set_error(
                &inner,
                GRAFILE_MISMATCH,
                &format!("column is {}, not text", other.type_name()),
            );
            std::ptr::null()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestDb {
        db: *mut GrafileDb,
        _dir: tempfile::TempDir,
    }

    impl TestDb {
        fn new(name: &str) -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            let path = dir.path().join(name);
            let c_path = CString::new(path.to_str().expect("path")).expect("cstring");
            let mut db: *mut GrafileDb = std::ptr::null_mut();
            let code = unsafe { grafile_open(c_path.as_ptr(), &mut db, std::ptr::null_mut()) };
            assert_eq!(code, GRAFILE_OK);
            Self { db, _dir: dir }
        }

        fn exec(&self, cypher: &str) -> c_int {
            let cypher = CString::new(cypher).expect("cstring");
            unsafe { grafile_exec(self.db, cypher.as_ptr(), std::ptr::null()) }
        }

        fn query_json(&self, cypher: &str) -> String {
            let cypher = CString::new(cypher).expect("cstring");
            let mut out: *mut c_char = std::ptr::null_mut();
            let code =
                unsafe { grafile_query_json(self.db, cypher.as_ptr(), std::ptr::null(), &mut out) };
            assert_eq!(code, GRAFILE_OK);
            let json = unsafe { CStr::from_ptr(out) }
                .to_str()
                .expect("utf8")
                .to_owned();
            unsafe { grafile_free(out as *mut c_void) };
            json
        }

        fn prepare(&self, cypher: &str) -> *mut GrafileStmt {
            let cypher = CString::new(cypher).expect("cstring");
            let mut stmt: *mut GrafileStmt = std::ptr::null_mut();
            let code = unsafe {
                grafile_prepare(self.db, cypher.as_ptr(), &mut stmt, std::ptr::null_mut())
            };
            assert_eq!(code, GRAFILE_OK);
            stmt
        }
    }

    impl Drop for TestDb {
        fn drop(&mut self) {
            unsafe { grafile_close(self.db) };
        }
    }

    #[test]
    fn version_reports_expected_values() {
        assert_eq!(grafile_libversion(), 1_000);
        let version = unsafe { CStr::from_ptr(grafile_version()) };
        assert_eq!(version.to_str().expect("utf8"), env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn open_exec_and_query_json_roundtrip() {
        let db = TestDb::new("roundtrip.db");
        assert_eq!(
            db.exec("CREATE (:Person {name: 'alice', age: 30})"),
            GRAFILE_OK
        );
        let json = db.query_json("MATCH (n:Person) RETURN n.name AS name, n.age AS age");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("json");
        assert_eq!(parsed[0]["name"], "alice");
        assert_eq!(parsed[0]["age"], 30);
    }

    #[test]
    fn prepare_step_and_typed_columns() {
        let db = TestDb::new("columns.db");
        assert_eq!(
            db.exec("CREATE (:Person {name: 'alice', age: 30})"),
            GRAFILE_OK
        );
        let stmt = db.prepare("MATCH (n:Person) RETURN n.name AS name, n.age AS age");
        assert_eq!(unsafe { grafile_step(stmt) }, GRAFILE_ROW);
        assert_eq!(unsafe { grafile_column_count(stmt) }, 2);
        let name = unsafe { CStr::from_ptr(grafile_column_name(stmt, 0)) };
        assert_eq!(name.to_str().expect("utf8"), "name");
        assert_eq!(unsafe { grafile_column_kind(stmt, 0) }, GRAFILE_TEXT);
        assert_eq!(unsafe { grafile_column_kind(stmt, 1) }, GRAFILE_INT);
        let text = unsafe { CStr::from_ptr(grafile_column_text(stmt, 0)) };
        assert_eq!(text.to_str().expect("utf8"), "alice");
        let mut age = 0i64;
        assert_eq!(
            unsafe { grafile_column_int64(stmt, 1, &mut age) },
            GRAFILE_OK
        );
        assert_eq!(age, 30);
        assert_eq!(unsafe { grafile_step(stmt) }, GRAFILE_DONE);
        unsafe { grafile_finalize(stmt) };
    }

    #[test]
    fn bind_params_and_reset() {
        let db = TestDb::new("params.db");
        assert_eq!(
            db.exec("CREATE (:Person {age: 20}), (:Person {age: 30}), (:Person {age: 40})"),
            GRAFILE_OK
        );
        let stmt = db.prepare("MATCH (n:Person) WHERE n.age > $min RETURN count(*) AS n");
        let params = CString::new("{\"min\": 25}").expect("cstring");
        assert_eq!(
            unsafe { grafile_bind_params_json(stmt, params.as_ptr()) },
            GRAFILE_OK
        );
        assert_eq!(unsafe { grafile_step(stmt) }, GRAFILE_ROW);
        let mut count = 0i64;
        assert_eq!(
            unsafe { grafile_column_int64(stmt, 0, &mut count) },
            GRAFILE_OK
        );
        assert_eq!(count, 2);
        assert_eq!(unsafe { grafile_step(stmt) }, GRAFILE_DONE);
        assert_eq!(unsafe { grafile_reset(stmt) }, GRAFILE_OK);
        assert_eq!(unsafe { grafile_step(stmt) }, GRAFILE_ROW);
        unsafe { grafile_finalize(stmt) };
    }

    #[test]
    fn entity_columns_expand_to_json() {
        let db = TestDb::new("entities.db");
        assert_eq!(
            db.exec("CREATE (:Person {name: 'alice'})-[:KNOWS]->(:Person {name: 'bob'})"),
            GRAFILE_OK
        );
        let stmt = db.prepare("MATCH ()-[r:KNOWS]->() RETURN r");
        assert_eq!(unsafe { grafile_step(stmt) }, GRAFILE_ROW);
        assert_eq!(unsafe { grafile_column_kind(stmt, 0) }, GRAFILE_EDGE);
        let mut out: *mut c_char = std::ptr::null_mut();
        assert_eq!(
            unsafe { grafile_column_json(stmt, 0, &mut out) },
            GRAFILE_OK
        );
        let json = unsafe { CStr::from_ptr(out) }.to_str().expect("utf8");
        assert!(json.contains("\"type\":\"KNOWS\""), "{json}");
        assert!(json.contains("\"start\":1"), "{json}");
        unsafe { grafile_free(out as *mut c_void) };
        unsafe { grafile_finalize(stmt) };
    }

    #[test]
    fn syntax_errors_surface_on_step() {
        let db = TestDb::new("errors.db");
        let stmt = db.prepare("MATCH (n RETURN n");
        assert_eq!(unsafe { grafile_step(stmt) }, GRAFILE_ERROR);
        let message = unsafe { CStr::from_ptr(grafile_errmsg(db.db)) };
        let message = message.to_str().expect("utf8");
        assert!(message.contains("Syntax"), "{message}");
        assert!(message.contains("line 1"), "{message}");
        assert_eq!(unsafe { grafile_errcode(db.db) }, GRAFILE_ERROR);
        unsafe { grafile_finalize(stmt) };
    }

    #[test]
    fn second_writer_handle_is_busy() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("busy.db");
        let c_path = CString::new(path.to_str().expect("path")).expect("cstring");
        let mut first: *mut GrafileDb = std::ptr::null_mut();
        let mut second: *mut GrafileDb = std::ptr::null_mut();
        assert_eq!(
            unsafe { grafile_open(c_path.as_ptr(), &mut first, std::ptr::null_mut()) },
            GRAFILE_OK
        );
        assert_eq!(
            unsafe { grafile_open(c_path.as_ptr(), &mut second, std::ptr::null_mut()) },
            GRAFILE_OK
        );
        assert_eq!(unsafe { grafile_begin(first) }, GRAFILE_OK);
        let cypher = CString::new("CREATE (:X)").expect("cstring");
        assert_eq!(
            unsafe { grafile_exec(second, cypher.as_ptr(), std::ptr::null()) },
            GRAFILE_BUSY
        );
        assert_eq!(unsafe { grafile_commit(first) }, GRAFILE_OK);
        assert_eq!(
            unsafe { grafile_exec(second, cypher.as_ptr(), std::ptr::null()) },
            GRAFILE_OK
        );
        unsafe { grafile_close(first) };
        unsafe { grafile_close(second) };
    }

    #[test]
    fn transactions_roll_back() {
        let db = TestDb::new("txn.db");
        assert_eq!(unsafe { grafile_begin(db.db) }, GRAFILE_OK);
        assert_eq!(db.exec("CREATE (:X)"), GRAFILE_OK);
        assert_eq!(unsafe { grafile_rollback(db.db) }, GRAFILE_OK);
        let json = db.query_json("MATCH (n:X) RETURN count(*) AS n");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("json");
        assert_eq!(parsed[0]["n"], 0);
    }

    #[test]
    fn meta_json_has_expected_keys() {
        let db = TestDb::new("meta.db");
        let raw_keys = ["page_size", "uuid", "created_at"];
        let human_keys = ["page_size", "uuid", "created", "roots", "allocated"];
        for (raw, keys) in [(1, &raw_keys[..]), (0, &human_keys[..])] {
            let mut out: *mut c_char = std::ptr::null_mut();
            assert_eq!(
                unsafe { grafile_meta_json(db.db, raw, &mut out) },
                GRAFILE_OK
            );
            let json = unsafe { CStr::from_ptr(out) }.to_str().expect("utf8");
            for key in keys {
                assert!(json.contains(&format!("\"{key}\"")), "{json}");
            }
            unsafe { grafile_free(out as *mut c_void) };
        }
    }

    #[test]
    fn null_pointers_and_bad_utf8_are_misuse() {
        let mut db: *mut GrafileDb = std::ptr::null_mut();
        assert_eq!(
            unsafe { grafile_open(std::ptr::null(), &mut db, std::ptr::null_mut()) },
            GRAFILE_MISUSE
        );
        assert_eq!(
            unsafe { grafile_errcode(std::ptr::null_mut()) },
            GRAFILE_MISUSE
        );
        let cypher = CString::new("RETURN 1").expect("cstring");
        assert_eq!(
            unsafe { grafile_exec(std::ptr::null_mut(), cypher.as_ptr(), std::ptr::null()) },
            GRAFILE_MISUSE
        );
        // Invalid UTF-8 path.
        let bad = CString::new(vec![0xFFu8, b'x']).expect("cstring");
        let code = unsafe { grafile_open(bad.as_ptr(), &mut db, std::ptr::null_mut()) };
        assert_eq!(code, GRAFILE_MISUSE);
    }

    #[test]
    fn null_columns_read_as_null() {
        let db = TestDb::new("nulls.db");
        assert_eq!(db.exec("CREATE (:X)"), GRAFILE_OK);
        let stmt = db.prepare("MATCH (n:X) RETURN n.missing AS missing");
        assert_eq!(unsafe { grafile_step(stmt) }, GRAFILE_ROW);
        assert_eq!(unsafe { grafile_column_kind(stmt, 0) }, GRAFILE_NULL);
        assert!(unsafe { grafile_column_text(stmt, 0) }.is_null());
        let mut value = 7i64;
        assert_eq!(
            unsafe { grafile_column_int64(stmt, 0, &mut value) },
            GRAFILE_OK
        );
        assert_eq!(value, 0);
        unsafe { grafile_finalize(stmt) };
    }

    #[test]
    fn error_state_recovers_after_a_successful_statement() {
        let db = TestDb::new("recover.db");
        assert_eq!(db.exec("MATCH (n RETURN n"), GRAFILE_ERROR);
        assert_eq!(unsafe { grafile_errcode(db.db) }, GRAFILE_ERROR);
        let message = unsafe { CStr::from_ptr(grafile_errmsg(db.db)) };
        assert!(message.to_str().expect("utf8").contains("Syntax"));

        // A successful statement clears the error code for the handle.
        assert_eq!(db.exec("RETURN 1"), GRAFILE_OK);
        assert_eq!(unsafe { grafile_errcode(db.db) }, GRAFILE_OK);
    }

    #[test]
    fn missing_params_fail_until_bound_and_rebinding_reexecutes() {
        let db = TestDb::new("params-lifecycle.db");
        assert_eq!(
            db.exec("CREATE (:Person {age: 20}), (:Person {age: 30}), (:Person {age: 40})"),
            GRAFILE_OK
        );
        let stmt = db.prepare("MATCH (n:Person) WHERE n.age > $min RETURN count(*) AS n");
        assert_eq!(unsafe { grafile_step(stmt) }, GRAFILE_NOTFOUND);
        let message = unsafe { CStr::from_ptr(grafile_errmsg(db.db)) };
        assert!(message.to_str().expect("utf8").contains("$min"));

        let low = CString::new("{\"min\": 25}").expect("cstring");
        assert_eq!(
            unsafe { grafile_bind_params_json(stmt, low.as_ptr()) },
            GRAFILE_OK
        );
        assert_eq!(unsafe { grafile_step(stmt) }, GRAFILE_ROW);
        let mut count = 0i64;
        assert_eq!(
            unsafe { grafile_column_int64(stmt, 0, &mut count) },
            GRAFILE_OK
        );
        assert_eq!(count, 2);
        assert_eq!(unsafe { grafile_step(stmt) }, GRAFILE_DONE);

        // Rebinding clears the materialized result, so the next step
        // re-executes with the new parameters.
        let high = CString::new("{\"min\": 35}").expect("cstring");
        assert_eq!(
            unsafe { grafile_bind_params_json(stmt, high.as_ptr()) },
            GRAFILE_OK
        );
        assert_eq!(unsafe { grafile_step(stmt) }, GRAFILE_ROW);
        assert_eq!(
            unsafe { grafile_column_int64(stmt, 0, &mut count) },
            GRAFILE_OK
        );
        assert_eq!(count, 1);
        unsafe { grafile_finalize(stmt) };
    }

    #[test]
    fn invalid_params_json_is_misuse_and_recoverable() {
        let db = TestDb::new("params-invalid.db");
        let stmt = db.prepare("RETURN 1 AS x");
        let bad = CString::new("{oops").expect("cstring");
        assert_eq!(
            unsafe { grafile_bind_params_json(stmt, bad.as_ptr()) },
            GRAFILE_MISUSE
        );
        let good = CString::new("{}").expect("cstring");
        assert_eq!(
            unsafe { grafile_bind_params_json(stmt, good.as_ptr()) },
            GRAFILE_OK
        );
        assert_eq!(unsafe { grafile_step(stmt) }, GRAFILE_ROW);
        unsafe { grafile_finalize(stmt) };
    }

    #[test]
    fn read_only_open_rejects_writes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("readonly.db");
        let c_path = CString::new(path.to_str().expect("path")).expect("cstring");
        {
            let mut db: *mut GrafileDb = std::ptr::null_mut();
            assert_eq!(
                unsafe { grafile_open(c_path.as_ptr(), &mut db, std::ptr::null_mut()) },
                GRAFILE_OK
            );
            let cypher = CString::new("CREATE (:X {v: 1})").expect("cstring");
            assert_eq!(
                unsafe { grafile_exec(db, cypher.as_ptr(), std::ptr::null()) },
                GRAFILE_OK
            );
            unsafe { grafile_close(db) };
        }

        let mut db: *mut GrafileDb = std::ptr::null_mut();
        let mut err: *mut c_char = std::ptr::null_mut();
        assert_eq!(
            unsafe { grafile_open_v2(c_path.as_ptr(), GRAFILE_OPEN_READONLY, &mut db, &mut err,) },
            GRAFILE_OK
        );
        assert!(err.is_null(), "no error message on success");
        let read = CString::new("MATCH (n:X) RETURN count(*) AS n").expect("cstring");
        let mut out: *mut c_char = std::ptr::null_mut();
        assert_eq!(
            unsafe { grafile_query_json(db, read.as_ptr(), std::ptr::null(), &mut out) },
            GRAFILE_OK
        );
        unsafe { grafile_free(out as *mut c_void) };
        let write = CString::new("CREATE (:Y)").expect("cstring");
        assert_eq!(
            unsafe { grafile_exec(db, write.as_ptr(), std::ptr::null()) },
            GRAFILE_READONLY
        );
        unsafe { grafile_close(db) };
    }

    #[test]
    fn open_error_reports_a_message_for_missing_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("missing.db");
        let c_path = CString::new(path.to_str().expect("path")).expect("cstring");
        let mut db: *mut GrafileDb = std::ptr::null_mut();
        let mut err: *mut c_char = std::ptr::null_mut();
        let code =
            unsafe { grafile_open_v2(c_path.as_ptr(), GRAFILE_OPEN_READWRITE, &mut db, &mut err) };
        assert_eq!(code, GRAFILE_NOTFOUND);
        assert!(db.is_null());
        assert!(!err.is_null(), "out_err must carry the message");
        let message = unsafe { CStr::from_ptr(err) }.to_str().expect("utf8");
        assert!(message.contains("NotFound"), "{message}");
        unsafe { grafile_free(err as *mut c_void) };
    }

    #[test]
    fn column_kind_covers_temporal_and_point_values() {
        let db = TestDb::new("kinds.db");
        let stmt = db.prepare(
            "RETURN date('2020-01-01') AS d, localdatetime('2020-01-01T00:00:00') AS ldt, \
             duration('P1D') AS du, point({x: 1, y: 2}) AS p",
        );
        assert_eq!(unsafe { grafile_step(stmt) }, GRAFILE_ROW);
        let kinds: Vec<c_int> = (0..4)
            .map(|index| unsafe { grafile_column_kind(stmt, index) })
            .collect();
        assert_eq!(
            kinds,
            vec![
                GRAFILE_DATE,
                GRAFILE_LOCALDATETIME,
                GRAFILE_DURATION,
                GRAFILE_POINT
            ]
        );
        unsafe { grafile_finalize(stmt) };
    }
}
