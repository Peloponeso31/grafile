//! Cypher front end: lexer, AST and parser.
//!
//! The parser implements the openCypher 9 surface described in
//! `docs/format.md`; unsupported constructs fail with a clear `Unsupported`
//! error rather than deviating from the standard.

pub mod ast;
pub mod lexer;
pub mod parser;

pub use ast::*;
pub use parser::parse;
