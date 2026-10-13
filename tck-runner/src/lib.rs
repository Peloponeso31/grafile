//! openCypher TCK harness for grafile.
//!
//! The runner discovers the vendored `.feature` files, executes every
//! scenario against an in-memory database through grafile's public API,
//! compares results and side effects against the TCK's expectations and
//! applies the allow-list gate. See `docs/tck.md`.

pub mod allowlist;
pub mod expected;
pub mod metrics;
pub mod report;
pub mod runner;
pub mod scenario;
pub mod steps;
pub mod value;
