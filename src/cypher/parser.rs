//! Cypher parser: recursive descent for clauses, Pratt-style precedence
//! climbing for expressions.
//!
//! Error messages carry 1-based line/column positions computed from the
//! token span.

use std::sync::Arc;

use super::ast::*;
use super::lexer::{Spanned, Token, lex, line_column};
use crate::error::{Error, Result};
use crate::record::Value;

/// Parse a full Cypher query.
pub fn parse(source: &str) -> Result<Query> {
    let tokens = lex(source).map_err(|(start, _)| {
        let (line, column) = line_column(source, start);
        Error::syntax(format!("invalid character at line {line} column {column}"))
    })?;
    let mut parser = Parser {
        source,
        tokens,
        pos: 0,
    };
    let query = parser.parse_query()?;
    Ok(query)
}

struct Parser<'a> {
    source: &'a str,
    tokens: Vec<Spanned>,
    pos: usize,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<Token> {
        self.tokens.get(self.pos).map(|spanned| spanned.token)
    }

    fn advance(&mut self) -> Option<Token> {
        let token = self.peek()?;
        self.pos += 1;
        Some(token)
    }

    fn eat(&mut self, token: Token) -> bool {
        if self.peek() == Some(token) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, token: Token, what: &str) -> Result<()> {
        if self.eat(token) {
            Ok(())
        } else {
            self.fail(format!("expected {what}"))
        }
    }

    fn fail<T>(&self, message: impl Into<String>) -> Result<T> {
        let (line, column, found) = match self.tokens.get(self.pos) {
            Some(spanned) => {
                let (line, column) = line_column(self.source, spanned.start);
                (line, column, describe(spanned.token))
            }
            None => {
                let (line, column) = line_column(self.source, self.source.len());
                (line, column, "end of input".to_owned())
            }
        };
        Err(Error::syntax(format!(
            "{} at line {line} column {column} (found {found})",
            message.into()
        )))
    }

    fn parse_query(&mut self) -> Result<Query> {
        let mut parts = vec![QueryPart {
            union_all: false,
            clauses: self.parse_clauses()?,
        }];
        let mut union_all = None;
        while self.eat(Token::Union) {
            let all = self.eat(Token::All);
            match union_all {
                None => union_all = Some(all),
                Some(previous) if previous != all => {
                    return self.fail("UNION and UNION ALL cannot be mixed");
                }
                Some(_) => {}
            }
            parts.push(QueryPart {
                union_all: all,
                clauses: self.parse_clauses()?,
            });
        }
        // Optional trailing semicolon.
        self.eat(Token::Semicolon);
        if self.peek().is_some() {
            return self.fail("unexpected input after end of query");
        }
        Ok(Query { parts })
    }

    /// Parse the clauses of one query part, up to `UNION` or the end.
    fn parse_clauses(&mut self) -> Result<Vec<Clause>> {
        let mut clauses = Vec::new();
        while self.peek().is_some()
            && self.peek() != Some(Token::Semicolon)
            && self.peek() != Some(Token::Union)
        {
            clauses.push(self.parse_clause()?);
        }
        if clauses.is_empty() {
            return self.fail("empty query");
        }
        Ok(clauses)
    }

    fn parse_clause(&mut self) -> Result<Clause> {
        match self.peek() {
            Some(Token::Match) => {
                self.advance();
                Ok(Clause::Match(self.parse_match_clause(false)?))
            }
            Some(Token::Optional) => {
                self.advance();
                self.expect(Token::Match, "MATCH after OPTIONAL")?;
                Ok(Clause::Match(self.parse_match_clause(true)?))
            }
            Some(Token::Return) => {
                self.advance();
                self.parse_return_clause()
            }
            Some(Token::With) => {
                self.advance();
                self.parse_with_clause()
            }
            Some(Token::Unwind) => {
                self.advance();
                let expr = self.parse_expr()?;
                self.expect(Token::As, "AS after UNWIND expression")?;
                let alias = self.parse_name()?;
                Ok(Clause::Unwind { expr, alias })
            }
            Some(Token::Create) => {
                self.advance();
                Ok(Clause::Create(self.parse_patterns()?))
            }
            Some(Token::Merge) => {
                self.advance();
                self.parse_merge_clause()
            }
            Some(Token::Set) => {
                self.advance();
                Ok(Clause::Set(self.parse_set_items(false)?))
            }
            Some(Token::Remove) => {
                self.advance();
                Ok(Clause::Remove(self.parse_set_items(true)?))
            }
            Some(Token::Delete) => {
                self.advance();
                Ok(Clause::Delete {
                    detach: false,
                    exprs: self.parse_expr_list()?,
                })
            }
            Some(Token::Detach) => {
                self.advance();
                self.expect(Token::Delete, "DELETE after DETACH")?;
                Ok(Clause::Delete {
                    detach: true,
                    exprs: self.parse_expr_list()?,
                })
            }
            _ => self.fail("expected a clause"),
        }
    }

    fn parse_match_clause(&mut self, optional: bool) -> Result<MatchClause> {
        let patterns = self.parse_patterns()?;
        let where_clause = if self.eat(Token::Where) {
            Some(self.parse_expr()?)
        } else {
            None
        };
        Ok(MatchClause {
            optional,
            patterns,
            where_clause,
        })
    }

    fn parse_return_clause(&mut self) -> Result<Clause> {
        let distinct = self.eat(Token::Distinct);
        let items = self.parse_return_items()?;
        let (order, skip, limit) = self.parse_result_tail()?;
        Ok(Clause::Return {
            distinct,
            items,
            order,
            skip,
            limit,
        })
    }

    fn parse_with_clause(&mut self) -> Result<Clause> {
        let distinct = self.eat(Token::Distinct);
        let items = self.parse_return_items()?;
        let (order, skip, limit) = self.parse_result_tail()?;
        let where_clause = if self.eat(Token::Where) {
            Some(self.parse_expr()?)
        } else {
            None
        };
        Ok(Clause::With {
            distinct,
            items,
            order,
            skip,
            limit,
            where_clause,
        })
    }

    fn parse_result_tail(&mut self) -> Result<(Vec<SortItem>, Option<Expr>, Option<Expr>)> {
        let mut order = Vec::new();
        if self.eat(Token::Order) {
            self.expect(Token::By, "BY after ORDER")?;
            loop {
                let expr = self.parse_expr()?;
                let ascending = if self.eat(Token::Desc) || self.eat(Token::Descending) {
                    false
                } else {
                    self.eat(Token::Asc);
                    self.eat(Token::Ascending);
                    true
                };
                order.push(SortItem { expr, ascending });
                if !self.eat(Token::Comma) {
                    break;
                }
            }
        }
        let skip = if self.eat(Token::Skip) {
            Some(self.parse_expr()?)
        } else {
            None
        };
        let limit = if self.eat(Token::Limit) {
            Some(self.parse_expr()?)
        } else {
            None
        };
        Ok((order, skip, limit))
    }

    fn parse_return_items(&mut self) -> Result<Vec<ReturnItem>> {
        let mut items = Vec::new();
        loop {
            if self.peek() == Some(Token::Star) {
                self.advance();
                items.push(ReturnItem {
                    star: true,
                    expr: Expr::Literal(Value::Null),
                    alias: None,
                });
            } else {
                let expr = self.parse_expr()?;
                let alias = if self.eat(Token::As) {
                    Some(self.parse_name()?)
                } else {
                    None
                };
                items.push(ReturnItem {
                    star: false,
                    expr,
                    alias,
                });
            }
            if !self.eat(Token::Comma) {
                break;
            }
        }
        Ok(items)
    }

    fn parse_expr_list(&mut self) -> Result<Vec<Expr>> {
        let mut exprs = vec![self.parse_expr()?];
        while self.eat(Token::Comma) {
            exprs.push(self.parse_expr()?);
        }
        Ok(exprs)
    }

    fn parse_merge_clause(&mut self) -> Result<Clause> {
        let pattern = self.parse_path()?;
        let mut on_create = Vec::new();
        let mut on_match = Vec::new();
        while self.eat(Token::On) {
            if self.eat(Token::Create) {
                self.expect(Token::Set, "SET after ON CREATE")?;
                on_create.extend(self.parse_set_items(false)?);
            } else if self.eat(Token::Match) {
                self.expect(Token::Set, "SET after ON MATCH")?;
                on_match.extend(self.parse_set_items(false)?);
            } else {
                return self.fail("expected CREATE or MATCH after ON");
            }
        }
        Ok(Clause::Merge {
            pattern,
            on_create,
            on_match,
        })
    }

    /// Parse `SET` (`remove = false`) or `REMOVE` (`remove = true`) items.
    /// `REMOVE` takes no `=` for property and label items.
    fn parse_set_items(&mut self, remove: bool) -> Result<Vec<SetItem>> {
        let mut items = Vec::new();
        loop {
            let variable = self.parse_name()?;
            if self.eat(Token::Dot) {
                let key = self.parse_name()?;
                let value = if remove {
                    Expr::Literal(Value::Null)
                } else {
                    self.expect(Token::Equals, "= in SET item")?;
                    self.parse_expr()?
                };
                items.push(SetItem::Property {
                    variable,
                    key,
                    value,
                });
            } else if self.eat(Token::Colon) {
                let mut labels = vec![self.parse_name()?];
                while self.eat(Token::Colon) {
                    labels.push(self.parse_name()?);
                }
                items.push(SetItem::Label { variable, labels });
            } else if !remove && self.eat(Token::PlusEquals) {
                let value = self.parse_expr()?;
                items.push(SetItem::Append { variable, value });
            } else if !remove && self.eat(Token::Equals) {
                let value = self.parse_expr()?;
                items.push(SetItem::Replace { variable, value });
            } else {
                return self.fail(if remove {
                    "expected '.' or ':' in REMOVE item"
                } else {
                    "expected '.', ':', '+=' or '=' in SET item"
                });
            }
            if !self.eat(Token::Comma) {
                break;
            }
        }
        Ok(items)
    }

    fn parse_patterns(&mut self) -> Result<Vec<PatternPath>> {
        let mut patterns = vec![self.parse_path()?];
        while self.eat(Token::Comma) {
            patterns.push(self.parse_path()?);
        }
        Ok(patterns)
    }

    fn parse_path(&mut self) -> Result<PatternPath> {
        let start = self.parse_node_pattern()?;
        let mut segments = Vec::new();
        while matches!(self.peek(), Some(Token::Minus) | Some(Token::ArrowLeft)) {
            let relationship = self.parse_relationship()?;
            let node = self.parse_node_pattern()?;
            segments.push(PathSegment { relationship, node });
        }
        Ok(PatternPath { start, segments })
    }

    fn parse_node_pattern(&mut self) -> Result<NodePattern> {
        self.expect(Token::LParen, "'(' to start a node pattern")?;
        let mut pattern = NodePattern::default();
        if matches!(self.peek(), Some(Token::Ident) | Some(Token::EscapedIdent)) {
            pattern.variable = Some(self.parse_name()?);
        }
        while self.eat(Token::Colon) {
            pattern.labels.push(self.parse_name()?);
        }
        if self.peek() == Some(Token::LBrace) {
            pattern.properties = Some(self.parse_map_literal()?);
        }
        self.expect(Token::RParen, "')' to close a node pattern")?;
        Ok(pattern)
    }

    fn parse_relationship(&mut self) -> Result<RelationshipPattern> {
        let start = match self.advance() {
            Some(Token::Minus) => Token::Minus,
            Some(Token::ArrowLeft) => Token::ArrowLeft,
            _ => return self.fail("expected '-' or '<-' in a relationship pattern"),
        };
        let mut pattern = RelationshipPattern {
            variable: None,
            types: Vec::new(),
            direction: RelDirection::Undirected,
            var_length: None,
            properties: None,
        };
        if self.eat(Token::LBracket) {
            if matches!(self.peek(), Some(Token::Ident) | Some(Token::EscapedIdent)) {
                pattern.variable = Some(self.parse_name()?);
            }
            if self.eat(Token::Colon) {
                pattern.types.push(self.parse_name()?);
                while self.eat(Token::Pipe) {
                    pattern.types.push(self.parse_name()?);
                }
            }
            if self.eat(Token::Star) {
                pattern.var_length = Some(self.parse_var_length()?);
            }
            if self.peek() == Some(Token::LBrace) {
                pattern.properties = Some(self.parse_map_literal()?);
            }
            self.expect(Token::RBracket, "']' to close a relationship pattern")?;
        }
        let tail = match self.advance() {
            Some(Token::Minus) => Token::Minus,
            Some(Token::ArrowRight) => Token::ArrowRight,
            _ => return self.fail("expected '-' or '->' to close a relationship pattern"),
        };
        pattern.direction = match (start, tail) {
            (Token::Minus, Token::Minus) => RelDirection::Undirected,
            (Token::Minus, Token::ArrowRight) => RelDirection::Outgoing,
            (Token::ArrowLeft, Token::Minus) => RelDirection::Incoming,
            _ => return self.fail("invalid relationship direction"),
        };
        Ok(pattern)
    }

    fn parse_var_length(&mut self) -> Result<VarLength> {
        let mut min = 1u32;
        let mut max = None;
        if self.peek() == Some(Token::Integer) {
            min = self.parse_u32("relationship length")?;
        }
        if self.eat(Token::DotDot) {
            if self.peek() == Some(Token::Integer) {
                max = Some(self.parse_u32("relationship length")?);
            }
        } else {
            max = Some(min);
        }
        Ok(VarLength { min, max })
    }

    fn parse_u32(&mut self, what: &str) -> Result<u32> {
        let spanned = self
            .tokens
            .get(self.pos)
            .copied()
            .ok_or_else(|| Error::syntax("unexpected end of input"))?;
        self.advance();
        self.source[spanned.start..spanned.end]
            .parse::<u32>()
            .map_err(|_| Error::syntax(format!("invalid {what}")))
    }

    /// Map keys: identifiers, escaped identifiers, quoted strings and
    /// reserved words (which are plain strings in this position, case kept).
    fn parse_map_key(&mut self) -> Result<String> {
        match self.peek() {
            Some(Token::StringSingle) | Some(Token::StringDouble) => {
                let spanned = self.tokens[self.pos];
                self.advance();
                unescape_string(&self.source[spanned.start + 1..spanned.end - 1])
            }
            Some(Token::Ident) | Some(Token::EscapedIdent) => self.parse_name(),
            Some(token) if token.is_keyword() => {
                let spanned = self.tokens[self.pos];
                self.advance();
                Ok(self.source[spanned.start..spanned.end].to_owned())
            }
            _ => self.fail("expected a map key"),
        }
    }

    fn parse_name(&mut self) -> Result<String> {
        match self.advance() {
            Some(Token::Ident) => {
                let spanned = self.tokens[self.pos - 1];
                Ok(self.source[spanned.start..spanned.end].to_owned())
            }
            Some(Token::EscapedIdent) => {
                let spanned = self.tokens[self.pos - 1];
                let raw = &self.source[spanned.start + 1..spanned.end - 1];
                Ok(raw.replace("``", "`"))
            }
            _ => {
                self.pos -= 1;
                self.fail("expected an identifier")
            }
        }
    }

    // ---- expressions ----

    fn parse_expr(&mut self) -> Result<Expr> {
        self.parse_or()
    }

    fn parse_or(&mut self) -> Result<Expr> {
        let mut left = self.parse_xor()?;
        while self.eat(Token::Or) {
            let right = self.parse_xor()?;
            left = binary(BinaryOp::Or, left, right);
        }
        Ok(left)
    }

    fn parse_xor(&mut self) -> Result<Expr> {
        let mut left = self.parse_and()?;
        while self.eat(Token::Xor) {
            let right = self.parse_and()?;
            left = binary(BinaryOp::Xor, left, right);
        }
        Ok(left)
    }

    fn parse_and(&mut self) -> Result<Expr> {
        let mut left = self.parse_not()?;
        while self.eat(Token::And) {
            let right = self.parse_not()?;
            left = binary(BinaryOp::And, left, right);
        }
        Ok(left)
    }

    fn parse_not(&mut self) -> Result<Expr> {
        if self.eat(Token::Not) {
            let expr = self.parse_not()?;
            return Ok(Expr::Unary {
                op: UnaryOp::Not,
                expr: Box::new(expr),
            });
        }
        self.parse_comparison()
    }

    fn parse_comparison(&mut self) -> Result<Expr> {
        let mut left = self.parse_additive()?;
        loop {
            let op = match self.peek() {
                Some(Token::Equals) => Some(BinaryOp::Eq),
                Some(Token::NotEquals) => Some(BinaryOp::Ne),
                Some(Token::Less) => Some(BinaryOp::Lt),
                Some(Token::LessEquals) => Some(BinaryOp::Le),
                Some(Token::Greater) => Some(BinaryOp::Gt),
                Some(Token::GreaterEquals) => Some(BinaryOp::Ge),
                Some(Token::RegexMatch) => Some(BinaryOp::Regex),
                Some(Token::In) => Some(BinaryOp::In),
                Some(Token::Starts) => Some(BinaryOp::StartsWith),
                Some(Token::Ends) => Some(BinaryOp::EndsWith),
                Some(Token::Contains) => Some(BinaryOp::Contains),
                _ => None,
            };
            if let Some(op) = op {
                self.advance();
                if op == BinaryOp::StartsWith {
                    self.expect(Token::With, "WITH after STARTS")?;
                } else if op == BinaryOp::EndsWith {
                    self.expect(Token::With, "WITH after ENDS")?;
                }
                let right = self.parse_additive()?;
                left = binary(op, left, right);
                continue;
            }
            if self.eat(Token::Is) {
                let negated = self.eat(Token::Not);
                self.expect(Token::Null, "NULL after IS")?;
                left = Expr::IsNull {
                    expr: Box::new(left),
                    negated,
                };
                continue;
            }
            break;
        }
        Ok(left)
    }

    fn parse_additive(&mut self) -> Result<Expr> {
        let mut left = self.parse_multiplicative()?;
        loop {
            let op = match self.peek() {
                Some(Token::Plus) => BinaryOp::Add,
                Some(Token::Minus) => BinaryOp::Sub,
                _ => break,
            };
            self.advance();
            let right = self.parse_multiplicative()?;
            left = binary(op, left, right);
        }
        Ok(left)
    }

    fn parse_multiplicative(&mut self) -> Result<Expr> {
        let mut left = self.parse_power()?;
        loop {
            let op = match self.peek() {
                Some(Token::Star) => BinaryOp::Mul,
                Some(Token::Slash) => BinaryOp::Div,
                Some(Token::Percent) => BinaryOp::Mod,
                _ => break,
            };
            self.advance();
            let right = self.parse_power()?;
            left = binary(op, left, right);
        }
        Ok(left)
    }

    fn parse_power(&mut self) -> Result<Expr> {
        let left = self.parse_unary()?;
        if self.eat(Token::Caret) {
            // Right-associative.
            let right = self.parse_power()?;
            return Ok(binary(BinaryOp::Pow, left, right));
        }
        Ok(left)
    }

    fn parse_unary(&mut self) -> Result<Expr> {
        if self.eat(Token::Minus) {
            let expr = self.parse_unary()?;
            return Ok(Expr::Unary {
                op: UnaryOp::Neg,
                expr: Box::new(expr),
            });
        }
        if self.eat(Token::Plus) {
            return self.parse_unary();
        }
        self.parse_postfix()
    }

    fn parse_postfix(&mut self) -> Result<Expr> {
        let mut expr = self.parse_primary()?;
        loop {
            match self.peek() {
                Some(Token::Dot) => {
                    self.advance();
                    let key = self.parse_name()?;
                    expr = Expr::Property {
                        base: Box::new(expr),
                        key,
                    };
                }
                Some(Token::LBracket) => {
                    self.advance();
                    let lower = if self.peek() == Some(Token::DotDot) {
                        None
                    } else {
                        Some(Box::new(self.parse_expr()?))
                    };
                    if self.eat(Token::DotDot) {
                        let upper = if self.peek() == Some(Token::RBracket) {
                            None
                        } else {
                            Some(Box::new(self.parse_expr()?))
                        };
                        self.expect(Token::RBracket, "']' after slice")?;
                        expr = Expr::Slice {
                            base: Box::new(expr),
                            lower,
                            upper,
                        };
                    } else {
                        let Some(index) = lower else {
                            return self.fail("expected an index or a slice");
                        };
                        self.expect(Token::RBracket, "']' after subscript")?;
                        expr = Expr::Subscript {
                            base: Box::new(expr),
                            index,
                        };
                    }
                }
                Some(Token::Colon) => {
                    self.advance();
                    let mut labels = vec![self.parse_name()?];
                    while self.eat(Token::Colon) {
                        labels.push(self.parse_name()?);
                    }
                    expr = Expr::LabelCheck {
                        expr: Box::new(expr),
                        labels,
                    };
                }
                _ => break,
            }
        }
        Ok(expr)
    }

    fn parse_primary(&mut self) -> Result<Expr> {
        let spanned = match self.tokens.get(self.pos).copied() {
            Some(spanned) => spanned,
            None => return self.fail("expected an expression"),
        };
        let text = || &self.source[spanned.start..spanned.end];
        match spanned.token {
            Token::Integer => {
                self.advance();
                let value = text().parse::<i64>().map_err(|_| {
                    Error::syntax(format!("integer literal {} out of range", text()))
                })?;
                Ok(Expr::Literal(Value::Int(value)))
            }
            Token::Float => {
                self.advance();
                let value = text()
                    .parse::<f64>()
                    .map_err(|_| Error::syntax(format!("invalid float literal {}", text())))?;
                Ok(Expr::Literal(Value::Float(value)))
            }
            Token::StringSingle | Token::StringDouble => {
                self.advance();
                let value = unescape_string(&text()[1..text().len() - 1])?;
                Ok(Expr::Literal(Value::Text(Arc::from(value.as_str()))))
            }
            Token::True => {
                self.advance();
                Ok(Expr::Literal(Value::Bool(true)))
            }
            Token::False => {
                self.advance();
                Ok(Expr::Literal(Value::Bool(false)))
            }
            Token::Null => {
                self.advance();
                Ok(Expr::Literal(Value::Null))
            }
            Token::Param => {
                self.advance();
                Ok(Expr::Parameter(text()[1..].to_owned()))
            }
            Token::Ident | Token::EscapedIdent => {
                let name = self.parse_name()?;
                if self.peek() == Some(Token::LParen) {
                    let lower = name.to_ascii_lowercase();
                    let quantifier = match lower.as_str() {
                        "all" => Some(QuantifierKind::All),
                        "any" => Some(QuantifierKind::Any),
                        "none" => Some(QuantifierKind::None),
                        "single" => Some(QuantifierKind::Single),
                        _ => None,
                    };
                    let followed_by_var_in = |parser: &Self, offset: usize| {
                        matches!(
                            parser
                                .tokens
                                .get(parser.pos + offset)
                                .map(|entry| entry.token),
                            Some(Token::Ident) | Some(Token::EscapedIdent)
                        ) && parser
                            .tokens
                            .get(parser.pos + offset + 1)
                            .map(|entry| entry.token)
                            == Some(Token::In)
                    };
                    if let Some(kind) = quantifier
                        && followed_by_var_in(self, 1)
                    {
                        return self.parse_quantifier(kind);
                    }
                    if lower == "reduce"
                        && matches!(
                            self.tokens.get(self.pos + 1).map(|entry| entry.token),
                            Some(Token::Ident) | Some(Token::EscapedIdent)
                        )
                        && self.tokens.get(self.pos + 2).map(|entry| entry.token)
                            == Some(Token::Equals)
                    {
                        self.advance();
                        let accumulator = self.parse_name()?;
                        self.expect(Token::Equals, "= after the reduce accumulator")?;
                        let init = self.parse_expr()?;
                        self.expect(Token::Comma, "',' after the reduce initial value")?;
                        let var = self.parse_name()?;
                        self.expect(Token::In, "IN in reduce")?;
                        let list = self.parse_expr()?;
                        self.expect(Token::Pipe, "'|' in reduce")?;
                        let expression = self.parse_expr()?;
                        self.expect(Token::RParen, "')' to close reduce")?;
                        return Ok(Expr::Reduce {
                            accumulator,
                            init: Box::new(init),
                            var,
                            list: Box::new(list),
                            expression: Box::new(expression),
                        });
                    }
                    self.advance();
                    let distinct = self.eat(Token::Distinct);
                    let mut star = false;
                    let mut args = Vec::new();
                    if self.peek() == Some(Token::Star) {
                        self.advance();
                        star = true;
                    } else if self.peek() != Some(Token::RParen) {
                        args.push(self.parse_expr()?);
                        while self.eat(Token::Comma) {
                            args.push(self.parse_expr()?);
                        }
                    }
                    self.expect(Token::RParen, "')' to close a function call")?;
                    Ok(Expr::Function {
                        name,
                        distinct,
                        star,
                        args,
                    })
                } else {
                    Ok(Expr::Variable(name))
                }
            }
            Token::LParen => {
                self.advance();
                let expr = self.parse_expr()?;
                self.expect(Token::RParen, "')' to close a parenthesized expression")?;
                Ok(expr)
            }
            Token::All => {
                // `all(x IN list WHERE predicate)`; `ALL` is otherwise only
                // valid in `UNION ALL`.
                self.advance();
                if self.peek() == Some(Token::LParen)
                    && matches!(
                        self.tokens.get(self.pos + 1).map(|entry| entry.token),
                        Some(Token::Ident) | Some(Token::EscapedIdent)
                    )
                    && self.tokens.get(self.pos + 2).map(|entry| entry.token) == Some(Token::In)
                {
                    return self.parse_quantifier(QuantifierKind::All);
                }
                self.fail("unexpected ALL")
            }
            Token::LBracket => {
                self.advance();
                // `[x IN list WHERE predicate | projection]`.
                if matches!(self.peek(), Some(Token::Ident) | Some(Token::EscapedIdent))
                    && self.tokens.get(self.pos + 1).map(|entry| entry.token) == Some(Token::In)
                {
                    let var = self.parse_name()?;
                    self.expect(Token::In, "IN after the comprehension variable")?;
                    let list = self.parse_expr()?;
                    let predicate = if self.eat(Token::Where) {
                        Some(Box::new(self.parse_expr()?))
                    } else {
                        None
                    };
                    let projection = if self.eat(Token::Pipe) {
                        Some(Box::new(self.parse_expr()?))
                    } else {
                        None
                    };
                    self.expect(Token::RBracket, "']' to close a list comprehension")?;
                    return Ok(Expr::ListComprehension {
                        var,
                        list: Box::new(list),
                        predicate,
                        projection,
                    });
                }
                let mut items = Vec::new();
                if self.peek() != Some(Token::RBracket) {
                    items.push(self.parse_expr()?);
                    while self.eat(Token::Comma) {
                        items.push(self.parse_expr()?);
                    }
                }
                self.expect(Token::RBracket, "']' to close a list literal")?;
                Ok(Expr::List(items))
            }
            Token::LBrace => self.parse_map_literal(),
            Token::Case => self.parse_case(),
            _ => self.fail("expected an expression"),
        }
    }

    /// Parse the body of a quantifier; the current token is `(`.
    fn parse_quantifier(&mut self, kind: QuantifierKind) -> Result<Expr> {
        self.expect(Token::LParen, "'(' to open the quantifier")?;
        let var = self.parse_name()?;
        self.expect(Token::In, "IN after the quantifier variable")?;
        let list = self.parse_expr()?;
        self.expect(Token::Where, "WHERE in a quantifier")?;
        let predicate = self.parse_expr()?;
        self.expect(Token::RParen, "')' to close the quantifier")?;
        Ok(Expr::Quantifier {
            kind,
            var,
            list: Box::new(list),
            predicate: Box::new(predicate),
        })
    }

    fn parse_map_literal(&mut self) -> Result<Expr> {
        self.expect(Token::LBrace, "'{' to start a map")?;
        let mut entries = Vec::new();
        if self.peek() != Some(Token::RBrace) {
            loop {
                let key = self.parse_map_key()?;
                self.expect(Token::Colon, "':' in map entry")?;
                let value = self.parse_expr()?;
                entries.push((key, value));
                if !self.eat(Token::Comma) {
                    break;
                }
            }
        }
        self.expect(Token::RBrace, "'}' to close a map")?;
        Ok(Expr::Map(entries))
    }

    fn parse_case(&mut self) -> Result<Expr> {
        self.expect(Token::Case, "CASE")?;
        let subject = if self.peek() == Some(Token::When) {
            None
        } else {
            Some(Box::new(self.parse_expr()?))
        };
        let mut alternatives = Vec::new();
        while self.eat(Token::When) {
            let condition = self.parse_expr()?;
            self.expect(Token::Then, "THEN after WHEN")?;
            let value = self.parse_expr()?;
            alternatives.push((condition, value));
        }
        if alternatives.is_empty() {
            return self.fail("CASE requires at least one WHEN");
        }
        let default = if self.eat(Token::Else) {
            Some(Box::new(self.parse_expr()?))
        } else {
            None
        };
        self.expect(Token::End, "END to close CASE")?;
        Ok(Expr::Case {
            subject,
            alternatives,
            default,
        })
    }
}

fn binary(op: BinaryOp, left: Expr, right: Expr) -> Expr {
    Expr::Binary {
        op,
        left: Box::new(left),
        right: Box::new(right),
    }
}

fn unescape_string(raw: &str) -> Result<String> {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        let Some(escaped) = chars.next() else {
            return Err(Error::syntax("string ends with a backslash"));
        };
        match escaped {
            'n' => out.push('\n'),
            't' => out.push('\t'),
            'r' => out.push('\r'),
            'b' => out.push('\u{0008}'),
            'f' => out.push('\u{000C}'),
            '\\' => out.push('\\'),
            '\'' => out.push('\''),
            '"' => out.push('"'),
            '0' => out.push('\0'),
            'u' => {
                let mut code = String::with_capacity(4);
                for _ in 0..4 {
                    let Some(hex) = chars.next() else {
                        return Err(Error::syntax("incomplete \\u escape"));
                    };
                    code.push(hex);
                }
                let value = u32::from_str_radix(&code, 16)
                    .map_err(|_| Error::syntax(format!("invalid \\u escape \\u{code}")))?;
                let ch = char::from_u32(value)
                    .ok_or_else(|| Error::syntax(format!("invalid \\u escape \\u{code}")))?;
                out.push(ch);
            }
            other => {
                return Err(Error::syntax(format!("unknown string escape \\{other}")));
            }
        }
    }
    Ok(out)
}

fn describe(token: Token) -> String {
    format!("{token:?}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expr_of(query: &str) -> Expr {
        let parsed = parse(query).expect("parse");
        match &parsed.parts[0].clauses[0] {
            Clause::Return { items, .. } => items[0].expr.clone(),
            other => panic!("unexpected clause {other:?}"),
        }
    }

    #[test]
    fn parses_set_append() {
        let query = parse("MATCH (n) SET n += {num: 42} RETURN n").expect("parse");
        let Clause::Set(items) = &query.parts[0].clauses[1] else {
            panic!("expected SET");
        };
        assert!(matches!(&items[0], SetItem::Append { variable, .. } if variable == "n"));
    }

    #[test]
    fn parses_union_parts() {
        let query =
            parse("RETURN 1 AS x UNION ALL RETURN 2 AS x UNION ALL RETURN 3 AS x").expect("parse");
        assert_eq!(query.parts.len(), 3);
        assert!(!query.parts[0].union_all);
        assert!(query.parts[1].union_all);
        assert!(query.parts[2].union_all);
        assert_eq!(query.parts[0].clauses.len(), 1);

        let distinct = parse("RETURN 1 AS x UNION RETURN 2 AS x").expect("parse");
        assert_eq!(distinct.parts.len(), 2);
        assert!(!distinct.parts[1].union_all);
    }

    #[test]
    fn rejects_mixed_union_kinds() {
        assert!(parse("RETURN 1 AS x UNION RETURN 2 AS x UNION RETURN 3 AS x").is_ok());
        assert!(parse("RETURN 1 AS x UNION RETURN 2 AS x UNION ALL RETURN 3 AS x").is_err());
        assert!(parse("RETURN 1 AS x UNION ALL RETURN 2 AS x UNION RETURN 3 AS x").is_err());
    }

    #[test]
    fn rejects_empty_union_part() {
        assert!(parse("RETURN 1 AS x UNION").is_err());
        assert!(parse("UNION RETURN 1 AS x").is_err());
    }

    #[test]
    fn parses_optional_match() {
        let query = parse("OPTIONAL MATCH (n)-[:T]->(m) WHERE m.x = 1 RETURN n").expect("parse");
        let Clause::Match(matched) = &query.parts[0].clauses[0] else {
            panic!("expected MATCH");
        };
        assert!(matched.optional);
        assert!(matched.where_clause.is_some());
    }

    #[test]
    fn parses_remove_without_equals() {
        let query = parse("MATCH (n) REMOVE n.name, n:Label RETURN n").expect("parse");
        let Clause::Remove(items) = &query.parts[0].clauses[1] else {
            panic!("expected REMOVE");
        };
        assert_eq!(items.len(), 2);
        let SetItem::Property { key, value, .. } = &items[0] else {
            panic!("expected property removal");
        };
        assert_eq!(key, "name");
        assert!(matches!(value, Expr::Literal(Value::Null)));
        assert!(matches!(&items[1], SetItem::Label { .. }));
    }

    #[test]
    fn parses_simple_match_return() {
        let query = parse("MATCH (n:Person {name: 'alice'})-[r:KNOWS]->(m) WHERE n.age > 30 RETURN n, m.name AS friend LIMIT 10")
            .expect("parse");
        assert_eq!(query.parts[0].clauses.len(), 2);
        let Clause::Match(matched) = &query.parts[0].clauses[0] else {
            panic!("expected MATCH");
        };
        assert_eq!(matched.patterns.len(), 1);
        let path = &matched.patterns[0];
        assert_eq!(path.start.variable.as_deref(), Some("n"));
        assert_eq!(path.start.labels, vec!["Person"]);
        assert!(path.start.properties.is_some());
        assert_eq!(path.segments.len(), 1);
        assert_eq!(
            path.segments[0].relationship.direction,
            RelDirection::Outgoing
        );
        assert!(matched.where_clause.is_some());
    }

    #[test]
    fn operator_precedence() {
        let expr = expr_of("RETURN 1 + 2 * 3");
        let Expr::Binary { op, right, .. } = expr else {
            panic!("expected binary");
        };
        assert_eq!(op, BinaryOp::Add);
        let Expr::Binary { op, .. } = *right else {
            panic!("expected binary");
        };
        assert_eq!(op, BinaryOp::Mul);
    }

    #[test]
    fn not_binds_looser_than_comparison() {
        let expr = expr_of("RETURN NOT a = b");
        let Expr::Unary { op, expr } = expr else {
            panic!("expected unary");
        };
        assert_eq!(op, UnaryOp::Not);
        let Expr::Binary { op, .. } = *expr else {
            panic!("expected comparison inside NOT");
        };
        assert_eq!(op, BinaryOp::Eq);
    }

    #[test]
    fn power_is_right_associative() {
        let expr = expr_of("RETURN 2 ^ 3 ^ 2");
        let Expr::Binary { op, right, .. } = expr else {
            panic!("expected binary");
        };
        assert_eq!(op, BinaryOp::Pow);
        let Expr::Binary { op, .. } = *right else {
            panic!("expected nested power");
        };
        assert_eq!(op, BinaryOp::Pow);
    }

    #[test]
    fn string_predicates() {
        let expr = expr_of("RETURN n.name STARTS WITH 'a'");
        let Expr::Binary { op, .. } = expr else {
            panic!("expected binary");
        };
        assert_eq!(op, BinaryOp::StartsWith);
    }

    #[test]
    fn list_and_map_literals() {
        let expr = expr_of("RETURN [1, 2, 3]");
        assert!(matches!(expr, Expr::List(items) if items.len() == 3));
        let expr = expr_of("RETURN {name: 'x', age: 1}");
        assert!(matches!(expr, Expr::Map(entries) if entries.len() == 2));
    }

    #[test]
    fn case_expression() {
        let expr = expr_of("RETURN CASE WHEN n.age > 30 THEN 'old' ELSE 'young' END");
        assert!(matches!(expr, Expr::Case { .. }));
    }

    #[test]
    fn unwind_with_parameters() {
        let query = parse("UNWIND $names AS name RETURN name").expect("parse");
        let Clause::Unwind { expr, alias } = &query.parts[0].clauses[0] else {
            panic!("expected UNWIND");
        };
        assert_eq!(expr, &Expr::Parameter("names".to_owned()));
        assert_eq!(alias, "name");
    }

    #[test]
    fn create_and_merge_and_set() {
        let query =
            parse("CREATE (a:Person {name: 'a'})-[:KNOWS]->(b:Person {name: 'b'}) RETURN a")
                .expect("parse");
        assert!(matches!(query.parts[0].clauses[0], Clause::Create(_)));

        let query =
            parse("MERGE (n:Person {name: 'a'}) ON CREATE SET n.age = 1 ON MATCH SET n.age = 2")
                .expect("parse");
        let Clause::Merge {
            on_create,
            on_match,
            ..
        } = &query.parts[0].clauses[0]
        else {
            panic!("expected MERGE");
        };
        assert_eq!(on_create.len(), 1);
        assert_eq!(on_match.len(), 1);

        let query = parse("MATCH (n) SET n.age = 1, n:Label RETURN n").expect("parse");
        let Clause::Set(items) = &query.parts[0].clauses[1] else {
            panic!("expected SET");
        };
        assert_eq!(items.len(), 2);
    }

    #[test]
    fn detach_delete_and_with_where() {
        let query = parse("MATCH (n) WITH n WHERE n.age > 1 DETACH DELETE n").expect("parse");
        let Clause::With { where_clause, .. } = &query.parts[0].clauses[1] else {
            panic!("expected WITH");
        };
        assert!(where_clause.is_some());
        let Clause::Delete { detach, .. } = &query.parts[0].clauses[2] else {
            panic!("expected DELETE");
        };
        assert!(detach);
    }

    #[test]
    fn relationship_directions_and_var_length() {
        let query = parse("MATCH (a)<-[r:KNOWS*1..3]-(b) RETURN r").expect("parse");
        let Clause::Match(matched) = &query.parts[0].clauses[0] else {
            panic!("expected MATCH");
        };
        let rel = &matched.patterns[0].segments[0].relationship;
        assert_eq!(rel.direction, RelDirection::Incoming);
        assert_eq!(
            rel.var_length,
            Some(VarLength {
                min: 1,
                max: Some(3)
            })
        );
    }

    #[test]
    fn errors_report_positions() {
        let err = parse("MATCH (n RETURN n").expect_err("missing paren");
        let message = err.message();
        assert!(message.contains("line 1"), "{message}");
        let err = parse("MATCH (n) RETURN").expect_err("missing expression");
        assert!(err.message().contains("line 1"), "{}", err.message());
    }

    #[test]
    fn escaped_identifiers_and_strings() {
        let expr = expr_of("RETURN `weird name`.prop");
        let Expr::Property { base, key } = expr else {
            panic!("expected property");
        };
        assert_eq!(*base, Expr::Variable("weird name".to_owned()));
        assert_eq!(key, "prop");
        let expr = expr_of("RETURN 'a\\n\\u0041'");
        let Expr::Literal(Value::Text(text)) = expr else {
            panic!("expected text");
        };
        assert_eq!(text.as_ref(), "a\nA");
    }
}
