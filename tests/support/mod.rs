//! Shared helpers for the end-to-end Cypher suites.
//!
//! Every `tests/*.rs` file is a separate crate, so this module is compiled
//! into each one; helpers unused by a given suite are expected.

#![allow(dead_code)]

use std::ops::{Deref, DerefMut};
use std::path::Path;
use std::sync::Arc;

use grafile::api::{Connection, Params};
use grafile::record::Value;
use grafile::vfs::mem::MemVfs;

/// A connection that asserts a clean integrity report when the test that
/// owns it finishes, so every suite ends with a checker assertion.
pub struct Checked(Connection);

impl Deref for Checked {
    type Target = Connection;

    fn deref(&self) -> &Connection {
        &self.0
    }
}

impl DerefMut for Checked {
    fn deref_mut(&mut self) -> &mut Connection {
        &mut self.0
    }
}

impl Drop for Checked {
    fn drop(&mut self) {
        // A failing test already reports its own panic.
        if std::thread::panicking() {
            return;
        }
        let report = self
            .0
            .database_mut()
            .check_integrity()
            .expect("integrity check runs");
        assert!(report.is_clean(), "integrity check failed:\n{report}");
    }
}

pub fn connection() -> Checked {
    Checked(
        Connection::create_with_vfs(Arc::new(MemVfs::new()), Path::new("test.db"), 4096)
            .expect("create connection"),
    )
}

pub fn rows_to_strings(rows: &mut grafile::api::Rows) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    while let Some(row) = rows.next_row() {
        out.push(row.iter().map(render).collect());
    }
    out
}

fn render(value: &Value) -> String {
    match value {
        Value::Null => "null".to_owned(),
        Value::Text(text) => text.to_string(),
        Value::Int(value) => value.to_string(),
        Value::Float(value) => format!("{value}"),
        Value::Bool(value) => value.to_string(),
        other => grafile::vdbe::format_value(other),
    }
}

pub fn query(conn: &mut Connection, source: &str) -> Vec<Vec<String>> {
    let mut rows = conn
        .query(source, &Params::new())
        .unwrap_or_else(|err| panic!("query failed: {err}\n{source}"));
    rows_to_strings(&mut rows)
}

pub fn seed(conn: &mut Connection) {
    conn.execute(
        "CREATE (alice:Person {name: 'alice', age: 30}), \
         (bob:Person {name: 'bob', age: 25}), \
         (carol:Person {name: 'carol', age: 35}), \
         (alice)-[:KNOWS {since: 2020}]->(bob), \
         (bob)-[:KNOWS {since: 2021}]->(carol), \
         (alice)-[:LIKES]->(carol)",
    )
    .expect("seed");
}
