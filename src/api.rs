//! Public API: connections, queries and result rows.
//!
//! ```no_run
//! use grafile::api::{Connection, Params};
//!
//! let mut conn = Connection::open("graph.grafile")?;
//! let rows = conn.query("MATCH (n:Person) RETURN n.name AS name", &Params::new())?;
//! for row in rows.iter() {
//!     println!("{:?}", row);
//! }
//! # Ok::<(), grafile::Error>(())
//! ```

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use crate::codegen;
use crate::cypher;
use crate::engine::Database;
use crate::error::{Error, Result};
use crate::pager::DEFAULT_PAGE_SIZE;
use crate::planner;
use crate::record::Value;
use crate::vdbe;
use crate::vfs::{OpenMode, StdVfs, Vfs};

/// Named query parameters.
#[derive(Debug, Clone, Default)]
pub struct Params {
    values: HashMap<String, Value>,
}

impl Params {
    /// Create an empty parameter set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Bind a parameter.
    pub fn insert(&mut self, name: impl Into<String>, value: impl Into<Value>) -> &mut Self {
        self.values.insert(name.into(), value.into());
        self
    }

    /// Builder-style binding.
    pub fn with(mut self, name: impl Into<String>, value: impl Into<Value>) -> Self {
        self.insert(name, value);
        self
    }

    /// The underlying map.
    pub fn values(&self) -> &HashMap<String, Value> {
        &self.values
    }
}

/// Materialized query results.
#[derive(Debug, Clone, Default)]
pub struct Rows {
    columns: Vec<String>,
    rows: Vec<Vec<Value>>,
    position: usize,
}

impl Rows {
    /// Output column names.
    pub fn columns(&self) -> &[String] {
        &self.columns
    }

    /// Number of result rows.
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// Whether there are no rows.
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Advance to the next row.
    pub fn next_row(&mut self) -> Option<&[Value]> {
        if self.position >= self.rows.len() {
            return None;
        }
        self.position += 1;
        Some(&self.rows[self.position - 1])
    }

    /// Iterate rows.
    pub fn iter(&self) -> impl Iterator<Item = &[Value]> {
        self.rows.iter().map(Vec::as_slice)
    }

    /// All rows.
    pub fn rows(&self) -> &[Vec<Value>] {
        &self.rows
    }
}

/// An embedded database connection.
pub struct Connection {
    db: Database,
    auto_commit: bool,
}

impl Connection {
    /// Create a new database file, failing if it already exists.
    pub fn create(path: impl AsRef<Path>) -> Result<Self> {
        Self::create_with_vfs(Arc::new(StdVfs), path.as_ref(), DEFAULT_PAGE_SIZE)
    }

    /// Create a new database through a custom VFS.
    pub fn create_with_vfs(vfs: Arc<dyn Vfs>, path: &Path, page_size: u32) -> Result<Self> {
        let db = Database::create(vfs, path, page_size)?;
        Ok(Self {
            db,
            auto_commit: true,
        })
    }

    /// Open an existing database for reading and writing.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_with_vfs(Arc::new(StdVfs), path.as_ref(), OpenMode::ReadWrite)
    }

    /// Open an existing database through a custom VFS.
    pub fn open_with_vfs(vfs: Arc<dyn Vfs>, path: &Path, mode: OpenMode) -> Result<Self> {
        let db = Database::open(vfs, path, mode)?;
        Ok(Self {
            db,
            auto_commit: true,
        })
    }

    /// Begin an explicit transaction.
    pub fn begin(&mut self) -> Result<()> {
        self.db.begin_write()
    }

    /// Commit the active transaction.
    pub fn commit(&mut self) -> Result<()> {
        self.db.commit()
    }

    /// Roll back the active transaction.
    pub fn rollback(&mut self) -> Result<()> {
        self.db.rollback()
    }

    /// Whether a write transaction is active.
    pub fn in_transaction(&self) -> bool {
        self.db.in_write_txn()
    }

    /// The schema, for introspection commands.
    pub fn schema(&self) -> &crate::catalog::Schema {
        self.db.schema()
    }

    /// Direct access to the underlying engine, for bulk loading and
    /// introspection. Mutations must run inside [`Connection::begin`] /
    /// [`Connection::commit`].
    pub fn database_mut(&mut self) -> &mut Database {
        &mut self.db
    }

    /// Run a Cypher query and return all result rows.
    pub fn query(&mut self, source: &str, params: &Params) -> Result<Rows> {
        let parsed = cypher::parse(source)?;
        let prepared = planner::plan_query(&mut self.db, &parsed, params.values())?;
        let program = codegen::compile(&self.db, &prepared)?;
        let started = self.db.in_write_txn();
        if program.mutating && !started && self.auto_commit {
            self.db.begin_write()?;
        }
        let result = vdbe::execute(&mut self.db, &program);
        match result {
            Ok(rows) => {
                if program.mutating && !started && self.auto_commit {
                    self.db.commit()?;
                }
                Ok(Rows {
                    columns: program.columns,
                    rows,
                    position: 0,
                })
            }
            Err(err) => {
                if program.mutating && !started && self.auto_commit {
                    let _ = self.db.rollback();
                }
                Err(err)
            }
        }
    }

    /// Run a query, discarding rows.
    pub fn execute(&mut self, source: &str) -> Result<()> {
        self.query(source, &Params::new()).map(|_| ())
    }

    /// Return the query plan as text (like `EXPLAIN QUERY PLAN`).
    pub fn explain(&mut self, source: &str) -> Result<String> {
        let parsed = cypher::parse(source)?;
        let prepared = planner::plan_query(&mut self.db, &parsed, &HashMap::new())?;
        let mut out = String::new();
        planner::explain(&prepared.plan, 0, &mut out);
        Ok(out)
    }

    /// Return the compiled VDBE program as text (like SQLite's `EXPLAIN`).
    pub fn explain_program(&mut self, source: &str) -> Result<String> {
        let parsed = cypher::parse(source)?;
        let prepared = planner::plan_query(&mut self.db, &parsed, &HashMap::new())?;
        let program = codegen::compile(&self.db, &prepared)?;
        let mut out = String::new();
        for (pc, op) in program.ops.iter().enumerate() {
            out.push_str(&format!("{pc:4} {op:?}\n"));
        }
        Ok(out)
    }

    /// Create an index on a node property (DDL extension).
    pub fn create_node_index(
        &mut self,
        key: &str,
        label: Option<&str>,
        unique: bool,
    ) -> Result<u32> {
        let started = self.db.in_write_txn();
        if !started {
            self.db.begin_write()?;
        }
        let result = self.db.create_node_index(key, label, unique);
        match result {
            Ok(id) => {
                if !started {
                    self.db.commit()?;
                }
                Ok(id)
            }
            Err(err) => {
                if !started {
                    let _ = self.db.rollback();
                }
                Err(err)
            }
        }
    }

    /// Create an index on an edge property (DDL extension).
    pub fn create_edge_index(
        &mut self,
        key: &str,
        edge_type: Option<&str>,
        unique: bool,
    ) -> Result<u32> {
        let started = self.db.in_write_txn();
        if !started {
            self.db.begin_write()?;
        }
        let result = self.db.create_edge_index(key, edge_type, unique);
        match result {
            Ok(id) => {
                if !started {
                    self.db.commit()?;
                }
                Ok(id)
            }
            Err(err) => {
                if !started {
                    let _ = self.db.rollback();
                }
                Err(err)
            }
        }
    }
}

/// Reject a query that did not produce any output columns, for callers that
/// require rows.
pub fn require_columns(rows: &Rows) -> Result<()> {
    if rows.columns().is_empty() {
        return Err(Error::misuse("query does not return rows"));
    }
    Ok(())
}
