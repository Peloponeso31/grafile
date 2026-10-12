//! Cypher lexer built on `logos`.
//!
//! Keywords are matched case-insensitively; identifiers, strings, numbers and
//! parameters keep their source text for the parser to interpret. The lexer
//! records byte spans so the parser can report line/column positions.

use logos::Logos;

/// A Cypher token.
#[derive(Logos, Debug, Clone, Copy, PartialEq, Eq)]
#[logos(skip r"[ \t\r\n]+")]
#[logos(skip(r"//[^\n]*", allow_greedy = true))]
#[logos(skip r"/\*([^*]|\*+[^*/])*\*+/")]
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
