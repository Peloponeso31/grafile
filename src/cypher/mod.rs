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
    let mut scanner = StatementScanner::default();
    let mut start = 0;
    scanner.feed(text, |semicolon, has_code| {
        if has_code {
            statements.push(text[start..semicolon].to_owned());
        }
        start = semicolon + 1;
    });
    if scanner.has_code {
        statements.push(text[start..].to_owned());
    }
    statements
}

/// Incremental lexical scanner that finds top-level semicolons, so callers
/// reading input line by line know whether they stand between statements.
///
/// Feed text in pieces that end at line breaks: comment and string markers
/// never span a line break, so lookahead stays within one piece.
///
/// # Examples
///
/// ```
/// use grafile::cypher::StatementScanner;
///
/// let mut scanner = StatementScanner::default();
/// scanner.feed("RETURN 'a;\n", |_, _| {});
/// assert!(!scanner.is_between_statements());
/// scanner.feed("b';\n", |_, _| {});
/// assert!(scanner.is_between_statements());
/// ```
#[derive(Debug, Clone, Default)]
pub struct StatementScanner {
    state: SplitState,
    escaped: bool,
    has_code: bool,
}

impl StatementScanner {
    /// Scan `text`, calling `on_semicolon(offset, has_code)` for every
    /// top-level semicolon with its byte offset in `text` and whether code
    /// (anything but whitespace and comments) preceded it since the last one.
    pub fn feed(&mut self, text: &str, mut on_semicolon: impl FnMut(usize, bool)) {
        let mut chars = text.char_indices().peekable();
        while let Some((offset, ch)) = chars.next() {
            let next = chars.peek().map(|(_, next)| *next);
            match self.state {
                SplitState::Code => match ch {
                    '/' if next == Some('/') => {
                        chars.next();
                        self.state = SplitState::LineComment;
                    }
                    '/' if next == Some('*') => {
                        chars.next();
                        self.state = SplitState::BlockComment;
                    }
                    '\'' | '"' | '`' => {
                        self.has_code = true;
                        self.state = match ch {
                            '\'' => SplitState::SingleQuoted,
                            '"' => SplitState::DoubleQuoted,
                            _ => SplitState::BacktickQuoted,
                        };
                    }
                    ';' => {
                        on_semicolon(offset, self.has_code);
                        self.has_code = false;
                    }
                    _ => self.has_code |= !ch.is_whitespace(),
                },
                SplitState::SingleQuoted | SplitState::DoubleQuoted => {
                    let quote = if self.state == SplitState::SingleQuoted {
                        '\''
                    } else {
                        '"'
                    };
                    if self.escaped {
                        self.escaped = false;
                    } else if ch == '\\' {
                        self.escaped = true;
                    } else if ch == quote {
                        self.state = SplitState::Code;
                    }
                }
                SplitState::BacktickQuoted => {
                    if ch == '`' {
                        self.state = SplitState::Code;
                    }
                }
                SplitState::LineComment => {
                    if ch == '\n' {
                        self.state = SplitState::Code;
                    }
                }
                SplitState::BlockComment => {
                    if ch == '*' && next == Some('/') {
                        chars.next();
                        self.state = SplitState::Code;
                    }
                }
            }
        }
    }

    /// Whether the text so far ends outside any string, identifier or block
    /// comment, with no code after the last top-level semicolon.
    pub fn is_between_statements(&self) -> bool {
        !self.has_code && self.is_outside_literals()
    }

    /// Whether the text so far ends outside any string, quoted identifier or
    /// block comment (a trailing `//` comment counts as outside).
    pub fn is_outside_literals(&self) -> bool {
        matches!(self.state, SplitState::Code | SplitState::LineComment)
    }
}

/// Lexical context used by [`StatementScanner`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum SplitState {
    /// Ordinary Cypher text.
    #[default]
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
    use super::{StatementScanner, split_statements};

    #[test]
    fn scanner_tracks_strings_and_comments_across_lines() {
        let mut scanner = StatementScanner::default();
        scanner.feed("RETURN 1 /* note;\n", |_, _| {});
        assert!(!scanner.is_outside_literals());
        scanner.feed("*/;\n", |_, _| {});
        assert!(scanner.is_between_statements());
        scanner.feed("RETURN 2 // done;\n", |_, _| {});
        assert!(!scanner.is_between_statements());
        assert!(scanner.is_outside_literals());
    }

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
