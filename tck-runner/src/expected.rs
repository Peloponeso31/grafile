//! Parser for the TCK's expected-value syntax.
//!
//! Result cells and parameter values are written in a compact Cypher-like
//! syntax: primitives (`1`, `1.5`, `'text'`, `true`, `null`, `NaN`),
//! containers (`[1, 2]`, `{key: 1}`) and graph patterns
//! (`(:Label {prop: 1})`, `[:TYPE {prop: 1}]`, `<()-[:T]->()>`).

use std::fmt;

/// A value expected by a TCK scenario.
#[derive(Debug, Clone, PartialEq)]
pub enum Expected {
    /// The `null` value.
    Null,
    /// A boolean.
    Bool(bool),
    /// An integer.
    Int(i64),
    /// A float, including `NaN`, `Inf` and `-Inf`.
    Float(f64),
    /// A string literal.
    Text(String),
    /// An ordered list.
    List(Vec<Expected>),
    /// A map with string keys.
    Map(Vec<(String, Expected)>),
    /// A node pattern with optional labels and properties.
    Node {
        /// Label names, order-insensitive when compared.
        labels: Vec<String>,
        /// Properties as key/value pairs.
        props: Vec<(String, Expected)>,
    },
    /// A relationship pattern with an optional type and properties.
    Rel {
        /// Relationship type name, if specified.
        ty: Option<String>,
        /// Properties as key/value pairs.
        props: Vec<(String, Expected)>,
    },
    /// A path, alternating nodes and relationships.
    Path(Vec<PathElem>),
}

/// One element of an expected path.
#[derive(Debug, Clone, PartialEq)]
pub enum PathElem {
    /// A node, optionally labelled and with properties.
    Node {
        /// Label names.
        labels: Vec<String>,
        /// Properties.
        props: Vec<(String, Expected)>,
    },
    /// A relationship with direction and optional type.
    Rel {
        /// Relationship type name, if specified.
        ty: Option<String>,
        /// Direction relative to the path.
        direction: Direction,
        /// Properties.
        props: Vec<(String, Expected)>,
    },
}

/// Relationship direction inside an expected path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// `-[:T]->`
    Outgoing,
    /// `<-[:T]-`
    Incoming,
    /// `-[:T]-`
    Undirected,
}

impl fmt::Display for Expected {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Null => f.write_str("null"),
            Self::Bool(value) => write!(f, "{value}"),
            Self::Int(value) => write!(f, "{value}"),
            Self::Float(value) => write_float(f, *value),
            Self::Text(value) => {
                write!(f, "'{}'", value.replace('\\', "\\\\").replace('\'', "\\'"))
            }
            Self::List(items) => {
                f.write_str("[")?;
                for (index, item) in items.iter().enumerate() {
                    if index > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{item}")?;
                }
                f.write_str("]")
            }
            Self::Map(entries) => write_entries(f, "{", "}", entries),
            Self::Node { labels, props } => write_pattern(f, labels, props),
            Self::Rel { ty, props } => write_standalone_rel(f, ty.as_deref(), props),
            Self::Path(elements) => {
                f.write_str("<")?;
                for element in elements {
                    match element {
                        PathElem::Node { labels, props } => write_pattern(f, labels, props)?,
                        PathElem::Rel {
                            ty,
                            direction,
                            props,
                        } => write_rel(f, ty.as_deref(), props, *direction)?,
                    }
                }
                f.write_str(">")
            }
        }
    }
}

fn write_float(f: &mut fmt::Formatter<'_>, value: f64) -> fmt::Result {
    if value.is_nan() {
        return f.write_str("NaN");
    }
    if value.is_infinite() {
        return f.write_str(if value > 0.0 { "Inf" } else { "-Inf" });
    }
    let mut buffer = ryu::Buffer::new();
    f.write_str(buffer.format(value))
}

fn write_entries(
    f: &mut fmt::Formatter<'_>,
    open: &str,
    close: &str,
    entries: &[(String, Expected)],
) -> fmt::Result {
    f.write_str(open)?;
    for (index, (key, value)) in entries.iter().enumerate() {
        if index > 0 {
            f.write_str(", ")?;
        }
        write!(f, "{key}: {value}")?;
    }
    f.write_str(close)
}

fn write_pattern(
    f: &mut fmt::Formatter<'_>,
    labels: &[String],
    props: &[(String, Expected)],
) -> fmt::Result {
    f.write_str("(")?;
    for label in labels {
        write!(f, ":{label}")?;
    }
    if !props.is_empty() {
        if !labels.is_empty() {
            f.write_str(" ")?;
        }
        write_entries(f, "{", "}", props)?;
    }
    f.write_str(")")
}

fn write_standalone_rel(
    f: &mut fmt::Formatter<'_>,
    ty: Option<&str>,
    props: &[(String, Expected)],
) -> fmt::Result {
    f.write_str("[")?;
    if let Some(ty) = ty {
        write!(f, ":{ty}")?;
    }
    if !props.is_empty() {
        if ty.is_some() {
            f.write_str(" ")?;
        }
        write_entries(f, "{", "}", props)?;
    }
    f.write_str("]")
}

fn write_rel(
    f: &mut fmt::Formatter<'_>,
    ty: Option<&str>,
    props: &[(String, Expected)],
    direction: Direction,
) -> fmt::Result {
    if direction == Direction::Incoming {
        f.write_str("<-")?;
    } else {
        f.write_str("-")?;
    }
    f.write_str("[")?;
    if let Some(ty) = ty {
        write!(f, ":{ty}")?;
    }
    if !props.is_empty() {
        if ty.is_some() {
            f.write_str(" ")?;
        }
        write_entries(f, "{", "}", props)?;
    }
    f.write_str("]")?;
    match direction {
        Direction::Outgoing => f.write_str("->"),
        Direction::Incoming | Direction::Undirected => f.write_str("-"),
    }
}

/// Properties of a node or relationship pattern.
type Props = Vec<(String, Expected)>;

/// Parse a single expected-value cell.
///
/// # Errors
///
/// Returns a message describing the offending position when the cell is not
/// valid TCK value syntax.
pub fn parse(text: &str) -> Result<Expected, String> {
    let mut parser = Parser { text, pos: 0 };
    let value = parser.parse_value()?;
    parser.skip_whitespace();
    if parser.pos != parser.text.len() {
        return Err(parser.error("unexpected trailing characters"));
    }
    Ok(value)
}

struct Parser<'a> {
    text: &'a str,
    pos: usize,
}

impl<'a> Parser<'a> {
    fn error(&self, message: &str) -> String {
        let rest = &self.text[self.pos..];
        let snippet: String = rest.chars().take(20).collect();
        if snippet.is_empty() {
            format!("{message} at end of `{}`", self.text)
        } else {
            format!("{message} near `{snippet}` in `{}`", self.text)
        }
    }

    fn peek(&self) -> Option<char> {
        self.text[self.pos..].chars().next()
    }

    fn bump(&mut self) -> Option<char> {
        let ch = self.peek()?;
        self.pos += ch.len_utf8();
        Some(ch)
    }

    fn skip_whitespace(&mut self) {
        while matches!(self.peek(), Some(ch) if ch.is_whitespace()) {
            self.pos += 1;
        }
    }

    fn eat(&mut self, expected: char) -> bool {
        self.skip_whitespace();
        if self.peek() == Some(expected) {
            self.pos += expected.len_utf8();
            true
        } else {
            false
        }
    }

    fn eat_str(&mut self, expected: &str) -> bool {
        if self.text[self.pos..].starts_with(expected) {
            self.pos += expected.len();
            true
        } else {
            false
        }
    }

    fn expect(&mut self, expected: char) -> Result<(), String> {
        if self.eat(expected) {
            Ok(())
        } else {
            Err(self.error(&format!("expected `{expected}`")))
        }
    }

    fn parse_value(&mut self) -> Result<Expected, String> {
        self.skip_whitespace();
        let Some(ch) = self.peek() else {
            return Err(self.error("expected a value"));
        };
        match ch {
            '\'' => Ok(Expected::Text(self.parse_string()?)),
            '[' => {
                if self.starts_relationship() {
                    let (ty, props) = self.parse_rel_pattern()?;
                    Ok(Expected::Rel { ty, props })
                } else {
                    self.parse_list()
                }
            }
            '{' => self.parse_map().map(Expected::Map),
            '(' => {
                let (labels, props) = self.parse_node_pattern()?;
                Ok(Expected::Node { labels, props })
            }
            '<' => self.parse_path(),
            't' | 'f' | 'n' | 'N' | 'I' => self.parse_keyword(),
            '-' | '0'..='9' => self.parse_number(),
            _ => Err(self.error("expected a value")),
        }
    }

    fn parse_keyword(&mut self) -> Result<Expected, String> {
        let rest = &self.text[self.pos..];
        for (word, value) in [
            ("true", Expected::Bool(true)),
            ("false", Expected::Bool(false)),
            ("null", Expected::Null),
            ("NaN", Expected::Float(f64::NAN)),
            ("Inf", Expected::Float(f64::INFINITY)),
        ] {
            if let Some(rest) = rest.strip_prefix(word) {
                let boundary = match rest.chars().next() {
                    None => true,
                    Some(ch) => !ch.is_alphanumeric() && ch != '_',
                };
                if boundary {
                    self.pos += word.len();
                    return Ok(value);
                }
            }
        }
        Err(self.error("unknown keyword"))
    }

    fn parse_string(&mut self) -> Result<String, String> {
        self.expect('\'')?;
        let mut out = String::new();
        loop {
            match self.bump() {
                None => return Err(self.error("unterminated string")),
                Some('\'') => return Ok(out),
                Some('\\') => self.parse_escape(&mut out)?,
                Some(ch) => out.push(ch),
            }
        }
    }

    /// Decode one Cypher string escape (the backslash is already consumed).
    fn parse_escape(&mut self, out: &mut String) -> Result<(), String> {
        match self.bump() {
            None => Err(self.error("unterminated escape")),
            Some('\\') => {
                out.push('\\');
                Ok(())
            }
            Some('\'') => {
                out.push('\'');
                Ok(())
            }
            Some('"') => {
                out.push('"');
                Ok(())
            }
            Some('n') => {
                out.push('\n');
                Ok(())
            }
            Some('t') => {
                out.push('\t');
                Ok(())
            }
            Some('b') => {
                out.push('\u{0008}');
                Ok(())
            }
            Some('f') => {
                out.push('\u{000c}');
                Ok(())
            }
            Some('r') => {
                out.push('\r');
                Ok(())
            }
            Some('u') => self.parse_unicode_escape(out, 4),
            Some('U') => self.parse_unicode_escape(out, 8),
            Some(other) => {
                out.push('\\');
                out.push(other);
                Ok(())
            }
        }
    }

    fn parse_unicode_escape(&mut self, out: &mut String, digits: usize) -> Result<(), String> {
        let mut code = 0u32;
        for _ in 0..digits {
            let Some(ch) = self.peek() else {
                return Err(self.error("incomplete unicode escape"));
            };
            let Some(value) = ch.to_digit(16) else {
                return Err(self.error("malformed unicode escape"));
            };
            code = code * 16 + value;
            self.pos += 1;
        }
        match char::from_u32(code) {
            Some(ch) => {
                out.push(ch);
                Ok(())
            }
            None => Err(self.error("unicode escape is not a scalar value")),
        }
    }

    fn parse_number(&mut self) -> Result<Expected, String> {
        let start = self.pos;
        if self.peek() == Some('-') {
            self.pos += 1;
        }
        let rest = &self.text[self.pos..];
        if rest.starts_with("Inf") {
            self.pos += 3;
            return Ok(Expected::Float(f64::NEG_INFINITY));
        }
        let mut seen_digit = false;
        let mut float = false;
        while let Some(ch) = self.peek() {
            match ch {
                '0'..='9' => {
                    seen_digit = true;
                    self.pos += 1;
                }
                '.' => {
                    float = true;
                    self.pos += 1;
                }
                'e' | 'E' => {
                    float = true;
                    self.pos += 1;
                    if matches!(self.peek(), Some('+') | Some('-')) {
                        self.pos += 1;
                    }
                }
                _ => break,
            }
        }
        if !seen_digit {
            return Err(self.error("malformed number"));
        }
        let text = &self.text[start..self.pos];
        if float {
            text.parse::<f64>()
                .map(Expected::Float)
                .map_err(|_| self.error("malformed float"))
        } else {
            text.parse::<i64>()
                .map(Expected::Int)
                .map_err(|_| self.error("malformed integer"))
        }
    }

    fn starts_relationship(&self) -> bool {
        self.text[self.pos + 1..].trim_start().starts_with(':')
    }

    fn parse_list(&mut self) -> Result<Expected, String> {
        self.expect('[')?;
        let mut items = Vec::new();
        self.skip_whitespace();
        if self.eat(']') {
            return Ok(Expected::List(items));
        }
        loop {
            items.push(self.parse_value()?);
            if self.eat(']') {
                return Ok(Expected::List(items));
            }
            self.expect(',')?;
        }
    }

    fn parse_map(&mut self) -> Result<Vec<(String, Expected)>, String> {
        self.expect('{')?;
        let mut entries = Vec::new();
        self.skip_whitespace();
        if self.eat('}') {
            return Ok(entries);
        }
        loop {
            let key = self.parse_key()?;
            self.expect(':')?;
            let value = self.parse_value()?;
            entries.push((key, value));
            if self.eat('}') {
                return Ok(entries);
            }
            self.expect(',')?;
        }
    }

    fn parse_key(&mut self) -> Result<String, String> {
        self.skip_whitespace();
        match self.peek() {
            Some('\'') => self.parse_string(),
            Some(ch) if ch.is_alphabetic() || ch == '_' || ch == '`' => {
                if ch == '`' {
                    self.pos += 1;
                    let start = self.pos;
                    while let Some(inner) = self.peek() {
                        if inner == '`' {
                            let key = self.text[start..self.pos].to_owned();
                            self.pos += 1;
                            return Ok(key);
                        }
                        self.pos += inner.len_utf8();
                    }
                    return Err(self.error("unterminated quoted key"));
                }
                let start = self.pos;
                while let Some(inner) = self.peek() {
                    if inner.is_alphanumeric() || inner == '_' {
                        self.pos += inner.len_utf8();
                    } else {
                        break;
                    }
                }
                Ok(self.text[start..self.pos].to_owned())
            }
            _ => Err(self.error("expected a key")),
        }
    }

    fn parse_node_pattern(&mut self) -> Result<(Vec<String>, Props), String> {
        self.expect('(')?;
        let mut labels = Vec::new();
        loop {
            self.skip_whitespace();
            if self.eat(':') {
                labels.push(self.parse_key()?);
            } else {
                break;
            }
        }
        self.skip_whitespace();
        let props = if self.peek() == Some('{') {
            self.parse_map()?
        } else {
            Vec::new()
        };
        self.expect(')')?;
        Ok((labels, props))
    }

    fn parse_rel_pattern(&mut self) -> Result<(Option<String>, Props), String> {
        self.expect('[')?;
        self.skip_whitespace();
        let ty = if self.eat(':') {
            Some(self.parse_key()?)
        } else {
            None
        };
        self.skip_whitespace();
        let props = if self.peek() == Some('{') {
            self.parse_map()?
        } else {
            Vec::new()
        };
        self.expect(']')?;
        Ok((ty, props))
    }

    fn parse_path(&mut self) -> Result<Expected, String> {
        self.expect('<')?;
        let mut elements = Vec::new();
        let (labels, props) = self.parse_node_pattern()?;
        elements.push(PathElem::Node { labels, props });
        loop {
            self.skip_whitespace();
            if self.eat('>') {
                return Ok(Expected::Path(elements));
            }
            let incoming = self.eat_str("<-");
            if !incoming {
                self.expect('-')?;
            }
            let (ty, props) = self.parse_rel_pattern()?;
            let direction = if self.eat_str("->") {
                Direction::Outgoing
            } else {
                self.expect('-')?;
                if incoming {
                    Direction::Incoming
                } else {
                    Direction::Undirected
                }
            };
            elements.push(PathElem::Rel {
                ty,
                direction,
                props,
            });
            self.skip_whitespace();
            let (labels, props) = self.parse_node_pattern()?;
            elements.push(PathElem::Node { labels, props });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_primitives() {
        assert_eq!(parse("null").expect("null"), Expected::Null);
        assert_eq!(parse("true").expect("bool"), Expected::Bool(true));
        assert_eq!(parse("-12").expect("int"), Expected::Int(-12));
        assert_eq!(parse("1.5").expect("float"), Expected::Float(1.5));
        assert_eq!(parse("1e3").expect("exp"), Expected::Float(1000.0));
        assert_eq!(
            parse("-Inf").expect("neg inf"),
            Expected::Float(f64::NEG_INFINITY)
        );
        assert!(matches!(parse("NaN").expect("nan"), Expected::Float(f) if f.is_nan()));
        assert_eq!(
            parse("'it\\'s'").expect("text"),
            Expected::Text("it's".into())
        );
    }

    #[test]
    fn parses_containers() {
        assert_eq!(
            parse("[1, 'a', null]").expect("list"),
            Expected::List(vec![
                Expected::Int(1),
                Expected::Text("a".into()),
                Expected::Null
            ])
        );
        assert_eq!(
            parse("{a: 1, b: [true]}").expect("map"),
            Expected::Map(vec![
                ("a".into(), Expected::Int(1)),
                ("b".into(), Expected::List(vec![Expected::Bool(true)])),
            ])
        );
    }

    #[test]
    fn parses_standalone_relationships() {
        assert_eq!(
            parse("[:T]").expect("rel"),
            Expected::Rel {
                ty: Some("T".into()),
                props: Vec::new(),
            }
        );
        let list = parse("[(:A), [:T], (:B)]").expect("list of patterns");
        match list {
            Expected::List(items) => assert_eq!(items.len(), 3),
            other => panic!("expected list, got {other:?}"),
        }
    }

    #[test]
    fn parses_patterns() {
        let node = parse("(:A:B {name: 'x'})").expect("node");
        assert_eq!(
            node,
            Expected::Node {
                labels: vec!["A".into(), "B".into()],
                props: vec![("name".into(), Expected::Text("x".into()))],
            }
        );
        let path = parse("<(:A)-[:T]->({n: 1})>").expect("path");
        match path {
            Expected::Path(elements) => {
                assert_eq!(elements.len(), 3);
                assert!(matches!(
                    elements[1],
                    PathElem::Rel {
                        direction: Direction::Outgoing,
                        ..
                    }
                ));
            }
            other => panic!("expected path, got {other:?}"),
        }
    }

    #[test]
    fn rejects_trailing_input() {
        assert!(parse("1 2").is_err());
        assert!(parse("'unterminated").is_err());
    }
}
