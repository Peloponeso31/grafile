//! Logical plan representation.
//!
//! The planner turns a parsed [`crate::cypher::Query`] into a tree of [`Plan`]
//! operators. Pattern elements (scans and expansions) form the streaming
//! core; `Project`, `Aggregate`, `Sort`, `Distinct`, `Skip`, `Limit` and
//! `Unwind` shape result pipelines; write operators mutate the graph.

use crate::cypher::{Expr, RelDirection, SetItem};

/// Aggregation functions supported by the v1 engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggFn {
    /// `count(*)` or `count(x)`.
    Count,
    /// `sum(x)`
    Sum,
    /// `avg(x)`
    Avg,
    /// `min(x)`
    Min,
    /// `max(x)`
    Max,
    /// `collect(x)`
    Collect,
    /// `percentileCont(value, percentile)`
    PercentileCont,
    /// `percentileDisc(value, percentile)`
    PercentileDisc,
    /// `stDev(x)`
    StDev,
    /// `stDevP(x)`
    StDevP,
}

/// One aggregation in an [`Plan::Aggregate`].
#[derive(Debug, Clone, PartialEq)]
pub struct AggItem {
    /// Aggregation function.
    pub func: AggFn,
    /// Argument expressions, in written order.
    pub arguments: Vec<Expr>,
    /// Whether `DISTINCT` was written.
    pub distinct: bool,
    /// Output name.
    pub name: String,
}

/// A property index seek chosen by the planner.
#[derive(Debug, Clone, PartialEq)]
pub struct IndexSeek {
    /// Node index id.
    pub index_id: u32,
    /// Indexed property key id.
    pub key_id: u32,
    /// Literal lookup value.
    pub value: crate::record::Value,
}

/// One element of a [`Plan::BindPath`] sequence.
#[derive(Debug, Clone, PartialEq)]
pub enum PathElement {
    /// A single bound value (a node, or a fixed relationship).
    Value(String),
    /// A variable-length segment: the relationship list and the node list
    /// (one more node than relationships), interleaved into the path.
    Segment {
        /// Variable bound to the relationship list.
        edges: String,
        /// Variable bound to the node list.
        nodes: String,
    },
}

/// An operator in the logical plan.
#[derive(Debug, Clone, PartialEq)]
pub enum Plan {
    /// Produces a single row with no bindings.
    Empty,
    /// Produces no rows (e.g. a label that does not exist).
    EmptyResult,
    /// Scan nodes carrying `label`, optionally via an index seek, keeping only
    /// nodes whose properties match `props`.
    NodeScan {
        /// Input rows (cross-product nesting).
        input: Box<Plan>,
        /// Variable to bind.
        var: String,
        /// Label name, or `None` for any label.
        label: Option<String>,
        /// Index seek when a usable index exists.
        seek: Option<IndexSeek>,
        /// Residual literal property filters, by key name.
        props: Vec<(String, crate::record::Value)>,
    },
    /// Scan every node.
    AllNodesScan {
        /// Input rows (cross-product nesting).
        input: Box<Plan>,
        /// Variable to bind.
        var: String,
    },
    /// Scan edges of a type (or every edge).
    EdgeScan {
        /// Input rows (cross-product nesting).
        input: Box<Plan>,
        /// Variable to bind.
        var: String,
        /// Edge type id, or `None` for any type.
        edge_type: Option<u32>,
    },
    /// Expand from a bound node through trails of bounded length.
    VarExpand {
        /// Input rows (cross-product nesting).
        input: Box<Plan>,
        /// Bound source variable.
        src: String,
        /// Optional variable for the edge list.
        edge: Option<String>,
        /// Variable for the end node.
        dst: String,
        /// Pattern direction.
        direction: RelDirection,
        /// Restricting edge type names; `None` means any type.
        types: Option<Vec<String>>,
        /// Literal property filters applied at every hop, by key name.
        props: Vec<(String, crate::record::Value)>,
        /// Minimum hops.
        min: u32,
        /// Maximum hops; `None` is unbounded.
        max: Option<u32>,
        /// Whether the trail runs against the pattern direction (the plan
        /// expands from the pattern's end), so the bound edge list has to
        /// be reversed to read from the pattern's start.
        reverse: bool,
        /// Optional variable bound to the traversal's node list.
        nodes: Option<String>,
    },
    /// Expand from a bound node to its neighbors.
    Expand {
        /// Input rows (cross-product nesting).
        input: Box<Plan>,
        /// Bound source variable.
        src: String,
        /// Optional variable for the edge.
        edge: Option<String>,
        /// Variable for the neighbor.
        dst: String,
        /// Pattern direction.
        direction: RelDirection,
        /// Restricting edge type names; `None` means any type.
        types: Option<Vec<String>>,
        /// Literal property filters on the edge, by key name.
        props: Vec<(String, crate::record::Value)>,
    },
    /// Filter rows by a predicate.
    Filter {
        /// Input plan.
        input: Box<Plan>,
        /// Predicate expression.
        predicate: Expr,
    },
    /// Concatenate the rows of several query parts (`UNION` / `UNION ALL`).
    Union {
        /// Query parts, each producing the same output columns.
        parts: Vec<Plan>,
        /// Whether this is `UNION ALL` (keep duplicates).
        all: bool,
    },
    /// Bind a variable to the path built from already bound elements.
    BindPath {
        /// Input plan.
        input: Box<Plan>,
        /// Variable to bind.
        var: String,
        /// Elements in path order.
        elements: Vec<PathElement>,
    },
    /// Run updating clauses once per element of a list.
    Foreach {
        /// Input plan.
        input: Box<Plan>,
        /// Loop variable.
        var: String,
        /// List expression.
        list: Expr,
        /// Body, evaluated per element.
        plan: Box<Plan>,
    },
    /// Call a built-in procedure and bind its outputs.
    ProcedureCall {
        /// Input plan.
        input: Box<Plan>,
        /// Dotted procedure name.
        procedure: String,
        /// Output columns in order.
        columns: Vec<String>,
        /// `(column, alias)` pairs to bind.
        yields: Vec<(String, String)>,
    },
    /// Shortest-path search from one endpoint to the other.
    ShortestPath {
        /// Input plan.
        input: Box<Plan>,
        /// Source node variable.
        src: String,
        /// Target node variable.
        dst: String,
        /// Relationship variable bound to the edge list.
        edge: Option<String>,
        /// Direction.
        direction: crate::cypher::RelDirection,
        /// Restricting edge type names; `None` means any type.
        types: Option<Vec<String>>,
        /// Literal property filters applied at every hop.
        props: Vec<(String, crate::record::Value)>,
        /// Minimum hops.
        min: u32,
        /// Maximum hops; `None` is unbounded.
        max: Option<u32>,
        /// Whether every path of minimal length is produced.
        all: bool,
        /// Optional variable bound to the path's node list.
        nodes: Option<String>,
    },
    /// Pattern predicate: binds `var` to whether the inner plan matches.
    PatternPredicate {
        /// Input plan.
        input: Box<Plan>,
        /// Pattern plan, rooted at `Empty`, referencing input variables.
        inner: Box<Plan>,
        /// Hidden boolean variable.
        var: String,
    },
    /// Pattern comprehension: binds `var` to the projected list.
    PatternComprehension {
        /// Input plan.
        input: Box<Plan>,
        /// Pattern plan, rooted at `Empty`, referencing input variables.
        inner: Box<Plan>,
        /// Value produced per match.
        project: Expr,
        /// Hidden list variable.
        var: String,
    },
    /// Match a pattern per input row, keeping the row with null bindings when
    /// the pattern does not match.
    OptionalMatch {
        /// Input plan.
        input: Box<Plan>,
        /// Pattern plan, rooted at `Empty`, referencing input variables.
        inner: Box<Plan>,
        /// Variables the pattern introduces; null-filled when it misses.
        vars: Vec<String>,
    },
    /// Compute output columns.
    Project {
        /// Input plan.
        input: Box<Plan>,
        /// `(expression, output name)` pairs.
        items: Vec<(Expr, String)>,
    },
    /// Group and aggregate.
    Aggregate {
        /// Input plan.
        input: Box<Plan>,
        /// `(expression, output name)` grouping keys.
        groups: Vec<(Expr, String)>,
        /// Aggregations.
        aggs: Vec<AggItem>,
    },
    /// Remove duplicate rows.
    Distinct {
        /// Input plan.
        input: Box<Plan>,
    },
    /// Sort rows.
    Sort {
        /// Input plan.
        input: Box<Plan>,
        /// `(expression, ascending)` sort keys.
        keys: Vec<(Expr, bool)>,
    },
    /// Skip rows.
    Skip {
        /// Input plan.
        input: Box<Plan>,
        /// Row count expression.
        count: Expr,
    },
    /// Limit rows.
    Limit {
        /// Input plan.
        input: Box<Plan>,
        /// Row count expression.
        count: Expr,
    },
    /// Expand a list expression into rows.
    Unwind {
        /// Input plan.
        input: Box<Plan>,
        /// List expression.
        expr: Expr,
        /// Alias bound to each element.
        alias: String,
    },
    /// Read a CSV file and expand its rows (one value per row).
    LoadCsv {
        /// Input plan.
        input: Box<Plan>,
        /// Source path expression (evaluated per input row).
        source: Expr,
        /// Alias bound to each row.
        alias: String,
        /// Whether the first row holds column names.
        with_headers: bool,
        /// Field separator; defaults to a comma.
        field_terminator: Option<char>,
    },
    /// Create nodes and edges.
    Create {
        /// Input plan.
        input: Box<Plan>,
        /// Elements in creation order.
        items: Vec<CreateItem>,
    },
    /// Match a whole pattern, creating its unbound elements when nothing
    /// matches.
    Merge {
        /// Input plan.
        input: Box<Plan>,
        /// Pattern plan rooted at `Empty`, using the same variable names as
        /// the create items.
        match_plan: Box<Plan>,
        /// Elements created when the pattern does not match.
        items: Vec<CreateItem>,
        /// Path variable and its `(variable, splice)` elements, in path
        /// order.
        path: Option<(String, Vec<(String, bool)>)>,
        /// Every pattern variable in path order, then the path variable;
        /// both merge paths collect rows in this order.
        path_vars: Vec<String>,
        /// Applied to created rows.
        on_create: Vec<SetItem>,
        /// Applied to matched rows.
        on_match: Vec<SetItem>,
    },
    /// `SET` properties and labels.
    SetItems {
        /// Input plan.
        input: Box<Plan>,
        /// Items to apply.
        items: Vec<SetItem>,
    },
    /// `REMOVE` properties and labels.
    RemoveItems {
        /// Input plan.
        input: Box<Plan>,
        /// Items to remove.
        items: Vec<SetItem>,
    },
    /// `DELETE` / `DETACH DELETE`.
    Delete {
        /// Input plan.
        input: Box<Plan>,
        /// Entity expressions.
        exprs: Vec<Expr>,
        /// Whether to delete incident edges first.
        detach: bool,
    },
}

/// A creation/merge element.
#[derive(Debug, Clone, PartialEq)]
pub enum CreateItem {
    /// Create a node.
    Node {
        /// Variable to bind.
        var: String,
        /// Label names (created at runtime when they do not exist yet).
        labels: Vec<String>,
        /// Literal property values keyed by name.
        props: Vec<(String, crate::record::Value)>,
        /// Non-literal property expressions keyed by name.
        prop_exprs: Vec<(String, Expr)>,
    },
    /// Create an edge between two bound nodes.
    Edge {
        /// Variable to bind.
        var: String,
        /// Edge type name (created at runtime when it does not exist yet).
        edge_type: String,
        /// Source variable.
        src: String,
        /// Destination variable.
        dst: String,
        /// Literal property values keyed by name.
        props: Vec<(String, crate::record::Value)>,
        /// Non-literal property expressions keyed by name.
        prop_exprs: Vec<(String, Expr)>,
    },
}

impl CreateItem {
    /// Variable bound by this item.
    pub fn var(&self) -> &str {
        match self {
            Self::Node { var, .. } | Self::Edge { var, .. } => var,
        }
    }
}

/// Render an edge type filter for `EXPLAIN`: `any`, `none`, or `A|B`.
fn types_text(types: &Option<Vec<String>>) -> String {
    match types {
        None => "any".to_owned(),
        Some(names) if names.is_empty() => "none".to_owned(),
        Some(names) => names.join("|"),
    }
}

/// Render a plan tree for `EXPLAIN` output.
pub fn explain(plan: &Plan, indent: usize, out: &mut String) {
    let pad = "  ".repeat(indent);
    match plan {
        Plan::Empty => out.push_str(&format!("{pad}Empty\n")),
        Plan::EmptyResult => out.push_str(&format!("{pad}EmptyResult\n")),
        Plan::NodeScan {
            input,
            var,
            label,
            seek,
            props,
        } => {
            out.push_str(&format!(
                "{pad}NodeScan {var} label={label:?} seek={} props={}\n",
                seek.as_ref()
                    .map(|s| s.index_id)
                    .map_or("-".to_owned(), |id| id.to_string()),
                props.len()
            ));
            explain(input, indent + 1, out);
        }
        Plan::AllNodesScan { input, var } => {
            out.push_str(&format!("{pad}AllNodesScan {var}\n"));
            explain(input, indent + 1, out);
        }
        Plan::EdgeScan {
            input,
            var,
            edge_type,
        } => {
            out.push_str(&format!("{pad}EdgeScan {var} type={edge_type:?}\n"));
            explain(input, indent + 1, out);
        }
        Plan::VarExpand {
            input,
            src,
            edge,
            dst,
            direction,
            types,
            props,
            min,
            max,
            reverse,
            nodes: _,
        } => {
            out.push_str(&format!(
                "{pad}VarExpand {src}-[{edge:?}:{} {direction:?} {min}..{max:?}]->{dst} props={} reverse={reverse}\n",
                types_text(types),
                props.len()
            ));
            explain(input, indent + 1, out);
        }
        Plan::Expand {
            input,
            src,
            edge,
            dst,
            direction,
            types,
            props,
        } => {
            out.push_str(&format!(
                "{pad}Expand {src}-[{edge:?}:{} {direction:?}]->{dst} props={}\n",
                types_text(types),
                props.len()
            ));
            explain(input, indent + 1, out);
        }
        Plan::Filter { input, .. } => {
            out.push_str(&format!("{pad}Filter\n"));
            explain(input, indent + 1, out);
        }
        Plan::Union { parts, all } => {
            out.push_str(&format!(
                "{pad}Union {}\n",
                if *all { "all" } else { "distinct" }
            ));
            for part in parts {
                explain(part, indent + 1, out);
            }
        }
        Plan::BindPath {
            input,
            var,
            elements,
        } => {
            out.push_str(&format!(
                "{pad}BindPath {var} ({} elements)\n",
                elements.len()
            ));
            explain(input, indent + 1, out);
        }
        Plan::ShortestPath {
            input,
            src,
            dst,
            all,
            ..
        } => {
            out.push_str(&format!(
                "{pad}ShortestPath {src} -> {dst}{}\n",
                if *all { " (all)" } else { "" }
            ));
            explain(input, indent + 1, out);
        }
        Plan::Foreach {
            input, plan, var, ..
        } => {
            out.push_str(&format!("{pad}Foreach {var}\n"));
            explain(plan, indent + 1, out);
            explain(input, indent + 1, out);
        }
        Plan::ProcedureCall {
            input,
            procedure,
            yields,
            ..
        } => {
            out.push_str(&format!(
                "{pad}ProcedureCall {procedure} {}\n",
                yields
                    .iter()
                    .map(|(column, alias)| format!("{column} AS {alias}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
            explain(input, indent + 1, out);
        }
        Plan::PatternPredicate { input, inner, var } => {
            out.push_str(&format!("{pad}PatternPredicate {var}\n"));
            explain(inner, indent + 1, out);
            explain(input, indent + 1, out);
        }
        Plan::PatternComprehension {
            input, inner, var, ..
        } => {
            out.push_str(&format!("{pad}PatternComprehension {var}\n"));
            explain(inner, indent + 1, out);
            explain(input, indent + 1, out);
        }
        Plan::OptionalMatch { input, inner, vars } => {
            out.push_str(&format!("{pad}OptionalMatch {}\n", vars.join(", ")));
            explain(inner, indent + 1, out);
            explain(input, indent + 1, out);
        }
        Plan::Project { input, items } => {
            out.push_str(&format!("{pad}Project {}\n", names(items)));
            explain(input, indent + 1, out);
        }
        Plan::Aggregate {
            input,
            groups,
            aggs,
        } => {
            out.push_str(&format!(
                "{pad}Aggregate groups={} aggs={}\n",
                names(groups),
                aggs.iter()
                    .map(|item| item.name.clone())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
            explain(input, indent + 1, out);
        }
        Plan::Distinct { input } => {
            out.push_str(&format!("{pad}Distinct\n"));
            explain(input, indent + 1, out);
        }
        Plan::Sort { input, keys } => {
            out.push_str(&format!("{pad}Sort {} keys\n", keys.len()));
            explain(input, indent + 1, out);
        }
        Plan::Skip { input, .. } => {
            out.push_str(&format!("{pad}Skip\n"));
            explain(input, indent + 1, out);
        }
        Plan::Limit { input, .. } => {
            out.push_str(&format!("{pad}Limit\n"));
            explain(input, indent + 1, out);
        }
        Plan::Unwind { input, alias, .. } => {
            out.push_str(&format!("{pad}Unwind AS {alias}\n"));
            explain(input, indent + 1, out);
        }
        Plan::LoadCsv {
            input,
            alias,
            with_headers,
            field_terminator,
            ..
        } => {
            out.push_str(&format!(
                "{pad}LoadCsv AS {alias}{}{}\n",
                if *with_headers { " with_headers" } else { "" },
                match field_terminator {
                    Some(ch) => format!(" fieldterminator={ch:?}"),
                    None => String::new(),
                }
            ));
            explain(input, indent + 1, out);
        }
        Plan::Create { input, items } => {
            out.push_str(&format!(
                "{pad}Create {}\n",
                items
                    .iter()
                    .map(|i| i.var().to_owned())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
            explain(input, indent + 1, out);
        }
        Plan::Merge {
            input,
            match_plan,
            items,
            ..
        } => {
            out.push_str(&format!(
                "{pad}Merge {}\n",
                items
                    .iter()
                    .map(|item| item.var().to_owned())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
            explain(match_plan, indent + 1, out);
            explain(input, indent + 1, out);
        }
        Plan::SetItems { input, items } => {
            out.push_str(&format!("{pad}Set {} items\n", items.len()));
            explain(input, indent + 1, out);
        }
        Plan::RemoveItems { input, items } => {
            out.push_str(&format!("{pad}Remove {} items\n", items.len()));
            explain(input, indent + 1, out);
        }
        Plan::Delete { input, detach, .. } => {
            out.push_str(&format!(
                "{pad}{}Delete\n",
                if *detach { "Detach" } else { "" }
            ));
            explain(input, indent + 1, out);
        }
    }
}

fn names(items: &[(Expr, String)]) -> String {
    items
        .iter()
        .map(|(_, name)| name.clone())
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::Value;

    fn render(plan: &Plan) -> String {
        let mut out = String::new();
        explain(plan, 0, &mut out);
        out
    }

    #[test]
    fn renders_leaf_operators() {
        assert_eq!(render(&Plan::Empty), "Empty\n");
        assert_eq!(render(&Plan::EmptyResult), "EmptyResult\n");
    }

    #[test]
    fn renders_node_scan_with_seek_and_residuals() {
        let plan = Plan::NodeScan {
            input: Box::new(Plan::Empty),
            var: "n".to_owned(),
            label: Some("Person".to_owned()),
            seek: Some(IndexSeek {
                index_id: 7,
                key_id: 1,
                value: Value::Int(3),
            }),
            props: vec![("name".to_owned(), Value::from("x"))],
        };
        assert_eq!(
            render(&plan),
            "NodeScan n label=Some(\"Person\") seek=7 props=1\n  Empty\n"
        );
    }

    #[test]
    fn renders_var_expand_type_filters() {
        let plan = |types: Option<Vec<String>>| Plan::VarExpand {
            input: Box::new(Plan::Empty),
            src: "a".to_owned(),
            edge: None,
            dst: "b".to_owned(),
            direction: crate::cypher::RelDirection::Outgoing,
            types,
            props: Vec::new(),
            min: 1,
            max: None,
            reverse: false,
            nodes: None,
        };
        assert!(render(&plan(None)).starts_with("VarExpand a-[None:any Outgoing 1..None]->b"));
        assert!(render(&plan(Some(Vec::new()))).starts_with("VarExpand a-[None:none Outgoing"));
        let named = plan(Some(vec!["A".to_owned(), "B".to_owned()]));
        assert!(render(&named).starts_with("VarExpand a-[None:A|B Outgoing"));
    }

    #[test]
    fn renders_pipeline_chain_in_nesting_order() {
        let plan = Plan::Limit {
            input: Box::new(Plan::Skip {
                input: Box::new(Plan::Sort {
                    input: Box::new(Plan::Empty),
                    keys: vec![(Expr::Variable("n".to_owned()), true)],
                }),
                count: Expr::Literal(Value::Int(1)),
            }),
            count: Expr::Literal(Value::Int(2)),
        };
        assert_eq!(
            render(&plan),
            "Limit\n  Skip\n    Sort 1 keys\n      Empty\n"
        );
    }

    #[test]
    fn renders_detach_delete_and_union_kinds() {
        let delete = Plan::Delete {
            input: Box::new(Plan::Empty),
            exprs: Vec::new(),
            detach: true,
        };
        assert!(render(&delete).starts_with("DetachDelete\n"));
        let union = Plan::Union {
            parts: vec![Plan::Empty, Plan::Empty],
            all: false,
        };
        assert!(render(&union).starts_with("Union distinct\n"));
    }

    #[test]
    fn renders_shortest_path_all_flag() {
        let plan = Plan::ShortestPath {
            input: Box::new(Plan::Empty),
            src: "a".to_owned(),
            dst: "b".to_owned(),
            edge: None,
            direction: crate::cypher::RelDirection::Outgoing,
            types: None,
            props: Vec::new(),
            min: 1,
            max: None,
            all: true,
            nodes: None,
        };
        assert!(render(&plan).starts_with("ShortestPath a -> b (all)\n"));
    }
}
