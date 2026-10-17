//! Cypher abstract syntax tree.

use crate::record::Value;

/// Unary operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    /// Arithmetic negation.
    Neg,
    /// Boolean negation.
    Not,
}

/// Binary operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOp {
    /// `+` (numeric addition or string concatenation).
    Add,
    /// `-`
    Sub,
    /// `*`
    Mul,
    /// `/`
    Div,
    /// `%`
    Mod,
    /// `^`
    Pow,
    /// `=`
    Eq,
    /// `<>`
    Ne,
    /// `<`
    Lt,
    /// `<=`
    Le,
    /// `>`
    Gt,
    /// `>=`
    Ge,
    /// `AND`
    And,
    /// `OR`
    Or,
    /// `XOR`
    Xor,
    /// `IN`
    In,
    /// `STARTS WITH`
    StartsWith,
    /// `ENDS WITH`
    EndsWith,
    /// `CONTAINS`
    Contains,
    /// `=~`
    Regex,
}

/// A Cypher expression.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    /// Literal value.
    Literal(Value),
    /// `$name` parameter.
    Parameter(String),
    /// Variable reference.
    Variable(String),
    /// `expr.key` property access.
    Property {
        /// Base expression.
        base: Box<Expr>,
        /// Property key.
        key: String,
    },
    /// `expr[index]` subscript.
    Subscript {
        /// Base expression.
        base: Box<Expr>,
        /// Index expression.
        index: Box<Expr>,
    },
    /// `[var IN list WHERE predicate | projection]` list comprehension.
    ListComprehension {
        /// Loop variable.
        var: String,
        /// Source list.
        list: Box<Expr>,
        /// Optional filter.
        predicate: Option<Box<Expr>>,
        /// Optional projection; defaults to the loop variable.
        projection: Option<Box<Expr>>,
    },
    /// `all|any|none|single(var IN list WHERE predicate)`.
    Quantifier {
        /// Which quantifier.
        kind: QuantifierKind,
        /// Loop variable.
        var: String,
        /// Source list.
        list: Box<Expr>,
        /// Predicate each element is tested against.
        predicate: Box<Expr>,
    },
    /// `reduce(accumulator = init, var IN list | expression)`.
    Reduce {
        /// Accumulator variable.
        accumulator: String,
        /// Initial accumulator value.
        init: Box<Expr>,
        /// Loop variable.
        var: String,
        /// Source list.
        list: Box<Expr>,
        /// Fold expression over the accumulator and the element.
        expression: Box<Expr>,
    },
    /// `expr[lower..upper]` list slice; omitted bounds are open.
    Slice {
        /// Base expression.
        base: Box<Expr>,
        /// Inclusive start, from the end when negative.
        lower: Option<Box<Expr>>,
        /// Exclusive end, from the end when negative.
        upper: Option<Box<Expr>>,
    },
    /// Pattern predicate `(a)-[:T]->(b)`; true when the pattern matches.
    Pattern(Box<PatternPath>),
    /// `exists { ... }` subquery: its clauses are only inspected for
    /// whether they match at least one row.
    ExistsSubquery(Box<QueryPart>),
    /// `[p = (a)-[:T]->(b) WHERE predicate | projection]` comprehension.
    PatternComprehension {
        /// Pattern matched for every input row.
        path: Box<PatternPath>,
        /// Optional filter.
        predicate: Option<Box<Expr>>,
        /// Value produced per match.
        projection: Box<Expr>,
    },
    /// Unary operation.
    Unary {
        /// Operator.
        op: UnaryOp,
        /// Operand.
        expr: Box<Expr>,
    },
    /// Binary operation.
    Binary {
        /// Operator.
        op: BinaryOp,
        /// Left operand.
        left: Box<Expr>,
        /// Right operand.
        right: Box<Expr>,
    },
    /// Function call (aggregations included; resolved later).
    Function {
        /// Function name as written.
        name: String,
        /// Whether `DISTINCT` was written.
        distinct: bool,
        /// Whether the sole argument is `*` (e.g. `count(*)`).
        star: bool,
        /// Argument expressions.
        args: Vec<Expr>,
    },
    /// List literal `[a, b]`.
    List(Vec<Expr>),
    /// Map literal `{k: v}`.
    Map(Vec<(String, Expr)>),
    /// `CASE` expression.
    Case {
        /// Optional subject for simple `CASE x WHEN ...`.
        subject: Option<Box<Expr>>,
        /// `WHEN`/`THEN` pairs.
        alternatives: Vec<(Expr, Expr)>,
        /// Optional `ELSE`.
        default: Option<Box<Expr>>,
    },
    /// `expr IS [NOT] NULL`.
    IsNull {
        /// Tested expression.
        expr: Box<Expr>,
        /// Whether `NOT` was written.
        negated: bool,
    },
    /// `expr:Label` predicate.
    LabelCheck {
        /// Tested expression.
        expr: Box<Expr>,
        /// Label names.
        labels: Vec<String>,
    },
}

/// The four list quantifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuantifierKind {
    /// `all`
    All,
    /// `any`
    Any,
    /// `none`
    None,
    /// `single`
    Single,
}

impl QuantifierKind {
    /// The name as written.
    pub const fn name(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Any => "any",
            Self::None => "none",
            Self::Single => "single",
        }
    }
}

/// Direction of a relationship pattern as written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelDirection {
    /// `-->` / `-[...]->`
    Outgoing,
    /// `<--` / `<-[...]-`
    Incoming,
    /// `--` / `-[...]-`
    Undirected,
}

/// Variable-length relationship bounds (`*min..max`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VarLength {
    /// Minimum hops (defaults to 1 when omitted).
    pub min: u32,
    /// Maximum hops, `None` for unbounded.
    pub max: Option<u32>,
}

/// A label (or relationship type) expression: names combined with `&`, `|`
/// and `!`, as introduced by Neo4j 5. `:` chains are conjunctions too.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LabelExpr {
    /// A single label or type name.
    Label(String),
    /// Every sub-expression must match (empty means no label was written).
    And(Vec<LabelExpr>),
    /// At least one sub-expression must match.
    Or(Vec<LabelExpr>),
    /// The sub-expression must not match.
    Not(Box<LabelExpr>),
}

impl Default for LabelExpr {
    fn default() -> Self {
        Self::And(Vec::new())
    }
}

impl LabelExpr {
    /// Whether no label was written at all.
    pub fn is_empty(&self) -> bool {
        matches!(self, Self::And(parts) if parts.is_empty())
    }

    /// The label names when the expression is a conjunction of plain labels,
    /// which is the only form write clauses accept.
    pub fn plain_labels(&self) -> Option<Vec<String>> {
        let mut labels = Vec::new();
        self.collect_plain(&mut labels).then_some(labels)
    }

    /// Every label a matching node must carry. Disjunctions and negations
    /// guarantee nothing even when they mention labels.
    pub fn guaranteed_labels(&self) -> Vec<String> {
        match self {
            Self::Label(label) => vec![label.clone()],
            Self::And(parts) => parts.iter().flat_map(Self::guaranteed_labels).collect(),
            Self::Or(_) | Self::Not(_) => Vec::new(),
        }
    }

    fn collect_plain(&self, labels: &mut Vec<String>) -> bool {
        match self {
            Self::Label(label) => {
                labels.push(label.clone());
                true
            }
            Self::And(parts) => parts.iter().all(|part| part.collect_plain(labels)),
            Self::Or(_) | Self::Not(_) => false,
        }
    }
}

/// Node pattern `(var:Label {props})`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct NodePattern {
    /// Optional variable.
    pub variable: Option<String>,
    /// Label expression.
    pub labels: LabelExpr,
    /// Optional inline property map.
    pub properties: Option<Expr>,
}

/// Relationship pattern `-[var:TYPE {props}]->`.
#[derive(Debug, Clone, PartialEq)]
pub struct RelationshipPattern {
    /// Optional variable.
    pub variable: Option<String>,
    /// Relationship type expression.
    pub types: LabelExpr,
    /// Direction.
    pub direction: RelDirection,
    /// Optional variable-length bounds.
    pub var_length: Option<VarLength>,
    /// Optional inline property map.
    pub properties: Option<Expr>,
}

/// One node-relationship-node step.
#[derive(Debug, Clone, PartialEq)]
pub struct PathSegment {
    /// Relationship pattern.
    pub relationship: RelationshipPattern,
    /// Destination node pattern.
    pub node: NodePattern,
}

/// How a path pattern searches for paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Shortest {
    /// Match every path.
    #[default]
    Plain,
    /// `shortestPath(...)`: one path of minimal length.
    Shortest,
    /// `allShortestPaths(...)`: every path of minimal length.
    AllShortest,
}

/// A path pattern starting at a node.
#[derive(Debug, Clone, PartialEq)]
pub struct PatternPath {
    /// Optional path variable (`p = (a)-->(b)`).
    pub variable: Option<String>,
    /// Whether the pattern is wrapped in `shortestPath`/`allShortestPaths`.
    pub shortest: Shortest,
    /// Starting node.
    pub start: NodePattern,
    /// Steps.
    pub segments: Vec<PathSegment>,
}

/// `MATCH` clause.
#[derive(Debug, Clone, PartialEq)]
pub struct MatchClause {
    /// Whether `OPTIONAL` was written.
    pub optional: bool,
    /// Patterns.
    pub patterns: Vec<PatternPath>,
    /// Trailing `WHERE`.
    pub where_clause: Option<Expr>,
}

/// A `RETURN`/`WITH` item.
#[derive(Debug, Clone, PartialEq)]
pub struct ReturnItem {
    /// Whether this item is `*`.
    pub star: bool,
    /// Expression (ignored when `star`).
    pub expr: Expr,
    /// Alias written with `AS`.
    pub alias: Option<String>,
    /// The expression exactly as written, used as the column name when the
    /// item has no alias.
    pub text: Option<String>,
}

/// An `ORDER BY` key.
#[derive(Debug, Clone, PartialEq)]
pub struct SortItem {
    /// Key expression.
    pub expr: Expr,
    /// Ascending order.
    pub ascending: bool,
}

/// `SET`/`REMOVE` item.
#[derive(Debug, Clone, PartialEq)]
pub enum SetItem {
    /// `n.key = value`
    Property {
        /// Target variable.
        variable: String,
        /// Property key.
        key: String,
        /// New value.
        value: Expr,
    },
    /// `n = {props}` or `n = other`
    Replace {
        /// Target variable.
        variable: String,
        /// New value (map or entity).
        value: Expr,
    },
    /// `n += {props}`
    Append {
        /// Target variable.
        variable: String,
        /// Map of properties to merge.
        value: Expr,
    },
    /// `n:Label` (SET) or `n:Label` (REMOVE)
    Label {
        /// Target variable.
        variable: String,
        /// Label names.
        labels: Vec<String>,
    },
}

/// A top-level clause.
#[derive(Debug, Clone, PartialEq)]
pub enum Clause {
    /// `MATCH`/`OPTIONAL MATCH`.
    Match(MatchClause),
    /// `CREATE` patterns.
    Create(Vec<PatternPath>),
    /// `MERGE` a single path pattern.
    Merge {
        /// Pattern to match or create.
        pattern: PatternPath,
        /// `ON CREATE SET`.
        on_create: Vec<SetItem>,
        /// `ON MATCH SET`.
        on_match: Vec<SetItem>,
    },
    /// `SET`.
    Set(Vec<SetItem>),
    /// `REMOVE`.
    Remove(Vec<SetItem>),
    /// `DELETE`/`DETACH DELETE`.
    Delete {
        /// Whether `DETACH` was written.
        detach: bool,
        /// Expressions evaluating to entities.
        exprs: Vec<Expr>,
    },
    /// `WITH`.
    With {
        /// Whether `DISTINCT` was written.
        distinct: bool,
        /// Items.
        items: Vec<ReturnItem>,
        /// `ORDER BY` keys.
        order: Vec<SortItem>,
        /// `SKIP` expression.
        skip: Option<Expr>,
        /// `LIMIT` expression.
        limit: Option<Expr>,
        /// Trailing `WHERE`.
        where_clause: Option<Expr>,
    },
    /// `RETURN`.
    Return {
        /// Whether `DISTINCT` was written.
        distinct: bool,
        /// Items.
        items: Vec<ReturnItem>,
        /// `ORDER BY` keys.
        order: Vec<SortItem>,
        /// `SKIP` expression.
        skip: Option<Expr>,
        /// `LIMIT` expression.
        limit: Option<Expr>,
    },
    /// `UNWIND`.
    Unwind {
        /// List expression.
        expr: Expr,
        /// Alias for each element.
        alias: String,
    },
    /// `LOAD CSV [WITH HEADERS] FROM source AS alias [FIELDTERMINATOR char]`.
    LoadCsv {
        /// Whether the first row holds column names.
        with_headers: bool,
        /// Source path expression (evaluated per input row).
        source: Expr,
        /// Variable bound to each row (a map with headers, a list without).
        alias: String,
        /// Field separator; defaults to a comma.
        field_terminator: Option<char>,
    },
    /// `FOREACH (var IN list | updating clauses)`.
    Foreach {
        /// Loop variable.
        var: String,
        /// List expression.
        list: Expr,
        /// Updating clauses run once per element.
        clauses: Vec<Clause>,
    },
    /// `CALL procedure(args) YIELD column [AS alias]`.
    Call {
        /// Dotted procedure name.
        procedure: String,
        /// Argument expressions.
        args: Vec<Expr>,
        /// Yielded columns with optional aliases.
        yields: Vec<(String, Option<String>)>,
    },
}

/// A parsed query: one or more union parts.
#[derive(Debug, Clone, PartialEq)]
pub struct Query {
    /// Query parts; more than one means `UNION` / `UNION ALL`.
    pub parts: Vec<QueryPart>,
}

/// One part of a query, a pipeline of clauses.
#[derive(Debug, Clone, PartialEq)]
pub struct QueryPart {
    /// Whether this part is joined to the previous one with `UNION ALL`
    /// (meaningless for the first part).
    pub union_all: bool,
    /// Clauses in order.
    pub clauses: Vec<Clause>,
}

/// A parsed statement: a query or a schema DDL command.
#[derive(Debug, Clone, PartialEq)]
pub enum Statement {
    /// A regular query.
    Query(Query),
    /// A schema command (`CREATE INDEX`, `CREATE CONSTRAINT`, `DROP ...`).
    Ddl(DdlStatement),
}

/// A schema DDL command.
#[derive(Debug, Clone, PartialEq)]
pub enum DdlStatement {
    /// `CREATE INDEX name [IF NOT EXISTS] FOR (n:Label) ON (n.key, ...)`
    CreateIndex(DdlIndex),
    /// `CREATE CONSTRAINT name [IF NOT EXISTS] FOR ... REQUIRE ...`
    CreateConstraint(DdlConstraint),
    /// `DROP INDEX name [IF EXISTS]`
    DropIndex {
        /// Index name.
        name: String,
        /// Whether a missing index is an error.
        if_exists: bool,
    },
    /// `DROP CONSTRAINT name [IF EXISTS]`
    DropConstraint {
        /// Constraint name.
        name: String,
        /// Whether a missing constraint is an error.
        if_exists: bool,
    },
}

impl DdlStatement {
    /// One-line description for `EXPLAIN` output.
    pub fn describe(&self) -> String {
        match self {
            Self::CreateIndex(index) => format!(
                "CreateIndex {} on {} keys=[{}]{}",
                index.name,
                index.entity.describe(),
                index.keys.join(", "),
                if index.if_not_exists {
                    " if_not_exists"
                } else {
                    ""
                }
            ),
            Self::CreateConstraint(constraint) => format!(
                "CreateConstraint {} on {} kind={:?} keys=[{}]{}",
                constraint.name,
                constraint.entity.describe(),
                constraint.kind,
                constraint.keys.join(", "),
                if constraint.if_not_exists {
                    " if_not_exists"
                } else {
                    ""
                }
            ),
            Self::DropIndex { name, if_exists } => format!(
                "DropIndex {name}{}",
                if *if_exists { " if_exists" } else { "" }
            ),
            Self::DropConstraint { name, if_exists } => format!(
                "DropConstraint {name}{}",
                if *if_exists { " if_exists" } else { "" }
            ),
        }
    }
}

/// The entity an index or constraint applies to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DdlEntity {
    /// Nodes carrying a label.
    Node {
        /// Label name.
        label: String,
    },
    /// Relationships of a type.
    Edge {
        /// Edge type name.
        edge_type: String,
    },
}

impl DdlEntity {
    /// One-line description for `EXPLAIN` output.
    pub fn describe(&self) -> String {
        match self {
            Self::Node { label } => format!("(n:{label})"),
            Self::Edge { edge_type } => format!("()-[r:{edge_type}]-()"),
        }
    }
}

/// `CREATE INDEX` body.
#[derive(Debug, Clone, PartialEq)]
pub struct DdlIndex {
    /// Index name.
    pub name: String,
    /// Whether an existing index of that name is not an error.
    pub if_not_exists: bool,
    /// Indexed entity.
    pub entity: DdlEntity,
    /// Property key names in key order.
    pub keys: Vec<String>,
}

/// `CREATE CONSTRAINT` body.
#[derive(Debug, Clone, PartialEq)]
pub struct DdlConstraint {
    /// Constraint name.
    pub name: String,
    /// Whether an existing constraint of that name is not an error.
    pub if_not_exists: bool,
    /// Constrained entity.
    pub entity: DdlEntity,
    /// Constraint kind.
    pub kind: DdlConstraintKind,
    /// Property key names in key order.
    pub keys: Vec<String>,
}

/// Kinds of DDL constraints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DdlConstraintKind {
    /// `IS UNIQUE`
    Unique,
    /// `IS NOT NULL`
    NotNull,
    /// `REQUIRE (n.a, ...) IS NODE KEY` (nodes only).
    NodeKey,
}
