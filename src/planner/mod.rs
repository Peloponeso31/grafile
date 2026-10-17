//! Query planner: parsed Cypher to logical [`Plan`].
//!
//! Planning is clause-by-clause. `MATCH` builds nested loops: an anchor node
//! (chosen by index availability, label selectivity, or full scan) followed
//! by breadth-first expansions. Projections, aggregation, sorting and writes
//! wrap the pattern pipeline. Label, type and property names are resolved
//! against the catalog at plan time; names that do not exist yet (possible in
//! `CREATE`) are resolved at runtime by the VDBE.

pub mod plan;

use std::collections::HashMap;

use hashlink::LinkedHashMap as FastMap;

use crate::cypher::{
    BinaryOp, Clause, Expr, MatchClause, NodePattern, PatternPath, QuantifierKind, Query,
    RelDirection, ReturnItem, SetItem, Shortest, SortItem, UnaryOp,
};
use crate::engine::Database;
use crate::error::{Error, ErrorCode, Result};
use crate::record::Value;
use crate::vdbe::NO_KEY;
use plan::{AggFn, AggItem, CreateItem, IndexSeek, PathElement, Plan};

pub use plan::explain;

/// A planned query: the plan plus its output column names.
#[derive(Debug, Clone)]
pub struct Prepared {
    /// Root of the logical plan.
    pub plan: Plan,
    /// Output column names.
    pub columns: Vec<String>,
    /// Whether the query mutates the graph.
    pub mutating: bool,
}

/// Kind of a variable in scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VarKind {
    Node,
    Edge,
    Value,
    /// A variable-length relationship binding: a list of relationships.
    EdgeList,
    /// A named path; usable neither as a node nor as a relationship.
    Path,
    /// A variable known to hold a scalar (or list or map) of this kind,
    /// which rules it out as a node or relationship binding.
    Scalar(Kind),
}

/// Where a pattern expression appears, which decides whether plan-level
/// extraction is legal.
#[derive(Clone, Copy, PartialEq, Eq)]
enum PatternContext {
    /// Predicate position: bare patterns and comprehensions are legal.
    Predicate,
    /// Value position: only comprehensions are legal.
    Value,
    /// Inside a list-comprehension, quantifier or `reduce` body, where the
    /// code generator evaluates the expression per element without plan
    /// nodes, so pattern expressions cannot be extracted.
    Nested,
}

/// Planner state.
struct Planner<'a> {
    db: &'a mut Database,
    anon_counter: u32,
    /// Relationship variables bound by patterns since the last projection;
    /// re-binding one of these is an error, while a relationship carried
    /// through `WITH` may constrain a later pattern.
    pattern_rels: std::collections::HashSet<String>,
}

/// Plan a parsed query with the given parameter values.
pub fn plan_query(
    db: &mut Database,
    query: &Query,
    params: &HashMap<String, Value>,
) -> Result<Prepared> {
    let query = substitute_query(query, params)?;
    let mut planner = Planner {
        db,
        anon_counter: 0,
        pattern_rels: std::collections::HashSet::new(),
    };
    planner.run(&query)
}

impl<'a> Planner<'a> {
    fn run(&mut self, query: &Query) -> Result<Prepared> {
        let mut plans = Vec::with_capacity(query.parts.len());
        let mut columns: Option<Vec<String>> = None;
        let mut mutating = false;
        for part in &query.parts {
            let (plan, part_columns, part_mutating) = self.plan_part(&part.clauses)?;
            match &columns {
                Some(first) if *first != part_columns => {
                    return Err(Error::syntax(
                        "all subqueries in a UNION must have the same column names",
                    ));
                }
                Some(_) => {}
                None => columns = Some(part_columns),
            }
            mutating |= part_mutating;
            plans.push(plan);
        }
        let columns = columns.unwrap_or_default();
        let plan = if plans.len() == 1 {
            plans.pop().expect("single query part")
        } else {
            let all = query.parts[1].union_all;
            let union = Plan::Union { parts: plans, all };
            if all {
                union
            } else {
                Plan::Distinct {
                    input: Box::new(union),
                }
            }
        };
        Ok(Prepared {
            plan,
            columns,
            mutating,
        })
    }

    /// Plan one query part (a pipeline of clauses) on its own scope.
    fn plan_part(&mut self, clauses: &[Clause]) -> Result<(Plan, Vec<String>, bool)> {
        self.plan_part_with(clauses, FastMap::new())
    }

    /// Plan one query part on top of an initial scope; used by `EXISTS`
    /// subqueries, whose clauses see the enclosing variables.
    fn plan_part_with(
        &mut self,
        clauses: &[Clause],
        scope: FastMap<String, VarKind>,
    ) -> Result<(Plan, Vec<String>, bool)> {
        let mut plan = Plan::Empty;
        let mut scope = scope;
        let mut columns = Vec::new();
        let mut mutating = false;
        for clause in clauses {
            match clause {
                Clause::Match(matched) => {
                    // Relationship-variable reuse errors apply within one
                    // MATCH clause; later clauses may rebind the variable.
                    self.pattern_rels.clear();
                    self.plan_match(matched, &mut plan, &mut scope)?;
                }
                clause @ (Clause::Create(_)
                | Clause::Merge { .. }
                | Clause::Set(_)
                | Clause::Remove(_)
                | Clause::Delete { .. }
                | Clause::Foreach { .. }) => {
                    if self.plan_updating_clause(clause, &mut plan, &mut scope)? {
                        mutating = true;
                        columns.clear();
                    }
                }
                Clause::Call {
                    procedure,
                    args,
                    yields,
                } => {
                    if !args.is_empty() {
                        return Err(Error::unsupported(
                            "procedures with arguments are not supported",
                        ));
                    }
                    let outputs = procedure_columns(procedure)?;
                    let yields: Vec<(String, String)> = if yields.is_empty() {
                        outputs
                            .iter()
                            .map(|name| (name.clone(), name.clone()))
                            .collect()
                    } else {
                        let mut out = Vec::with_capacity(yields.len());
                        for (column, alias) in yields {
                            if !outputs.contains(column) {
                                return Err(Error::syntax(format!(
                                    "procedure `{procedure}` has no column `{column}`"
                                )));
                            }
                            out.push((
                                column.clone(),
                                alias.clone().unwrap_or_else(|| column.clone()),
                            ));
                        }
                        out
                    };
                    for (_, alias) in &yields {
                        scope.insert(alias.clone(), VarKind::Value);
                    }
                    plan = Plan::ProcedureCall {
                        input: Box::new(plan),
                        procedure: procedure.clone(),
                        columns: outputs,
                        yields: yields.clone(),
                    };
                    columns.clear();
                    columns.extend(yields.iter().map(|(_, alias)| alias.clone()));
                }
                Clause::Unwind { expr, alias } => {
                    validate_expr(expr, &scope)?;
                    plan = Plan::Unwind {
                        input: Box::new(plan),
                        expr: expr.clone(),
                        alias: alias.clone(),
                    };
                    scope.insert(alias.clone(), VarKind::Value);
                }
                Clause::With {
                    distinct,
                    items,
                    order,
                    skip,
                    limit,
                    where_clause,
                } => {
                    let (new_plan, new_scope, _) = self.plan_projection(
                        plan,
                        items,
                        *distinct,
                        order,
                        skip,
                        limit,
                        where_clause.as_ref(),
                        &scope,
                        true,
                    )?;
                    plan = new_plan;
                    scope = new_scope;
                    self.pattern_rels.clear();
                    columns.clear();
                }
                Clause::Return {
                    distinct,
                    items,
                    order,
                    skip,
                    limit,
                } => {
                    let (new_plan, _, new_columns) = self.plan_projection(
                        plan, items, *distinct, order, skip, limit, None, &scope, false,
                    )?;
                    plan = new_plan;
                    columns = new_columns;
                    scope.clear();
                    self.pattern_rels.clear();
                }
            }
        }
        Ok((plan, columns, mutating))
    }

    /// Plan one updating clause (`CREATE`, `MERGE`, `SET`, `REMOVE`,
    /// `DELETE`, `FOREACH`). Returns whether it writes to the database.
    fn plan_updating_clause(
        &mut self,
        clause: &Clause,
        plan: &mut Plan,
        scope: &mut FastMap<String, VarKind>,
    ) -> Result<bool> {
        match clause {
            Clause::Create(patterns) => {
                let items = self.plan_create(patterns, scope)?;
                for item in &items {
                    let kind = match item {
                        CreateItem::Node { .. } => VarKind::Node,
                        CreateItem::Edge { .. } => VarKind::Edge,
                    };
                    scope.insert(item.var().to_owned(), kind);
                }
                *plan = Plan::Create {
                    input: Box::new(std::mem::replace(plan, Plan::Empty)),
                    items,
                };
                Ok(true)
            }
            Clause::Merge {
                pattern,
                on_create,
                on_match,
            } => {
                self.plan_merge(pattern, on_create, on_match, plan, scope)?;
                Ok(true)
            }
            Clause::Set(items) => {
                for item in items {
                    self.validate_set_item(item, scope)?;
                }
                *plan = Plan::SetItems {
                    input: Box::new(std::mem::replace(plan, Plan::Empty)),
                    items: items.clone(),
                };
                Ok(true)
            }
            Clause::Remove(items) => {
                for item in items {
                    self.validate_set_item(item, scope)?;
                }
                *plan = Plan::RemoveItems {
                    input: Box::new(std::mem::replace(plan, Plan::Empty)),
                    items: items.clone(),
                };
                Ok(true)
            }
            Clause::Delete { detach, exprs } => {
                for expr in exprs {
                    validate_expr(expr, scope)?;
                    if let Some(kind) = static_kind(expr, scope)
                        && !matches!(
                            kind,
                            Kind::Node | Kind::Edge | Kind::Path | Kind::List | Kind::Null
                        )
                    {
                        return Err(Error::syntax("DELETE expects a node, relationship or path"));
                    }
                }
                *plan = Plan::Delete {
                    input: Box::new(std::mem::replace(plan, Plan::Empty)),
                    exprs: exprs.clone(),
                    detach: *detach,
                };
                Ok(true)
            }
            Clause::Foreach { var, list, clauses } => {
                validate_expr(list, scope)?;
                let mut inner = Plan::Empty;
                let mut inner_scope = scope.clone();
                inner_scope.insert(var.clone(), VarKind::Value);
                for clause in clauses {
                    self.plan_updating_clause(clause, &mut inner, &mut inner_scope)?;
                }
                *plan = Plan::Foreach {
                    input: Box::new(std::mem::replace(plan, Plan::Empty)),
                    var: var.clone(),
                    list: list.clone(),
                    plan: Box::new(inner),
                };
                Ok(true)
            }
            _ => Err(Error::syntax(
                "only updating clauses are allowed in FOREACH",
            )),
        }
    }

    // ---- MATCH ----

    fn plan_match(
        &mut self,
        clause: &MatchClause,
        plan: &mut Plan,
        scope: &mut FastMap<String, VarKind>,
    ) -> Result<()> {
        if clause.optional {
            return self.plan_optional_match(clause, plan, scope);
        }
        for path in &clause.patterns {
            self.plan_path(path, plan, scope)?;
        }
        if let Some(predicate) = &clause.where_clause {
            let mut predicate = predicate.clone();
            self.extract_patterns(&mut predicate, plan, scope, PatternContext::Predicate)?;
            validate_expr(&predicate, scope)?;
            check_predicate_kind(&predicate, scope)?;
            *plan = Plan::Filter {
                input: Box::new(plan.clone()),
                predicate,
            };
        }
        Ok(())
    }

    /// Plan `OPTIONAL MATCH`: the patterns become an inner plan rooted at
    /// `Empty` that references the outer variables; the code generator keeps
    /// one null-extended row when the inner plan yields none.
    fn plan_optional_match(
        &mut self,
        clause: &MatchClause,
        plan: &mut Plan,
        scope: &mut FastMap<String, VarKind>,
    ) -> Result<()> {
        let mut inner = Plan::Empty;
        let mut inner_scope = scope.clone();
        for path in &clause.patterns {
            self.plan_path(path, &mut inner, &mut inner_scope)?;
        }
        if let Some(predicate) = &clause.where_clause {
            let mut predicate = predicate.clone();
            self.extract_patterns(
                &mut predicate,
                &mut inner,
                &mut inner_scope,
                PatternContext::Predicate,
            )?;
            validate_expr(&predicate, &inner_scope)?;
            check_predicate_kind(&predicate, &inner_scope)?;
            inner = Plan::Filter {
                input: Box::new(inner),
                predicate,
            };
        }
        let vars: Vec<String> = inner_scope
            .keys()
            .filter(|var| !scope.contains_key(*var))
            .cloned()
            .collect();
        *scope = inner_scope;
        *plan = Plan::OptionalMatch {
            input: Box::new(plan.clone()),
            inner: Box::new(inner),
            vars,
        };
        Ok(())
    }

    // ---- Pattern expressions ----

    /// Extract pattern expressions into hidden variables bound by wrapping
    /// plan nodes: pattern predicates become booleans, pattern
    /// comprehensions become lists.
    fn extract_patterns(
        &mut self,
        expr: &mut Expr,
        plan: &mut Plan,
        scope: &mut FastMap<String, VarKind>,
        context: PatternContext,
    ) -> Result<()> {
        if let Expr::Function { name, args, .. } = expr
            && args.len() == 1
            && (name.eq_ignore_ascii_case("shortestPath")
                || name.eq_ignore_ascii_case("allShortestPaths"))
            && let Expr::Pattern(path) = &args[0]
        {
            let all = name.eq_ignore_ascii_case("allShortestPaths");
            let mut path = (**path).clone();
            path.variable = None;
            for segment in &mut path.segments {
                segment.relationship.variable = None;
            }
            for node in std::iter::once(&path.start).chain(path.segments.iter().map(|s| &s.node)) {
                if let Some(var) = &node.variable
                    && !scope.contains_key(var)
                {
                    return Err(undefined_variable(var));
                }
            }
            let hidden = self.anon_name("spath");
            // A pattern expression yields null when no path exists, so the
            // search runs inside an optional plan.
            let mut inner = Plan::Empty;
            let mut inner_scope = scope.clone();
            self.plan_shortest(
                &path,
                all,
                Some(hidden.clone()),
                true,
                &mut inner,
                &mut inner_scope,
            )?;
            let vars: Vec<String> = inner_scope
                .keys()
                .filter(|var| !scope.contains_key(*var))
                .cloned()
                .collect();
            *scope = inner_scope;
            *plan = Plan::OptionalMatch {
                input: Box::new(std::mem::replace(plan, Plan::Empty)),
                inner: Box::new(inner),
                vars,
            };
            *expr = Expr::Variable(hidden);
            return Ok(());
        }
        if let Expr::Function { name, args, .. } = expr
            && name.eq_ignore_ascii_case("exists")
            && args.len() == 1
        {
            self.extract_patterns(&mut args[0], plan, scope, PatternContext::Predicate)?;
            let arg = args[0].clone();
            *expr = match arg {
                Expr::Variable(_) => arg,
                other => Expr::IsNull {
                    expr: Box::new(other),
                    negated: true,
                },
            };
            return Ok(());
        }
        if let Expr::ExistsSubquery(part) = expr {
            if context == PatternContext::Nested {
                return Err(Error::unsupported(
                    "EXISTS subqueries inside comprehensions are not supported",
                ));
            }
            // The subquery runs as a semi-join: its clauses build an inner
            // plan over the enclosing scope, and only whether it produced a
            // row is visible.
            for clause in &part.clauses {
                match clause {
                    Clause::Match(_)
                    | Clause::With { .. }
                    | Clause::Return { .. }
                    | Clause::Unwind { .. } => {}
                    _ => {
                        return Err(Error::syntax(
                            "InvalidClauseComposition: EXISTS only allows reading clauses",
                        ));
                    }
                }
            }
            let (inner, _, _) = self.plan_part_with(&part.clauses, scope.clone())?;
            let hidden = self.anon_name("exists");
            scope.insert(hidden.clone(), VarKind::Value);
            *plan = Plan::PatternPredicate {
                input: Box::new(std::mem::replace(plan, Plan::Empty)),
                inner: Box::new(inner),
                var: hidden.clone(),
            };
            *expr = Expr::Variable(hidden);
            return Ok(());
        }
        match expr {
            Expr::ExistsSubquery(_) => Err(Error::internal(
                "EXISTS subquery survived pattern extraction",
            )),
            Expr::Pattern(_) | Expr::PatternComprehension { .. } => {
                if context == PatternContext::Nested {
                    return Err(Error::unsupported(
                        "pattern expressions inside list comprehensions are not supported",
                    ));
                }
                if let Expr::Pattern(path) = expr
                    && let Some(var) = &path.start.variable
                    && path.segments.is_empty()
                {
                    // A lone node pattern is a label predicate when it has
                    // labels; a bare variable pattern has no value.
                    if path.start.labels.is_empty() && path.start.properties.is_none() {
                        return Err(Error::syntax(
                            "InvalidArgumentType: a single node pattern is not a value",
                        ));
                    }
                    if !path.start.labels.is_empty() && path.start.properties.is_none() {
                        *expr = Expr::LabelCheck {
                            expr: Box::new(Expr::Variable(var.clone())),
                            labels: path.start.labels.clone(),
                        };
                        return Ok(());
                    }
                }
                if context == PatternContext::Value && matches!(expr, Expr::Pattern(_)) {
                    return Err(Error::syntax(
                        "pattern expressions are only allowed in predicates",
                    ));
                }
                match expr {
                    Expr::Pattern(path) => {
                        let hidden = self.extract_pattern_predicate(path, plan, scope)?;
                        *expr = Expr::Variable(hidden);
                    }
                    Expr::PatternComprehension {
                        path,
                        predicate,
                        projection,
                    } => {
                        let hidden = self.extract_pattern_comprehension(
                            path, predicate, projection, plan, scope,
                        )?;
                        *expr = Expr::Variable(hidden);
                    }
                    _ => return Err(Error::internal("pattern extraction mismatch")),
                }
                Ok(())
            }
            Expr::Unary {
                op: UnaryOp::Not,
                expr: inner,
            } => self.extract_patterns(inner, plan, scope, PatternContext::Predicate),
            Expr::Binary { op, left, right } => {
                let context = if matches!(op, BinaryOp::And | BinaryOp::Or | BinaryOp::Xor)
                    && context != PatternContext::Nested
                {
                    PatternContext::Predicate
                } else {
                    context
                };
                self.extract_patterns(left, plan, scope, context)?;
                self.extract_patterns(right, plan, scope, context)
            }
            Expr::Function { args, .. } => {
                for arg in args.iter_mut() {
                    self.extract_patterns(arg, plan, scope, context)?;
                }
                Ok(())
            }
            Expr::Quantifier {
                list, predicate, ..
            } => {
                self.extract_patterns(list, plan, scope, context)?;
                self.extract_patterns(predicate, plan, scope, PatternContext::Nested)
            }
            Expr::ListComprehension {
                list,
                predicate,
                projection,
                ..
            } => {
                self.extract_patterns(list, plan, scope, context)?;
                if let Some(predicate) = predicate {
                    self.extract_patterns(predicate, plan, scope, PatternContext::Nested)?;
                }
                if let Some(projection) = projection {
                    self.extract_patterns(projection, plan, scope, PatternContext::Nested)?;
                }
                Ok(())
            }
            Expr::Case {
                subject,
                alternatives,
                default,
            } => {
                if let Some(subject) = subject {
                    self.extract_patterns(subject, plan, scope, context)?;
                }
                for (when, then) in alternatives.iter_mut() {
                    self.extract_patterns(when, plan, scope, PatternContext::Predicate)?;
                    self.extract_patterns(then, plan, scope, context)?;
                }
                if let Some(default) = default {
                    self.extract_patterns(default, plan, scope, context)?;
                }
                Ok(())
            }
            Expr::Reduce {
                init,
                list,
                expression,
                ..
            } => {
                self.extract_patterns(init, plan, scope, context)?;
                self.extract_patterns(list, plan, scope, context)?;
                self.extract_patterns(expression, plan, scope, PatternContext::Nested)
            }
            Expr::List(items) => {
                for item in items.iter_mut() {
                    self.extract_patterns(item, plan, scope, context)?;
                }
                Ok(())
            }
            Expr::Map(entries) => {
                for (_, value) in entries.iter_mut() {
                    self.extract_patterns(value, plan, scope, context)?;
                }
                Ok(())
            }
            Expr::Unary { expr: inner, .. }
            | Expr::Property { base: inner, .. }
            | Expr::IsNull { expr: inner, .. }
            | Expr::LabelCheck { expr: inner, .. } => {
                self.extract_patterns(inner, plan, scope, context)
            }
            Expr::Subscript { base, index } => {
                self.extract_patterns(base, plan, scope, context)?;
                self.extract_patterns(index, plan, scope, context)
            }
            Expr::Slice { base, lower, upper } => {
                self.extract_patterns(base, plan, scope, context)?;
                if let Some(lower) = lower {
                    self.extract_patterns(lower, plan, scope, context)?;
                }
                match upper {
                    Some(upper) => self.extract_patterns(upper, plan, scope, context),
                    None => Ok(()),
                }
            }
            Expr::Literal(_) | Expr::Parameter(_) | Expr::Variable(_) => Ok(()),
        }
    }

    /// Wrap the plan in a pattern predicate node: the inner pattern plan runs
    /// per row and binds a hidden boolean.
    fn extract_pattern_predicate(
        &mut self,
        path: &PatternPath,
        plan: &mut Plan,
        scope: &mut FastMap<String, VarKind>,
    ) -> Result<String> {
        for node in std::iter::once(&path.start).chain(path.segments.iter().map(|s| &s.node)) {
            if let Some(var) = &node.variable
                && !scope.contains_key(var)
            {
                return Err(undefined_variable(var));
            }
        }
        for segment in &path.segments {
            if let Some(var) = &segment.relationship.variable
                && !scope.contains_key(var)
            {
                return Err(undefined_variable(var));
            }
        }
        let mut inner = Plan::Empty;
        let mut inner_scope = scope.clone();
        self.plan_path(path, &mut inner, &mut inner_scope)?;
        let hidden = self.anon_name("pred");
        scope.insert(hidden.clone(), VarKind::Value);
        *plan = Plan::PatternPredicate {
            input: Box::new(std::mem::replace(plan, Plan::Empty)),
            inner: Box::new(inner),
            var: hidden.clone(),
        };
        Ok(hidden)
    }

    /// Wrap the plan in a pattern comprehension node: the inner pattern plan
    /// runs per row and binds a hidden list of projected values.
    fn extract_pattern_comprehension(
        &mut self,
        path: &PatternPath,
        predicate: &Option<Box<Expr>>,
        projection: &Expr,
        plan: &mut Plan,
        scope: &mut FastMap<String, VarKind>,
    ) -> Result<String> {
        let mut inner = Plan::Empty;
        let mut inner_scope = scope.clone();
        self.plan_path(path, &mut inner, &mut inner_scope)?;
        if let Some(predicate) = predicate {
            let mut predicate = (**predicate).clone();
            self.extract_patterns(
                &mut predicate,
                &mut inner,
                &mut inner_scope,
                PatternContext::Predicate,
            )?;
            validate_expr(&predicate, &inner_scope)?;
            inner = Plan::Filter {
                input: Box::new(inner),
                predicate,
            };
        }
        let mut projection = projection.clone();
        self.extract_patterns(
            &mut projection,
            &mut inner,
            &mut inner_scope,
            PatternContext::Value,
        )?;
        validate_expr(&projection, &inner_scope)?;
        let hidden = self.anon_name("comp");
        scope.insert(hidden.clone(), VarKind::Value);
        *plan = Plan::PatternComprehension {
            input: Box::new(std::mem::replace(plan, Plan::Empty)),
            inner: Box::new(inner),
            project: projection,
            var: hidden.clone(),
        };
        Ok(hidden)
    }

    /// Plan `shortestPath`/`allShortestPaths`: a single variable-length
    /// relationship searched breadth-first from a bound endpoint (or from
    /// every scanned start node when neither endpoint is bound).
    fn plan_shortest(
        &mut self,
        path: &PatternPath,
        all: bool,
        bind_path: Option<String>,
        require_bound: bool,
        plan: &mut Plan,
        scope: &mut FastMap<String, VarKind>,
    ) -> Result<()> {
        check_path_conflicts(path)?;
        let segment = match path.segments.as_slice() {
            [segment] => segment,
            _ => {
                return Err(Error::syntax(
                    "shortestPath requires exactly one relationship pattern",
                ));
            }
        };
        let rel = &segment.relationship;
        let Some(bounds) = &rel.var_length else {
            return Err(Error::syntax(
                "shortestPath requires a variable-length relationship pattern",
            ));
        };
        let (props, prop_exprs) =
            self.pattern_props(rel.properties.as_ref(), "relationship", scope)?;
        if !prop_exprs.is_empty() {
            return Err(Error::unsupported(
                "shortestPath requires literal property filters",
            ));
        }
        let start = self.planned_node(&path.start, scope)?;
        let end = self.planned_node(&segment.node, scope)?;
        let src_bound = scope.contains_key(&start.var);
        let dst_bound = scope.contains_key(&end.var);
        if require_bound {
            // Expression form: both endpoints must already be bound.
            if !src_bound {
                return Err(undefined_variable(&start.var));
            }
            if !dst_bound {
                return Err(undefined_variable(&end.var));
            }
        }
        // Search from a bound endpoint when there is one so bound variables
        // keep their values; the path value stays in the written order.
        let (src_node, dst_node, direction) = if !src_bound && dst_bound {
            (&end, &start, flip(rel.direction))
        } else {
            (&start, &end, rel.direction)
        };
        if !scope.contains_key(&src_node.var) {
            let (label, seek, scan_props) = self.anchor_plan(src_node)?;
            *plan = Plan::NodeScan {
                input: Box::new(plan.clone()),
                var: src_node.var.clone(),
                label,
                seek,
                props: scan_props,
            };
            scope.insert(src_node.var.clone(), VarKind::Node);
            if let Some(predicate) = node_constraints(src_node, true) {
                *plan = Plan::Filter {
                    input: Box::new(plan.clone()),
                    predicate,
                };
            }
        } else if let Some(predicate) = node_constraints(src_node, false) {
            *plan = Plan::Filter {
                input: Box::new(plan.clone()),
                predicate,
            };
        }
        if !scope.contains_key(&dst_node.var) {
            scope.insert(dst_node.var.clone(), VarKind::Node);
        }
        let edge_var = match &rel.variable {
            Some(var) => {
                if scope.contains_key(var) {
                    return Err(Error::syntax(format!("variable `{var}` is already bound")));
                }
                scope.insert(var.clone(), VarKind::Edge);
                self.pattern_rels.insert(var.clone());
                var.clone()
            }
            None => self.anon_name("rel"),
        };
        let shortest_nodes_var = bind_path.as_ref().map(|_| self.anon_name("nodes"));
        *plan = Plan::ShortestPath {
            input: Box::new(plan.clone()),
            src: src_node.var.clone(),
            dst: dst_node.var.clone(),
            edge: Some(edge_var.clone()),
            direction,
            types: rel.types.clone(),
            props: props
                .into_iter()
                .map(|(name, _, value)| (name, value))
                .collect(),
            min: bounds.min,
            max: bounds.max,
            all,
            nodes: shortest_nodes_var.clone(),
        };
        // The search binds the target; labels and properties it does not
        // enforce become a post-filter.
        if let Some(predicate) = node_constraints(dst_node, false) {
            *plan = Plan::Filter {
                input: Box::new(plan.clone()),
                predicate,
            };
        }
        if let Some(path_var) = bind_path {
            if scope.contains_key(&path_var) {
                return Err(Error::syntax(format!(
                    "variable `{path_var}` is already bound"
                )));
            }
            let elements = vec![
                PathElement::Value(start.var.clone()),
                PathElement::Segment {
                    edges: edge_var.clone(),
                    nodes: shortest_nodes_var.clone().expect("shortest path node list"),
                },
            ];
            scope.insert(path_var.clone(), VarKind::Path);
            *plan = Plan::BindPath {
                input: Box::new(plan.clone()),
                var: path_var,
                elements,
            };
        }
        Ok(())
    }

    fn plan_path(
        &mut self,
        path: &PatternPath,
        plan: &mut Plan,
        scope: &mut FastMap<String, VarKind>,
    ) -> Result<()> {
        check_path_conflicts(path)?;
        if path.shortest != Shortest::Plain {
            let all = path.shortest == Shortest::AllShortest;
            return self.plan_shortest(path, all, path.variable.clone(), false, plan, scope);
        }
        let mut nodes: Vec<PlannedNode> = Vec::new();
        nodes.push(self.planned_node(&path.start, scope)?);
        struct RelInfo {
            index: usize,
            var: String,
            types: Vec<String>,
            direction: RelDirection,
            props: Vec<(String, Value)>,
            prop_exprs: Vec<(String, Expr)>,
            var_length: Option<crate::cypher::VarLength>,
            /// Hidden variable for the traversal's node list, when a named
            /// path needs the intermediate nodes.
            nodes_var: Option<String>,
        }
        let mut rels: Vec<RelInfo> = Vec::new();
        for segment in &path.segments {
            let rel = &segment.relationship;
            let node = self.planned_node(&segment.node, scope)?;
            nodes.push(node);
            let var = match &rel.variable {
                Some(var) => var.clone(),
                None => self.anon_name("rel"),
            };
            let types = rel.types.clone();
            let (literals, prop_exprs) =
                self.pattern_props(rel.properties.as_ref(), "relationship", scope)?;
            if rel.var_length.is_some() && !prop_exprs.is_empty() {
                return Err(Error::unsupported(
                    "variable-length relationship patterns require literal property filters",
                ));
            }
            rels.push(RelInfo {
                index: nodes.len() - 2,
                var,
                types,
                direction: rel.direction,
                props: literals
                    .into_iter()
                    .map(|(name, _, value)| (name, value))
                    .collect(),
                prop_exprs,
                var_length: rel.var_length,
                nodes_var: if rel.var_length.is_some() && path.variable.is_some() {
                    Some(self.anon_name("nodes"))
                } else {
                    None
                },
            });
        }

        let mut bound: Vec<bool> = nodes
            .iter()
            .map(|node| scope.contains_key(&node.var))
            .collect();
        let mut scanned = vec![false; nodes.len()];
        let mut handled = vec![false; rels.len()];
        // Relationship variables bound by this pattern; openCypher requires
        // every relationship in a pattern to be distinct.
        let mut distinct_edges: Vec<String> = Vec::new();
        // Variable-length segments: their edge lists must not overlap with
        // any other relationship of the pattern either.
        let mut segments: Vec<String> = Vec::new();
        loop {
            if bound.iter().all(|b| *b) && handled.iter().all(|h| *h) {
                break;
            }
            // Expand from a bound endpoint into an unbound one.
            let mut progressed = false;
            for (idx, rel) in rels.iter().enumerate() {
                if handled[idx] {
                    continue;
                }
                let (from, to) = (rel.index, rel.index + 1);
                let (direction, reverse) = if bound[from] && !bound[to] {
                    (Some((from, to, rel.direction)), false)
                } else if bound[to] && !bound[from] {
                    (Some((to, from, flip(rel.direction))), true)
                } else {
                    (None, false)
                };
                let Some((from_idx, to_idx, direction)) = direction else {
                    continue;
                };
                let edge_bound =
                    self.bound_relationship(&rel.var, rel.var_length.is_some(), scope)?;
                let edge_var = if edge_bound {
                    self.anon_name("rel")
                } else {
                    // A variable-length binding is a list of relationships.
                    scope.insert(
                        rel.var.clone(),
                        if rel.var_length.is_some() {
                            VarKind::EdgeList
                        } else {
                            VarKind::Edge
                        },
                    );
                    self.pattern_rels.insert(rel.var.clone());
                    rel.var.clone()
                };
                *plan = match rel.var_length {
                    Some(bounds) => Plan::VarExpand {
                        input: Box::new(plan.clone()),
                        src: nodes[from_idx].var.clone(),
                        edge: Some(edge_var.clone()),
                        dst: nodes[to_idx].var.clone(),
                        direction,
                        types: rel.types.clone(),
                        props: rel.props.clone(),
                        min: bounds.min,
                        max: bounds.max,
                        reverse,
                        nodes: rel.nodes_var.clone(),
                    },
                    None => Plan::Expand {
                        input: Box::new(plan.clone()),
                        src: nodes[from_idx].var.clone(),
                        edge: Some(edge_var.clone()),
                        dst: nodes[to_idx].var.clone(),
                        direction,
                        types: rel.types.clone(),
                        props: rel.props.clone(),
                    },
                };
                constrain_edge(plan, &edge_var, &rel.var, edge_bound);
                constrain_edge_props(plan, &edge_var, &rel.prop_exprs);
                scope.insert(nodes[to_idx].var.clone(), VarKind::Node);
                if rel.var_length.is_none() {
                    distinct_edges.push(edge_var.clone());
                } else {
                    segments.push(edge_var.clone());
                }
                // Binding the endpoint may bind a repeated variable used by
                // other pattern elements as well.
                for (index, node) in nodes.iter().enumerate() {
                    if scope.contains_key(&node.var) {
                        bound[index] = true;
                    }
                }
                handled[idx] = true;
                progressed = true;
                break;
            }
            if progressed {
                continue;
            }
            // Both endpoints are bound: verify the relationship exists by
            // expanding one endpoint and matching the other.
            for (idx, rel) in rels.iter().enumerate() {
                if handled[idx] {
                    continue;
                }
                let (from, to) = (rel.index, rel.index + 1);
                if !(bound[from] && bound[to]) {
                    continue;
                }
                let fresh_node = self.anon_name("node");
                let edge_bound =
                    self.bound_relationship(&rel.var, rel.var_length.is_some(), scope)?;
                let edge_var = if edge_bound {
                    self.anon_name("rel")
                } else {
                    // A variable-length binding is a list of relationships.
                    scope.insert(
                        rel.var.clone(),
                        if rel.var_length.is_some() {
                            VarKind::EdgeList
                        } else {
                            VarKind::Edge
                        },
                    );
                    self.pattern_rels.insert(rel.var.clone());
                    rel.var.clone()
                };
                *plan = match rel.var_length {
                    Some(bounds) => Plan::VarExpand {
                        input: Box::new(plan.clone()),
                        src: nodes[from].var.clone(),
                        edge: Some(edge_var.clone()),
                        dst: fresh_node.clone(),
                        direction: rel.direction,
                        types: rel.types.clone(),
                        props: rel.props.clone(),
                        min: bounds.min,
                        max: bounds.max,
                        reverse: false,
                        nodes: rel.nodes_var.clone(),
                    },
                    None => Plan::Expand {
                        input: Box::new(plan.clone()),
                        src: nodes[from].var.clone(),
                        edge: Some(edge_var.clone()),
                        dst: fresh_node.clone(),
                        direction: rel.direction,
                        types: rel.types.clone(),
                        props: rel.props.clone(),
                    },
                };
                scope.insert(fresh_node.clone(), VarKind::Node);
                *plan = Plan::Filter {
                    input: Box::new(plan.clone()),
                    predicate: eq_expr(
                        Expr::Variable(fresh_node),
                        Expr::Variable(nodes[to].var.clone()),
                    ),
                };
                constrain_edge(plan, &edge_var, &rel.var, edge_bound);
                constrain_edge_props(plan, &edge_var, &rel.prop_exprs);
                if rel.var_length.is_none() {
                    distinct_edges.push(edge_var.clone());
                } else {
                    segments.push(edge_var.clone());
                }
                handled[idx] = true;
                progressed = true;
                break;
            }
            if progressed {
                continue;
            }
            // New anchor: choose the most selective unbound node.
            let idx = self.best_anchor(&nodes, &bound);
            let node = &nodes[idx];
            let (label, seek, props) = self.anchor_plan(node)?;
            if !scope.contains_key(&node.var) {
                scope.insert(node.var.clone(), VarKind::Node);
            }
            *plan = Plan::NodeScan {
                input: Box::new(plan.clone()),
                var: node.var.clone(),
                label,
                seek,
                props,
            };
            bound[idx] = true;
            scanned[idx] = true;
            // The scan may bind a variable that other pattern elements
            // mention; they are bound now too.
            for (index, node) in nodes.iter().enumerate() {
                if scope.contains_key(&node.var) {
                    bound[index] = true;
                }
            }
        }
        // Every relationship in one pattern binds a distinct edge. A
        // variable-length segment is a list, so its relationships must not
        // contain any fixed relationship of the pattern nor overlap another
        // segment's list.
        for left in 0..distinct_edges.len() {
            for right in (left + 1)..distinct_edges.len() {
                *plan = Plan::Filter {
                    input: Box::new(plan.clone()),
                    predicate: Expr::Binary {
                        op: BinaryOp::Ne,
                        left: Box::new(Expr::Variable(distinct_edges[left].clone())),
                        right: Box::new(Expr::Variable(distinct_edges[right].clone())),
                    },
                };
            }
        }
        for segment in &segments {
            for fixed in &distinct_edges {
                *plan = Plan::Filter {
                    input: Box::new(plan.clone()),
                    predicate: Expr::Unary {
                        op: UnaryOp::Not,
                        expr: Box::new(Expr::Binary {
                            op: BinaryOp::In,
                            left: Box::new(Expr::Variable(fixed.clone())),
                            right: Box::new(Expr::Variable(segment.clone())),
                        }),
                    },
                };
            }
        }
        for left in 0..segments.len() {
            for right in (left + 1)..segments.len() {
                let element = self.anon_name("element");
                *plan = Plan::Filter {
                    input: Box::new(plan.clone()),
                    predicate: Expr::Unary {
                        op: UnaryOp::Not,
                        expr: Box::new(Expr::Quantifier {
                            kind: QuantifierKind::Any,
                            var: element.clone(),
                            list: Box::new(Expr::Variable(segments[left].clone())),
                            predicate: Box::new(Expr::Binary {
                                op: BinaryOp::In,
                                left: Box::new(Expr::Variable(element)),
                                right: Box::new(Expr::Variable(segments[right].clone())),
                            }),
                        }),
                    },
                };
            }
        }
        // Constraints a scan did not enforce: every label beyond the first on
        // scanned nodes, and all labels and literal properties on nodes that
        // were bound by the scope or an expansion.
        for (idx, node) in nodes.iter().enumerate() {
            if let Some(predicate) = node_constraints(node, scanned[idx]) {
                *plan = Plan::Filter {
                    input: Box::new(plan.clone()),
                    predicate,
                };
            }
        }
        // `p = (a)-[...]->(b)` binds the alternating node/relationship value.
        if let Some(path_var) = &path.variable {
            if scope.contains_key(path_var) {
                return Err(Error::syntax(format!(
                    "variable `{path_var}` is already bound"
                )));
            }
            let mut elements = vec![PathElement::Value(nodes[0].var.clone())];
            for (idx, rel) in rels.iter().enumerate() {
                match (&rel.var_length, &rel.nodes_var) {
                    (Some(_), Some(nodes_var)) => {
                        // A variable-length segment carries its own node
                        // list; the end node is its last element.
                        elements.push(PathElement::Segment {
                            edges: rel.var.clone(),
                            nodes: nodes_var.clone(),
                        });
                    }
                    _ => {
                        elements.push(PathElement::Value(rel.var.clone()));
                        elements.push(PathElement::Value(nodes[idx + 1].var.clone()));
                    }
                }
            }
            scope.insert(path_var.clone(), VarKind::Path);
            *plan = Plan::BindPath {
                input: Box::new(plan.clone()),
                var: path_var.clone(),
                elements,
            };
        }
        Ok(())
    }

    /// Whether a relationship variable is already bound. A variable carried
    /// through a projection may constrain a pattern; one bound by an earlier
    /// pattern in the same section may not be bound again.
    fn bound_relationship(
        &self,
        var: &str,
        var_length: bool,
        scope: &FastMap<String, VarKind>,
    ) -> Result<bool> {
        let Some(kind) = scope.get(var) else {
            return Ok(false);
        };
        match kind {
            // A value may hold a relationship, or a relationship list when
            // the pattern is variable-length.
            VarKind::Value => return Ok(true),
            // A variable-length binding may serve another variable-length
            // segment (as a list of relationships).
            VarKind::EdgeList if var_length => {}
            VarKind::Edge => {}
            VarKind::EdgeList | VarKind::Node | VarKind::Path | VarKind::Scalar(_) => {
                return Err(Error::syntax(format!(
                    "variable `{var}` is already bound to a {}",
                    if *kind == VarKind::Node {
                        "node"
                    } else if *kind == VarKind::Path {
                        "path"
                    } else if matches!(kind, VarKind::Scalar(_)) {
                        "value"
                    } else {
                        "relationship"
                    }
                )));
            }
        }
        if self.pattern_rels.contains(var) {
            return Err(Error::syntax(format!(
                "variable `{var}` is already bound in a preceding pattern"
            )));
        }
        Ok(true)
    }

    fn best_anchor(&self, nodes: &[PlannedNode], bound: &[bool]) -> usize {
        let mut best = (0usize, i32::MAX);
        for (idx, node) in nodes.iter().enumerate() {
            if bound[idx] {
                continue;
            }
            let mut score = 0i32;
            if self.has_usable_index(node) {
                score -= 100;
            }
            if !node.label_ids.is_empty() {
                score -= 10;
            }
            score += node.props.len() as i32;
            if score < best.1 {
                best = (idx, score);
            }
        }
        best.0
    }

    /// Whether a literal property of this node can be served by an index.
    fn has_usable_index(&self, node: &PlannedNode) -> bool {
        let schema = self.db.schema();
        node.props.iter().any(|(_, key_id, _)| {
            schema.node_indexes.iter().any(|index| {
                index.key_id == *key_id
                    && (index.label_id == 0 || node.label_ids.contains(&index.label_id))
            })
        })
    }

    #[allow(clippy::type_complexity)]
    fn anchor_plan(
        &self,
        node: &PlannedNode,
    ) -> Result<(Option<String>, Option<IndexSeek>, Vec<(String, Value)>)> {
        let schema = self.db.schema();
        // Prefer an index seek on a literal property.
        for (_, key_id, value) in &node.props {
            let index = schema.node_indexes.iter().find(|index| {
                index.key_id == *key_id
                    && (index.label_id == 0 || node.label_ids.contains(&index.label_id))
            });
            if let Some(index) = index {
                let remaining: Vec<(String, Value)> = node
                    .props
                    .iter()
                    .filter(|(_, key, _)| *key != *key_id)
                    .map(|(name, _, value)| (name.clone(), value.clone()))
                    .collect();
                return Ok((
                    node.label_names.first().cloned(),
                    Some(IndexSeek {
                        index_id: index.id,
                        key_id: *key_id,
                        value: value.clone(),
                    }),
                    remaining,
                ));
            }
        }
        Ok((
            node.label_names.first().cloned(),
            None,
            node.props
                .iter()
                .map(|(name, _, value)| (name.clone(), value.clone()))
                .collect(),
        ))
    }

    fn planned_node(
        &mut self,
        pattern: &NodePattern,
        scope: &FastMap<String, VarKind>,
    ) -> Result<PlannedNode> {
        let var = match &pattern.variable {
            Some(var) => {
                if let Some(kind) = scope.get(var)
                    && matches!(
                        kind,
                        VarKind::Edge | VarKind::EdgeList | VarKind::Path | VarKind::Scalar(_)
                    )
                {
                    return Err(Error::syntax(format!(
                        "variable `{var}` is already bound to a {}",
                        if *kind == VarKind::Path {
                            "path"
                        } else if matches!(kind, VarKind::Scalar(_)) {
                            "value"
                        } else {
                            "relationship"
                        }
                    )));
                }
                var.clone()
            }
            None => self.anon_name("node"),
        };
        let mut label_ids = Vec::new();
        for label in &pattern.labels {
            // A missing label yields an impossible id so the scan is empty
            // but the variable is still bound.
            label_ids.push(self.db.label_id(label).unwrap_or(u32::MAX));
        }
        let (props, prop_exprs) = self.pattern_props(pattern.properties.as_ref(), "node", scope)?;
        Ok(PlannedNode {
            var,
            label_ids,
            label_names: pattern.labels.clone(),
            props,
            prop_exprs,
        })
    }

    /// Split pattern properties into literal filters (usable for index seeks
    /// and scans) and runtime expressions (lowered to filters).
    #[allow(clippy::type_complexity)]
    fn pattern_props(
        &self,
        properties: Option<&Expr>,
        what: &str,
        scope: &FastMap<String, VarKind>,
    ) -> Result<(Vec<(String, u32, Value)>, Vec<(String, Expr)>)> {
        let Some(properties) = properties else {
            return Ok((Vec::new(), Vec::new()));
        };
        let Expr::Map(entries) = properties else {
            return Err(Error::unsupported(format!(
                "{what} pattern properties must be a map literal"
            )));
        };
        let mut literals = Vec::new();
        let mut exprs = Vec::new();
        for (key, value) in entries {
            match value {
                Expr::Literal(value) => {
                    if value.is_null() {
                        continue;
                    }
                    let key_id = self.db.property_key_id(key).unwrap_or(NO_KEY);
                    literals.push((key.clone(), key_id, value.clone()));
                }
                value => {
                    validate_expr(value, scope)?;
                    exprs.push((key.clone(), value.clone()));
                }
            }
        }
        Ok((literals, exprs))
    }

    fn anon_name(&mut self, prefix: &str) -> String {
        self.anon_counter += 1;
        format!("  {prefix}{}", self.anon_counter)
    }

    // ---- CREATE / MERGE ----

    fn plan_create(
        &mut self,
        patterns: &[PatternPath],
        scope: &FastMap<String, VarKind>,
    ) -> Result<Vec<CreateItem>> {
        let mut items = Vec::new();
        let mut local_scope: FastMap<String, VarKind> = scope.clone();
        for path in patterns {
            let start =
                self.create_node_item(&path.start, &local_scope, !path.segments.is_empty())?;
            if let Some(item) = &start {
                local_scope.insert(item.var().to_owned(), VarKind::Node);
                items.push(item.clone());
            }
            let mut prev_var = match &start {
                Some(item) => item.var().to_owned(),
                None => path
                    .start
                    .variable
                    .clone()
                    .ok_or_else(|| Error::internal("anonymous bound node"))?,
            };
            for segment in &path.segments {
                let node_item = self.create_node_item(&segment.node, &local_scope, true)?;
                if let Some(item) = &node_item {
                    local_scope.insert(item.var().to_owned(), VarKind::Node);
                    items.push(item.clone());
                }
                let next_var = match &node_item {
                    Some(item) => item.var().to_owned(),
                    None => segment
                        .node
                        .variable
                        .clone()
                        .ok_or_else(|| Error::internal("anonymous bound node"))?,
                };
                let rel = &segment.relationship;
                if rel.direction == RelDirection::Undirected {
                    return Err(Error::syntax("CREATE requires a directed relationship"));
                }
                if rel.types.len() != 1 {
                    return Err(Error::syntax(
                        "relationships in CREATE must have exactly one type",
                    ));
                }
                if rel.var_length.is_some() {
                    return Err(Error::syntax(
                        "CreatingVarLength: variable-length relationships cannot be created",
                    ));
                }
                let var = match &rel.variable {
                    Some(var) => var.clone(),
                    None => self.anon_name("rel"),
                };
                let (props, prop_exprs) =
                    self.create_props(rel.properties.as_ref(), &local_scope)?;
                local_scope.insert(var.clone(), VarKind::Edge);
                // A left-pointing pattern (`(a)<-[:T]-(b)`) swaps the
                // endpoints: the relationship still runs from `b` to `a`.
                let (src, dst) = match rel.direction {
                    RelDirection::Incoming => (next_var.clone(), prev_var),
                    RelDirection::Outgoing | RelDirection::Undirected => {
                        (prev_var, next_var.clone())
                    }
                };
                items.push(CreateItem::Edge {
                    var,
                    edge_type: rel.types[0].clone(),
                    src,
                    dst,
                    props,
                    prop_exprs,
                });
                prev_var = next_var;
            }
        }
        Ok(items)
    }

    fn create_node_item(
        &mut self,
        pattern: &NodePattern,
        scope: &FastMap<String, VarKind>,
        attached: bool,
    ) -> Result<Option<CreateItem>> {
        let var = match &pattern.variable {
            Some(var) if scope.contains_key(var) => {
                // An already-bound variable may anchor a relationship, but
                // cannot gain labels or properties, nor stand alone.
                if !pattern.labels.is_empty() || pattern.properties.is_some() {
                    return Err(Error::syntax(format!("variable `{var}` is already bound")));
                }
                if !attached {
                    return Err(Error::syntax(format!("variable `{var}` is already bound")));
                }
                return Ok(None);
            }
            Some(var) => var.clone(),
            None => self.anon_name("node"),
        };
        let (props, prop_exprs) = self.create_props(pattern.properties.as_ref(), scope)?;
        Ok(Some(CreateItem::Node {
            var,
            labels: pattern.labels.clone(),
            props,
            prop_exprs,
        }))
    }

    #[allow(clippy::type_complexity)]
    fn create_props(
        &self,
        properties: Option<&Expr>,
        scope: &FastMap<String, VarKind>,
    ) -> Result<(Vec<(String, Value)>, Vec<(String, Expr)>)> {
        let Some(properties) = properties else {
            return Ok((Vec::new(), Vec::new()));
        };
        let Expr::Map(entries) = properties else {
            return Err(Error::syntax("properties must be a map literal"));
        };
        let mut literals = Vec::new();
        let mut exprs = Vec::new();
        for (key, value) in entries {
            if let Expr::Literal(value) = value {
                // Null literals are kept so MERGE can reject them; creating
                // simply drops them.
                literals.push((key.clone(), value.clone()));
            } else {
                validate_expr(value, scope)?;
                exprs.push((key.clone(), value.clone()));
            }
        }
        Ok((literals, exprs))
    }

    /// Plan `MERGE`: the whole pattern is matched first (one row per match);
    /// when nothing matches, the unbound elements are created and the
    /// `ON CREATE` items apply.
    fn plan_merge(
        &mut self,
        path: &PatternPath,
        on_create: &[SetItem],
        on_match: &[SetItem],
        plan: &mut Plan,
        scope: &mut FastMap<String, VarKind>,
    ) -> Result<()> {
        if path
            .segments
            .iter()
            .any(|segment| segment.relationship.var_length.is_some())
        {
            return Err(Error::syntax(
                "variable-length relationships cannot be merged",
            ));
        }
        for node in std::iter::once(&path.start).chain(path.segments.iter().map(|s| &s.node)) {
            if let Some(var) = &node.variable
                && scope.contains_key(var)
                && !node.labels.is_empty()
            {
                return Err(Error::syntax(format!(
                    "MERGE cannot add labels to the bound variable `{var}`"
                )));
            }
        }
        for segment in &path.segments {
            let rel = &segment.relationship;
            if rel.types.len() != 1 {
                return Err(Error::syntax(
                    "relationships in MERGE must have exactly one type",
                ));
            }
            if let Some(var) = &rel.variable
                && scope.contains_key(var)
            {
                return Err(Error::syntax(format!(
                    "MERGE cannot re-bind the relationship variable `{var}`"
                )));
            }
        }

        // Create items for the unbound elements, in creation order, while
        // recording the variable each path element uses.
        let mut items: Vec<CreateItem> = Vec::new();
        let mut path_vars: Vec<String> = Vec::new();
        let mut local = scope.clone();
        let start_item = self.create_node_item(&path.start, &local, true)?;
        if path.segments.is_empty() && start_item.is_none() {
            let name = path.start.variable.as_deref().unwrap_or("?");
            return Err(Error::syntax(format!(
                "variable `{name}` is already bound and cannot be merged"
            )));
        }
        let start_var = match &start_item {
            Some(item) => {
                local.insert(item.var().to_owned(), VarKind::Node);
                item.var().to_owned()
            }
            None => path
                .start
                .variable
                .clone()
                .ok_or_else(|| Error::syntax("MERGE requires a variable or a new node"))?,
        };
        path_vars.push(start_var.clone());
        if let Some(item) = start_item {
            items.push(item);
        }
        let mut src = start_var;
        for segment in &path.segments {
            let rel = &segment.relationship;
            let node_item = self.create_node_item(&segment.node, &local, true)?;
            let node_var = match &node_item {
                Some(item) => {
                    local.insert(item.var().to_owned(), VarKind::Node);
                    item.var().to_owned()
                }
                None => segment
                    .node
                    .variable
                    .clone()
                    .ok_or_else(|| Error::syntax("MERGE requires a variable or a new node"))?,
            };
            if let Some(item) = node_item {
                items.push(item);
            }
            let var = match &rel.variable {
                Some(var) => var.clone(),
                None => self.anon_name("rel"),
            };
            let (from, to) = match rel.direction {
                RelDirection::Incoming => (node_var.clone(), src),
                RelDirection::Outgoing | RelDirection::Undirected => (src, node_var.clone()),
            };
            let (props, prop_exprs) = self.create_props(rel.properties.as_ref(), scope)?;
            items.push(CreateItem::Edge {
                var: var.clone(),
                edge_type: rel.types[0].clone(),
                src: from,
                dst: to,
                props,
                prop_exprs,
            });
            path_vars.push(var);
            path_vars.push(node_var.clone());
            src = node_var;
        }

        // The match plan uses the same variable names, with the pattern's
        // unbound elements still unbound.
        let mut match_pattern = path.clone();
        match_pattern.start.variable = Some(path_vars[0].clone());
        let mut index = 1;
        for segment in &mut match_pattern.segments {
            segment.relationship.variable = Some(path_vars[index].clone());
            segment.node.variable = Some(path_vars[index + 1].clone());
            index += 2;
        }
        let mut match_plan = Plan::Empty;
        let mut match_scope = scope.clone();
        self.plan_path(&match_pattern, &mut match_plan, &mut match_scope)?;

        // Created elements are bound from here on.
        for item in &items {
            let kind = match item {
                CreateItem::Node { .. } => VarKind::Node,
                CreateItem::Edge { .. } => VarKind::Edge,
            };
            scope.insert(item.var().to_owned(), kind);
        }
        for set_item in on_create.iter().chain(on_match) {
            self.validate_set_item(set_item, scope)?;
        }

        let path_binding = path.variable.as_ref().map(|var| {
            let mut elements = Vec::with_capacity(path_vars.len());
            for element in &path_vars {
                elements.push((element.clone(), false));
            }
            (var.clone(), elements)
        });
        if let Some(var) = &path.variable {
            path_vars.push(var.clone());
            scope.insert(var.clone(), VarKind::Path);
        }
        *plan = Plan::Merge {
            input: Box::new(plan.clone()),
            match_plan: Box::new(match_plan),
            items,
            path: path_binding,
            path_vars,
            on_create: on_create.to_vec(),
            on_match: on_match.to_vec(),
        };
        Ok(())
    }

    fn validate_set_item(&self, item: &SetItem, scope: &FastMap<String, VarKind>) -> Result<()> {
        match item {
            SetItem::Property {
                variable,
                value,
                key: _,
            } => {
                if !scope.contains_key(variable) {
                    return Err(undefined_variable(variable));
                }
                validate_expr(value, scope)
            }
            SetItem::Replace { variable, value } => {
                if !scope.contains_key(variable) {
                    return Err(undefined_variable(variable));
                }
                validate_expr(value, scope)
            }
            SetItem::Append { variable, value } => {
                if !scope.contains_key(variable) {
                    return Err(undefined_variable(variable));
                }
                validate_expr(value, scope)
            }
            SetItem::Label { variable, .. } => {
                if !scope.contains_key(variable) {
                    return Err(undefined_variable(variable));
                }
                Ok(())
            }
        }
    }

    // ---- projection ----

    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::too_many_arguments)]
    fn plan_projection(
        &mut self,
        input: Plan,
        items: &[ReturnItem],
        distinct: bool,
        order: &[SortItem],
        skip: &Option<Expr>,
        limit: &Option<Expr>,
        where_clause: Option<&Expr>,
        scope: &FastMap<String, VarKind>,
        require_aliases: bool,
    ) -> Result<(Plan, FastMap<String, VarKind>, Vec<String>)> {
        // Expand `*` items; the TCK pins the column order to the sorted
        // variable names.
        let mut expanded: Vec<(Expr, String)> = Vec::new();
        for item in items {
            if item.star {
                let mut stars: Vec<&String> =
                    scope.keys().filter(|var| !var.starts_with("  ")).collect();
                stars.sort();
                for var in stars {
                    expanded.push((Expr::Variable(var.clone()), var.clone()));
                }
            } else {
                if require_aliases
                    && item.alias.is_none()
                    && !matches!(item.expr, Expr::Variable(_))
                {
                    return Err(Error::syntax(
                        "unaliased expression in WITH; use AS to name it",
                    ));
                }
                let name = match &item.alias {
                    Some(alias) => alias.clone(),
                    // The column keeps the expression exactly as written.
                    None => item
                        .text
                        .clone()
                        .unwrap_or_else(|| default_name(&item.expr)),
                };
                expanded.push((item.expr.clone(), name));
            }
        }
        if expanded.is_empty() {
            // `WITH *` over an empty scope is a legal no-op; `RETURN *`
            // without variables is an error.
            let all_stars = items.iter().all(|item| item.star);
            if all_stars && require_aliases && order.is_empty() && skip.is_none() && limit.is_none()
            {
                return Ok((input, FastMap::new(), Vec::new()));
            }
            return Err(Error::syntax("RETURN/WITH requires at least one item"));
        }
        let mut seen_names: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for (_, name) in &expanded {
            if !seen_names.insert(name.as_str()) {
                return Err(Error::syntax(format!(
                    "MultipleResultsWithSameName: `{name}` is projected more than once"
                )));
            }
        }
        let mut input = input;
        let mut scope = scope.clone();
        for (expr, _) in expanded.iter_mut() {
            self.extract_patterns(expr, &mut input, &mut scope, PatternContext::Value)?;
        }
        for (expr, _) in &expanded {
            validate_expr(expr, &scope)?;
        }

        // A `WITH ... WHERE` may reference variables of the incoming scope
        // (not only the projected aliases); those predicates filter before
        // the projection. Aliases take precedence when they resolve.
        let mut filter_after = None;
        if let Some(predicate) = where_clause {
            let projected: FastMap<String, VarKind> = expanded
                .iter()
                .map(|(expr, name)| (name.clone(), expr_kind(expr, &scope)))
                .collect();
            if validate_expr(predicate, &projected).is_ok() {
                check_predicate_kind(predicate, &projected)?;
                filter_after = Some(predicate.clone());
            } else if let Ok(rewritten) = rewrite_expr(predicate, &mut |sub| {
                // Replace projected aliases by their defining expressions so
                // the predicate can filter before the projection; this is how
                // a WHERE sees variables bound before and after the WITH.
                let Expr::Variable(name) = sub else {
                    return Ok(None);
                };
                match expanded.iter().find(|(_, item)| item == name) {
                    Some((expr, _)) if !contains_aggregation(expr) => Ok(Some(expr.clone())),
                    _ => Ok(None),
                }
            }) && !contains_aggregation(&rewritten)
                && validate_expr(&rewritten, &scope).is_ok()
            {
                check_predicate_kind(&rewritten, &scope)?;
                self.extract_patterns(
                    &mut rewritten.clone(),
                    &mut input,
                    &mut scope,
                    PatternContext::Predicate,
                )?;
                input = Plan::Filter {
                    input: Box::new(input),
                    predicate: rewritten,
                };
            } else {
                let mut predicate = predicate.clone();
                self.extract_patterns(
                    &mut predicate,
                    &mut input,
                    &mut scope,
                    PatternContext::Predicate,
                )?;
                validate_expr(&predicate, &scope)?;
                check_predicate_kind(&predicate, &scope)?;
                input = Plan::Filter {
                    input: Box::new(input),
                    predicate,
                };
            }
        }

        // Partition the projection items: plain expressions become grouping
        // keys, plain aggregation calls become aggregations, and expressions
        // containing aggregations are evaluated after aggregation over
        // references to the grouping keys and aggregations.
        let mut groups: Vec<(Expr, String)> = Vec::new();
        let mut aggs: Vec<AggItem> = Vec::new();
        let mut post: Vec<(Expr, String)> = Vec::new();
        // Plain items are grouping keys even when an aggregate expression
        // that references them appears first.
        for (expr, name) in &expanded {
            if !contains_aggregation(expr) {
                groups.push((expr.clone(), name.clone()));
            }
        }
        for (expr, name) in &expanded {
            if has_aggregation_inside_comprehension(expr) {
                return Err(Error::syntax(
                    "aggregations are not allowed inside list comprehensions",
                ));
            }
            if let Some((func, arguments, star, distinct)) = aggregate_call(expr)? {
                check_aggregate_arguments(&arguments)?;
                aggs.push(AggItem {
                    func,
                    arguments: aggregate_arguments(func, &arguments, star, &scope)?,
                    distinct,
                    name: name.clone(),
                });
            } else if contains_aggregation(expr) {
                let mut residual = rewrite_expr(expr, &mut |sub| {
                    let Some((func, arguments, star, distinct)) = aggregate_call(sub)? else {
                        return Ok(None);
                    };
                    check_aggregate_arguments(&arguments)?;
                    let hidden = format!("  agg{}", aggs.len());
                    aggs.push(AggItem {
                        func,
                        arguments: aggregate_arguments(func, &arguments, star, &scope)?,
                        distinct,
                        name: hidden.clone(),
                    });
                    Ok(Some(Expr::Variable(hidden)))
                })?;
                residual = hoist_groups(&residual, &mut groups, &aggs, &scope)?;
                // Everything left must be a grouping key, an aggregation or
                // a constant; anything else has no value to take.
                if has_free_variable(&residual, &groups, &aggs) {
                    return Err(Error::syntax(
                        "AmbiguousAggregationExpression: an expression with an aggregation may \
                         only reference projected values",
                    ));
                }
                post.push((residual, name.clone()));
            }
        }
        // Post-aggregation expressions may only reference grouping keys,
        // aggregations and constants.
        if !post.is_empty() {
            let mut post_scope: FastMap<String, VarKind> = FastMap::new();
            for (_, group) in &groups {
                post_scope.insert(group.clone(), VarKind::Value);
            }
            for item in &aggs {
                post_scope.insert(item.name.clone(), VarKind::Value);
            }
            for (residual, _) in &post {
                validate_expr(residual, &post_scope)?;
            }
        }

        // Resolve ORDER BY keys before building the projection: keys that do
        // not reference projected values become hidden projection columns.
        let mut hidden_keys: Vec<(Expr, String)> = Vec::new();
        let mut sort_keys: Vec<(Expr, bool)> = Vec::new();
        if !order.is_empty() {
            let visible_names: FastMap<String, VarKind> = expanded
                .iter()
                .map(|(expr, name)| (name.clone(), expr_kind(expr, &scope)))
                .collect();
            let mut agg_scope: FastMap<String, VarKind> = FastMap::new();
            if !aggs.is_empty() {
                for (_, group) in &groups {
                    agg_scope.insert(group.clone(), VarKind::Value);
                }
                for item in &aggs {
                    agg_scope.insert(item.name.clone(), VarKind::Value);
                }
                for (_, name) in &post {
                    agg_scope.insert(name.clone(), VarKind::Value);
                }
            }
            for key in order {
                let named = match &key.expr {
                    Expr::Variable(name) if expanded.iter().any(|(_, item)| item == name) => {
                        Some(key.expr.clone())
                    }
                    _ => None,
                };
                let expr = if let Some(expr) = named {
                    expr
                } else if let Some((_, alias)) = expanded.iter().find(|(expr, _)| expr == &key.expr)
                {
                    Expr::Variable(alias.clone())
                } else if aggs.is_empty() {
                    if contains_aggregation(&key.expr) {
                        return Err(Error::syntax(
                            "InvalidAggregation: ORDER BY cannot aggregate when RETURN/WITH does not",
                        ));
                    }
                    if validate_expr(&key.expr, &visible_names).is_ok() {
                        key.expr.clone()
                    } else if !distinct {
                        validate_expr(&key.expr, &scope)?;
                        let hidden = format!("  sort{}", hidden_keys.len());
                        hidden_keys.push((key.expr.clone(), hidden.clone()));
                        Expr::Variable(hidden)
                    } else {
                        return Err(Error::syntax(
                            "ORDER BY cannot sort on values removed by DISTINCT",
                        ));
                    }
                } else {
                    let rewritten = rewrite_sort_key(&key.expr, &groups, &aggs)?;
                    validate_expr(&rewritten, &agg_scope)?;
                    rewritten
                };
                sort_keys.push((expr, key.ascending));
            }
        }

        let mut projection_items = groups.clone();
        projection_items.extend(hidden_keys.clone());

        let mut plan;
        let mut new_scope: FastMap<String, VarKind> = FastMap::new();
        if aggs.is_empty() {
            plan = Plan::Project {
                input: Box::new(input),
                items: projection_items.clone(),
            };
            for (expr, name) in &projection_items {
                new_scope.insert(name.clone(), alias_kind(expr, &scope));
            }
        } else {
            plan = Plan::Aggregate {
                input: Box::new(input),
                groups: groups.clone(),
                aggs: aggs.clone(),
            };
            for (_, name) in &groups {
                new_scope.insert(name.clone(), VarKind::Value);
            }
            for item in &aggs {
                new_scope.insert(item.name.clone(), VarKind::Value);
            }
            // The aggregate window holds groups before aggregations; project
            // the items in written order when that differs or when a
            // post-aggregation expression has to be evaluated.
            let window_names: Vec<&str> = groups
                .iter()
                .map(|(_, name)| name.as_str())
                .chain(aggs.iter().map(|item| item.name.as_str()))
                .collect();
            let item_names: Vec<&str> = expanded.iter().map(|(_, name)| name.as_str()).collect();
            if window_names != item_names || !post.is_empty() {
                let items: Vec<(Expr, String)> = expanded
                    .iter()
                    .map(|(expr, name)| {
                        let rewritten = if let Some((residual, _)) =
                            post.iter().find(|(_, item_name)| item_name == name)
                        {
                            residual.clone()
                        } else if groups.iter().any(|(_, group)| group == name)
                            || aggs.iter().any(|item| &item.name == name)
                        {
                            Expr::Variable(name.clone())
                        } else {
                            expr.clone()
                        };
                        (rewritten, name.clone())
                    })
                    .collect();
                plan = Plan::Project {
                    input: Box::new(plan),
                    items,
                };
            }
            for (_, name) in &post {
                new_scope.insert(name.clone(), VarKind::Value);
            }
        }

        if distinct {
            plan = Plan::Distinct {
                input: Box::new(plan),
            };
        }
        if !sort_keys.is_empty() {
            plan = Plan::Sort {
                input: Box::new(plan),
                keys: sort_keys,
            };
        }
        if let Some(skip) = skip {
            validate_projection_limit(skip, "SKIP")?;
            plan = Plan::Skip {
                input: Box::new(plan),
                count: skip.clone(),
            };
        }
        if let Some(limit) = limit {
            validate_projection_limit(limit, "LIMIT")?;
            plan = Plan::Limit {
                input: Box::new(plan),
                count: limit.clone(),
            };
        }
        if !hidden_keys.is_empty() {
            // Trim the hidden sort columns from the output window.
            let visible: Vec<(Expr, String)> = expanded
                .iter()
                .map(|(_, name)| (Expr::Variable(name.clone()), name.clone()))
                .collect();
            plan = Plan::Project {
                input: Box::new(plan),
                items: visible,
            };
        }
        if let Some(predicate) = filter_after {
            plan = Plan::Filter {
                input: Box::new(plan),
                predicate,
            };
        }

        let columns: Vec<String> = expanded.iter().map(|(_, name)| name.clone()).collect();
        // Output scope maps names to output registers; nodes keep their kind.
        let mut output_scope = FastMap::new();
        for (expr, name) in &expanded {
            output_scope.insert(name.clone(), alias_kind(expr, &scope));
        }
        let _ = std::mem::replace(&mut new_scope, output_scope);
        Ok((plan, new_scope, columns))
    }
}

/// A node pattern with resolved names.
struct PlannedNode {
    var: String,
    label_ids: Vec<u32>,
    label_names: Vec<String>,
    /// Literal properties as `(name, key id, value)`.
    props: Vec<(String, u32, Value)>,
    prop_exprs: Vec<(String, Expr)>,
}

/// `expr = expr`, used to constrain expansions to pre-bound elements.
fn eq_expr(left: Expr, right: Expr) -> Expr {
    Expr::Binary {
        op: BinaryOp::Eq,
        left: Box::new(left),
        right: Box::new(right),
    }
}

/// When the pattern's relationship variable was already bound, the expansion
/// binds a fresh variable; constrain it to the bound one.
/// Lower non-literal relationship property values to runtime filters.
fn constrain_edge_props(plan: &mut Plan, edge_var: &str, props: &[(String, Expr)]) {
    for (key, value) in props {
        *plan = Plan::Filter {
            input: Box::new(plan.clone()),
            predicate: eq_expr(
                Expr::Property {
                    base: Box::new(Expr::Variable(edge_var.to_owned())),
                    key: key.clone(),
                },
                value.clone(),
            ),
        };
    }
}

fn constrain_edge(plan: &mut Plan, edge_var: &str, rel_var: &str, edge_bound: bool) {
    if !edge_bound {
        return;
    }
    *plan = Plan::Filter {
        input: Box::new(plan.clone()),
        predicate: eq_expr(
            Expr::Variable(edge_var.to_owned()),
            Expr::Variable(rel_var.to_owned()),
        ),
    };
}

/// Predicates for node constraints a scan did not apply. Scans enforce the
/// first label and all literal properties, so only the remaining labels are
/// left; nodes bound by the scope or an expansion need everything.
fn node_constraints(node: &PlannedNode, scanned: bool) -> Option<Expr> {
    let mut predicates: Vec<Expr> = Vec::new();
    let labels: &[String] = if scanned {
        node.label_names.get(1..).unwrap_or(&[])
    } else {
        &node.label_names
    };
    if !labels.is_empty() {
        predicates.push(Expr::LabelCheck {
            expr: Box::new(Expr::Variable(node.var.clone())),
            labels: labels.to_vec(),
        });
    }
    if !scanned {
        // Scans apply literal properties themselves; bound nodes need them
        // re-checked.
        for (key, _, value) in &node.props {
            predicates.push(Expr::Binary {
                op: BinaryOp::Eq,
                left: Box::new(Expr::Property {
                    base: Box::new(Expr::Variable(node.var.clone())),
                    key: key.clone(),
                }),
                right: Box::new(Expr::Literal(value.clone())),
            });
        }
    }
    // Expression properties always become runtime filters.
    for (key, value) in &node.prop_exprs {
        predicates.push(Expr::Binary {
            op: BinaryOp::Eq,
            left: Box::new(Expr::Property {
                base: Box::new(Expr::Variable(node.var.clone())),
                key: key.clone(),
            }),
            right: Box::new(value.clone()),
        });
    }
    predicates.into_iter().reduce(|left, right| Expr::Binary {
        op: BinaryOp::And,
        left: Box::new(left),
        right: Box::new(right),
    })
}

fn flip(direction: RelDirection) -> RelDirection {
    match direction {
        RelDirection::Outgoing => RelDirection::Incoming,
        RelDirection::Incoming => RelDirection::Outgoing,
        RelDirection::Undirected => RelDirection::Undirected,
    }
}

/// Output columns of a supported built-in procedure.
fn procedure_columns(name: &str) -> Result<Vec<String>> {
    match name {
        "db.labels" => Ok(vec!["label".to_owned()]),
        "db.propertyKeys" => Ok(vec!["propertyKey".to_owned()]),
        "db.relationshipTypes" => Ok(vec!["relationshipType".to_owned()]),
        _ => Err(Error::unsupported(format!(
            "procedure `{name}` is not supported"
        ))),
    }
}

fn undefined_variable(name: &str) -> Error {
    Error::new(
        ErrorCode::NotFound,
        format!("variable `{name}` is not defined"),
    )
}

/// Validate that all variables in an expression are in scope.
fn validate_expr(expr: &Expr, scope: &FastMap<String, VarKind>) -> Result<()> {
    check_expr_kinds(expr, scope)?;
    match expr {
        Expr::Variable(name) => {
            if scope.contains_key(name) {
                Ok(())
            } else {
                Err(undefined_variable(name))
            }
        }
        Expr::Literal(_) | Expr::Parameter(_) => Ok(()),
        Expr::Property { base, .. } => validate_expr(base, scope),
        Expr::Subscript { base, index } => {
            validate_expr(base, scope)?;
            validate_expr(index, scope)
        }
        Expr::Slice { base, lower, upper } => {
            validate_expr(base, scope)?;
            if let Some(lower) = lower {
                validate_expr(lower, scope)?;
            }
            if let Some(upper) = upper {
                validate_expr(upper, scope)?;
            }
            Ok(())
        }
        Expr::ListComprehension {
            var,
            list,
            predicate,
            projection,
        } => {
            validate_expr(list, scope)?;
            let mut inner = scope.clone();
            inner.insert(var.clone(), VarKind::Value);
            if let Some(predicate) = predicate {
                validate_expr(predicate, &inner)?;
            }
            if let Some(projection) = projection {
                validate_expr(projection, &inner)?;
            }
            Ok(())
        }
        Expr::Quantifier {
            var,
            list,
            predicate,
            ..
        } => {
            validate_expr(list, scope)?;
            let mut inner = scope.clone();
            inner.insert(var.clone(), VarKind::Value);
            validate_expr(predicate, &inner)
        }
        Expr::Reduce {
            accumulator,
            init,
            var,
            list,
            expression,
        } => {
            validate_expr(init, scope)?;
            validate_expr(list, scope)?;
            let mut inner = scope.clone();
            inner.insert(var.clone(), VarKind::Value);
            inner.insert(accumulator.clone(), VarKind::Value);
            validate_expr(expression, &inner)
        }
        Expr::Pattern(_) | Expr::PatternComprehension { .. } => Err(Error::syntax(
            "pattern expressions are only allowed in predicates",
        )),
        // Rewritten into a pattern predicate before validation; a survivor
        // is boolean by construction.
        Expr::ExistsSubquery(_) => Ok(()),
        Expr::Unary { expr, .. } => validate_expr(expr, scope),
        Expr::Binary { left, right, .. } => {
            validate_expr(left, scope)?;
            validate_expr(right, scope)
        }
        Expr::Function { args, .. } => {
            for arg in args {
                validate_expr(arg, scope)?;
            }
            Ok(())
        }
        Expr::List(items) => {
            for item in items {
                validate_expr(item, scope)?;
            }
            Ok(())
        }
        Expr::Map(entries) => {
            for (_, value) in entries {
                validate_expr(value, scope)?;
            }
            Ok(())
        }
        Expr::Case {
            subject,
            alternatives,
            default,
        } => {
            if let Some(subject) = subject {
                validate_expr(subject, scope)?;
            }
            for (when, then) in alternatives {
                validate_expr(when, scope)?;
                validate_expr(then, scope)?;
            }
            if let Some(default) = default {
                validate_expr(default, scope)?;
            }
            Ok(())
        }
        Expr::IsNull { expr, .. } => validate_expr(expr, scope),
        Expr::LabelCheck { expr, .. } => validate_expr(expr, scope),
    }
}

/// A statically known value kind, used for compile-time type checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// `null` (usable where any type is expected).
    Null,
    /// A boolean.
    Boolean,
    /// A number.
    Number,
    /// A string.
    Text,
    /// A list.
    List,
    /// A map.
    Map,
    /// A node.
    Node,
    /// A relationship.
    Edge,
    /// A named path.
    Path,
    /// A temporal value.
    Temporal,
    /// A spatial point.
    Point,
}

impl Kind {
    /// A short name for error messages.
    fn name(self) -> &'static str {
        match self {
            Self::Null => "null",
            Self::Boolean => "boolean",
            Self::Number => "number",
            Self::Text => "string",
            Self::List => "list",
            Self::Map => "map",
            Self::Node => "node",
            Self::Edge => "relationship",
            Self::Path => "path",
            Self::Temporal => "temporal",
            Self::Point => "point",
        }
    }
}

/// The kind of an expression when it can be decided without looking at the
/// data; `None` when it depends on runtime values.
fn static_kind(expr: &Expr, scope: &FastMap<String, VarKind>) -> Option<Kind> {
    static_kind_with(expr, scope, None)
}

/// [`static_kind`] with an optional overlay of locally bound kinds (for
/// example the element kind of a comprehension's source list).
fn static_kind_with(
    expr: &Expr,
    scope: &FastMap<String, VarKind>,
    locals: Option<&FastMap<String, Kind>>,
) -> Option<Kind> {
    match expr {
        Expr::ExistsSubquery(_) => Some(Kind::Boolean),
        Expr::Literal(value) => Some(match value {
            Value::Null => Kind::Null,
            Value::Bool(_) => Kind::Boolean,
            Value::Int(_) | Value::Float(_) => Kind::Number,
            Value::Text(_) => Kind::Text,
            Value::Bytes(_) => Kind::List,
            Value::List(_) => Kind::List,
            Value::Map(_) => Kind::Map,
            Value::Node(_) => Kind::Node,
            Value::Edge(_) => Kind::Edge,
            Value::Date(_)
            | Value::LocalTime(_)
            | Value::Time(_)
            | Value::LocalDateTime(_)
            | Value::DateTime(_)
            | Value::Duration(_) => Kind::Temporal,
            Value::Point(_) => Kind::Point,
        }),
        Expr::Parameter(_) => None,
        Expr::Variable(name) => match locals.and_then(|locals| locals.get(name).copied()) {
            Some(kind) => Some(kind),
            None => match scope.get(name) {
                Some(VarKind::Node) => Some(Kind::Node),
                Some(VarKind::Edge) => Some(Kind::Edge),
                Some(VarKind::EdgeList) => Some(Kind::List),
                Some(VarKind::Path) => Some(Kind::Path),
                Some(VarKind::Scalar(kind)) => Some(*kind),
                Some(VarKind::Value) | None => None,
            },
        },
        Expr::Property { base, key } => {
            match (static_kind_with(base, scope, locals), base.as_ref()) {
                // A map literal exposes its entries' kinds.
                (Some(Kind::Map), Expr::Map(entries)) => entries
                    .iter()
                    .find(|(name, _)| name == key)
                    .and_then(|(_, value)| static_kind_with(value, scope, locals)),
                _ => None,
            }
        }
        // Indexing yields an element or property value of unknown kind.
        Expr::Subscript { .. } => None,
        Expr::Slice { base, .. } => {
            static_kind(base, scope).filter(|kind| *kind == Kind::List || *kind == Kind::Text)
        }
        Expr::ListComprehension { .. } | Expr::PatternComprehension { .. } => Some(Kind::List),
        Expr::Quantifier { .. } => Some(Kind::Boolean),
        Expr::Reduce { .. } => None,
        Expr::Unary { op, .. } => Some(match op {
            UnaryOp::Neg => Kind::Number,
            UnaryOp::Not => Kind::Boolean,
        }),
        Expr::Binary { op, left, right } => Some(match op {
            BinaryOp::Add => {
                let (left, right) = (
                    static_kind_with(left, scope, locals),
                    static_kind_with(right, scope, locals),
                );
                if left == Some(Kind::Text) || right == Some(Kind::Text) {
                    Kind::Text
                } else if left == Some(Kind::List) || right == Some(Kind::List) {
                    Kind::List
                } else if left == Some(Kind::Number) && right == Some(Kind::Number) {
                    Kind::Number
                } else {
                    return None;
                }
            }
            BinaryOp::Sub | BinaryOp::Mul | BinaryOp::Div | BinaryOp::Mod | BinaryOp::Pow => {
                Kind::Number
            }
            _ => Kind::Boolean,
        }),
        Expr::Function { name, args, .. } => static_function_kind(name, args, scope, locals),
        Expr::List(_) => Some(Kind::List),
        Expr::Map(_) => Some(Kind::Map),
        Expr::Case {
            alternatives,
            default,
            ..
        } => {
            let mut kinds = alternatives
                .iter()
                .map(|(_, then)| static_kind(then, scope))
                .chain(
                    default
                        .as_deref()
                        .map(|default| static_kind(default, scope)),
                )
                .collect::<Vec<_>>();
            kinds.dedup();
            match kinds.as_slice() {
                [Some(kind)] => Some(*kind),
                _ => None,
            }
        }
        Expr::IsNull { .. } | Expr::LabelCheck { .. } => Some(Kind::Boolean),
        Expr::Pattern(_) => Some(Kind::Boolean),
    }
}

/// The result kind of a built-in function, when known.
fn static_function_kind(
    name: &str,
    args: &[Expr],
    scope: &FastMap<String, VarKind>,
    locals: Option<&FastMap<String, Kind>>,
) -> Option<Kind> {
    Some(match name.to_ascii_lowercase().as_str() {
        "id" | "size" | "length" | "count" | "sum" | "avg" | "stdev" | "stdevp"
        | "percentilecont" | "percentiledisc" | "tointeger" | "tofloat" | "distance" | "abs"
        | "sign" | "ceil" | "floor" | "round" | "sqrt" | "exp" | "log" | "log10" | "sin"
        | "cos" | "tan" | "cot" | "asin" | "acos" | "atan" | "atan2" | "degrees" | "radians"
        | "haversin" | "pi" | "e" | "rand" => Kind::Number,
        "labels" | "keys" | "nodes" | "relationships" | "collect" | "range" | "tail"
        | "reverse" | "split" => Kind::List,
        "properties" => Kind::Map,
        "type" | "tostring" | "substring" | "toupper" | "tolower" | "trim" | "ltrim" | "rtrim"
        | "left" | "right" | "replace" => Kind::Text,
        "toboolean" => Kind::Boolean,
        "startnode" | "endnode" => Kind::Node,
        "date" | "localtime" | "time" | "localdatetime" | "datetime" | "duration" => Kind::Temporal,
        "point" => Kind::Point,
        "coalesce" => {
            let mut kinds = args
                .iter()
                .map(|arg| static_kind_with(arg, scope, locals))
                .collect::<Vec<_>>();
            kinds.dedup();
            match kinds.as_slice() {
                [Some(kind)] => *kind,
                _ => return None,
            }
        }
        _ => return None,
    })
}

/// Statically check an expression's operators and built-in arguments.
fn check_expr_kinds(expr: &Expr, scope: &FastMap<String, VarKind>) -> Result<()> {
    check_expr_kinds_with(expr, scope, None)
}

/// The element kind of a list expression, when it can be decided (a list
/// literal with elements of one kind).
fn list_element_kind(
    list: &Expr,
    scope: &FastMap<String, VarKind>,
    locals: Option<&FastMap<String, Kind>>,
) -> Option<Kind> {
    let Expr::List(items) = list else {
        return None;
    };
    let mut kinds = items
        .iter()
        .map(|item| static_kind_with(item, scope, locals))
        .collect::<Vec<_>>();
    kinds.dedup();
    match kinds.as_slice() {
        [Some(kind)] => Some(*kind),
        _ => None,
    }
}

fn check_expr_kinds_with(
    expr: &Expr,
    scope: &FastMap<String, VarKind>,
    locals: Option<&FastMap<String, Kind>>,
) -> Result<()> {
    let require = |kind: Option<Kind>, wanted: &[Kind], what: &str| -> Result<()> {
        if let Some(kind) = kind
            && !wanted.contains(&kind)
        {
            return Err(Error::syntax(format!(
                "{what} expects {}, got {}",
                wanted
                    .iter()
                    .map(|kind| kind.name())
                    .collect::<Vec<_>>()
                    .join(" or "),
                kind.name()
            )));
        }
        Ok(())
    };
    let require_type = |kind: Option<Kind>, wanted: &[Kind], what: &str| -> Result<()> {
        if let Some(kind) = kind
            && !wanted.contains(&kind)
        {
            return Err(Error::type_error(format!(
                "{what} expects {}, got {}",
                wanted
                    .iter()
                    .map(|kind| kind.name())
                    .collect::<Vec<_>>()
                    .join(" or "),
                kind.name()
            )));
        }
        Ok(())
    };
    let kind = |e: &Expr| static_kind_with(e, scope, locals);
    match expr {
        Expr::Unary {
            op: UnaryOp::Not,
            expr: operand,
        } => {
            require(
                static_kind(operand, scope),
                &[Kind::Boolean, Kind::Null],
                "NOT",
            )?;
        }
        Expr::Binary { op, left, right } => match op {
            BinaryOp::And | BinaryOp::Or | BinaryOp::Xor => {
                require(kind(left), &[Kind::Boolean, Kind::Null], "boolean operator")?;
                require(
                    kind(right),
                    &[Kind::Boolean, Kind::Null],
                    "boolean operator",
                )?;
            }
            BinaryOp::In => {
                require(kind(right), &[Kind::List, Kind::Null], "IN")?;
            }
            _ => {}
        },
        Expr::Property { base, .. } => {
            // Property access works on maps, entities, temporal values and
            // points; lists and scalars are type errors, a path is not even
            // statically resolvable.
            let base_kind = kind(base);
            if base_kind == Some(Kind::Path) {
                return Err(Error::syntax("a path has no properties"));
            }
            require_type(
                base_kind,
                &[
                    Kind::Map,
                    Kind::Node,
                    Kind::Edge,
                    Kind::Temporal,
                    Kind::Point,
                    Kind::Null,
                ],
                "property access",
            )?;
        }
        Expr::Function { name, args, .. } => {
            let first = args.first();
            let argument = first.and_then(&kind);
            match name.to_ascii_lowercase().as_str() {
                "labels" | "properties" | "keys" => require(
                    argument,
                    &[Kind::Node, Kind::Edge, Kind::Map, Kind::Null],
                    name,
                )?,
                "type" | "startnode" | "endnode" => {
                    require(argument, &[Kind::Edge, Kind::Null], name)?
                }
                "length" => require(argument, &[Kind::List, Kind::Path, Kind::Null], name)?,
                "size" => require(argument, &[Kind::List, Kind::Text, Kind::Null], name)?,
                _ => {}
            }
        }
        Expr::Quantifier {
            var,
            list,
            predicate,
            ..
        } => {
            check_expr_kinds_with(list, scope, locals)?;
            let inner = binder_locals(var, list, scope, locals);
            check_expr_kinds_with(predicate, scope, inner.as_ref())?;
            require(
                static_kind_with(predicate, scope, inner.as_ref()),
                &[Kind::Boolean, Kind::Null],
                "quantifier predicate",
            )?;
            return Ok(());
        }
        Expr::ListComprehension {
            var,
            list,
            predicate,
            projection,
        } => {
            check_expr_kinds_with(list, scope, locals)?;
            let inner = binder_locals(var, list, scope, locals);
            if let Some(predicate) = predicate {
                check_expr_kinds_with(predicate, scope, inner.as_ref())?;
                require(
                    static_kind_with(predicate, scope, inner.as_ref()),
                    &[Kind::Boolean, Kind::Null],
                    "comprehension predicate",
                )?;
            }
            if let Some(projection) = projection {
                check_expr_kinds_with(projection, scope, inner.as_ref())?;
            }
            return Ok(());
        }
        _ => {}
    }
    // Arithmetic requires numbers, with the documented exceptions: `+`
    // concatenates and appends, and durations scale by numbers.
    if let Expr::Binary { op, left, right } = expr {
        let (left_kind, right_kind) = (kind(left), kind(right));
        let arithmetic = |wanted: &[Kind], what: &str| -> Result<()> {
            require(left_kind, wanted, what)?;
            require(right_kind, wanted, what)
        };
        match op {
            BinaryOp::Sub | BinaryOp::Mod | BinaryOp::Pow => {
                arithmetic(&[Kind::Number, Kind::Temporal, Kind::Null], "arithmetic")?
            }
            BinaryOp::Mul | BinaryOp::Div => {
                arithmetic(&[Kind::Number, Kind::Temporal, Kind::Null], "arithmetic")?;
                // A duration scales by a number; two durations do not.
                if left_kind == Some(Kind::Temporal) && right_kind == Some(Kind::Temporal) {
                    return Err(Error::syntax(
                        "a duration scales by a number, not another duration",
                    ));
                }
            }
            BinaryOp::Add => {
                // `+` concatenates strings, appends to lists (whatever the
                // element is) and adds numbers or durations.
                let allowed = left_kind == Some(Kind::Text)
                    || right_kind == Some(Kind::Text)
                    || left_kind == Some(Kind::List)
                    || right_kind == Some(Kind::List)
                    || matches!(
                        left_kind,
                        None | Some(Kind::Number) | Some(Kind::Temporal) | Some(Kind::Null)
                    ) && matches!(
                        right_kind,
                        None | Some(Kind::Number) | Some(Kind::Temporal) | Some(Kind::Null)
                    );
                if !allowed {
                    return Err(Error::syntax(
                        "arithmetic expects numbers, strings or lists",
                    ));
                }
            }
            _ => {}
        }
    }
    // Recurse into subexpressions: the checks above only see the node they
    // match, and operators nest arbitrarily.
    let mut error = None;
    for_each_child(expr, &mut |child| {
        if error.is_none() {
            error = check_expr_kinds_with(child, scope, locals).err();
        }
    });
    match error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

/// The element-kind overlay for a comprehension or quantifier binder, when
/// the list's element kind is known.
fn binder_locals(
    var: &str,
    list: &Expr,
    scope: &FastMap<String, VarKind>,
    locals: Option<&FastMap<String, Kind>>,
) -> Option<FastMap<String, Kind>> {
    let element = list_element_kind(list, scope, locals)?;
    let mut inner = locals.cloned().unwrap_or_default();
    inner.insert(var.to_owned(), element);
    Some(inner)
}

fn expr_kind(expr: &Expr, scope: &FastMap<String, VarKind>) -> VarKind {
    match expr {
        Expr::Variable(name) => scope.get(name).copied().unwrap_or(VarKind::Value),
        _ => VarKind::Value,
    }
}

/// The scope kind to record for a projection alias. Carrying through a
/// variable keeps its kind; a statically known non-entity expression
/// becomes a scalar binding so patterns cannot reuse it as an entity.
fn alias_kind(expr: &Expr, scope: &FastMap<String, VarKind>) -> VarKind {
    let kind = expr_kind(expr, scope);
    if kind != VarKind::Value {
        return kind;
    }
    // A list literal of relationships stays usable as a variable-length
    // relationship binding.
    if let Some(Kind::Edge) = list_element_kind(expr, scope, None) {
        return VarKind::EdgeList;
    }
    match static_kind(expr, scope) {
        Some(Kind::Node) => VarKind::Node,
        Some(Kind::Edge) => VarKind::Edge,
        Some(Kind::Path) => VarKind::Path,
        // Null is compatible with every kind, so it stays unconstrained.
        Some(Kind::Null) | None => VarKind::Value,
        Some(other) => VarKind::Scalar(other),
    }
}

fn default_name(expr: &Expr) -> String {
    match expr {
        Expr::Variable(name) => name.clone(),
        Expr::Pattern(_) => "pattern".to_owned(),
        Expr::ExistsSubquery(_) => "exists { ... }".to_owned(),
        Expr::PatternComprehension { projection, .. } => {
            format!("[pattern | {}]", default_name(projection))
        }
        Expr::Parameter(name) => format!("${name}"),
        Expr::Literal(value) => literal_text(value),
        Expr::LabelCheck { expr, labels } => {
            format!("({}:{})", default_name(expr), labels.join(":"))
        }
        Expr::Property { base, key } => format!("{}.{}", property_base_name(base), key),
        Expr::Subscript { base, index } => {
            format!("{}[{}]", subscript_base_name(base), default_name(index))
        }
        Expr::Slice { base, lower, upper } => {
            let lower = lower.as_ref().map(|e| default_name(e)).unwrap_or_default();
            let upper = upper.as_ref().map(|e| default_name(e)).unwrap_or_default();
            format!("{}[{lower}..{upper}]", subscript_base_name(base))
        }
        Expr::ListComprehension {
            var,
            list,
            predicate,
            projection,
        } => {
            let mut text = format!("[{var} IN {}]", default_name(list));
            if let Some(predicate) = predicate {
                text.push_str(&format!(" WHERE {}", default_name(predicate)));
            }
            if let Some(projection) = projection {
                text.push_str(&format!(" | {}", default_name(projection)));
            }
            text.insert(text.len() - 1, ' ');
            text
        }
        Expr::Quantifier {
            kind,
            var,
            list,
            predicate,
        } => format!(
            "{}({var} IN {} WHERE {})",
            kind.name(),
            default_name(list),
            default_name(predicate)
        ),
        Expr::Reduce {
            accumulator,
            init,
            var,
            list,
            expression,
        } => format!(
            "reduce({accumulator} = {}, {var} IN {} | {})",
            default_name(init),
            default_name(list),
            default_name(expression)
        ),
        Expr::Function {
            name,
            distinct,
            star,
            args,
        } => {
            if *star {
                format!("{name}(*)")
            } else {
                let prefix = if *distinct { "DISTINCT " } else { "" };
                let args: Vec<String> = args.iter().map(default_name).collect();
                format!("{name}({prefix}{})", args.join(", "))
            }
        }
        Expr::Unary { op, expr } => match op {
            UnaryOp::Neg => format!("-{}", operand_name(expr, UNARY_PRECEDENCE, false, None)),
            UnaryOp::Not => format!("NOT {}", operand_name(expr, NOT_PRECEDENCE, false, None)),
        },
        Expr::Binary { op, left, right } => {
            let precedence = op_precedence(*op);
            let left = operand_name(left, precedence, false, Some(*op));
            let right = operand_name(right, precedence, true, Some(*op));
            format!("{left} {} {right}", binary_text(*op))
        }
        Expr::IsNull { expr, negated } => format!(
            "{} IS {}NULL",
            default_name(expr),
            if *negated { "NOT " } else { "" }
        ),
        Expr::List(items) => format!(
            "[{}]",
            items
                .iter()
                .map(default_name)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Expr::Map(entries) => format!(
            "{{{}}}",
            entries
                .iter()
                .map(|(key, value)| format!("{key}: {}", default_name(value)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Expr::Case { .. } => "CASE".to_owned(),
    }
}

/// Precedence levels mirroring the expression grammar in the parser; larger
/// values bind tighter.
const OR_PRECEDENCE: u8 = 1;
const XOR_PRECEDENCE: u8 = 2;
const AND_PRECEDENCE: u8 = 3;
const NOT_PRECEDENCE: u8 = 4;
const COMPARISON_PRECEDENCE: u8 = 5;
const ADDITIVE_PRECEDENCE: u8 = 6;
const MULTIPLICATIVE_PRECEDENCE: u8 = 7;
const POWER_PRECEDENCE: u8 = 8;
const UNARY_PRECEDENCE: u8 = 9;
const ATOMIC_PRECEDENCE: u8 = 10;

/// Binding strength of a binary operator.
fn op_precedence(op: BinaryOp) -> u8 {
    match op {
        BinaryOp::Or => OR_PRECEDENCE,
        BinaryOp::Xor => XOR_PRECEDENCE,
        BinaryOp::And => AND_PRECEDENCE,
        BinaryOp::Eq
        | BinaryOp::Ne
        | BinaryOp::Lt
        | BinaryOp::Le
        | BinaryOp::Gt
        | BinaryOp::Ge
        | BinaryOp::Regex
        | BinaryOp::In
        | BinaryOp::StartsWith
        | BinaryOp::EndsWith
        | BinaryOp::Contains => COMPARISON_PRECEDENCE,
        BinaryOp::Add | BinaryOp::Sub => ADDITIVE_PRECEDENCE,
        BinaryOp::Mul | BinaryOp::Div | BinaryOp::Mod => MULTIPLICATIVE_PRECEDENCE,
        BinaryOp::Pow => POWER_PRECEDENCE,
    }
}

/// Whether chaining the operator needs no parentheses: `a + b + c`,
/// `a AND b AND c`.
fn op_associative(op: BinaryOp) -> bool {
    matches!(
        op,
        BinaryOp::Add | BinaryOp::Mul | BinaryOp::And | BinaryOp::Or | BinaryOp::Xor
    )
}

/// Binding strength of an arbitrary expression: binary operators, unary
/// operators and the comparison-shaped predicates.
fn expr_precedence(expr: &Expr) -> u8 {
    match expr {
        Expr::Binary { op, .. } => op_precedence(*op),
        Expr::Unary {
            op: UnaryOp::Not, ..
        } => NOT_PRECEDENCE,
        Expr::Unary {
            op: UnaryOp::Neg, ..
        } => UNARY_PRECEDENCE,
        Expr::IsNull { .. } | Expr::LabelCheck { .. } => COMPARISON_PRECEDENCE,
        _ => ATOMIC_PRECEDENCE,
    }
}

/// Render `expr` as an operand of an operator with `precedence`, adding
/// parentheses wherever the operator tree would otherwise re-parse
/// differently, so `12 / 4 * (3 - 2 * 4)` keeps its parentheses.
fn operand_name(expr: &Expr, precedence: u8, right: bool, parent: Option<BinaryOp>) -> String {
    let child = expr_precedence(expr);
    let parenthesize = if child < precedence {
        true
    } else if child > precedence {
        false
    } else if !right {
        // A same-precedence left operand only needs parentheses under a
        // right-associative operator, which the grammar has none of.
        false
    } else {
        match parent {
            // `a - (b - c)` and `a / (b * c)` would re-associate; associative
            // operators only need parentheses when the operator differs.
            Some(parent_op) => {
                let same = matches!(expr, Expr::Binary { op, .. } if *op == parent_op);
                !(same && op_associative(parent_op))
            }
            None => true,
        }
    };
    if parenthesize {
        format!("({})", default_name(expr))
    } else {
        default_name(expr)
    }
}

/// Render the base of a property access; indexing expressions are wrapped so
/// `(list[1]).missing` reads unambiguously.
fn property_base_name(expr: &Expr) -> String {
    match expr {
        Expr::Variable(_)
        | Expr::Property { .. }
        | Expr::List(_)
        | Expr::Map(_)
        | Expr::Function { .. } => default_name(expr),
        other => format!("({})", default_name(other)),
    }
}

/// Render the base of a subscript or slice; literals and indexing chains need
/// no parentheses.
fn subscript_base_name(expr: &Expr) -> String {
    match expr {
        Expr::Variable(_)
        | Expr::Property { .. }
        | Expr::Subscript { .. }
        | Expr::Slice { .. }
        | Expr::List(_)
        | Expr::Map(_)
        | Expr::Function { .. } => default_name(expr),
        other => format!("({})", default_name(other)),
    }
}

fn literal_text(value: &Value) -> String {
    match value {
        Value::Null => "null".to_owned(),
        Value::Bool(inner) => inner.to_string(),
        Value::Int(inner) => inner.to_string(),
        Value::Float(inner) => {
            let mut buffer = ryu::Buffer::new();
            buffer.format(*inner).to_owned()
        }
        Value::Text(inner) => format!("'{inner}'"),
        other => format!("{other:?}"),
    }
}

fn binary_text(op: BinaryOp) -> &'static str {
    match op {
        BinaryOp::Add => "+",
        BinaryOp::Sub => "-",
        BinaryOp::Mul => "*",
        BinaryOp::Div => "/",
        BinaryOp::Mod => "%",
        BinaryOp::Pow => "^",
        BinaryOp::Eq => "=",
        BinaryOp::Ne => "<>",
        BinaryOp::Lt => "<",
        BinaryOp::Le => "<=",
        BinaryOp::Gt => ">",
        BinaryOp::Ge => ">=",
        BinaryOp::And => "AND",
        BinaryOp::Or => "OR",
        BinaryOp::Xor => "XOR",
        BinaryOp::In => "IN",
        BinaryOp::StartsWith => "STARTS WITH",
        BinaryOp::EndsWith => "ENDS WITH",
        BinaryOp::Contains => "CONTAINS",
        BinaryOp::Regex => "=~",
    }
}

/// Whether an expression is exactly an aggregation call, returning the
/// function, its arguments and the star flag.
#[allow(clippy::type_complexity)]
fn aggregate_call(expr: &Expr) -> Result<Option<(AggFn, Vec<&Expr>, bool, bool)>> {
    let Expr::Function {
        name,
        distinct,
        star,
        args,
    } = expr
    else {
        return Ok(None);
    };
    let func = match name.to_ascii_lowercase().as_str() {
        "count" => AggFn::Count,
        "sum" => AggFn::Sum,
        "avg" => AggFn::Avg,
        "min" => AggFn::Min,
        "max" => AggFn::Max,
        "collect" => AggFn::Collect,
        "percentilecont" => AggFn::PercentileCont,
        "percentiledisc" => AggFn::PercentileDisc,
        "stdev" => AggFn::StDev,
        "stdevp" => AggFn::StDevP,
        _ => return Ok(None),
    };
    if *star {
        if func != AggFn::Count {
            return Err(Error::syntax("only count(*) accepts *"));
        }
        if *distinct {
            return Err(Error::syntax("count(DISTINCT *) is not valid"));
        }
        return Ok(Some((func, Vec::new(), true, false)));
    }
    let expected = match func {
        AggFn::PercentileCont | AggFn::PercentileDisc => 2,
        _ => 1,
    };
    if args.len() != expected {
        return Err(Error::syntax(format!(
            "{name}() expects {expected} argument(s)"
        )));
    }
    Ok(Some((func, args.iter().collect(), false, *distinct)))
}

/// The arguments of an aggregation, with `count(*)` becoming `count(1)`.
fn aggregate_arguments(
    func: AggFn,
    arguments: &[&Expr],
    star: bool,
    scope: &FastMap<String, VarKind>,
) -> Result<Vec<Expr>> {
    if star {
        return Ok(vec![Expr::Literal(Value::Int(1))]);
    }
    if arguments.is_empty() {
        return Err(Error::syntax(format!("{func:?} requires an argument")));
    }
    for argument in arguments {
        validate_expr(argument, scope)?;
    }
    Ok(arguments
        .iter()
        .map(|argument| (*argument).clone())
        .collect())
}

/// Reject aggregations of aggregations and non-deterministic arguments.
fn check_aggregate_arguments(arguments: &[&Expr]) -> Result<()> {
    for argument in arguments {
        if contains_aggregation(argument) {
            return Err(Error::syntax(
                "NestedAggregation: an aggregation cannot contain another aggregation",
            ));
        }
        if contains_function(argument, &["rand"]) {
            return Err(Error::syntax(
                "NonConstantExpression: an aggregation cannot use non-deterministic functions",
            ));
        }
    }
    Ok(())
}

/// Rewrite an ORDER BY key of an aggregating projection: aggregation calls
/// become references to their projected columns, and simple projected
/// expressions become references to their grouping keys.
///
/// # Errors
///
/// Returns `Error::syntax` when an aggregation is not projected or a complex
/// expression is combined with an aggregation (ambiguous semantics).
fn rewrite_sort_key(expr: &Expr, groups: &[(Expr, String)], aggs: &[AggItem]) -> Result<Expr> {
    rewrite_expr(expr, &mut |sub| {
        if let Some((func, args, star, distinct)) = aggregate_call(sub)? {
            for item in aggs {
                let same_args = if star {
                    matches!(&item.arguments[..], [Expr::Literal(Value::Int(1))])
                } else {
                    item.arguments.len() == args.len()
                        && item.arguments.iter().zip(args.iter()).all(|(a, b)| a == *b)
                };
                if item.func == func && item.distinct == distinct && same_args {
                    return Ok(Some(Expr::Variable(item.name.clone())));
                }
            }
            return Err(Error::syntax(
                "UndefinedVariable: ORDER BY cannot use an aggregation that is not projected",
            ));
        }
        if let Some((_, group)) = groups.iter().find(|(group, _)| group == sub) {
            if is_atom_expr(sub) {
                return Ok(Some(Expr::Variable(group.clone())));
            }
            return Err(Error::syntax(
                "AmbiguousAggregationExpression: an ORDER BY item with an aggregation may only \
                 combine projected variables and aggregations",
            ));
        }
        Ok(None)
    })
}

/// A relationship and a node of the same pattern may not share a variable.
///
/// # Errors
///
/// Returns `Error::syntax` when a variable names both a node and a
/// relationship within one path.
fn check_path_conflicts(path: &PatternPath) -> Result<()> {
    let mut rel_vars: Vec<&str> = Vec::new();
    for segment in &path.segments {
        if let Some(var) = &segment.relationship.variable {
            rel_vars.push(var);
        }
    }
    if rel_vars.is_empty() {
        return Ok(());
    }
    let mut node_vars: Vec<&str> = Vec::new();
    if let Some(var) = &path.start.variable {
        node_vars.push(var);
    }
    for segment in &path.segments {
        if let Some(var) = &segment.node.variable {
            node_vars.push(var);
        }
    }
    if let Some(var) = path
        .variable
        .as_deref()
        .filter(|var| rel_vars.contains(var) || node_vars.contains(var))
    {
        return Err(Error::syntax(format!(
            "VariableTypeConflict: path variable `{var}` is also bound in the pattern"
        )));
    }
    if let Some(var) = rel_vars.iter().find(|var| node_vars.contains(*var)) {
        return Err(Error::syntax(format!(
            "VariableTypeConflict: variable `{var}` is bound both as a node and as a relationship"
        )));
    }
    Ok(())
}

/// Whether an expression is a constant, a variable or a chain of property
/// accesses, i.e. something that can appear next to an aggregation without
/// changing the grouping.
fn is_atom_expr(expr: &Expr) -> bool {
    match expr {
        Expr::Variable(_) | Expr::Literal(_) | Expr::Parameter(_) => true,
        Expr::Property { base, .. } => is_atom_expr(base),
        _ => false,
    }
}

/// `SKIP` and `LIMIT` must not depend on variables; the value itself is
/// range checked at compile time for literals (in `substitute_items`) and at
/// execution time for everything else.
fn validate_projection_limit(expr: &Expr, what: &str) -> Result<()> {
    if has_free_variable(expr, &[], &[]) {
        return Err(Error::syntax(format!(
            "NonConstantExpression: {what} must not depend on variables"
        )));
    }
    Ok(())
}

/// Rewrite an expression top-down; `f` may replace a subexpression and stop
/// the descent by returning `Some`.
fn rewrite_expr(expr: &Expr, f: &mut dyn FnMut(&Expr) -> Result<Option<Expr>>) -> Result<Expr> {
    if let Some(replacement) = f(expr)? {
        return Ok(replacement);
    }
    Ok(match expr {
        Expr::Literal(_) | Expr::Parameter(_) | Expr::Variable(_) => expr.clone(),
        Expr::ExistsSubquery(_) => expr.clone(),
        Expr::Pattern(path) => Expr::Pattern(Box::new(rewrite_pattern(path, f)?)),
        Expr::PatternComprehension {
            path,
            predicate,
            projection,
        } => Expr::PatternComprehension {
            path: Box::new(rewrite_pattern(path, f)?),
            predicate: predicate
                .as_ref()
                .map(|predicate| rewrite_expr(predicate, f).map(Box::new))
                .transpose()?,
            projection: Box::new(rewrite_expr(projection, f)?),
        },
        Expr::Property { base, key } => Expr::Property {
            base: Box::new(rewrite_expr(base, f)?),
            key: key.clone(),
        },
        Expr::Subscript { base, index } => Expr::Subscript {
            base: Box::new(rewrite_expr(base, f)?),
            index: Box::new(rewrite_expr(index, f)?),
        },
        Expr::Slice { base, lower, upper } => Expr::Slice {
            base: Box::new(rewrite_expr(base, f)?),
            lower: lower
                .as_ref()
                .map(|lower| rewrite_expr(lower, f).map(Box::new))
                .transpose()?,
            upper: upper
                .as_ref()
                .map(|upper| rewrite_expr(upper, f).map(Box::new))
                .transpose()?,
        },
        Expr::ListComprehension {
            var,
            list,
            predicate,
            projection,
        } => Expr::ListComprehension {
            var: var.clone(),
            list: Box::new(rewrite_expr(list, f)?),
            predicate: predicate
                .as_ref()
                .map(|predicate| rewrite_expr(predicate, f).map(Box::new))
                .transpose()?,
            projection: projection
                .as_ref()
                .map(|projection| rewrite_expr(projection, f).map(Box::new))
                .transpose()?,
        },
        Expr::Quantifier {
            kind,
            var,
            list,
            predicate,
        } => Expr::Quantifier {
            kind: *kind,
            var: var.clone(),
            list: Box::new(rewrite_expr(list, f)?),
            predicate: Box::new(rewrite_expr(predicate, f)?),
        },
        Expr::Reduce {
            accumulator,
            init,
            var,
            list,
            expression,
        } => Expr::Reduce {
            accumulator: accumulator.clone(),
            init: Box::new(rewrite_expr(init, f)?),
            var: var.clone(),
            list: Box::new(rewrite_expr(list, f)?),
            expression: Box::new(rewrite_expr(expression, f)?),
        },
        Expr::Unary { op, expr: inner } => Expr::Unary {
            op: *op,
            expr: Box::new(rewrite_expr(inner, f)?),
        },
        Expr::Binary { op, left, right } => Expr::Binary {
            op: *op,
            left: Box::new(rewrite_expr(left, f)?),
            right: Box::new(rewrite_expr(right, f)?),
        },
        Expr::Function {
            name,
            distinct,
            star,
            args,
        } => Expr::Function {
            name: name.clone(),
            distinct: *distinct,
            star: *star,
            args: args
                .iter()
                .map(|arg| rewrite_expr(arg, f))
                .collect::<Result<_>>()?,
        },
        Expr::List(items) => Expr::List(
            items
                .iter()
                .map(|item| rewrite_expr(item, f))
                .collect::<Result<_>>()?,
        ),
        Expr::Map(entries) => Expr::Map(
            entries
                .iter()
                .map(|(key, value)| Ok((key.clone(), rewrite_expr(value, f)?)))
                .collect::<Result<_>>()?,
        ),
        Expr::Case {
            subject,
            alternatives,
            default,
        } => Expr::Case {
            subject: subject
                .as_ref()
                .map(|subject| rewrite_expr(subject, f).map(Box::new))
                .transpose()?,
            alternatives: alternatives
                .iter()
                .map(|(when, then)| Ok((rewrite_expr(when, f)?, rewrite_expr(then, f)?)))
                .collect::<Result<_>>()?,
            default: default
                .as_ref()
                .map(|default| rewrite_expr(default, f).map(Box::new))
                .transpose()?,
        },
        Expr::IsNull {
            expr: inner,
            negated,
        } => Expr::IsNull {
            expr: Box::new(rewrite_expr(inner, f)?),
            negated: *negated,
        },
        Expr::LabelCheck {
            expr: inner,
            labels,
        } => Expr::LabelCheck {
            expr: Box::new(rewrite_expr(inner, f)?),
            labels: labels.clone(),
        },
    })
}

/// Rewrite the inline property expressions of a pattern path.
fn rewrite_pattern(
    path: &PatternPath,
    f: &mut dyn FnMut(&Expr) -> Result<Option<Expr>>,
) -> Result<PatternPath> {
    let mut path = path.clone();
    if let Some(properties) = &path.start.properties {
        path.start.properties = Some(rewrite_expr(properties, f)?);
    }
    for segment in &mut path.segments {
        if let Some(properties) = &segment.relationship.properties {
            segment.relationship.properties = Some(rewrite_expr(properties, f)?);
        }
    }
    Ok(path)
}

/// Whether the inline property expressions of a pattern path contain an
/// aggregation.
fn pattern_has_aggregation(path: &PatternPath) -> bool {
    path.start
        .properties
        .as_ref()
        .is_some_and(contains_aggregation)
        || path.segments.iter().any(|segment| {
            segment
                .relationship
                .properties
                .as_ref()
                .is_some_and(contains_aggregation)
        })
}

/// A predicate must be boolean (or null); statically known scalars are
/// rejected when the clause is planned.
fn check_predicate_kind(expr: &Expr, scope: &FastMap<String, VarKind>) -> Result<()> {
    if let Some(kind) = static_kind(expr, scope)
        && !matches!(kind, Kind::Boolean | Kind::Null)
    {
        return Err(Error::syntax(format!(
            "InvalidArgumentType: a predicate must be a boolean, got {}",
            kind.name()
        )));
    }
    Ok(())
}

/// Whether `expr` references a variable that is neither bound inside the
/// expression itself (comprehension, quantifier, `reduce` and pattern
/// binders) nor the name of a grouping key or aggregation output.
fn has_free_variable(expr: &Expr, groups: &[(Expr, String)], aggs: &[AggItem]) -> bool {
    fn walk(
        expr: &Expr,
        bound: &mut Vec<String>,
        groups: &[(Expr, String)],
        aggs: &[AggItem],
    ) -> bool {
        match expr {
            Expr::Variable(name) => {
                if bound.iter().any(|entry| entry == name) {
                    return false;
                }
                !(groups.iter().any(|(_, group)| group == name)
                    || aggs.iter().any(|item| &item.name == name))
            }
            Expr::Literal(_) | Expr::Parameter(_) => false,
            Expr::Pattern(path) => walk_pattern(path, bound, groups, aggs),
            Expr::PatternComprehension {
                path,
                predicate,
                projection,
            } => {
                if walk_pattern(path, bound, groups, aggs) {
                    return true;
                }
                let mark = bound.len();
                pattern_variables(path, bound);
                let found = predicate
                    .as_deref()
                    .is_some_and(|predicate| walk(predicate, bound, groups, aggs))
                    || walk(projection, bound, groups, aggs);
                bound.truncate(mark);
                found
            }
            Expr::ListComprehension {
                var,
                list,
                predicate,
                projection,
            } => {
                if walk(list, bound, groups, aggs) {
                    return true;
                }
                bound.push(var.clone());
                let found = predicate
                    .as_deref()
                    .is_some_and(|predicate| walk(predicate, bound, groups, aggs))
                    || projection
                        .as_deref()
                        .is_some_and(|projection| walk(projection, bound, groups, aggs));
                bound.pop();
                found
            }
            Expr::Quantifier {
                var,
                list,
                predicate,
                ..
            } => {
                if walk(list, bound, groups, aggs) {
                    return true;
                }
                bound.push(var.clone());
                let found = walk(predicate, bound, groups, aggs);
                bound.pop();
                found
            }
            Expr::Reduce {
                accumulator,
                init,
                var,
                list,
                expression,
            } => {
                if walk(init, bound, groups, aggs) || walk(list, bound, groups, aggs) {
                    return true;
                }
                bound.push(accumulator.clone());
                bound.push(var.clone());
                let found = walk(expression, bound, groups, aggs);
                bound.pop();
                bound.pop();
                found
            }
            other => {
                let mut found = false;
                for_each_child(other, &mut |child| {
                    if !found {
                        found = walk(child, bound, groups, aggs);
                    }
                });
                found
            }
        }
    }

    fn walk_pattern(
        path: &PatternPath,
        bound: &mut Vec<String>,
        groups: &[(Expr, String)],
        aggs: &[AggItem],
    ) -> bool {
        pattern_properties(path)
            .iter()
            .any(|expr| walk(expr, bound, groups, aggs))
    }

    walk(expr, &mut Vec::new(), groups, aggs)
}

/// Rewrite a post-aggregation residual, hoisting every subexpression that
/// references a pre-aggregation variable onto a grouping key. References to
/// variables bound inside comprehensions, quantifiers and `reduce` bodies
/// stay in place: they are resolved by their binder, not by a grouping key.
fn hoist_groups(
    expr: &Expr,
    groups: &mut Vec<(Expr, String)>,
    aggs: &[AggItem],
    scope: &FastMap<String, VarKind>,
) -> Result<Expr> {
    hoist_walk(expr, groups, aggs, scope, &[])
}

fn hoist_walk(
    expr: &Expr,
    groups: &mut Vec<(Expr, String)>,
    aggs: &[AggItem],
    scope: &FastMap<String, VarKind>,
    bound: &[String],
) -> Result<Expr> {
    if let Some((_, group)) = groups.iter().find(|(group, _)| group == expr) {
        // Only constants and property chains may be picked out of an
        // aggregate-containing expression; anything more complex is
        // ambiguous.
        if is_atom_expr(expr) {
            return Ok(Expr::Variable(group.clone()));
        }
        return Err(Error::syntax(
            "AmbiguousAggregationExpression: an expression with an aggregation may not combine \
             non-constant subexpressions",
        ));
    }
    Ok(match expr {
        Expr::Literal(_) | Expr::Parameter(_) | Expr::Variable(_) => expr.clone(),
        Expr::ExistsSubquery(_) => expr.clone(),
        Expr::Pattern(path) => {
            Expr::Pattern(Box::new(hoist_pattern(path, groups, aggs, scope, bound)?))
        }
        Expr::PatternComprehension {
            path,
            predicate,
            projection,
        } => {
            let path = hoist_pattern(path, groups, aggs, scope, bound)?;
            let mut inner = bound.to_vec();
            pattern_variables(&path, &mut inner);
            Expr::PatternComprehension {
                path: Box::new(path),
                predicate: predicate
                    .as_deref()
                    .map(|predicate| {
                        hoist_walk(predicate, groups, aggs, scope, &inner).map(Box::new)
                    })
                    .transpose()?,
                projection: Box::new(hoist_walk(projection, groups, aggs, scope, &inner)?),
            }
        }
        Expr::Property { base, key } => Expr::Property {
            base: Box::new(hoist_walk(base, groups, aggs, scope, bound)?),
            key: key.clone(),
        },
        Expr::Subscript { base, index } => Expr::Subscript {
            base: Box::new(hoist_walk(base, groups, aggs, scope, bound)?),
            index: Box::new(hoist_walk(index, groups, aggs, scope, bound)?),
        },
        Expr::Slice { base, lower, upper } => Expr::Slice {
            base: Box::new(hoist_walk(base, groups, aggs, scope, bound)?),
            lower: lower
                .as_deref()
                .map(|lower| hoist_walk(lower, groups, aggs, scope, bound).map(Box::new))
                .transpose()?,
            upper: upper
                .as_deref()
                .map(|upper| hoist_walk(upper, groups, aggs, scope, bound).map(Box::new))
                .transpose()?,
        },
        Expr::ListComprehension {
            var,
            list,
            predicate,
            projection,
        } => {
            let list = Box::new(hoist_walk(list, groups, aggs, scope, bound)?);
            let mut inner = bound.to_vec();
            inner.push(var.clone());
            Expr::ListComprehension {
                var: var.clone(),
                list,
                predicate: predicate
                    .as_deref()
                    .map(|predicate| {
                        hoist_walk(predicate, groups, aggs, scope, &inner).map(Box::new)
                    })
                    .transpose()?,
                projection: projection
                    .as_deref()
                    .map(|projection| {
                        hoist_walk(projection, groups, aggs, scope, &inner).map(Box::new)
                    })
                    .transpose()?,
            }
        }
        Expr::Quantifier {
            kind,
            var,
            list,
            predicate,
        } => {
            let list = Box::new(hoist_walk(list, groups, aggs, scope, bound)?);
            let mut inner = bound.to_vec();
            inner.push(var.clone());
            Expr::Quantifier {
                kind: *kind,
                var: var.clone(),
                list,
                predicate: Box::new(hoist_walk(predicate, groups, aggs, scope, &inner)?),
            }
        }
        Expr::Reduce {
            accumulator,
            init,
            var,
            list,
            expression,
        } => {
            let init = Box::new(hoist_walk(init, groups, aggs, scope, bound)?);
            let list = Box::new(hoist_walk(list, groups, aggs, scope, bound)?);
            let mut inner = bound.to_vec();
            inner.push(accumulator.clone());
            inner.push(var.clone());
            Expr::Reduce {
                accumulator: accumulator.clone(),
                init,
                var: var.clone(),
                list,
                expression: Box::new(hoist_walk(expression, groups, aggs, scope, &inner)?),
            }
        }
        Expr::Unary { op, expr: operand } => Expr::Unary {
            op: *op,
            expr: Box::new(hoist_walk(operand, groups, aggs, scope, bound)?),
        },
        Expr::Binary { op, left, right } => Expr::Binary {
            op: *op,
            left: Box::new(hoist_walk(left, groups, aggs, scope, bound)?),
            right: Box::new(hoist_walk(right, groups, aggs, scope, bound)?),
        },
        Expr::Function {
            name,
            distinct,
            star,
            args,
        } => Expr::Function {
            name: name.clone(),
            distinct: *distinct,
            star: *star,
            args: args
                .iter()
                .map(|arg| hoist_walk(arg, groups, aggs, scope, bound))
                .collect::<Result<_>>()?,
        },
        Expr::List(items) => Expr::List(
            items
                .iter()
                .map(|item| hoist_walk(item, groups, aggs, scope, bound))
                .collect::<Result<_>>()?,
        ),
        Expr::Map(entries) => Expr::Map(
            entries
                .iter()
                .map(|(key, value)| {
                    Ok((key.clone(), hoist_walk(value, groups, aggs, scope, bound)?))
                })
                .collect::<Result<_>>()?,
        ),
        Expr::Case {
            subject,
            alternatives,
            default,
        } => Expr::Case {
            subject: subject
                .as_deref()
                .map(|subject| hoist_walk(subject, groups, aggs, scope, bound).map(Box::new))
                .transpose()?,
            alternatives: alternatives
                .iter()
                .map(|(when, then)| {
                    Ok((
                        hoist_walk(when, groups, aggs, scope, bound)?,
                        hoist_walk(then, groups, aggs, scope, bound)?,
                    ))
                })
                .collect::<Result<_>>()?,
            default: default
                .as_deref()
                .map(|default| hoist_walk(default, groups, aggs, scope, bound).map(Box::new))
                .transpose()?,
        },
        Expr::IsNull {
            expr: inner,
            negated,
        } => Expr::IsNull {
            expr: Box::new(hoist_walk(inner, groups, aggs, scope, bound)?),
            negated: *negated,
        },
        Expr::LabelCheck {
            expr: inner,
            labels,
        } => Expr::LabelCheck {
            expr: Box::new(hoist_walk(inner, groups, aggs, scope, bound)?),
            labels: labels.clone(),
        },
    })
}

/// Hoist the free variables of a pattern's inline property maps.
fn hoist_pattern(
    path: &PatternPath,
    groups: &mut Vec<(Expr, String)>,
    aggs: &[AggItem],
    scope: &FastMap<String, VarKind>,
    bound: &[String],
) -> Result<PatternPath> {
    let mut path = path.clone();
    if let Some(properties) = &path.start.properties {
        path.start.properties = Some(hoist_walk(properties, groups, aggs, scope, bound)?);
    }
    for segment in &mut path.segments {
        if let Some(properties) = &segment.relationship.properties {
            segment.relationship.properties =
                Some(hoist_walk(properties, groups, aggs, scope, bound)?);
        }
    }
    Ok(path)
}

/// Append the variables a pattern path binds to `names`.
fn pattern_variables(path: &PatternPath, names: &mut Vec<String>) {
    if let Some(name) = &path.variable {
        names.push(name.clone());
    }
    if let Some(name) = &path.start.variable {
        names.push(name.clone());
    }
    for segment in &path.segments {
        if let Some(name) = &segment.relationship.variable {
            names.push(name.clone());
        }
        if let Some(name) = &segment.node.variable {
            names.push(name.clone());
        }
    }
}

/// Whether an aggregation appears inside a comprehension body, quantifier
/// predicate or `reduce` expression, where openCypher forbids it.
fn has_aggregation_inside_comprehension(expr: &Expr) -> bool {
    match expr {
        Expr::PatternComprehension {
            path,
            predicate,
            projection,
        } => {
            pattern_has_aggregation(path)
                || predicate.as_deref().is_some_and(contains_aggregation)
                || contains_aggregation(projection)
        }
        Expr::Pattern(path) => pattern_has_aggregation(path),
        Expr::ExistsSubquery(_) => false,
        Expr::ListComprehension {
            list,
            predicate,
            projection,
            ..
        } => {
            predicate.as_deref().is_some_and(contains_aggregation)
                || projection.as_deref().is_some_and(contains_aggregation)
                || has_aggregation_inside_comprehension(list)
        }
        Expr::Quantifier {
            list, predicate, ..
        } => contains_aggregation(predicate) || has_aggregation_inside_comprehension(list),
        Expr::Reduce {
            init,
            list,
            expression,
            ..
        } => {
            contains_aggregation(expression)
                || has_aggregation_inside_comprehension(init)
                || has_aggregation_inside_comprehension(list)
        }
        Expr::Property { base, .. } | Expr::Unary { expr: base, .. } => {
            has_aggregation_inside_comprehension(base)
        }
        Expr::Subscript { base, index } => {
            has_aggregation_inside_comprehension(base)
                || has_aggregation_inside_comprehension(index)
        }
        Expr::Slice { base, lower, upper } => {
            has_aggregation_inside_comprehension(base)
                || lower
                    .as_deref()
                    .is_some_and(has_aggregation_inside_comprehension)
                || upper
                    .as_deref()
                    .is_some_and(has_aggregation_inside_comprehension)
        }
        Expr::Binary { left, right, .. } => {
            has_aggregation_inside_comprehension(left)
                || has_aggregation_inside_comprehension(right)
        }
        Expr::Function { args, .. } => args.iter().any(has_aggregation_inside_comprehension),
        Expr::List(items) => items.iter().any(has_aggregation_inside_comprehension),
        Expr::Map(entries) => entries
            .iter()
            .any(|(_, value)| has_aggregation_inside_comprehension(value)),
        Expr::Case {
            subject,
            alternatives,
            default,
        } => {
            subject
                .as_deref()
                .is_some_and(has_aggregation_inside_comprehension)
                || alternatives.iter().any(|(when, then)| {
                    has_aggregation_inside_comprehension(when)
                        || has_aggregation_inside_comprehension(then)
                })
                || default
                    .as_deref()
                    .is_some_and(has_aggregation_inside_comprehension)
        }
        Expr::IsNull { expr, .. } | Expr::LabelCheck { expr, .. } => {
            has_aggregation_inside_comprehension(expr)
        }
        Expr::Literal(_) | Expr::Parameter(_) | Expr::Variable(_) => false,
    }
}

/// Call `f` on every direct child expression of `expr`.
fn for_each_child(expr: &Expr, mut f: &mut dyn FnMut(&Expr)) {
    match expr {
        Expr::ExistsSubquery(_) => {}
        Expr::Function { args, .. } => args.iter().for_each(&mut f),
        Expr::Property { base, .. } | Expr::Unary { expr: base, .. } => f(base),
        Expr::Subscript { base, index } => {
            f(base);
            f(index);
        }
        Expr::Slice { base, lower, upper } => {
            f(base);
            for bound in [lower, upper].into_iter().flatten() {
                f(bound);
            }
        }
        Expr::ListComprehension {
            list,
            predicate,
            projection,
            ..
        } => {
            f(list);
            for part in [predicate, projection].into_iter().flatten() {
                f(part);
            }
        }
        Expr::Quantifier {
            list, predicate, ..
        } => {
            f(list);
            f(predicate);
        }
        Expr::Reduce {
            init,
            list,
            expression,
            ..
        } => {
            f(init);
            f(list);
            f(expression);
        }
        Expr::Binary { left, right, .. } => {
            f(left);
            f(right);
        }
        Expr::List(items) => items.iter().for_each(&mut f),
        Expr::Map(entries) => entries.iter().for_each(|(_, value)| f(value)),
        Expr::Case {
            subject,
            alternatives,
            default,
        } => {
            for part in subject.iter() {
                f(part);
            }
            for (when, then) in alternatives {
                f(when);
                f(then);
            }
            for part in default.iter() {
                f(part);
            }
        }
        Expr::IsNull { expr, .. } | Expr::LabelCheck { expr, .. } => f(expr),
        Expr::Pattern(path) => pattern_properties(path).iter().for_each(|expr| f(expr)),
        Expr::PatternComprehension {
            path,
            predicate,
            projection,
        } => {
            pattern_properties(path).iter().for_each(|expr| f(expr));
            if let Some(predicate) = predicate {
                f(predicate);
            }
            f(projection);
        }
        Expr::Literal(_) | Expr::Parameter(_) | Expr::Variable(_) => {}
    }
}

/// Whether any subexpression satisfies `pred` (including `expr` itself).
fn any_subexpr(expr: &Expr, pred: &mut dyn FnMut(&Expr) -> bool) -> bool {
    if pred(expr) {
        return true;
    }
    let mut found = false;
    for_each_child(expr, &mut |child| {
        if !found {
            found = any_subexpr(child, pred);
        }
    });
    found
}

/// The inline property expressions of every node and relationship of a path.
fn pattern_properties(path: &PatternPath) -> Vec<&Expr> {
    let mut out = Vec::new();
    if let Some(expr) = &path.start.properties {
        out.push(expr);
    }
    for segment in &path.segments {
        if let Some(expr) = &segment.node.properties {
            out.push(expr);
        }
        if let Some(expr) = &segment.relationship.properties {
            out.push(expr);
        }
    }
    out
}

/// The aggregate function names of the Cypher aggregation family.
const AGGREGATE_NAMES: [&str; 10] = [
    "count",
    "sum",
    "avg",
    "min",
    "max",
    "collect",
    "percentilecont",
    "percentiledisc",
    "stdev",
    "stdevp",
];

fn contains_aggregation(expr: &Expr) -> bool {
    any_subexpr(expr, &mut |sub| {
        matches!(sub, Expr::Function { name, .. }
            if AGGREGATE_NAMES.contains(&name.to_ascii_lowercase().as_str()))
    })
}

/// Whether `expr` calls one of the named functions.
fn contains_function(expr: &Expr, names: &[&str]) -> bool {
    any_subexpr(expr, &mut |sub| {
        matches!(sub, Expr::Function { name, .. }
            if names.contains(&name.to_ascii_lowercase().as_str()))
    })
}

// ---- parameter substitution ----

fn substitute_query(query: &Query, params: &HashMap<String, Value>) -> Result<Query> {
    let mut out = query.clone();
    for part in &mut out.parts {
        for clause in &mut part.clauses {
            substitute_clause(clause, params)?;
        }
    }
    Ok(out)
}

fn substitute_clause(clause: &mut Clause, params: &HashMap<String, Value>) -> Result<()> {
    match clause {
        Clause::Match(matched) => {
            for path in &mut matched.patterns {
                substitute_path(path, params)?;
            }
            if let Some(predicate) = &mut matched.where_clause {
                substitute_expr(predicate, params)?;
            }
            Ok(())
        }
        Clause::Create(patterns) => {
            for path in &mut patterns.iter_mut() {
                substitute_path(path, params)?;
            }
            Ok(())
        }
        Clause::Foreach { list, clauses, .. } => {
            substitute_expr(list, params)?;
            for clause in clauses.iter_mut() {
                substitute_clause(clause, params)?;
            }
            Ok(())
        }
        Clause::Call { args, .. } => {
            for arg in args.iter_mut() {
                substitute_expr(arg, params)?;
            }
            Ok(())
        }
        Clause::Merge {
            pattern,
            on_create,
            on_match,
        } => {
            substitute_path(pattern, params)?;
            for item in on_create.iter_mut().chain(on_match) {
                substitute_set_item(item, params)?;
            }
            Ok(())
        }
        Clause::Set(items) | Clause::Remove(items) => {
            for item in items {
                substitute_set_item(item, params)?;
            }
            Ok(())
        }
        Clause::Delete { exprs, .. } => {
            for expr in exprs {
                substitute_expr(expr, params)?;
            }
            Ok(())
        }
        Clause::With {
            items,
            order,
            skip,
            limit,
            where_clause,
            ..
        } => {
            substitute_items(items, order, skip, limit, params)?;
            if let Some(predicate) = where_clause {
                substitute_expr(predicate, params)?;
            }
            Ok(())
        }
        Clause::Return {
            items,
            order,
            skip,
            limit,
            ..
        } => substitute_items(items, order, skip, limit, params),
        Clause::Unwind { expr, .. } => substitute_expr(expr, params),
    }
}

fn substitute_items(
    items: &mut [ReturnItem],
    order: &mut [SortItem],
    skip: &mut Option<Expr>,
    limit: &mut Option<Expr>,
    params: &HashMap<String, Value>,
) -> Result<()> {
    for item in items {
        substitute_expr(&mut item.expr, params)?;
    }
    for key in order {
        substitute_expr(&mut key.expr, params)?;
    }
    // Bad count literals are compile errors; parameter values that only
    // become literals here are range checked during execution instead.
    for (bound, what) in [(skip.as_ref(), "SKIP"), (limit.as_ref(), "LIMIT")] {
        match bound {
            Some(Expr::Literal(Value::Int(value))) if *value < 0 => {
                return Err(Error::syntax(format!(
                    "NegativeIntegerArgument: {what} expects a non-negative integer"
                )));
            }
            Some(Expr::Literal(Value::Float(_))) => {
                return Err(Error::syntax(format!(
                    "InvalidArgumentType: {what} expects an integer"
                )));
            }
            _ => {}
        }
    }
    if let Some(skip) = skip {
        substitute_expr(skip, params)?;
    }
    if let Some(limit) = limit {
        substitute_expr(limit, params)?;
    }
    Ok(())
}

fn substitute_set_item(item: &mut SetItem, params: &HashMap<String, Value>) -> Result<()> {
    match item {
        SetItem::Property { value, .. }
        | SetItem::Replace { value, .. }
        | SetItem::Append { value, .. } => substitute_expr(value, params),
        SetItem::Label { .. } => Ok(()),
    }
}

fn substitute_path(path: &mut PatternPath, params: &HashMap<String, Value>) -> Result<()> {
    substitute_node(&mut path.start, params)?;
    for segment in &mut path.segments {
        if let Some(properties) = &mut segment.relationship.properties {
            substitute_expr(properties, params)?;
        }
        substitute_node(&mut segment.node, params)?;
    }
    Ok(())
}

fn substitute_node(node: &mut NodePattern, params: &HashMap<String, Value>) -> Result<()> {
    if let Some(properties) = &mut node.properties {
        substitute_expr(properties, params)?;
    }
    Ok(())
}

/// Substitute parameters inside a pattern's inline property expressions.
fn substitute_pattern(path: &mut PatternPath, params: &HashMap<String, Value>) -> Result<()> {
    if let Some(properties) = &mut path.start.properties {
        substitute_expr(properties, params)?;
    }
    for segment in &mut path.segments {
        if let Some(properties) = &mut segment.relationship.properties {
            substitute_expr(properties, params)?;
        }
    }
    Ok(())
}

fn substitute_expr(expr: &mut Expr, params: &HashMap<String, Value>) -> Result<()> {
    match expr {
        Expr::ExistsSubquery(part) => {
            for clause in &mut part.clauses {
                substitute_clause(clause, params)?;
            }
            Ok(())
        }
        Expr::Parameter(name) => {
            let value = params.get(name).cloned().ok_or_else(|| {
                Error::new(
                    ErrorCode::NotFound,
                    format!("parameter `${name}` was not provided"),
                )
            })?;
            *expr = Expr::Literal(value);
            Ok(())
        }
        Expr::Pattern(path) => substitute_pattern(path, params),
        Expr::PatternComprehension {
            path,
            predicate,
            projection,
        } => {
            substitute_pattern(path, params)?;
            if let Some(predicate) = predicate {
                substitute_expr(predicate, params)?;
            }
            substitute_expr(projection, params)
        }
        Expr::Property { base, .. } => substitute_expr(base, params),
        Expr::Subscript { base, index } => {
            substitute_expr(base, params)?;
            substitute_expr(index, params)
        }
        Expr::Slice { base, lower, upper } => {
            substitute_expr(base, params)?;
            if let Some(lower) = lower {
                substitute_expr(lower, params)?;
            }
            if let Some(upper) = upper {
                substitute_expr(upper, params)?;
            }
            Ok(())
        }
        Expr::ListComprehension {
            list,
            predicate,
            projection,
            ..
        } => {
            substitute_expr(list, params)?;
            if let Some(predicate) = predicate {
                substitute_expr(predicate, params)?;
            }
            if let Some(projection) = projection {
                substitute_expr(projection, params)?;
            }
            Ok(())
        }
        Expr::Quantifier {
            list, predicate, ..
        } => {
            substitute_expr(list, params)?;
            substitute_expr(predicate, params)
        }
        Expr::Reduce {
            init,
            list,
            expression,
            ..
        } => {
            substitute_expr(init, params)?;
            substitute_expr(list, params)?;
            substitute_expr(expression, params)
        }
        Expr::Unary { expr, .. } => substitute_expr(expr, params),
        Expr::Binary { left, right, .. } => {
            substitute_expr(left, params)?;
            substitute_expr(right, params)
        }
        Expr::Function { args, .. } => {
            for arg in args {
                substitute_expr(arg, params)?;
            }
            Ok(())
        }
        Expr::List(items) => {
            for item in items {
                substitute_expr(item, params)?;
            }
            Ok(())
        }
        Expr::Map(entries) => {
            for (_, value) in entries {
                substitute_expr(value, params)?;
            }
            Ok(())
        }
        Expr::Case {
            subject,
            alternatives,
            default,
        } => {
            if let Some(subject) = subject {
                substitute_expr(subject, params)?;
            }
            for (when, then) in alternatives {
                substitute_expr(when, params)?;
                substitute_expr(then, params)?;
            }
            if let Some(default) = default {
                substitute_expr(default, params)?;
            }
            Ok(())
        }
        Expr::IsNull { expr, .. } => substitute_expr(expr, params),
        Expr::LabelCheck { expr, .. } => substitute_expr(expr, params),
        Expr::Literal(_) | Expr::Variable(_) => Ok(()),
    }
}
