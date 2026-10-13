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

/// Split a script into statements at top-level semicolons.
///
/// Semicolons inside single-quoted strings, double-quoted strings and
/// backtick-quoted identifiers do not split. Whitespace-only trailing text is
/// dropped, so a script ending in `;` yields no empty final statement.
///
/// # Examples
///
/// ```
/// use grafile::cypher::split_statements;
///
/// let statements = split_statements("CREATE (a); RETURN 'x;y';");
/// assert_eq!(statements.len(), 2);
/// ```
pub fn split_statements(text: &str) -> Vec<String> {
    let mut statements = Vec::new();
    let mut current = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut in_backtick = false;
    let mut escaped = false;
    for ch in text.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }
        match ch {
            '\\' if in_single || in_double => {
                current.push(ch);
                escaped = true;
            }
            '\'' if !in_double && !in_backtick => {
                in_single = !in_single;
                current.push(ch);
            }
            '"' if !in_single && !in_backtick => {
                in_double = !in_double;
                current.push(ch);
            }
            '`' if !in_single && !in_double => {
                in_backtick = !in_backtick;
                current.push(ch);
            }
            ';' if !in_single && !in_double && !in_backtick => {
                statements.push(std::mem::take(&mut current));
            }
            _ => current.push(ch),
        }
    }
    if !current.trim().is_empty() {
        statements.push(current);
    }
    statements
}
