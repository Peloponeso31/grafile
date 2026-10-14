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
        /// Label id, or `None` for any label.
        label: Option<u32>,
        /// Index seek when a usable index exists.
        seek: Option<IndexSeek>,
        /// Residual literal property filters.
        props: Vec<(u32, crate::record::Value)>,
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
        /// Restricting edge type ids (empty = any).
        types: Vec<u32>,
        /// Literal property filters on the edge.
        props: Vec<(u32, crate::record::Value)>,
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
        /// `(function, argument, output name)` aggregations.
        aggs: Vec<(AggFn, Expr, String)>,
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
    /// Create nodes and edges.
    Create {
        /// Input plan.
        input: Box<Plan>,
        /// Elements in creation order.
        items: Vec<CreateItem>,
    },
    /// Match or create elements (a node, or a node plus a relationship).
    Merge {
        /// Input plan.
        input: Box<Plan>,
        /// Elements to merge, in order.
        items: Vec<CreateItem>,
        /// Applied when any element was created.
        on_create: Vec<SetItem>,
        /// Applied when every element already existed.
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
                "{pad}Expand {src}-[{edge:?}:{types:?} {direction:?}]->{dst} props={}\n",
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
                    .map(|(_, _, name)| name.clone())
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
        Plan::Merge { input, items, .. } => {
            out.push_str(&format!(
                "{pad}Merge {}\n",
                items
                    .iter()
                    .map(|item| item.var().to_owned())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
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
