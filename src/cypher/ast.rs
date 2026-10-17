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

/// Node pattern `(var:Label {props})`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct NodePattern {
    /// Optional variable.
    pub variable: Option<String>,
    /// Label names.
    pub labels: Vec<String>,
    /// Optional inline property map.
    pub properties: Option<Expr>,
}

/// Relationship pattern `-[var:TYPE {props}]->`.
#[derive(Debug, Clone, PartialEq)]
pub struct RelationshipPattern {
    /// Optional variable.
    pub variable: Option<String>,
    /// Edge type names.
    pub types: Vec<String>,
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
