//! Cypher lexer built on `logos`.
//!
//! Keywords are matched case-insensitively; identifiers, strings, numbers and
//! parameters keep their source text for the parser to interpret. The lexer
//! records byte spans so the parser can report line/column positions.

use logos::Logos;

/// A Cypher token.
#[derive(Logos, Debug, Clone, Copy, PartialEq, Eq)]
#[logos(skip r"[ \t\r\n]+")]
pub enum Token {
    /// `MATCH`
    #[token("MATCH", ignore(case))]
    Match,
    /// `OPTIONAL`
    #[token("OPTIONAL", ignore(case))]
    Optional,
    /// `WHERE`
    #[token("WHERE", ignore(case))]
    Where,
    /// `RETURN`
    #[token("RETURN", ignore(case))]
    Return,
    /// `WITH`
    #[token("WITH", ignore(case))]
    With,
    /// `UNWIND`
    #[token("UNWIND", ignore(case))]
    Unwind,
    /// `CREATE`
    #[token("CREATE", ignore(case))]
    Create,
    /// `MERGE`
    #[token("MERGE", ignore(case))]
    Merge,
    /// `SET`
    #[token("SET", ignore(case))]
    Set,
    /// `REMOVE`
    #[token("REMOVE", ignore(case))]
    Remove,
    /// `DELETE`
    #[token("DELETE", ignore(case))]
    Delete,
    /// `DETACH`
    #[token("DETACH", ignore(case))]
    Detach,
    /// `ORDER`
    #[token("ORDER", ignore(case))]
    Order,
    /// `BY`
    #[token("BY", ignore(case))]
    By,
    /// `SKIP`
    #[token("SKIP", ignore(case))]
    Skip,
    /// `LIMIT`
    #[token("LIMIT", ignore(case))]
    Limit,
    /// `AS`
    #[token("AS", ignore(case))]
    As,
    /// `AND`
    #[token("AND", ignore(case))]
    And,
    /// `OR`
    #[token("OR", ignore(case))]
    Or,
    /// `XOR`
    #[token("XOR", ignore(case))]
    Xor,
    /// `NOT`
    #[token("NOT", ignore(case))]
    Not,
    /// `IN`
    #[token("IN", ignore(case))]
    In,
    /// `IS`
    #[token("IS", ignore(case))]
    Is,
    /// `NULL`
    #[token("NULL", ignore(case))]
    Null,
    /// `TRUE`
    #[token("TRUE", ignore(case))]
    True,
    /// `FALSE`
    #[token("FALSE", ignore(case))]
    False,
    /// `DISTINCT`
    #[token("DISTINCT", ignore(case))]
    Distinct,
    /// `STARTS`
    #[token("STARTS", ignore(case))]
    Starts,
    /// `ENDS`
    #[token("ENDS", ignore(case))]
    Ends,
    /// `CONTAINS`
    #[token("CONTAINS", ignore(case))]
    Contains,
    /// `ASC`
    #[token("ASC", ignore(case))]
    Asc,
    /// `ASCENDING`
    #[token("ASCENDING", ignore(case))]
    Ascending,
    /// `DESC`
    #[token("DESC", ignore(case))]
    Desc,
    /// `DESCENDING`
    #[token("DESCENDING", ignore(case))]
    Descending,
    /// `ON`
    #[token("ON", ignore(case))]
    On,
    /// `CASE`
    #[token("CASE", ignore(case))]
    Case,
    /// `WHEN`
    #[token("WHEN", ignore(case))]
    When,
    /// `THEN`
    #[token("THEN", ignore(case))]
    Then,
    /// `ELSE`
    #[token("ELSE", ignore(case))]
    Else,
    /// `END`
    #[token("END", ignore(case))]
    End,
    /// `UNION`
    #[token("UNION", ignore(case))]
    Union,
    /// `ALL`
    #[token("ALL", ignore(case))]
    All,

    /// Identifier.
    #[regex(r"[A-Za-z_][A-Za-z0-9_]*")]
    Ident,
    /// Backtick-quoted identifier (escapes doubled backticks).
    #[regex(r"`([^`]|``)+`")]
    EscapedIdent,
    /// Decimal integer.
    #[regex(r"[0-9]+")]
    Integer,
    /// Floating point literal.
    #[regex(r"([0-9]+\.[0-9]+|\.[0-9]+)([eE][+-]?[0-9]+)?|[0-9]+[eE][+-]?[0-9]+")]
    Float,
    /// Single-quoted string.
    #[regex(r"'([^'\\]|\\.)*'")]
    StringSingle,
    /// Double-quoted string.
    #[regex(r#""([^"\\]|\\.)*""#)]
    StringDouble,
    /// `$parameter`.
    #[regex(r"\$[A-Za-z_][A-Za-z0-9_]*")]
    Param,

    /// `=~`
    #[token("=~")]
    RegexMatch,
    /// `<>`
    #[token("<>")]
    NotEquals,
    /// `<=`
    #[token("<=")]
    LessEquals,
    /// `>=`
    #[token(">=")]
    GreaterEquals,
    /// `->`
    #[token("->")]
    ArrowRight,
    /// `<-`
    #[token("<-")]
    ArrowLeft,
    /// `..`
    #[token("..")]
    DotDot,
    /// `+`
    #[token("+")]
    Plus,
    /// `-`
    #[token("-")]
    Minus,
    /// `*`
    #[token("*")]
    Star,
    /// `/`
    #[token("/")]
    Slash,
    /// `%`
    #[token("%")]
    Percent,
    /// `^`
    #[token("^")]
    Caret,
    /// `=`
    #[token("=")]
    Equals,
    /// `<`
    #[token("<")]
    Less,
    /// `>`
    #[token(">")]
    Greater,
    /// `.`
    #[token(".")]
    Dot,
    /// `,`
    #[token(",")]
    Comma,
    /// `:`
    #[token(":")]
    Colon,
    /// `;`
    #[token(";")]
    Semicolon,
    /// `|`
    #[token("|")]
    Pipe,
    /// `(`
    #[token("(")]
    LParen,
    /// `)`
    #[token(")")]
    RParen,
    /// `[`
    #[token("[")]
    LBracket,
    /// `]`
    #[token("]")]
    RBracket,
    /// `{`
    #[token("{")]
    LBrace,
    /// `}`
    #[token("}")]
    RBrace,

    /// `// ...` line comment.
    #[regex(r"//[^\n]*", allow_greedy = true)]
    LineComment,
    /// `/* ... */` block comment.
    #[regex(r"/\*([^*]|\*+[^*/])*\*+/")]
    BlockComment,
}

/// A lexed token with its byte span.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Spanned {
    /// The token.
    pub token: Token,
    /// Byte offset of the first character.
    pub start: usize,
    /// Byte offset one past the last character.
    pub end: usize,
}

/// Lex `source` into tokens, returning the source position of the first
/// invalid character as an error.
pub fn lex(source: &str) -> Result<Vec<Spanned>, (usize, usize)> {
    let mut tokens = Vec::new();
    let mut lexer = Token::lexer(source);
    while let Some(result) = lexer.next() {
        match result {
            Ok(token) => {
                // Comments are lexed so the highlighter can color them, but
                // the parser never sees them.
                if matches!(token, Token::LineComment | Token::BlockComment) {
                    continue;
                }
                let span = lexer.span();
                tokens.push(Spanned {
                    token,
                    start: span.start,
                    end: span.end,
                });
            }
            Err(()) => {
                let span = lexer.span();
                return Err((span.start, span.end));
            }
        }
    }
    Ok(tokens)
}

/// Convert a byte offset into a 1-based line and column.
pub fn line_column(source: &str, offset: usize) -> (usize, usize) {
    let mut line = 1usize;
    let mut column = 1usize;
    for (idx, ch) in source.char_indices() {
        if idx >= offset {
            break;
        }
        if ch == '\n' {
            line += 1;
            column = 1;
        } else {
            column += 1;
        }
    }
    (line, column)
}

/// Classification of a lexed token for syntax highlighting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenClass {
    /// Cypher keywords (`MATCH`, `RETURN`, ...).
    Keyword,
    /// Function and aggregation names (`count`, `collect`, ...).
    Function,
    /// String literals.
    String,
    /// Numeric literals.
    Number,
    /// `$parameters`.
    Parameter,
    /// Line and block comments.
    Comment,
    /// Operators and arrows.
    Operator,
    /// Label and edge type names (after `:` or `|`).
    Label,
    /// Property keys (after `.`).
    Property,
    /// Punctuation (`(`, `)`, `,`, ...).
    Punctuation,
    /// Plain identifiers.
    Variable,
}

/// Lex `line` tolerantly and classify each token for highlighting.
///
/// Unlike [`lex`], invalid characters are skipped rather than reported, so
/// half-typed input (`MATCH (n:Person`, unterminated strings) still
/// highlights. Returned spans are ordered, non-overlapping and byte-aligned.
pub fn highlight_spans(line: &str) -> Vec<(usize, usize, TokenClass)> {
    let mut tokens: Vec<(usize, usize, Token)> = Vec::new();
    let mut lexer = Token::lexer(line);
    while let Some(result) = lexer.next() {
        if let Ok(token) = result {
            let span = lexer.span();
            tokens.push((span.start, span.end, token));
        }
        // On error logos advances at least one byte, so iteration terminates.
    }
    tokens
        .iter()
        .enumerate()
        .map(|(index, (start, end, token))| (*start, *end, classify(*token, &tokens, index)))
        .collect()
}

fn classify(token: Token, tokens: &[(usize, usize, Token)], index: usize) -> TokenClass {
    use Token::*;
    match token {
        Match | Optional | Where | Return | With | Unwind | Create | Merge | Set | Remove
        | Delete | Detach | Order | By | Skip | Limit | As | And | Or | Xor | Not | In | Is
        | Null | True | False | Distinct | Starts | Ends | Contains | Asc | Ascending | Desc
        | Descending | On | Case | When | Then | Else | End | Union | All => TokenClass::Keyword,
        LineComment | BlockComment => TokenClass::Comment,
        StringSingle | StringDouble => TokenClass::String,
        Integer | Float => TokenClass::Number,
        Param => TokenClass::Parameter,
        Ident => {
            // `name(` is a function or aggregation call.
            if matches!(tokens.get(index + 1).map(|entry| entry.2), Some(LParen)) {
                TokenClass::Function
            } else {
                context_class(tokens, index)
            }
        }
        EscapedIdent => context_class(tokens, index),
        Plus | Minus | Star | Slash | Percent | Caret | Equals | Less | Greater | NotEquals
        | LessEquals | GreaterEquals | RegexMatch | ArrowRight | ArrowLeft | DotDot => {
            TokenClass::Operator
        }
        LParen | RParen | LBracket | RBracket | LBrace | RBrace | Comma | Semicolon | Colon
        | Pipe | Dot => TokenClass::Punctuation,
    }
}

fn context_class(tokens: &[(usize, usize, Token)], index: usize) -> TokenClass {
    match index
        .checked_sub(1)
        .and_then(|previous| tokens.get(previous))
        .map(|entry| entry.2)
    {
        Some(Token::Colon) | Some(Token::Pipe) => TokenClass::Label,
        Some(Token::Dot) => TokenClass::Property,
        _ => TokenClass::Variable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens(source: &str) -> Vec<Token> {
        lex(source)
            .expect("lex")
            .into_iter()
            .map(|spanned| spanned.token)
            .collect()
    }

    #[test]
    fn keywords_are_case_insensitive() {
        assert_eq!(
            tokens("match RETURN Where"),
            vec![Token::Match, Token::Return, Token::Where]
        );
    }

    #[test]
    fn identifiers_and_escaped_identifiers() {
        assert_eq!(
            tokens("person `weird name`"),
            vec![Token::Ident, Token::EscapedIdent]
        );
    }

    #[test]
    fn numbers() {
        assert_eq!(
            tokens("42 3.14 .5 1e10 2.5E-3"),
            vec![
                Token::Integer,
                Token::Float,
                Token::Float,
                Token::Float,
                Token::Float
            ]
        );
    }

    #[test]
    fn strings_and_params() {
        assert_eq!(
            tokens("'hello' \"world\" $name"),
            vec![Token::StringSingle, Token::StringDouble, Token::Param]
        );
    }

    #[test]
    fn operators_and_arrows() {
        assert_eq!(
            tokens("-> <- --> <-- =~ <> <= >="),
            vec![
                Token::ArrowRight,
                Token::ArrowLeft,
                Token::Minus,
                Token::ArrowRight,
                Token::ArrowLeft,
                Token::Minus,
                Token::RegexMatch,
                Token::NotEquals,
                Token::LessEquals,
                Token::GreaterEquals,
            ]
        );
    }

    #[test]
    fn comments_are_skipped() {
        assert_eq!(
            tokens("MATCH // line comment\n RETURN /* block\ncomment */ 1"),
            vec![Token::Match, Token::Return, Token::Integer]
        );
    }

    #[test]
    fn invalid_character_reports_position() {
        let err = lex("MATCH @").expect_err("invalid");
        assert_eq!(err.0, 6);
    }

    #[test]
    fn line_column_tracks_newlines() {
        assert_eq!(line_column("ab\ncd", 0), (1, 1));
        assert_eq!(line_column("ab\ncd", 3), (2, 1));
        assert_eq!(line_column("ab\ncd", 4), (2, 2));
    }
}

#[cfg(test)]
mod highlight_tests {
    use super::*;

    fn classes(line: &str) -> Vec<TokenClass> {
        highlight_spans(line)
            .into_iter()
            .map(|(_, _, class)| class)
            .collect()
    }

    fn class_of(line: &str, needle: &str) -> TokenClass {
        let start = line.find(needle).expect("needle in line");
        highlight_spans(line)
            .into_iter()
            .find(|(s, _, _)| *s == start)
            .map(|(_, _, class)| class)
            .expect("token at needle")
    }

    #[test]
    fn classifies_keywords_functions_and_literals() {
        assert_eq!(
            classes("MATCH (n) RETURN count(*)"),
            vec![
                TokenClass::Keyword,
                TokenClass::Punctuation,
                TokenClass::Variable,
                TokenClass::Punctuation,
                TokenClass::Keyword,
                TokenClass::Function,
                TokenClass::Punctuation,
                TokenClass::Operator,
                TokenClass::Punctuation,
            ]
        );
        assert_eq!(class_of("RETURN 'x'", "'x'"), TokenClass::String);
        assert_eq!(class_of("RETURN 42", "42"), TokenClass::Number);
        assert_eq!(class_of("RETURN $p", "$p"), TokenClass::Parameter);
    }

    #[test]
    fn classifies_labels_properties_and_comments() {
        assert_eq!(class_of("MATCH (n:Person)", "Person"), TokenClass::Label);
        assert_eq!(
            class_of("MATCH ()-[:KNOWS|LIKES]->()", "LIKES"),
            TokenClass::Label
        );
        assert_eq!(class_of("RETURN n.name", "name"), TokenClass::Property);
        assert_eq!(
            class_of("MATCH (n) // comment", "// comment"),
            TokenClass::Comment
        );
        assert_eq!(
            class_of("MATCH (n) /* block */ RETURN n", "/* block */"),
            TokenClass::Comment
        );
    }

    #[test]
    fn tolerates_incomplete_input() {
        // Unterminated string: no panic, spans stay in bounds.
        let line = "MATCH (n {name: 'abc";
        for (start, end, _) in highlight_spans(line) {
            assert!(start < end && end <= line.len());
        }
        // Invalid character.
        assert!(!highlight_spans("MATCH @@ RETURN").is_empty());
    }

    #[test]
    fn spans_are_ordered_and_non_overlapping() {
        let line = "MATCH (a:Person {name: 'x'})-[:KNOWS]->(b) RETURN a.name, b.age";
        let spans = highlight_spans(line);
        let mut previous_end = 0usize;
        for (start, end, _) in &spans {
            assert!(*start >= previous_end, "spans overlap: {spans:?}");
            assert!(start < end, "empty span");
            previous_end = *end;
        }
        // Concatenating the source slices reproduces the line exactly.
        let joined: String = spans.iter().map(|(s, e, _)| &line[*s..*e]).collect();
        let without_whitespace: String = line.chars().filter(|c| !c.is_whitespace()).collect();
        let joined_without_whitespace: String =
            joined.chars().filter(|c| !c.is_whitespace()).collect();
        assert_eq!(joined_without_whitespace, without_whitespace);
    }

    #[test]
    fn lex_still_filters_comments() {
        let tokens: Vec<Token> = lex("MATCH // hi\nRETURN /* x */ 1")
            .expect("lex")
            .into_iter()
            .map(|spanned| spanned.token)
            .collect();
        assert_eq!(tokens, vec![Token::Match, Token::Return, Token::Integer]);
    }
}
