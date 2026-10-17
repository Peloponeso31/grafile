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
    let mut parser = parser_for(source)?;
    let query = parser.parse_query()?;
    Ok(query)
}

/// Parse a statement: a DDL command or a query.
pub fn parse_statement(source: &str) -> Result<Statement> {
    let mut parser = parser_for(source)?;
    parser.parse_statement()
}

fn parser_for(source: &str) -> Result<Parser<'_>> {
    let tokens = lex(source).map_err(|(start, _)| {
        let (line, column) = line_column(source, start);
        Error::syntax(format!("invalid character at line {line} column {column}"))
    })?;
    Ok(Parser {
        source,
        tokens,
        pos: 0,
    })
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

    /// Source text of the next token when it is a plain identifier.
    fn peek_ident_text(&self) -> Option<&'a str> {
        let spanned = self.tokens.get(self.pos)?;
        (spanned.token == Token::Ident).then(|| {
            self.source
                .get(spanned.start..spanned.end)
                .expect("token span is valid")
        })
    }

    fn eat_ident_text(&mut self, text: &str) -> bool {
        if self
            .peek_ident_text()
            .is_some_and(|token| token.eq_ignore_ascii_case(text))
        {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expect_ident_text(&mut self, text: &str) -> Result<()> {
        if self.eat_ident_text(text) {
            Ok(())
        } else {
            self.fail(format!("expected {text}"))
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

    /// Parse a statement: a DDL command or a query.
    fn parse_statement(&mut self) -> Result<Statement> {
        if let Some(text) = self.peek_ident_text() {
            if text.eq_ignore_ascii_case("show") {
                return Err(Error::unsupported(
                    "SHOW statements are not supported (only CREATE/DROP INDEX and CONSTRAINT)",
                ));
            }
            if text.eq_ignore_ascii_case("drop") {
                return self.parse_drop();
            }
        }
        if self.peek() == Some(Token::Create)
            && self
                .tokens
                .get(self.pos + 1)
                .and_then(|spanned| self.source.get(spanned.start..spanned.end))
                .is_some_and(|text| {
                    text.eq_ignore_ascii_case("index") || text.eq_ignore_ascii_case("constraint")
                })
        {
            let ddl = self.parse_create_ddl()?;
            self.eat(Token::Semicolon);
            if self.peek().is_some() {
                return self.fail("unexpected input after the DDL statement");
            }
            return Ok(ddl);
        }
        let query = self.parse_query()?;
        Ok(Statement::Query(query))
    }

    fn parse_create_ddl(&mut self) -> Result<Statement> {
        self.expect(Token::Create, "CREATE")?;
        if self.eat_ident_text("index") {
            self.parse_create_index()
        } else if self.eat_ident_text("constraint") {
            self.parse_create_constraint()
        } else {
            self.fail("expected INDEX or CONSTRAINT after CREATE")
        }
    }

    fn parse_create_index(&mut self) -> Result<Statement> {
        if self.peek() == Some(Token::On) {
            return Err(Error::syntax(
                "the `CREATE INDEX ON :Label(property)` form was removed in Neo4j 4; \
                 use `CREATE INDEX name FOR (n:Label) ON (n.property)`",
            ));
        }
        self.reject_missing_name("an index")?;
        let name = self.parse_name()?;
        let if_not_exists = self.parse_if_not_exists()?;
        let (entity, var) = self.parse_ddl_entity()?;
        self.expect(Token::On, "ON after the pattern")?;
        let keys = self.parse_ddl_keys(&var)?;
        Ok(Statement::Ddl(DdlStatement::CreateIndex(DdlIndex {
            name,
            if_not_exists,
            entity,
            keys,
        })))
    }

    fn parse_create_constraint(&mut self) -> Result<Statement> {
        if self.peek() == Some(Token::On) {
            return Err(Error::syntax(
                "the `CREATE CONSTRAINT ON ... ASSERT ...` form was removed in Neo4j 4; \
                 use `CREATE CONSTRAINT name FOR (n:Label) REQUIRE n.property IS UNIQUE`",
            ));
        }
        self.reject_missing_name("a constraint")?;
        let name = self.parse_name()?;
        let if_not_exists = self.parse_if_not_exists()?;
        let (entity, var) = self.parse_ddl_entity()?;
        if self.peek_ident_text() == Some("assert") {
            return Err(Error::syntax(
                "ASSERT was replaced by REQUIRE in Neo4j 4; \
                 use `CREATE CONSTRAINT name FOR (n:Label) REQUIRE n.property IS UNIQUE`",
            ));
        }
        self.expect_ident_text("require")?;
        let keys = if self.eat(Token::LParen) {
            let mut keys = Vec::new();
            loop {
                keys.push(self.parse_ddl_property(&var)?);
                if !self.eat(Token::Comma) {
                    break;
                }
            }
            self.expect(Token::RParen, "')' to close the property list")?;
            keys
        } else {
            vec![self.parse_ddl_property(&var)?]
        };
        self.expect(Token::Is, "IS")?;
        let kind = if self.eat_ident_text("unique") {
            DdlConstraintKind::Unique
        } else if self.eat(Token::Not) {
            self.expect(Token::Null, "NULL")?;
            DdlConstraintKind::NotNull
        } else if self.eat_ident_text("node") {
            self.expect_ident_text("key")?;
            DdlConstraintKind::NodeKey
        } else {
            return self.fail("expected UNIQUE, NOT NULL or NODE KEY after IS");
        };
        self.validate_constraint(&kind, &entity, keys.len())?;
        Ok(Statement::Ddl(DdlStatement::CreateConstraint(
            DdlConstraint {
                name,
                if_not_exists,
                entity,
                kind,
                keys,
            },
        )))
    }

    fn validate_constraint(
        &self,
        kind: &DdlConstraintKind,
        entity: &DdlEntity,
        key_count: usize,
    ) -> Result<()> {
        match kind {
            DdlConstraintKind::Unique | DdlConstraintKind::NotNull if key_count != 1 => Err(
                Error::syntax("UNIQUE and NOT NULL constraints cover exactly one property"),
            ),
            DdlConstraintKind::NodeKey if matches!(entity, DdlEntity::Edge { .. }) => Err(
                Error::syntax("NODE KEY constraints apply to nodes, not relationships"),
            ),
            DdlConstraintKind::Unique | DdlConstraintKind::NotNull | DdlConstraintKind::NodeKey => {
                Ok(())
            }
        }
    }

    fn parse_drop(&mut self) -> Result<Statement> {
        self.advance();
        let index = if self.eat_ident_text("index") {
            true
        } else if self.eat_ident_text("constraint") {
            false
        } else {
            return self.fail("expected INDEX or CONSTRAINT after DROP");
        };
        let name = self.parse_name()?;
        let if_exists = if self.eat_ident_text("if") {
            self.expect_ident_text("exists")?;
            true
        } else {
            false
        };
        self.eat(Token::Semicolon);
        if self.peek().is_some() {
            return self.fail("unexpected input after the DDL statement");
        }
        Ok(Statement::Ddl(if index {
            DdlStatement::DropIndex { name, if_exists }
        } else {
            DdlStatement::DropConstraint { name, if_exists }
        }))
    }

    fn parse_if_not_exists(&mut self) -> Result<bool> {
        if !self.eat_ident_text("if") {
            return Ok(false);
        }
        self.expect(Token::Not, "NOT")?;
        self.expect_ident_text("exists")?;
        Ok(true)
    }

    fn reject_missing_name(&self, what: &str) -> Result<()> {
        if self
            .peek_ident_text()
            .is_some_and(|text| text.eq_ignore_ascii_case("for"))
        {
            return Err(Error::syntax(format!("a {what} name is required")));
        }
        Ok(())
    }

    /// Parse `FOR (n:Label)` or `FOR ()-[r:TYPE]-()`; returns the entity and
    /// the variable the property expressions must use.
    fn parse_ddl_entity(&mut self) -> Result<(DdlEntity, String)> {
        self.expect_ident_text("for")?;
        self.expect(Token::LParen, "'(' after FOR")?;
        if self.eat(Token::RParen) {
            self.expect(Token::Minus, "'-' before the relationship pattern")?;
            self.expect(Token::LBracket, "'[' to open the relationship pattern")?;
            let var = self.parse_name()?;
            self.expect(Token::Colon, "':' before the relationship type")?;
            let edge_type = self.parse_symbol_name()?;
            self.expect(Token::RBracket, "']' to close the relationship pattern")?;
            self.expect(Token::Minus, "'-' after the relationship pattern")?;
            self.expect(Token::LParen, "'(' after the relationship pattern")?;
            self.expect(Token::RParen, "')' after the relationship pattern")?;
            return Ok((DdlEntity::Edge { edge_type }, var));
        }
        let var = self.parse_name()?;
        self.expect(Token::Colon, "':' before the label")?;
        let label = self.parse_symbol_name()?;
        self.expect(Token::RParen, "')' after the label")?;
        Ok((DdlEntity::Node { label }, var))
    }

    fn parse_ddl_keys(&mut self, var: &str) -> Result<Vec<String>> {
        self.expect(Token::LParen, "'(' to open the property list")?;
        let mut keys = Vec::new();
        loop {
            keys.push(self.parse_ddl_property(var)?);
            if !self.eat(Token::Comma) {
                break;
            }
        }
        self.expect(Token::RParen, "')' to close the property list")?;
        Ok(keys)
    }

    /// A property expression `var.key`; the variable must be the pattern's.
    fn parse_ddl_property(&mut self, var: &str) -> Result<String> {
        let base = self.parse_name()?;
        if base != var {
            return self.fail(format!("the property expression must use `{var}`"));
        }
        self.expect(Token::Dot, "'.' in the property expression")?;
        self.parse_symbol_name()
    }

    /// Parse the clauses of one query part, up to `UNION` or the end.
    fn parse_clauses(&mut self) -> Result<Vec<Clause>> {
        let mut clauses = Vec::new();
        while self.peek().is_some()
            && self.peek() != Some(Token::Semicolon)
            && self.peek() != Some(Token::Union)
            && self.peek() != Some(Token::RBrace)
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
            Some(Token::Foreach) => self.parse_foreach_clause(),
            Some(Token::Call) => self.parse_call_clause(),
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
                    text: None,
                });
            } else {
                let start = self.tokens.get(self.pos).map(|spanned| spanned.start);
                let expr = self.parse_expr()?;
                let end = self
                    .tokens
                    .get(self.pos.wrapping_sub(1))
                    .map(|spanned| spanned.end);
                let text = match (start, end) {
                    (Some(start), Some(end)) if start <= end => {
                        Some(self.source[start..end].to_owned())
                    }
                    _ => None,
                };
                let alias = if self.eat(Token::As) {
                    Some(self.parse_name()?)
                } else {
                    None
                };
                items.push(ReturnItem {
                    star: false,
                    expr,
                    alias,
                    text,
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
            // The target may be parenthesized: `SET (n).name = 'x'`.
            let variable = if self.eat(Token::LParen) {
                let name = self.parse_name()?;
                self.expect(Token::RParen, "')' after the SET target")?;
                name
            } else {
                self.parse_name()?
            };
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
                let mut labels = vec![self.parse_symbol_name()?];
                while self.eat(Token::Colon) {
                    labels.push(self.parse_symbol_name()?);
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
        // `p = (a)-->(b)` names the path.
        let mut variable = None;
        if matches!(self.peek(), Some(Token::Ident) | Some(Token::EscapedIdent))
            && self.tokens.get(self.pos + 1).map(|entry| entry.token) == Some(Token::Equals)
        {
            variable = Some(self.parse_name()?);
            self.expect(Token::Equals, "'=' after the path variable")?;
        }
        // `shortestPath((a)-[*]->(b))` wraps the pattern.
        let mut shortest = Shortest::Plain;
        if matches!(self.peek(), Some(Token::Ident)) {
            let spanned = self.tokens[self.pos];
            let name = &self.source[spanned.start..spanned.end];
            if (name == "shortestPath" || name == "allShortestPaths")
                && self.tokens.get(self.pos + 1).map(|entry| entry.token) == Some(Token::LParen)
            {
                shortest = if name == "allShortestPaths" {
                    Shortest::AllShortest
                } else {
                    Shortest::Shortest
                };
                self.advance();
                self.advance();
                let inner = self.parse_path()?;
                self.expect(Token::RParen, "')' to close shortestPath")?;
                return Ok(PatternPath {
                    variable,
                    shortest,
                    ..inner
                });
            }
        }
        let start = self.parse_node_pattern()?;
        let mut segments = Vec::new();
        while matches!(self.peek(), Some(Token::Minus) | Some(Token::ArrowLeft)) {
            let relationship = self.parse_relationship()?;
            let node = self.parse_node_pattern()?;
            segments.push(PathSegment { relationship, node });
        }
        Ok(PatternPath {
            variable,
            shortest,
            start,
            segments,
        })
    }

    /// Parse a pattern predicate when the parenthesis starts one; restore
    /// the position and return `None` when it does not.
    fn try_parse_pattern_expr(&mut self) -> Result<Option<Expr>> {
        let saved = self.pos;
        match self.parse_path() {
            Ok(path) if pattern_expression_shape(&path) => Ok(Some(Expr::Pattern(Box::new(path)))),
            _ => {
                self.pos = saved;
                Ok(None)
            }
        }
    }

    /// Parse `[p = (a)-[:T]->(b) WHERE predicate | projection]`; restore the
    /// position and return `None` when the bracket does not start one.
    fn try_parse_pattern_comprehension(&mut self) -> Result<Option<Expr>> {
        let starts_pattern = matches!(self.peek(), Some(Token::LParen))
            || (matches!(self.peek(), Some(Token::Ident) | Some(Token::EscapedIdent))
                && self.tokens.get(self.pos + 1).map(|entry| entry.token) == Some(Token::Equals));
        if !starts_pattern {
            return Ok(None);
        }
        let saved = self.pos;
        let Ok(path) = self.parse_path() else {
            self.pos = saved;
            return Ok(None);
        };
        if path.segments.is_empty() {
            self.pos = saved;
            return Ok(None);
        }
        let predicate = if self.eat(Token::Where) {
            Some(Box::new(self.parse_expr()?))
        } else {
            None
        };
        if !self.eat(Token::Pipe) {
            self.pos = saved;
            return Ok(None);
        }
        let projection = self.parse_expr()?;
        self.expect(Token::RBracket, "']' to close a pattern comprehension")?;
        Ok(Some(Expr::PatternComprehension {
            path: Box::new(path),
            predicate,
            projection: Box::new(projection),
        }))
    }

    fn parse_foreach_clause(&mut self) -> Result<Clause> {
        self.advance();
        self.expect(Token::LParen, "'(' after FOREACH")?;
        let var = self.parse_name()?;
        self.expect(Token::In, "IN after the FOREACH variable")?;
        let list = self.parse_expr()?;
        self.expect(Token::Pipe, "'|' before the FOREACH body")?;
        let mut clauses = Vec::new();
        while self.peek() != Some(Token::RParen) {
            clauses.push(self.parse_clause()?);
        }
        self.expect(Token::RParen, "')' to close FOREACH")?;
        if clauses.is_empty() {
            return self.fail("FOREACH requires at least one updating clause");
        }
        Ok(Clause::Foreach { var, list, clauses })
    }

    fn parse_call_clause(&mut self) -> Result<Clause> {
        self.advance();
        let mut procedure = self.parse_name()?;
        while self.eat(Token::Dot) {
            procedure.push('.');
            procedure.push_str(&self.parse_name()?);
        }
        self.expect(Token::LParen, "'(' after the procedure name")?;
        let mut args = Vec::new();
        if self.peek() != Some(Token::RParen) {
            args = self.parse_expr_list()?;
        }
        self.expect(Token::RParen, "')' to close the procedure call")?;
        let mut yields = Vec::new();
        if self.eat(Token::Yield) {
            loop {
                let column = self.parse_name()?;
                let alias = if self.eat(Token::As) {
                    Some(self.parse_name()?)
                } else {
                    None
                };
                yields.push((column, alias));
                if !self.eat(Token::Comma) {
                    break;
                }
            }
        }
        Ok(Clause::Call {
            procedure,
            args,
            yields,
        })
    }

    fn parse_node_pattern(&mut self) -> Result<NodePattern> {
        self.expect(Token::LParen, "'(' to start a node pattern")?;
        let mut pattern = NodePattern::default();
        if matches!(self.peek(), Some(Token::Ident) | Some(Token::EscapedIdent)) {
            pattern.variable = Some(self.parse_name()?);
        }
        if self.eat(Token::Colon) {
            pattern.labels = self.parse_label_expr()?;
        }
        if self.peek() == Some(Token::LBrace) {
            pattern.properties = Some(self.parse_map_literal()?);
        }
        self.expect(Token::RParen, "')' to close a node pattern")?;
        Ok(pattern)
    }

    /// Label/type expression: `|` has the lowest precedence, then `&` and the
    /// `:` chain shorthand, then `!` and parentheses.
    fn parse_label_expr(&mut self) -> Result<LabelExpr> {
        let mut parts = vec![self.parse_label_and()?];
        while self.eat(Token::Pipe) {
            parts.push(self.parse_label_and()?);
        }
        Ok(if parts.len() == 1 {
            parts.remove(0)
        } else {
            LabelExpr::Or(parts)
        })
    }

    fn parse_label_and(&mut self) -> Result<LabelExpr> {
        let mut parts = vec![self.parse_label_unary()?];
        while self.eat(Token::Amp) || self.eat(Token::Colon) {
            parts.push(self.parse_label_unary()?);
        }
        Ok(if parts.len() == 1 {
            parts.remove(0)
        } else {
            LabelExpr::And(parts)
        })
    }

    fn parse_label_unary(&mut self) -> Result<LabelExpr> {
        // `:T|:U` spells the second alternative with its own colon.
        self.eat(Token::Colon);
        if self.eat(Token::Bang) {
            return Ok(LabelExpr::Not(Box::new(self.parse_label_unary()?)));
        }
        if self.eat(Token::LParen) {
            let expr = self.parse_label_expr()?;
            self.expect(Token::RParen, "')' to close a label expression")?;
            return Ok(expr);
        }
        Ok(LabelExpr::Label(self.parse_symbol_name()?))
    }

    fn parse_relationship(&mut self) -> Result<RelationshipPattern> {
        let start = match self.advance() {
            Some(Token::Minus) => Token::Minus,
            Some(Token::ArrowLeft) => Token::ArrowLeft,
            _ => return self.fail("expected '-' or '<-' in a relationship pattern"),
        };
        let mut pattern = RelationshipPattern {
            variable: None,
            types: LabelExpr::default(),
            direction: RelDirection::Undirected,
            var_length: None,
            properties: None,
        };
        if self.eat(Token::LBracket) {
            if matches!(self.peek(), Some(Token::Ident) | Some(Token::EscapedIdent)) {
                pattern.variable = Some(self.parse_name()?);
            }
            if self.eat(Token::Colon) {
                pattern.types = self.parse_label_expr()?;
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
            // Arrowheads on both sides (`<--` with `-->`, with or without
            // brackets) spell an undirected relationship; the writer's
            // CREATE planner rejects it there.
            (Token::ArrowLeft, Token::ArrowRight) => RelDirection::Undirected,
            _ => return self.fail("invalid relationship direction"),
        };
        Ok(pattern)
    }

    fn parse_var_length(&mut self) -> Result<VarLength> {
        let mut min = 1u32;
        let mut max = None;
        let mut explicit = false;
        if self.peek() == Some(Token::Integer) {
            min = self.parse_u32("relationship length")?;
            explicit = true;
        }
        if self.eat(Token::DotDot) {
            if self.peek() == Some(Token::Integer) {
                max = Some(self.parse_u32("relationship length")?);
            }
        } else if explicit {
            // `*2` means exactly two hops.
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

    /// A label or relationship type name: identifiers and keywords are
    /// both accepted (`:CONTAINS` is a valid type).
    fn parse_symbol_name(&mut self) -> Result<String> {
        if matches!(self.peek(), Some(Token::Ident) | Some(Token::EscapedIdent)) {
            return self.parse_name();
        }
        match self.peek() {
            Some(token) if token.is_keyword() => {
                self.advance();
                let spanned = self.tokens[self.pos - 1];
                Ok(self.source[spanned.start..spanned.end].to_owned())
            }
            _ => self.fail("expected a label or type name"),
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

    /// Comparison operators bind looser than membership and string-list
    /// predicates, and a chain such as `a < b < c` is the conjunction
    /// `a < b AND b < c` (openCypher's chained comparison).
    fn parse_comparison(&mut self) -> Result<Expr> {
        let mut left = self.parse_membership()?;
        let mut conjuncts: Vec<Expr> = Vec::new();
        loop {
            let op = match self.peek() {
                Some(Token::Equals) => BinaryOp::Eq,
                Some(Token::NotEquals) => BinaryOp::Ne,
                Some(Token::Less) => BinaryOp::Lt,
                Some(Token::LessEquals) => BinaryOp::Le,
                Some(Token::Greater) => BinaryOp::Gt,
                Some(Token::GreaterEquals) => BinaryOp::Ge,
                Some(Token::RegexMatch) => BinaryOp::Regex,
                _ => break,
            };
            self.advance();
            let right = self.parse_membership()?;
            conjuncts.push(binary(op, left.clone(), right.clone()));
            left = right;
        }
        Ok(match conjuncts.len() {
            0 => left,
            1 => conjuncts.pop().expect("one conjunct"),
            _ => {
                let mut chains = conjuncts.into_iter();
                let mut combined = chains.next().expect("first conjunct");
                for conjunct in chains {
                    combined = binary(BinaryOp::And, combined, conjunct);
                }
                combined
            }
        })
    }

    /// `IN`, `STARTS WITH`, `ENDS WITH` and `CONTAINS` bind tighter than the
    /// comparison operators: `a = b IN c` is `a = (b IN c)`.
    fn parse_membership(&mut self) -> Result<Expr> {
        let mut left = self.parse_additive()?;
        loop {
            let op = match self.peek() {
                Some(Token::In) => BinaryOp::In,
                Some(Token::Starts) => BinaryOp::StartsWith,
                Some(Token::Ends) => BinaryOp::EndsWith,
                Some(Token::Contains) => BinaryOp::Contains,
                _ => break,
            };
            self.advance();
            if op == BinaryOp::StartsWith {
                self.expect(Token::With, "WITH after STARTS")?;
            } else if op == BinaryOp::EndsWith {
                self.expect(Token::With, "WITH after ENDS")?;
            }
            let right = self.parse_additive()?;
            left = binary(op, left, right);
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

    /// Exponentiation is left-associative: `4 ^ 6 ^ 3` is `(4 ^ 6) ^ 3`.
    fn parse_power(&mut self) -> Result<Expr> {
        let mut left = self.parse_unary()?;
        while self.eat(Token::Caret) {
            let right = self.parse_unary()?;
            left = binary(BinaryOp::Pow, left, right);
        }
        Ok(left)
    }

    fn parse_unary(&mut self) -> Result<Expr> {
        if self.eat(Token::Minus) {
            // Fold the sign into a following number literal so that the full
            // range down to `i64::MIN` and huge negative floats parse, and
            // `-0.0` normalizes to `0.0` like Neo4j.
            if let Some(literal) = self.try_parse_negative_number()? {
                return Ok(literal);
            }
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

    /// The sign of a number literal immediately following a unary minus.
    fn try_parse_negative_number(&mut self) -> Result<Option<Expr>> {
        let spanned = match self.tokens.get(self.pos) {
            Some(spanned) => *spanned,
            None => return Ok(None),
        };
        let text = &self.source[spanned.start..spanned.end];
        let value = match spanned.token {
            Token::Float => {
                self.advance();
                let value = parse_float(text)?;
                // Negative zero is normalized to positive zero.
                Number::Float(if value == 0.0 { 0.0 } else { -value })
            }
            Token::Integer => {
                self.advance();
                parse_negative_decimal_integer(text)?
            }
            Token::HexInteger => {
                self.advance();
                parse_negative_radix_integer(text, 16, "0x")?
            }
            Token::OctalInteger => {
                self.advance();
                parse_negative_radix_integer(text, 8, "0o")?
            }
            _ => return Ok(None),
        };
        Ok(Some(match value {
            Number::Int(value) => Expr::Literal(Value::Int(value)),
            Number::Float(value) => Expr::Literal(Value::Float(value)),
        }))
    }

    /// Parse a function-call argument list; the opening parenthesis has
    /// already been consumed.
    fn parse_call_arguments(&mut self) -> Result<Vec<Expr>> {
        let mut args = Vec::new();
        if self.peek() != Some(Token::RParen) {
            args.push(self.parse_expr()?);
            while self.eat(Token::Comma) {
                args.push(self.parse_expr()?);
            }
        }
        self.expect(Token::RParen, "')' to close a function call")?;
        Ok(args)
    }

    fn parse_postfix(&mut self) -> Result<Expr> {
        let mut expr = self.parse_primary()?;
        loop {
            match self.peek() {
                // `IS [NOT] NULL` binds tighter than comparisons:
                // `false = true IS NULL` is `false = (true IS NULL)`.
                Some(Token::Is) => {
                    self.advance();
                    let negated = self.eat(Token::Not);
                    self.expect(Token::Null, "NULL after IS")?;
                    expr = Expr::IsNull {
                        expr: Box::new(expr),
                        negated,
                    };
                }
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
                    let mut labels = vec![self.parse_symbol_name()?];
                    while self.eat(Token::Colon) {
                        labels.push(self.parse_symbol_name()?);
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
                Ok(match parse_decimal_integer(text())? {
                    Number::Int(value) => Expr::Literal(Value::Int(value)),
                    Number::Float(value) => Expr::Literal(Value::Float(value)),
                })
            }
            Token::HexInteger => {
                self.advance();
                let value = parse_radix_integer(text(), 16, "0x")?;
                Ok(Expr::Literal(Value::Int(value)))
            }
            Token::OctalInteger => {
                self.advance();
                let value = parse_radix_integer(text(), 8, "0o")?;
                Ok(Expr::Literal(Value::Int(value)))
            }
            Token::Float => {
                self.advance();
                let value = parse_float(text())?;
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
                // `exists { ... }` is an existential subquery, not a call.
                if name.eq_ignore_ascii_case("exists") && self.peek() == Some(Token::LBrace) {
                    self.advance();
                    let clauses =
                        if matches!(self.peek(), Some(Token::LParen) | Some(Token::LBracket)) {
                            let patterns = self.parse_patterns()?;
                            let where_clause = if self.eat(Token::Where) {
                                Some(self.parse_expr()?)
                            } else {
                                None
                            };
                            vec![Clause::Match(MatchClause {
                                optional: false,
                                patterns,
                                where_clause,
                            })]
                        } else {
                            self.parse_clauses()?
                        };
                    self.expect(Token::RBrace, "'}' to close the EXISTS subquery")?;
                    return Ok(Expr::ExistsSubquery(Box::new(QueryPart {
                        union_all: false,
                        clauses,
                    })));
                }
                // Namespaced function calls such as `date.truncate(...)` or
                // `duration.inDays(...)`: an identifier, a dot, a second
                // identifier and an argument list.
                if self.peek() == Some(Token::Dot)
                    && matches!(
                        self.tokens.get(self.pos + 1).map(|entry| entry.token),
                        Some(Token::Ident) | Some(Token::EscapedIdent)
                    )
                    && self.tokens.get(self.pos + 2).map(|entry| entry.token) == Some(Token::LParen)
                {
                    self.advance();
                    let member = self.parse_name()?;
                    self.advance();
                    let args = self.parse_call_arguments()?;
                    let qualified = format!(
                        "{}.{}",
                        name.to_ascii_lowercase(),
                        member.to_ascii_lowercase()
                    );
                    return Ok(Expr::Function {
                        name: qualified,
                        distinct: false,
                        star: false,
                        args,
                    });
                }
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
                if let Some(pattern) = self.try_parse_pattern_expr()? {
                    return Ok(pattern);
                }
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
                if let Some(comprehension) = self.try_parse_pattern_comprehension()? {
                    return Ok(comprehension);
                }
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

/// A parsed number literal, before it becomes an [`Expr`].
enum Number {
    Int(i64),
    Float(f64),
}

/// Parse a decimal integer literal, falling back to a float when the value
/// exceeds the `u64` range (Neo4j parses arbitrarily long numbers as floats
/// but rejects values that could have been a 64-bit unsigned integer).
fn parse_decimal_integer(text: &str) -> Result<Number> {
    if let Ok(value) = text.parse::<i64>() {
        return Ok(Number::Int(value));
    }
    if text.parse::<u64>().is_ok() {
        return Err(Error::syntax("integer literal overflows 64 bits"));
    }
    Ok(Number::Float(parse_float(text)?))
}

/// Parse a `0x`/`0o` integer literal.
fn parse_radix_integer(text: &str, radix: u32, prefix: &str) -> Result<i64> {
    let digits = text
        .strip_prefix(prefix)
        .or_else(|| text.strip_prefix(&prefix.to_ascii_uppercase()))
        .unwrap_or(text);
    let magnitude = u64::from_str_radix(digits, radix)
        .map_err(|_| Error::syntax(format!("invalid integer literal `{text}`")))?;
    i64::try_from(magnitude).map_err(|_| Error::syntax("integer literal overflows 64 bits"))
}

/// Parse the text following a unary minus.
fn parse_negative_decimal_integer(text: &str) -> Result<Number> {
    let signed = format!("-{text}");
    if let Ok(value) = signed.parse::<i64>() {
        return Ok(Number::Int(value));
    }
    match text.parse::<u64>() {
        // Within the unsigned range: an overflow rather than a float.
        Ok(_) => Err(Error::syntax("integer literal overflows 64 bits")),
        Err(_) => Ok(Number::Float(parse_float(&signed)?)),
    }
}

/// Parse the text following a unary minus for `0x`/`0o` literals.
fn parse_negative_radix_integer(text: &str, radix: u32, prefix: &str) -> Result<Number> {
    let digits = text
        .strip_prefix(prefix)
        .or_else(|| text.strip_prefix(&prefix.to_ascii_uppercase()))
        .unwrap_or(text);
    let magnitude = u64::from_str_radix(digits, radix)
        .map_err(|_| Error::syntax(format!("invalid integer literal `{text}`")))?;
    // Negating the magnitude covers `-2^63` exactly.
    i64::try_from(0_i128 - magnitude as i128)
        .map(Number::Int)
        .map_err(|_| Error::syntax("integer literal overflows 64 bits"))
}

/// Parse a float literal, rejecting values that do not fit `f64`.
fn parse_float(text: &str) -> Result<f64> {
    let value = text
        .parse::<f64>()
        .map_err(|_| Error::syntax(format!("invalid float literal `{text}`")))?;
    if !value.is_finite() {
        return Err(Error::syntax("floating point literal overflows 64 bits"));
    }
    Ok(value)
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

/// A pattern used as an expression needs a relationship or an element
/// constraint; `(x)` alone stays a parenthesized expression.
fn pattern_expression_shape(path: &PatternPath) -> bool {
    !path.segments.is_empty()
        || path.variable.is_some()
        || !path.start.labels.is_empty()
        || path.start.properties.is_some()
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
        assert_eq!(path.start.labels, LabelExpr::Label("Person".to_owned()));
        assert!(path.start.properties.is_some());
        assert_eq!(path.segments.len(), 1);
        assert_eq!(
            path.segments[0].relationship.direction,
            RelDirection::Outgoing
        );
        assert!(matched.where_clause.is_some());
    }

    #[test]
    fn parses_label_expressions() {
        let query = parse("MATCH (n:A|B:C), (m:!D), (o:(A|B)&C) RETURN n, m, o").expect("parse");
        let Clause::Match(matched) = &query.parts[0].clauses[0] else {
            panic!("expected MATCH");
        };
        let label = |name: &str| LabelExpr::Label(name.to_owned());
        assert_eq!(
            matched.patterns[0].start.labels,
            LabelExpr::Or(vec![
                label("A"),
                LabelExpr::And(vec![label("B"), label("C")])
            ])
        );
        assert_eq!(
            matched.patterns[1].start.labels,
            LabelExpr::Not(Box::new(label("D")))
        );
        assert_eq!(
            matched.patterns[2].start.labels,
            LabelExpr::And(vec![
                LabelExpr::Or(vec![label("A"), label("B")]),
                label("C"),
            ])
        );
        // Relationship type expressions share the grammar, including the
        // `:T|:U` spelling of the second alternative.
        let query = parse("MATCH ()-[:X|:!Y]-() RETURN 1").expect("parse");
        let Clause::Match(matched) = &query.parts[0].clauses[0] else {
            panic!("expected MATCH");
        };
        assert_eq!(
            matched.patterns[0].segments[0].relationship.types,
            LabelExpr::Or(vec![label("X"), LabelExpr::Not(Box::new(label("Y")))])
        );
    }

    #[test]
    fn parses_ddl_statements() {
        let statement = parse_statement(
            "CREATE INDEX person_name IF NOT EXISTS FOR (n:Person) ON (n.name, n.age)",
        )
        .expect("parse");
        let Statement::Ddl(DdlStatement::CreateIndex(index)) = statement else {
            panic!("expected CREATE INDEX");
        };
        assert_eq!(index.name, "person_name");
        assert!(index.if_not_exists);
        assert_eq!(
            index.entity,
            DdlEntity::Node {
                label: "Person".to_owned()
            }
        );
        assert_eq!(index.keys, vec!["name", "age"]);

        let statement =
            parse_statement("CREATE INDEX knows_weight FOR ()-[r:KNOWS]-() ON (r.weight)")
                .expect("parse");
        let Statement::Ddl(DdlStatement::CreateIndex(index)) = statement else {
            panic!("expected CREATE INDEX");
        };
        assert_eq!(
            index.entity,
            DdlEntity::Edge {
                edge_type: "KNOWS".to_owned()
            }
        );
        assert!(!index.if_not_exists);
        assert_eq!(index.keys, vec!["weight"]);

        let statement = parse_statement(
            "CREATE CONSTRAINT person_key FOR (n:Person) REQUIRE (n.name, n.surname) IS NODE KEY",
        )
        .expect("parse");
        let Statement::Ddl(DdlStatement::CreateConstraint(constraint)) = statement else {
            panic!("expected CREATE CONSTRAINT");
        };
        assert_eq!(constraint.kind, DdlConstraintKind::NodeKey);
        assert_eq!(constraint.keys, vec!["name", "surname"]);

        let statement = parse_statement(
            "CREATE CONSTRAINT knows_weight FOR ()-[r:KNOWS]-() REQUIRE r.weight IS NOT NULL",
        )
        .expect("parse");
        let Statement::Ddl(DdlStatement::CreateConstraint(constraint)) = statement else {
            panic!("expected CREATE CONSTRAINT");
        };
        assert_eq!(constraint.kind, DdlConstraintKind::NotNull);

        let statement = parse_statement("DROP CONSTRAINT person_key IF EXISTS").expect("parse");
        assert_eq!(
            statement,
            Statement::Ddl(DdlStatement::DropConstraint {
                name: "person_key".to_owned(),
                if_exists: true,
            })
        );
        let statement = parse_statement("DROP INDEX person_name").expect("parse");
        assert_eq!(
            statement,
            Statement::Ddl(DdlStatement::DropIndex {
                name: "person_name".to_owned(),
                if_exists: false,
            })
        );
    }

    #[test]
    fn ddl_errors_point_at_the_replacement() {
        let err = parse_statement("CREATE INDEX ON :Person(name)").expect_err("old form");
        assert!(err.message().contains("CREATE INDEX name FOR"), "{err}");
        let err = parse_statement("CREATE CONSTRAINT ON (n:Person) ASSERT n.name IS UNIQUE")
            .expect_err("old form");
        assert!(err.message().contains("REQUIRE"), "{err}");
        let err = parse_statement("SHOW INDEXES").expect_err("show");
        assert_eq!(err.code(), crate::error::ErrorCode::Unsupported);
        let err = parse_statement("CREATE INDEX FOR (n:Person) ON (n.name)").expect_err("name");
        assert!(err.message().contains("name is required"), "{err}");
        let err = parse_statement("CREATE CONSTRAINT c FOR ()-[r:T]-() REQUIRE r.p IS NODE KEY")
            .expect_err("edge node key");
        assert!(err.message().contains("NODE KEY"), "{err}");
        let err = parse_statement("CREATE CONSTRAINT c FOR (n:N) REQUIRE (n.a, n.b) IS UNIQUE")
            .expect_err("two keys");
        assert!(err.message().contains("exactly one property"), "{err}");
    }

    #[test]
    fn create_patterns_are_not_ddl() {
        // `CREATE` followed by a pattern stays a query; identifiers named
        // `index` and `drop` still work as variables.
        let statement = parse_statement("CREATE (n:Index) RETURN n").expect("parse");
        assert!(matches!(statement, Statement::Query(_)));
        let statement = parse_statement("MATCH (index) RETURN index").expect("parse");
        assert!(matches!(statement, Statement::Query(_)));
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
    fn parses_hex_octal_and_extreme_literals() {
        let int_of = |query: &str| match expr_of(query) {
            Expr::Literal(Value::Int(value)) => value,
            other => panic!("expected integer literal, got {other:?}"),
        };
        assert_eq!(int_of("RETURN 0x1A2b"), 0x1a2b);
        assert_eq!(int_of("RETURN 0o17"), 0o17);
        assert_eq!(int_of("RETURN 0x7FFFFFFFFFFFFFFF"), i64::MAX);
        assert_eq!(int_of("RETURN -0x8000000000000000"), i64::MIN);
        assert_eq!(int_of("RETURN -0o1000000000000000000000"), i64::MIN);
        assert_eq!(int_of("RETURN -9223372036854775808"), i64::MIN);

        for query in [
            "RETURN 9223372036854775808",
            "RETURN -9223372036854775809",
            "RETURN 0o1000000000000000000000",
            "RETURN 1.34E999",
        ] {
            assert!(parse(query).is_err(), "{query}");
        }

        let float_of = |query: &str| match expr_of(query) {
            Expr::Literal(Value::Float(value)) => value,
            other => panic!("expected float literal, got {other:?}"),
        };
        assert_eq!(float_of("RETURN -0.0"), 0.0);
        assert_eq!(float_of("RETURN -.0"), 0.0);
        assert!(float_of("RETURN -12635418652381263541826355").is_sign_negative());
    }

    #[test]
    fn power_is_left_associative() {
        let expr = expr_of("RETURN 2 ^ 3 ^ 2");
        let Expr::Binary { op, left, .. } = expr else {
            panic!("expected binary");
        };
        assert_eq!(op, BinaryOp::Pow);
        let Expr::Binary { op, .. } = *left else {
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
