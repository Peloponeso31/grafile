//! LOAD CSV: headers, field terminators, parameters and failure modes.

mod support;

use grafile::api::Params;
use support::{connection, query, rows_to_strings};

#[test]
fn load_csv_reads_local_files() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("people.csv");
    std::fs::write(&path, "name,age\nalice,30\nbob,31\n").expect("write");
    let source = path.to_str().expect("path");
    let mut conn = connection();
    // WITH HEADERS binds a map per row.
    let rows = query(
        &mut conn,
        &format!(
            "LOAD CSV WITH HEADERS FROM '{source}' AS row \
             RETURN row.name, row.age ORDER BY row.name"
        ),
    );
    assert_eq!(rows, vec![vec!["alice", "30"], vec!["bob", "31"]]);
    // Without headers each row is a list of strings, in file order.
    let rows = query(
        &mut conn,
        &format!("LOAD CSV FROM '{source}' AS row RETURN row"),
    );
    assert_eq!(
        rows,
        vec![vec!["[name, age]"], vec!["[alice, 30]"], vec!["[bob, 31]"]]
    );
    // The rows feed write clauses like any other row source.
    conn.execute(&format!(
        "LOAD CSV WITH HEADERS FROM '{source}' AS row CREATE (:Person {{name: row.name}})"
    ))
    .expect("load");
    let rows = query(&mut conn, "MATCH (n:Person) RETURN count(*)");
    assert_eq!(rows, vec![vec!["2"]]);
}

#[test]
fn load_csv_options_and_errors() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("semi.csv");
    std::fs::write(&path, "a;b\nc\nd;e\n").expect("write");
    let source = path.to_str().expect("path");
    let mut conn = connection();
    // FIELDTERMINATOR changes the separator and short rows fill nulls.
    let rows = query(
        &mut conn,
        &format!(
            "LOAD CSV WITH HEADERS FROM '{source}' AS row FIELDTERMINATOR ';' \
             RETURN row.a, row.b"
        ),
    );
    assert_eq!(rows, vec![vec!["c", "null"], vec!["d", "e"]]);
    // A parameter can carry the path.
    let mut params = Params::new();
    params.insert("path", source);
    let mut rows = conn
        .query("LOAD CSV FROM $path AS row RETURN count(*)", &params)
        .expect("query");
    assert_eq!(rows_to_strings(&mut rows), vec![vec!["3"]]);
    // Missing files and remote sources fail clearly.
    let err = conn
        .query(
            "LOAD CSV FROM '/no/such/file.csv' AS row RETURN row",
            &Params::new(),
        )
        .expect_err("missing file");
    assert_eq!(err.code(), grafile::ErrorCode::Io);
    let err = conn
        .query(
            "LOAD CSV FROM 'https://example.com/x.csv' AS row RETURN row",
            &Params::new(),
        )
        .expect_err("remote");
    assert_eq!(err.code(), grafile::ErrorCode::Unsupported);
}
