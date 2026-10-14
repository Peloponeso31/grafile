//! Side-effect metrics.
//!
//! The TCK defines four observable metrics as *sets*: nodes, relationships,
//! labels (distinct) and properties (entity/key/value triples). A write
//! query's side effects are the elements added to and removed from those
//! sets, so changing a property value counts as one removal plus one
//! addition, exactly like the TCK's defining queries. The metrics are
//! computed by scanning the engine directly, so the harness does not depend
//! on query features.

use std::collections::BTreeSet;

use grafile::engine::Database;
use grafile::record::Value;

/// One property triple: entity kind (0 = node, 1 = edge), entity id, key id
/// and value.
type PropertyTriple = (u8, u64, u32, Value);

/// Absolute metric values for one graph state.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Metrics {
    /// Node ids.
    pub nodes: BTreeSet<u64>,
    /// Edge ids.
    pub relationships: BTreeSet<u64>,
    /// Distinct label ids present in the graph.
    pub labels: BTreeSet<u32>,
    /// Entity/key/value property triples.
    pub properties: BTreeSet<PropertyTriple>,
}

/// Signed difference between two metric snapshots, as set additions and
/// removals.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Delta {
    /// Nodes added.
    pub added_nodes: i64,
    /// Nodes removed.
    pub removed_nodes: i64,
    /// Relationships added.
    pub added_relationships: i64,
    /// Relationships removed.
    pub removed_relationships: i64,
    /// Labels added.
    pub added_labels: i64,
    /// Labels removed.
    pub removed_labels: i64,
    /// Properties added.
    pub added_properties: i64,
    /// Properties removed.
    pub removed_properties: i64,
}

impl Delta {
    /// Difference between two snapshots.
    pub fn between(before: Metrics, after: Metrics) -> Self {
        let diff = |removed: usize, added: usize| (added as i64, removed as i64);
        let (added_nodes, removed_nodes) = diff(
            before.nodes.difference(&after.nodes).count(),
            after.nodes.difference(&before.nodes).count(),
        );
        let (added_relationships, removed_relationships) = diff(
            before
                .relationships
                .difference(&after.relationships)
                .count(),
            after
                .relationships
                .difference(&before.relationships)
                .count(),
        );
        let (added_labels, removed_labels) = diff(
            before.labels.difference(&after.labels).count(),
            after.labels.difference(&before.labels).count(),
        );
        let (added_properties, removed_properties) = diff(
            before.properties.difference(&after.properties).count(),
            after.properties.difference(&before.properties).count(),
        );
        Self {
            added_nodes,
            removed_nodes,
            added_relationships,
            removed_relationships,
            added_labels,
            removed_labels,
            added_properties,
            removed_properties,
        }
    }

    /// The count for a signed metric name (`+nodes`, `-properties`, ...).
    pub fn count(&self, sign: char, name: &str) -> Option<i64> {
        let positive = sign == '+';
        match (name, positive) {
            ("nodes", true) => Some(self.added_nodes),
            ("nodes", false) => Some(self.removed_nodes),
            ("relationships", true) => Some(self.added_relationships),
            ("relationships", false) => Some(self.removed_relationships),
            ("labels", true) => Some(self.added_labels),
            ("labels", false) => Some(self.removed_labels),
            ("properties", true) => Some(self.added_properties),
            ("properties", false) => Some(self.removed_properties),
            _ => None,
        }
    }

    /// Whether every addition and removal is zero.
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
    let mut labels = BTreeSet::new();
    let mut properties = BTreeSet::new();
    for id in &node_ids {
        let Some(record) = db
            .get_node(*id)
            .map_err(|err| format!("cannot read node {id}: {err}"))?
        else {
            continue;
        };
        for label in &record.labels {
            labels.insert(*label);
        }
        for (key, value) in &record.props {
            properties.insert((0u8, *id, *key, value.clone()));
        }
    }
    for id in &edge_ids {
        let Some(record) = db
            .get_edge(*id)
            .map_err(|err| format!("cannot read edge {id}: {err}"))?
        else {
            continue;
        };
        for (key, value) in &record.props {
            properties.insert((1u8, *id, *key, value.clone()));
        }
    }
    Ok(Metrics {
        nodes: node_ids.into_iter().collect(),
        relationships: edge_ids.into_iter().collect(),
        labels,
        properties,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn value_change_is_an_addition_and_a_removal() {
        let before = Metrics {
            nodes: [1].into_iter().collect(),
            properties: [(0u8, 1u64, 7u32, Value::Int(1))].into_iter().collect(),
            ..Metrics::default()
        };
        let after = Metrics {
            nodes: [1].into_iter().collect(),
            properties: [(0u8, 1u64, 7u32, Value::Int(2))].into_iter().collect(),
            ..Metrics::default()
        };
        let delta = Delta::between(before, after);
        assert_eq!(delta.added_properties, 1);
        assert_eq!(delta.removed_properties, 1);
        assert_eq!(delta.added_nodes, 0);
        assert!(!delta.is_zero());
        assert_eq!(delta.count('+', "properties"), Some(1));
        assert_eq!(delta.count('-', "properties"), Some(1));
        assert_eq!(delta.count('+', "nope"), None);
    }

    #[test]
    fn unchanged_snapshot_is_zero() {
        let metrics = Metrics {
            nodes: [1, 2].into_iter().collect(),
            labels: [3].into_iter().collect(),
            ..Metrics::default()
        };
        assert!(Delta::between(metrics.clone(), metrics).is_zero());
    }
}
