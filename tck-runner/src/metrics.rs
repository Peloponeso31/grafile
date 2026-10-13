//! Side-effect metrics.
//!
//! The TCK defines four observable metrics: the number of nodes, of
//! relationships, of properties (counted as entity/key/value triples) and of
//! distinct labels in the graph. A write query's side effects are the
//! difference between the metrics before and after it runs. The metrics are
//! computed by scanning the engine directly instead of running the defining
//! Cypher queries, so the harness does not depend on query features.

use std::collections::HashSet;

use grafile::engine::Database;

/// Absolute metric values for one graph state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Metrics {
    /// Number of nodes.
    pub nodes: i64,
    /// Number of relationships.
    pub relationships: i64,
    /// Number of entity/key/value property triples.
    pub properties: i64,
    /// Number of distinct labels present in the graph.
    pub labels: i64,
}

/// Signed difference between two metric snapshots.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Delta {
    /// Change in the node count.
    pub nodes: i64,
    /// Change in the relationship count.
    pub relationships: i64,
    /// Change in the property count.
    pub properties: i64,
    /// Change in the distinct-label count.
    pub labels: i64,
}

impl Delta {
    /// Difference between two snapshots.
    pub fn between(before: Metrics, after: Metrics) -> Self {
        Self {
            nodes: after.nodes - before.nodes,
            relationships: after.relationships - before.relationships,
            properties: after.properties - before.properties,
            labels: after.labels - before.labels,
        }
    }

    /// The signed delta for a metric name (`nodes`, `relationships`,
    /// `properties`, `labels`).
    pub fn get(&self, name: &str) -> Option<i64> {
        match name {
            "nodes" => Some(self.nodes),
            "relationships" => Some(self.relationships),
            "properties" => Some(self.properties),
            "labels" => Some(self.labels),
            _ => None,
        }
    }

    /// Whether all deltas are zero.
    pub fn is_zero(&self) -> bool {
        *self == Self::default()
    }
}

/// Take a snapshot of the graph metrics.
///
/// # Errors
///
/// Returns a message when the engine fails to enumerate nodes or edges.
pub fn measure(db: &mut Database) -> Result<Metrics, String> {
    let node_ids = db
        .all_node_ids()
        .map_err(|err| format!("cannot list nodes: {err}"))?;
    let edge_ids = db
        .all_edge_ids()
        .map_err(|err| format!("cannot list edges: {err}"))?;
    let mut properties = 0i64;
    let mut labels = HashSet::new();
    for id in &node_ids {
        let Some(record) = db
            .get_node(*id)
            .map_err(|err| format!("cannot read node {id}: {err}"))?
        else {
            continue;
        };
        properties += record.props.len() as i64;
        for label in &record.labels {
            labels.insert(*label);
        }
    }
    for id in &edge_ids {
        let Some(record) = db
            .get_edge(*id)
            .map_err(|err| format!("cannot read edge {id}: {err}"))?
        else {
            continue;
        };
        properties += record.props.len() as i64;
    }
    Ok(Metrics {
        nodes: node_ids.len() as i64,
        relationships: edge_ids.len() as i64,
        properties,
        labels: labels.len() as i64,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delta_between_snapshots() {
        let before = Metrics {
            nodes: 1,
            relationships: 2,
            properties: 3,
            labels: 1,
        };
        let after = Metrics {
            nodes: 2,
            relationships: 2,
            properties: 5,
            labels: 2,
        };
        let delta = Delta::between(before, after);
        assert_eq!(delta.nodes, 1);
        assert_eq!(delta.relationships, 0);
        assert_eq!(delta.properties, 2);
        assert_eq!(delta.labels, 1);
        assert_eq!(delta.get("nodes"), Some(1));
        assert_eq!(delta.get("nope"), None);
        assert!(!delta.is_zero());
        assert!(Delta::between(before, before).is_zero());
    }
}
