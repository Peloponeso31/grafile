//! grafile: a graph database that lives in a single file.
//!
//! The architecture follows SQLite's proven decomposition: a Cypher front end
//! compiles queries into bytecode for a register virtual machine (VDBE), which
//! drives cursors over B+trees; B+trees sit on a pager that provides page
//! caching, transactions and crash recovery; the pager talks to the operating
//! system through a small VFS trait.
//!
//! On top of that, grafile keeps a chunked incidence matrix as a bitmap index
//! over the graph: fixed-size fragments (256 nodes x 256 edges per 8 KiB page)
//! stored as records in a B+tree, giving fast neighbor iteration, edge
//! existence checks and bitmap set operations.

pub mod btree;
pub mod catalog;
pub mod cypher;
pub mod engine;
pub mod error;
pub mod matrix;
pub mod pager;
pub mod record;
pub mod vfs;

pub use error::{Error, ErrorCode, Result};
