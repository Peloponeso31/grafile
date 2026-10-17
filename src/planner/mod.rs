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
    BinaryOp, Clause, Expr, MatchClause, NodePattern, PatternPath, Query, RelDirection, ReturnItem,
    SetItem, SortItem, UnaryOp,
};
use crate::engine::Database;
use crate::error::{Error, ErrorCode, Result};
use crate::record::Value;
use crate::vdbe::NO_KEY;
use plan::{AggFn, AggItem, CreateItem, IndexSeek, Plan};

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
    /// A named path; usable neither as a node nor as a relationship.
    Path,
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
        let mut plan = Plan::Empty;
        let mut scope: FastMap<String, VarKind> = FastMap::new();
        let mut columns = Vec::new();
        let mut mutating = false;
        for clause in clauses {
            match clause {
                Clause::Match(matched) => {
                    self.plan_match(matched, &mut plan, &mut scope)?;
                }
                Clause::Create(patterns) => {
                    let items = self.plan_create(patterns, &scope)?;
                    for item in &items {
                        let kind = match item {
                            CreateItem::Node { .. } => VarKind::Node,
                            CreateItem::Edge { .. } => VarKind::Edge,
                        };
                        scope.insert(item.var().to_owned(), kind);
                    }
                    plan = Plan::Create {
                        input: Box::new(plan),
                        items,
                    };
                    mutating = true;
                    columns.clear();
                }
                Clause::Merge {
                    pattern,
                    on_create,
                    on_match,
                } => {
                    self.plan_merge(pattern, on_create, on_match, &mut plan, &mut scope)?;
                    mutating = true;
                    columns.clear();
                }
                Clause::Set(items) => {
                    for item in items {
                        self.validate_set_item(item, &scope)?;
                    }
                    plan = Plan::SetItems {
                        input: Box::new(plan),
                        items: items.clone(),
                    };
                    mutating = true;
                    columns.clear();
                }
                Clause::Remove(items) => {
                    for item in items {
                        self.validate_set_item(item, &scope)?;
                    }
                    plan = Plan::RemoveItems {
                        input: Box::new(plan),
                        items: items.clone(),
                    };
                    mutating = true;
                    columns.clear();
                }
                Clause::Delete { detach, exprs } => {
                    for expr in exprs {
                        validate_expr(expr, &scope)?;
                    }
                    plan = Plan::Delete {
                        input: Box::new(plan),
                        exprs: exprs.clone(),
                        detach: *detach,
                    };
                    mutating = true;
                    columns.clear();
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
                        plan, items, *distinct, order, skip, limit, None, &scope,
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
            validate_expr(predicate, scope)?;
            *plan = Plan::Filter {
                input: Box::new(plan.clone()),
                predicate: predicate.clone(),
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
            validate_expr(predicate, &inner_scope)?;
            inner = Plan::Filter {
                input: Box::new(inner),
                predicate: predicate.clone(),
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

    fn plan_path(
        &mut self,
        path: &PatternPath,
        plan: &mut Plan,
        scope: &mut FastMap<String, VarKind>,
    ) -> Result<()> {
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
            });
        }

        let mut bound: Vec<bool> = nodes
            .iter()
            .map(|node| scope.contains_key(&node.var))
            .collect();
        let mut scanned = vec![false; nodes.len()];
        let mut handled = vec![false; rels.len()];
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
                let direction = if bound[from] && !bound[to] {
                    Some((from, to, rel.direction))
                } else if bound[to] && !bound[from] {
                    Some((to, from, flip(rel.direction)))
                } else {
                    None
                };
                let Some((from_idx, to_idx, direction)) = direction else {
                    continue;
                };
                let edge_bound =
                    self.bound_relationship(&rel.var, rel.var_length.is_some(), scope)?;
                let edge_var = if edge_bound {
                    self.anon_name("rel")
                } else {
                    scope.insert(rel.var.clone(), VarKind::Edge);
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
                bound[to_idx] = true;
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
                    scope.insert(rel.var.clone(), VarKind::Edge);
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
            let mut elements = vec![(nodes[0].var.clone(), false)];
            for (idx, rel) in rels.iter().enumerate() {
                elements.push((rel.var.clone(), rel.var_length.is_some()));
                elements.push((nodes[idx + 1].var.clone(), false));
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
            VarKind::Edge => {}
            VarKind::Node | VarKind::Path => {
                return Err(Error::syntax(format!(
                    "variable `{var}` is already bound to a {}",
                    if *kind == VarKind::Node {
                        "node"
                    } else {
                        "path"
                    }
                )));
            }
        }
        if !var_length && self.pattern_rels.contains(var) {
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
                    && matches!(kind, VarKind::Edge | VarKind::Path)
                {
                    return Err(Error::syntax(format!(
                        "variable `{var}` is already bound to a {}",
                        if *kind == VarKind::Edge {
                            "relationship"
                        } else {
                            "path"
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
            let start = self.create_node_item(&path.start, &local_scope)?;
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
                let node_item = self.create_node_item(&segment.node, &local_scope)?;
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
                if rel.types.len() != 1 {
                    return Err(Error::syntax(
                        "relationships in CREATE must have exactly one type",
                    ));
                }
                if rel.var_length.is_some() {
                    return Err(Error::unsupported(
                        "variable-length relationships cannot be created",
                    ));
                }
                let var = match &rel.variable {
                    Some(var) => var.clone(),
                    None => self.anon_name("rel"),
                };
                let (props, prop_exprs) =
                    self.create_props(rel.properties.as_ref(), &local_scope)?;
                local_scope.insert(var.clone(), VarKind::Edge);
                items.push(CreateItem::Edge {
                    var,
                    edge_type: rel.types[0].clone(),
                    src: prev_var,
                    dst: next_var.clone(),
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
    ) -> Result<Option<CreateItem>> {
        let var = match &pattern.variable {
            Some(var) if scope.contains_key(var) => return Ok(None),
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
        let start_item = self.create_node_item(&path.start, &local)?;
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
            let node_item = self.create_node_item(&segment.node, &local)?;
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
    ) -> Result<(Plan, FastMap<String, VarKind>, Vec<String>)> {
        // Expand `*` items.
        let mut expanded: Vec<(Expr, String)> = Vec::new();
        for item in items {
            if item.star {
                for (var, _) in scope.iter() {
                    if var.starts_with("  ") {
                        continue;
                    }
                    expanded.push((Expr::Variable(var.clone()), var.clone()));
                }
            } else {
                let name = match &item.alias {
                    Some(alias) => alias.clone(),
                    None => default_name(&item.expr),
                };
                expanded.push((item.expr.clone(), name));
            }
        }
        if expanded.is_empty() {
            return Err(Error::syntax("RETURN/WITH requires at least one item"));
        }
        for (expr, _) in &expanded {
            validate_expr(expr, scope)?;
        }

        // A `WITH ... WHERE` may reference variables of the incoming scope
        // (not only the projected aliases); those predicates filter before
        // the projection. Aliases take precedence when they resolve.
        let mut input = input;
        let mut filter_after = None;
        if let Some(predicate) = where_clause {
            let projected: FastMap<String, VarKind> = expanded
                .iter()
                .map(|(expr, name)| (name.clone(), expr_kind(expr, scope)))
                .collect();
            if validate_expr(predicate, &projected).is_ok() {
                filter_after = Some(predicate.clone());
            } else {
                validate_expr(predicate, scope)?;
                input = Plan::Filter {
                    input: Box::new(input),
                    predicate: predicate.clone(),
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
        for (expr, name) in &expanded {
            if has_aggregation_inside_comprehension(expr) {
                return Err(Error::syntax(
                    "aggregations are not allowed inside list comprehensions",
                ));
            }
            if let Some((func, argument, star, distinct)) = aggregate_call(expr)? {
                aggs.push(AggItem {
                    func,
                    argument: aggregate_argument(func, argument, star, scope)?,
                    distinct,
                    name: name.clone(),
                });
            } else if contains_aggregation(expr) {
                let mut residual = rewrite_expr(expr, &mut |sub| {
                    let Some((func, argument, star, distinct)) = aggregate_call(sub)? else {
                        return Ok(None);
                    };
                    let hidden = format!("  agg{}", aggs.len());
                    aggs.push(AggItem {
                        func,
                        argument: aggregate_argument(func, argument, star, scope)?,
                        distinct,
                        name: hidden.clone(),
                    });
                    Ok(Some(Expr::Variable(hidden)))
                })?;
                residual = rewrite_expr(&residual, &mut |sub| {
                    if let Some((_, group)) = groups.iter().find(|(group, _)| group == sub) {
                        return Ok(Some(Expr::Variable(group.clone())));
                    }
                    if references_unresolved(sub, &groups, &aggs)
                        && !references_aggregation(sub, &aggs)
                    {
                        let hidden = format!("  grp{}", groups.len());
                        validate_expr(sub, scope)?;
                        groups.push((sub.clone(), hidden.clone()));
                        return Ok(Some(Expr::Variable(hidden)));
                    }
                    Ok(None)
                })?;
                post.push((residual, name.clone()));
            } else {
                groups.push((expr.clone(), name.clone()));
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
                .map(|(expr, name)| (name.clone(), expr_kind(expr, scope)))
                .collect();
            for key in order {
                let expr =
                    if let Some((_, alias)) = expanded.iter().find(|(expr, _)| expr == &key.expr) {
                        Expr::Variable(alias.clone())
                    } else if validate_expr(&key.expr, &visible_names).is_ok() {
                        key.expr.clone()
                    } else if aggs.is_empty() && !distinct {
                        validate_expr(&key.expr, scope)?;
                        let hidden = format!("  sort{}", hidden_keys.len());
                        hidden_keys.push((key.expr.clone(), hidden.clone()));
                        Expr::Variable(hidden)
                    } else if aggs.is_empty() {
                        return Err(Error::unsupported(
                            "ORDER BY on non-projected values cannot be combined with DISTINCT",
                        ));
                    } else {
                        return Err(Error::unsupported(
                            "ORDER BY after aggregation must reference projected values",
                        ));
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
                new_scope.insert(name.clone(), expr_kind(expr, scope));
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
            validate_expr(skip, &new_scope)?;
            plan = Plan::Skip {
                input: Box::new(plan),
                count: skip.clone(),
            };
        }
        if let Some(limit) = limit {
            validate_expr(limit, &new_scope)?;
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
            output_scope.insert(name.clone(), expr_kind(expr, scope));
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

fn undefined_variable(name: &str) -> Error {
    Error::new(
        ErrorCode::NotFound,
        format!("variable `{name}` is not defined"),
    )
}

/// Validate that all variables in an expression are in scope.
fn validate_expr(expr: &Expr, scope: &FastMap<String, VarKind>) -> Result<()> {
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

fn expr_kind(expr: &Expr, scope: &FastMap<String, VarKind>) -> VarKind {
    match expr {
        Expr::Variable(name) => scope.get(name).copied().unwrap_or(VarKind::Value),
        _ => VarKind::Value,
    }
}

fn default_name(expr: &Expr) -> String {
    match expr {
        Expr::Variable(name) => name.clone(),
        Expr::Parameter(name) => format!("${name}"),
        Expr::Literal(value) => literal_text(value),
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
            UnaryOp::Neg => format!("-{}", default_name(expr)),
            UnaryOp::Not => format!("NOT {}", default_name(expr)),
        },
        Expr::Binary { op, left, right } => format!(
            "{} {} {}",
            default_name(left),
            binary_text(*op),
            default_name(right)
        ),
        Expr::IsNull { expr, negated } => format!(
            "{} IS {}NULL",
            default_name(expr),
            if *negated { "NOT " } else { "" }
        ),
        Expr::LabelCheck { expr, labels } => {
            format!("{}:{}", default_name(expr), labels.join(":"))
        }
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
        Value::Float(inner) => format!("{inner}"),
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
/// function, optional argument and star flag.
#[allow(clippy::type_complexity)]
fn aggregate_call(expr: &Expr) -> Result<Option<(AggFn, Option<&Expr>, bool, bool)>> {
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
        _ => return Ok(None),
    };
    if *star {
        if func != AggFn::Count {
            return Err(Error::syntax("only count(*) accepts *"));
        }
        if *distinct {
            return Err(Error::syntax("count(DISTINCT *) is not valid"));
        }
        return Ok(Some((func, None, true, false)));
    }
    match args.len() {
        1 => Ok(Some((func, Some(&args[0]), false, *distinct))),
        0 => Err(Error::syntax(format!("{name} requires an argument"))),
        _ => Err(Error::unsupported("aggregations with multiple arguments")),
    }
}

/// The argument of an aggregation, with `count(*)` becoming `count(1)`.
fn aggregate_argument(
    func: AggFn,
    argument: Option<&Expr>,
    star: bool,
    scope: &FastMap<String, VarKind>,
) -> Result<Expr> {
    if star {
        return Ok(Expr::Literal(Value::Int(1)));
    }
    let argument =
        argument.ok_or_else(|| Error::syntax(format!("{func:?} requires an argument")))?;
    validate_expr(argument, scope)?;
    Ok(argument.clone())
}

/// Rewrite an expression top-down; `f` may replace a subexpression and stop
/// the descent by returning `Some`.
fn rewrite_expr(expr: &Expr, f: &mut dyn FnMut(&Expr) -> Result<Option<Expr>>) -> Result<Expr> {
    if let Some(replacement) = f(expr)? {
        return Ok(replacement);
    }
    Ok(match expr {
        Expr::Literal(_) | Expr::Parameter(_) | Expr::Variable(_) => expr.clone(),
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

/// Whether any variable reference in `expr` satisfies `pred`.
fn expr_references(expr: &Expr, pred: &mut dyn FnMut(&str) -> bool) -> bool {
    match expr {
        Expr::Variable(name) => pred(name),
        Expr::Literal(_) | Expr::Parameter(_) => false,
        Expr::Property { base, .. } => expr_references(base, pred),
        Expr::Subscript { base, index } => {
            expr_references(base, pred) || expr_references(index, pred)
        }
        Expr::Slice { base, lower, upper } => {
            expr_references(base, pred)
                || lower.as_deref().is_some_and(|e| expr_references(e, pred))
                || upper.as_deref().is_some_and(|e| expr_references(e, pred))
        }
        Expr::ListComprehension {
            list,
            predicate,
            projection,
            ..
        } => {
            expr_references(list, pred)
                || predicate
                    .as_deref()
                    .is_some_and(|e| expr_references(e, pred))
                || projection
                    .as_deref()
                    .is_some_and(|e| expr_references(e, pred))
        }
        Expr::Quantifier {
            list, predicate, ..
        } => expr_references(list, pred) || expr_references(predicate, pred),
        Expr::Reduce {
            init,
            list,
            expression,
            ..
        } => {
            expr_references(init, pred)
                || expr_references(list, pred)
                || expr_references(expression, pred)
        }
        Expr::Unary { expr, .. } => expr_references(expr, pred),
        Expr::Binary { left, right, .. } => {
            expr_references(left, pred) || expr_references(right, pred)
        }
        Expr::Function { args, .. } => args.iter().any(|arg| expr_references(arg, pred)),
        Expr::List(items) => items.iter().any(|item| expr_references(item, pred)),
        Expr::Map(entries) => entries
            .iter()
            .any(|(_, value)| expr_references(value, pred)),
        Expr::Case {
            subject,
            alternatives,
            default,
        } => {
            subject
                .as_deref()
                .is_some_and(|subject| expr_references(subject, pred))
                || alternatives
                    .iter()
                    .any(|(when, then)| expr_references(when, pred) || expr_references(then, pred))
                || default
                    .as_deref()
                    .is_some_and(|default| expr_references(default, pred))
        }
        Expr::IsNull { expr, .. } | Expr::LabelCheck { expr, .. } => expr_references(expr, pred),
    }
}

/// Whether `expr` references a variable that is not a grouping key or an
/// aggregation output.
fn references_unresolved(expr: &Expr, groups: &[(Expr, String)], aggs: &[AggItem]) -> bool {
    expr_references(expr, &mut |name| {
        !groups.iter().any(|(_, group)| group == name) && !aggs.iter().any(|item| item.name == name)
    })
}

/// Whether `expr` references an aggregation output.
fn references_aggregation(expr: &Expr, aggs: &[AggItem]) -> bool {
    expr_references(expr, &mut |name| aggs.iter().any(|item| item.name == name))
}

/// Whether an aggregation appears inside a comprehension body, quantifier
/// predicate or `reduce` expression, where openCypher forbids it.
fn has_aggregation_inside_comprehension(expr: &Expr) -> bool {
    match expr {
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

fn contains_aggregation(expr: &Expr) -> bool {
    match expr {
        Expr::Function { name, args, .. } => {
            let lower = name.to_ascii_lowercase();
            matches!(
                lower.as_str(),
                "count" | "sum" | "avg" | "min" | "max" | "collect"
            ) || args.iter().any(contains_aggregation)
        }
        Expr::Property { base, .. } | Expr::Unary { expr: base, .. } => contains_aggregation(base),
        Expr::Subscript { base, index } => {
            contains_aggregation(base) || contains_aggregation(index)
        }
        Expr::Slice { base, lower, upper } => {
            contains_aggregation(base)
                || lower.as_deref().is_some_and(contains_aggregation)
                || upper.as_deref().is_some_and(contains_aggregation)
        }
        Expr::ListComprehension {
            list,
            predicate,
            projection,
            ..
        } => {
            contains_aggregation(list)
                || predicate.as_deref().is_some_and(contains_aggregation)
                || projection.as_deref().is_some_and(contains_aggregation)
        }
        Expr::Quantifier {
            list, predicate, ..
        } => contains_aggregation(list) || contains_aggregation(predicate),
        Expr::Reduce {
            init,
            list,
            expression,
            ..
        } => {
            contains_aggregation(init)
                || contains_aggregation(list)
                || contains_aggregation(expression)
        }
        Expr::Binary { left, right, .. } => {
            contains_aggregation(left) || contains_aggregation(right)
        }
        Expr::List(items) => items.iter().any(contains_aggregation),
        Expr::Map(entries) => entries.iter().any(|(_, value)| contains_aggregation(value)),
        Expr::Case {
            subject,
            alternatives,
            default,
        } => {
            subject.as_ref().is_some_and(|s| contains_aggregation(s))
                || alternatives
                    .iter()
                    .any(|(a, b)| contains_aggregation(a) || contains_aggregation(b))
                || default.as_ref().is_some_and(|d| contains_aggregation(d))
        }
        Expr::IsNull { expr, .. } | Expr::LabelCheck { expr, .. } => contains_aggregation(expr),
        Expr::Literal(_) | Expr::Parameter(_) | Expr::Variable(_) => false,
    }
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

fn substitute_expr(expr: &mut Expr, params: &HashMap<String, Value>) -> Result<()> {
    match expr {
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
