//! Cypher front end: lexer, AST and parser.
//!
//! The parser implements the openCypher 9 surface described in
//! `docs/format.md`; unsupported constructs fail with a clear `Unsupported`
//! error rather than deviating from the standard.

pub mod ast;
pub mod lexer;
pub mod parser;

pub use ast::*;
pub use parser::{parse, parse_statement};

/// Split a script into statements at top-level semicolons.
///
/// Semicolons inside single-quoted strings, double-quoted strings,
/// backtick-quoted identifiers, `//` line comments and `/* ... */` block
/// comments do not split. Comments are kept in the statement text; the lexer
/// discards them. Chunks without code (comments or whitespace only) are
/// dropped, so commented-out statements never reach the parser.
///
/// # Examples
///
/// ```
/// use grafile::cypher::split_statements;
///
/// let statements = split_statements("CREATE (a); RETURN 'x;y';");
/// assert_eq!(statements.len(), 2);
///
/// let commented = split_statements("// DELETE n;\nRETURN 1;");
/// assert_eq!(commented.len(), 1);
/// ```
pub fn split_statements(text: &str) -> Vec<String> {
    let mut statements = Vec::new();
    let mut current = String::new();
    let mut state = SplitState::Code;
    let mut escaped = false;
    let mut has_code = false;
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        match state {
            SplitState::Code => match ch {
                '/' if chars.peek() == Some(&'/') => {
                    current.push(ch);
                    current.push(chars.next().expect("peeked character"));
                    state = SplitState::LineComment;
                }
                '/' if chars.peek() == Some(&'*') => {
                    current.push(ch);
                    current.push(chars.next().expect("peeked character"));
                    state = SplitState::BlockComment;
                }
                '\'' => {
                    has_code = true;
                    current.push(ch);
                    state = SplitState::SingleQuoted;
                }
                '"' => {
                    has_code = true;
                    current.push(ch);
                    state = SplitState::DoubleQuoted;
                }
                '`' => {
                    has_code = true;
                    current.push(ch);
                    state = SplitState::BacktickQuoted;
                }
                ';' => {
                    if has_code {
                        statements.push(std::mem::take(&mut current));
                    } else {
                        current.clear();
                    }
                    has_code = false;
                }
                _ => {
                    if !ch.is_whitespace() {
                        has_code = true;
                    }
                    current.push(ch);
                }
            },
            SplitState::SingleQuoted => {
                current.push(ch);
                if escaped {
                    escaped = false;
                } else if ch == '\\' {
                    escaped = true;
                } else if ch == '\'' {
                    state = SplitState::Code;
                }
            }
            SplitState::DoubleQuoted => {
                current.push(ch);
                if escaped {
                    escaped = false;
                } else if ch == '\\' {
                    escaped = true;
                } else if ch == '"' {
                    state = SplitState::Code;
                }
            }
            SplitState::BacktickQuoted => {
                current.push(ch);
                if ch == '`' {
                    state = SplitState::Code;
                }
            }
            SplitState::LineComment => {
                current.push(ch);
                if ch == '\n' {
                    state = SplitState::Code;
                }
            }
            SplitState::BlockComment => {
                current.push(ch);
                if ch == '*' && chars.peek() == Some(&'/') {
                    current.push(chars.next().expect("peeked character"));
                    state = SplitState::Code;
                }
            }
        }
    }
    if has_code {
        statements.push(current);
    }
    statements
}

/// Lexical context used by [`split_statements`].
#[derive(Clone, Copy, PartialEq, Eq)]
enum SplitState {
    /// Ordinary Cypher text.
    Code,
    /// Inside a single-quoted string.
    SingleQuoted,
    /// Inside a double-quoted string.
    DoubleQuoted,
    /// Inside a backtick-quoted identifier.
    BacktickQuoted,
    /// Inside a `//` comment, through the newline.
    LineComment,
    /// Inside a `/* ... */` comment.
    BlockComment,
}

#[cfg(test)]
mod tests {
    use super::split_statements;

    #[test]
    fn splits_on_top_level_semicolons() {
        let statements = split_statements("CREATE (a); RETURN a;");
        assert_eq!(statements.len(), 2);
        assert_eq!(statements[0], "CREATE (a)");
        assert_eq!(statements[1], " RETURN a");
    }

    #[test]
    fn ignores_semicolons_in_strings_and_identifiers() {
        let statements = split_statements("RETURN 'a;b', \"c;d\", `e;f`; RETURN 1;");
        assert_eq!(statements.len(), 2);
        assert!(statements[0].contains("`e;f`"));
    }

    #[test]
    fn ignores_semicolons_in_line_comments() {
        let statements = split_statements(
            "// MATCH (n) CALL { WITH n DETACH DELETE n } IN TRANSACTIONS OF 10000 ROWS;\n\
             RETURN 1;",
        );
        assert_eq!(statements.len(), 1);
        assert!(statements[0].contains("RETURN 1"));
    }

    #[test]
    fn ignores_semicolons_in_block_comments() {
        let statements = split_statements("RETURN 1 /* a; b; */; RETURN 2;");
        assert_eq!(statements.len(), 2);
        assert!(statements[0].contains("/* a; b; */"));
    }

    #[test]
    fn drops_comment_only_statements() {
        let statements = split_statements(
            "// wipe;\n\
             RETURN 1;\n\
             /* DELETE n; */\n\
             RETURN 2; // trailing;\n",
        );
        assert_eq!(statements.len(), 2);
        assert!(statements[0].contains("RETURN 1"));
        assert!(statements[1].contains("RETURN 2"));
    }

    #[test]
    fn comment_markers_inside_strings_are_not_comments() {
        let statements = split_statements("RETURN '// not a comment;' AS x; RETURN '/* nope */';");
        assert_eq!(statements.len(), 2);
    }

    #[test]
    fn empty_and_comment_only_input_yields_no_statements() {
        let statements = split_statements("  ;; \n\t; // only a comment;\n/* block; */");
        assert!(statements.is_empty());
    }

    #[test]
    fn handles_crlf_line_endings() {
        let statements = split_statements("RETURN 1;\r\n// comment;\r\nRETURN 2;\r\n");
        assert_eq!(statements.len(), 2);
        assert!(statements[1].contains("RETURN 2"));
    }

    #[test]
    fn trailing_comment_without_newline_is_ignored() {
        let statements = split_statements("RETURN 1; // no newline");
        assert_eq!(statements.len(), 1);
        assert_eq!(statements[0], "RETURN 1");
    }
}
