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

use hashbrown::HashMap as FastMap;

use crate::cypher::{
    Clause, Expr, MatchClause, NodePattern, PatternPath, Query, RelDirection, ReturnItem, SetItem,
    SortItem,
};
use crate::engine::Database;
use crate::error::{Error, ErrorCode, Result};
use crate::record::Value;
use crate::vdbe::NO_KEY;
use plan::{AggFn, CreateItem, IndexSeek, Plan};

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
}

/// Planner state.
struct Planner<'a> {
    db: &'a mut Database,
    anon_counter: u32,
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
    };
    planner.run(&query)
}

impl<'a> Planner<'a> {
    fn run(&mut self, query: &Query) -> Result<Prepared> {
        let mut plan = Plan::Empty;
        let mut scope: FastMap<String, VarKind> = FastMap::new();
        let mut columns = Vec::new();
        let mut mutating = false;
        for clause in &query.clauses {
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
                    let items = self.plan_merge(pattern, &scope)?;
                    for item in &items {
                        let kind = match item {
                            CreateItem::Node { .. } => VarKind::Node,
                            CreateItem::Edge { .. } => VarKind::Edge,
                        };
                        scope.insert(item.var().to_owned(), kind);
                    }
                    for set_item in on_create.iter().chain(on_match) {
                        self.validate_set_item(set_item, &scope)?;
                    }
                    plan = Plan::Merge {
                        input: Box::new(plan),
                        items,
                        on_create: on_create.clone(),
                        on_match: on_match.clone(),
                    };
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
                    let (new_plan, new_scope, _) =
                        self.plan_projection(plan, items, *distinct, order, skip, limit, &scope)?;
                    plan = new_plan;
                    scope = new_scope;
                    if let Some(predicate) = where_clause {
                        validate_expr(predicate, &scope)?;
                        plan = Plan::Filter {
                            input: Box::new(plan),
                            predicate: predicate.clone(),
                        };
                    }
                    columns.clear();
                }
                Clause::Return {
                    distinct,
                    items,
                    order,
                    skip,
                    limit,
                } => {
                    let (new_plan, _, new_columns) =
                        self.plan_projection(plan, items, *distinct, order, skip, limit, &scope)?;
                    plan = new_plan;
                    columns = new_columns;
                    scope.clear();
                }
            }
        }
        Ok(Prepared {
            plan,
            columns,
            mutating,
        })
    }

    // ---- MATCH ----

    fn plan_match(
        &mut self,
        clause: &MatchClause,
        plan: &mut Plan,
        scope: &mut FastMap<String, VarKind>,
    ) -> Result<()> {
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
            types: Vec<u32>,
            direction: RelDirection,
            props: Vec<(u32, Value)>,
        }
        let mut rels: Vec<RelInfo> = Vec::new();
        for segment in &path.segments {
            let rel = &segment.relationship;
            if rel.var_length.is_some() {
                return Err(Error::unsupported(
                    "variable-length relationship patterns are not supported yet",
                ));
            }
            let node = self.planned_node(&segment.node, scope)?;
            nodes.push(node);
            let var = match &rel.variable {
                Some(var) => var.clone(),
                None => self.anon_name("rel"),
            };
            let types = self.resolve_types(&rel.types);
            let props = self.pattern_props(rel.properties.as_ref(), "relationship")?;
            rels.push(RelInfo {
                index: nodes.len() - 2,
                var,
                types,
                direction: rel.direction,
                props,
            });
        }

        let mut bound: Vec<bool> = nodes
            .iter()
            .map(|node| scope.contains_key(&node.var))
            .collect();
        // Bind edges for anonymous relationships when expanded.
        loop {
            if bound.iter().all(|b| *b) {
                break;
            }
            // Try to expand from a bound node.
            let mut expanded = false;
            for rel in &rels {
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
                let from_var = nodes[from_idx].var.clone();
                let to_var = nodes[to_idx].var.clone();
                if !scope.contains_key(&rel.var) {
                    scope.insert(rel.var.clone(), VarKind::Edge);
                }
                *plan = Plan::Expand {
                    input: Box::new(plan.clone()),
                    src: from_var,
                    edge: Some(rel.var.clone()),
                    dst: to_var.clone(),
                    direction,
                    types: rel.types.clone(),
                    props: rel.props.clone(),
                };
                scope.insert(to_var, VarKind::Node);
                bound[to_idx] = true;
                expanded = true;
                break;
            }
            if expanded {
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
        }
        Ok(())
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
        node.props.iter().any(|(key_id, _)| {
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
    ) -> Result<(Option<u32>, Option<IndexSeek>, Vec<(u32, Value)>)> {
        let schema = self.db.schema();
        // Prefer an index seek on a literal property.
        for (key_id, value) in &node.props {
            let index = schema.node_indexes.iter().find(|index| {
                index.key_id == *key_id
                    && (index.label_id == 0 || node.label_ids.contains(&index.label_id))
            });
            if let Some(index) = index {
                let remaining: Vec<(u32, Value)> = node
                    .props
                    .iter()
                    .filter(|(key, _)| *key != *key_id)
                    .cloned()
                    .collect();
                return Ok((
                    node.label_ids.first().copied(),
                    Some(IndexSeek {
                        index_id: index.id,
                        key_id: *key_id,
                        value: value.clone(),
                    }),
                    remaining,
                ));
            }
        }
        Ok((node.label_ids.first().copied(), None, node.props.clone()))
    }

    fn planned_node(
        &mut self,
        pattern: &NodePattern,
        scope: &FastMap<String, VarKind>,
    ) -> Result<PlannedNode> {
        let var = match &pattern.variable {
            Some(var) => var.clone(),
            None => self.anon_name("node"),
        };
        let mut label_ids = Vec::new();
        for label in &pattern.labels {
            // A missing label yields an impossible id so the scan is empty
            // but the variable is still bound.
            label_ids.push(self.db.label_id(label).unwrap_or(u32::MAX));
        }
        let props = self.pattern_props(pattern.properties.as_ref(), "node")?;
        let _ = scope;
        Ok(PlannedNode {
            var,
            label_ids,
            props,
        })
    }

    fn pattern_props(&self, properties: Option<&Expr>, what: &str) -> Result<Vec<(u32, Value)>> {
        let Some(properties) = properties else {
            return Ok(Vec::new());
        };
        let Expr::Map(entries) = properties else {
            return Err(Error::unsupported(format!(
                "{what} pattern properties must be a map literal"
            )));
        };
        let mut props = Vec::new();
        for (key, value) in entries {
            let Expr::Literal(value) = value else {
                return Err(Error::unsupported(format!(
                    "{what} pattern property values must be literals in this version"
                )));
            };
            if value.is_null() {
                continue;
            }
            let key_id = self.db.property_key_id(key).unwrap_or(NO_KEY);
            props.push((key_id, value.clone()));
        }
        Ok(props)
    }

    fn resolve_types(&self, types: &[String]) -> Vec<u32> {
        if types.is_empty() {
            return Vec::new();
        }
        let mut ids: Vec<u32> = types
            .iter()
            .filter_map(|name| self.db.edge_type_id(name))
            .collect();
        if ids.is_empty() {
            // No such type: an impossible id keeps the variable bound while
            // yielding no rows.
            ids.push(u32::MAX);
        }
        ids
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
                if !value.is_null() {
                    literals.push((key.clone(), value.clone()));
                }
            } else {
                validate_expr(value, scope)?;
                exprs.push((key.clone(), value.clone()));
            }
        }
        Ok((literals, exprs))
    }

    fn plan_merge(
        &mut self,
        path: &PatternPath,
        scope: &FastMap<String, VarKind>,
    ) -> Result<Vec<CreateItem>> {
        if path.segments.is_empty() {
            let item = self.create_node_item(&path.start, scope)?;
            return Ok(item.into_iter().collect());
        }
        if path.segments.len() > 1 {
            return Err(Error::unsupported(
                "MERGE with multiple relationships is not supported yet",
            ));
        }
        let segment = &path.segments[0];
        let mut items = Vec::new();
        let mut local = scope.clone();
        // Unbound endpoints are merged as nodes first.
        let start_item = self.create_node_item(&path.start, &local)?;
        let src = match &start_item {
            Some(item) => {
                local.insert(item.var().to_owned(), VarKind::Node);
                item.var().to_owned()
            }
            None => path
                .start
                .variable
                .clone()
                .ok_or_else(|| Error::unsupported("MERGE requires a variable or new node"))?,
        };
        if let Some(item) = start_item {
            items.push(item);
        }
        let node_item = self.create_node_item(&segment.node, &local)?;
        let dst = match &node_item {
            Some(item) => {
                local.insert(item.var().to_owned(), VarKind::Node);
                item.var().to_owned()
            }
            None => segment
                .node
                .variable
                .clone()
                .ok_or_else(|| Error::unsupported("MERGE requires a variable or new node"))?,
        };
        if let Some(item) = node_item {
            items.push(item);
        }
        let rel = &segment.relationship;
        if rel.types.len() != 1 {
            return Err(Error::syntax(
                "relationships in MERGE must have exactly one type",
            ));
        }
        if rel.var_length.is_some() {
            return Err(Error::unsupported(
                "variable-length relationships cannot be merged",
            ));
        }
        let (src, dst) = match rel.direction {
            RelDirection::Outgoing => (src, dst),
            RelDirection::Incoming => (dst, src),
            RelDirection::Undirected => (src, dst),
        };
        let var = match &rel.variable {
            Some(var) => var.clone(),
            None => self.anon_name("rel"),
        };
        let (props, prop_exprs) = self.create_props(rel.properties.as_ref(), scope)?;
        items.push(CreateItem::Edge {
            var,
            edge_type: rel.types[0].clone(),
            src,
            dst,
            props,
            prop_exprs,
        });
        Ok(items)
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
    fn plan_projection(
        &mut self,
        input: Plan,
        items: &[ReturnItem],
        distinct: bool,
        order: &[SortItem],
        skip: &Option<Expr>,
        limit: &Option<Expr>,
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

        let mut groups = Vec::new();
        let mut aggs: Vec<(AggFn, Expr, String)> = Vec::new();
        for (expr, name) in &expanded {
            if let Some((func, argument, star)) = aggregate_call(expr)? {
                if star {
                    aggs.push((func, Expr::Literal(Value::Int(1)), name.clone()));
                } else {
                    let argument = argument
                        .ok_or_else(|| Error::syntax(format!("{func:?} requires an argument")))?;
                    validate_expr(argument, scope)?;
                    aggs.push((func, argument.clone(), name.clone()));
                }
            } else if contains_aggregation(expr) {
                return Err(Error::unsupported(
                    "expressions mixing aggregation and non-aggregated values are not supported yet",
                ));
            } else {
                groups.push((expr.clone(), name.clone()));
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
            for (_, _, name) in &aggs {
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
    props: Vec<(u32, Value)>,
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
        Expr::Property { key, .. } => key.clone(),
        Expr::Function { name, .. } => name.clone(),
        Expr::Literal(Value::Text(text)) => text.to_string(),
        other => format!("{other:?}"),
    }
}

/// Whether an expression is exactly an aggregation call, returning the
/// function, optional argument and star flag.
fn aggregate_call(expr: &Expr) -> Result<Option<(AggFn, Option<&Expr>, bool)>> {
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
    if *distinct {
        return Err(Error::unsupported(
            "DISTINCT inside aggregations is not supported yet",
        ));
    }
    if *star {
        if func != AggFn::Count {
            return Err(Error::syntax("only count(*) accepts *"));
        }
        return Ok(Some((func, None, true)));
    }
    match args.len() {
        1 => Ok(Some((func, Some(&args[0]), false))),
        0 => Err(Error::syntax(format!("{name} requires an argument"))),
        _ => Err(Error::unsupported("aggregations with multiple arguments")),
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
    for clause in &mut out.clauses {
        substitute_clause(clause, params)?;
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
        SetItem::Property { value, .. } | SetItem::Replace { value, .. } => {
            substitute_expr(value, params)
        }
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
