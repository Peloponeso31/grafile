//! Model-based tests: random write sequences mirrored in an in-memory
//! reference graph. After every step the engine's expansions, scans, index
//! seeks and `edge_exists` must match the model and the integrity checker
//! must report nothing.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;

use proptest::prelude::*;

use super::Database;
use crate::error::Result;
use crate::matrix::Direction;
use crate::record::Value;
use crate::vfs::OpenMode;
use crate::vfs::mem::{FaultConfig, MemVfs, new_store};

const LABELS: u32 = 3;
const TYPES: u32 = 3;
/// Property values are drawn from a small range so index seeks hit.
const VALUES: i64 = 4;

/// First ids biased toward incidence (256) and label/type (65536) bands.
const START_IDS: [u64; 6] = [1, 254, 511, 65_534, 131_070, 16_777_214];

#[derive(Debug, Default, Clone, PartialEq)]
struct ModelNode {
    labels: BTreeSet<u32>,
    value: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct ModelEdge {
    edge_type: u32,
    src: u64,
    dst: u64,
}

#[derive(Debug, Default, Clone, PartialEq)]
struct Model {
    nodes: BTreeMap<u64, ModelNode>,
    edges: BTreeMap<u64, ModelEdge>,
}

/// Entities are picked by index into the live set, so every operation
/// targets something that exists (when anything does).
#[derive(Debug, Clone)]
enum Op {
    CreateNode {
        labels: Vec<u32>,
    },
    CreateEdge {
        src: usize,
        dst: usize,
        edge_type: u32,
    },
    DeleteEdge {
        edge: usize,
    },
    DeleteNode {
        node: usize,
        detach: bool,
    },
    AddLabel {
        node: usize,
        label: u32,
    },
    RemoveLabel {
        node: usize,
        label: u32,
    },
    SetValue {
        node: usize,
        value: i64,
    },
    RemoveValue {
        node: usize,
    },
}

fn op_strategy() -> impl Strategy<Value = Op> {
    let label = 1..=LABELS;
    prop_oneof![
        3 => proptest::collection::vec(label.clone(), 0..3)
            .prop_map(|labels| Op::CreateNode { labels }),
        4 => (any::<usize>(), any::<usize>(), 1..=TYPES)
            .prop_map(|(src, dst, edge_type)| Op::CreateEdge { src, dst, edge_type }),
        1 => any::<usize>().prop_map(|edge| Op::DeleteEdge { edge }),
        1 => (any::<usize>(), any::<bool>())
            .prop_map(|(node, detach)| Op::DeleteNode { node, detach }),
        1 => (any::<usize>(), label.clone()).prop_map(|(node, label)| Op::AddLabel { node, label }),
        1 => (any::<usize>(), label).prop_map(|(node, label)| Op::RemoveLabel { node, label }),
        2 => (any::<usize>(), 0..VALUES).prop_map(|(node, value)| Op::SetValue { node, value }),
        1 => any::<usize>().prop_map(|node| Op::RemoveValue { node }),
    ]
}

/// Initial nodes, so operations act on a populated graph from the start
/// and ids cross the start band early.
fn seed_strategy() -> impl Strategy<Value = Vec<Op>> {
    proptest::collection::vec(
        proptest::collection::vec(1..=LABELS, 0..3).prop_map(|labels| Op::CreateNode { labels }),
        4..16,
    )
}

fn pick<K: Copy, V>(map: &BTreeMap<K, V>, index: usize) -> Option<K> {
    (!map.is_empty()).then(|| *map.keys().nth(index % map.len()).expect("index in range"))
}

/// Ids of the schema objects every model database starts with.
#[derive(Debug, Clone, Copy)]
struct Schema {
    key: u32,
    index: u32,
}

fn create_db(vfs: Arc<MemVfs>, path: &str, start: u64) -> (Database, Schema) {
    let mut db = Database::create(vfs, Path::new(path), 4096).expect("create");
    let schema = db
        .write(|db| {
            for label in 1..=LABELS {
                db.ensure_label(&format!("L{label}"))?;
            }
            for edge_type in 1..=TYPES {
                db.ensure_edge_type(&format!("T{edge_type}"))?;
            }
            let key = db.ensure_property_key("v")?;
            let index = db.create_node_index("v", Some("L1"), false)?;
            db.pager.update_header(|header| {
                header.next_node_id = start;
                header.next_edge_id = start;
            });
            Ok(Schema { key, index })
        })
        .expect("schema");
    (db, schema)
}

/// Apply `op` to the engine, mirroring the expected effect in `model`.
/// Operations the engine must reject leave the model unchanged.
fn apply(db: &mut Database, schema: Schema, model: &mut Model, op: &Op) -> Result<()> {
    match op {
        Op::CreateNode { labels } => {
            let id = db.create_node(labels, vec![])?;
            let node = ModelNode {
                labels: labels.iter().copied().collect(),
                value: None,
            };
            model.nodes.insert(id, node);
        }
        Op::CreateEdge {
            src,
            dst,
            edge_type,
        } => {
            let (Some(src), Some(dst)) = (pick(&model.nodes, *src), pick(&model.nodes, *dst))
            else {
                return Ok(());
            };
            let id = db.create_edge(*edge_type, src, dst, vec![])?;
            let edge = ModelEdge {
                edge_type: *edge_type,
                src,
                dst,
            };
            model.edges.insert(id, edge);
        }
        Op::DeleteEdge { edge } => {
            if let Some(id) = pick(&model.edges, *edge) {
                assert!(db.delete_edge(id)?);
                model.edges.remove(&id);
            }
        }
        Op::DeleteNode { node, detach } => {
            let Some(id) = pick(&model.nodes, *node) else {
                return Ok(());
            };
            let attached = model
                .edges
                .values()
                .any(|edge| edge.src == id || edge.dst == id);
            if attached && !detach {
                assert!(db.delete_node(id, false).is_err(), "node {id} has edges");
                return Ok(());
            }
            assert!(db.delete_node(id, *detach)?);
            model.nodes.remove(&id);
            model
                .edges
                .retain(|_, edge| edge.src != id && edge.dst != id);
        }
        Op::AddLabel { node, label } => {
            if let Some(id) = pick(&model.nodes, *node) {
                db.add_node_labels(id, &[*label])?;
                model
                    .nodes
                    .get_mut(&id)
                    .expect("live")
                    .labels
                    .insert(*label);
            }
        }
        Op::RemoveLabel { node, label } => {
            if let Some(id) = pick(&model.nodes, *node) {
                db.remove_node_labels(id, &[*label])?;
                model.nodes.get_mut(&id).expect("live").labels.remove(label);
            }
        }
        Op::SetValue { node, value } => {
            if let Some(id) = pick(&model.nodes, *node) {
                db.set_node_property(id, schema.key, Value::Int(*value))?;
                model.nodes.get_mut(&id).expect("live").value = Some(*value);
            }
        }
        Op::RemoveValue { node } => {
            if let Some(id) = pick(&model.nodes, *node) {
                db.set_node_property(id, schema.key, Value::Null)?;
                model.nodes.get_mut(&id).expect("live").value = None;
            }
        }
    }
    Ok(())
}

fn sorted<T: Ord>(mut items: Vec<T>) -> Vec<T> {
    items.sort_unstable();
    items
}

/// Every read path must agree with the model, and the checker must be clean.
fn verify(db: &mut Database, schema: Schema, model: &Model) {
    let node_ids: Vec<u64> = model.nodes.keys().copied().collect();
    assert_eq!(db.all_node_ids().expect("nodes"), node_ids);
    let edge_ids: Vec<u64> = model.edges.keys().copied().collect();
    assert_eq!(db.all_edge_ids().expect("edges"), edge_ids);
    for (id, node) in &model.nodes {
        for direction in [Direction::Out, Direction::In] {
            for edge_type in 0..=TYPES {
                let types: &[u32] = if edge_type == 0 { &[] } else { &[edge_type] };
                let expected: Vec<(u64, u64)> = model
                    .edges
                    .iter()
                    .filter(|(_, edge)| edge_type == 0 || edge.edge_type == edge_type)
                    .filter_map(|(edge_id, edge)| match direction {
                        Direction::Out => (edge.src == *id).then_some((*edge_id, edge.dst)),
                        Direction::In => (edge.dst == *id).then_some((*edge_id, edge.src)),
                    })
                    .collect();
                let actual = sorted(db.expand(*id, direction, types).expect("expand"));
                assert_eq!(
                    actual, expected,
                    "{direction:?} type {edge_type} of node {id}"
                );
            }
        }
        let record = db.get_node(*id).expect("get").expect("live node");
        let labels: Vec<u32> = node.labels.iter().copied().collect();
        assert_eq!(record.labels, labels, "labels of node {id}");
        assert_eq!(record.get(schema.key), node.value.map(Value::Int).as_ref());
    }
    for label in 1..=LABELS {
        let expected: Vec<u64> = model
            .nodes
            .iter()
            .filter(|(_, node)| node.labels.contains(&label))
            .map(|(id, _)| *id)
            .collect();
        assert_eq!(
            db.label_nodes(label).expect("scan"),
            expected,
            "label {label}"
        );
    }
    for labels in [&[1, 2][..], &[1, 3], &[2, 3], &[1, 2, 3]] {
        let expected: Vec<u64> = model
            .nodes
            .iter()
            .filter(|(_, node)| labels.iter().all(|label| node.labels.contains(label)))
            .map(|(id, _)| *id)
            .collect();
        assert_eq!(
            db.nodes_with_labels(labels).expect("intersection"),
            expected,
            "labels {labels:?}"
        );
    }
    for edge_type in 1..=TYPES {
        let expected: Vec<u64> = model
            .edges
            .iter()
            .filter(|(_, edge)| edge.edge_type == edge_type)
            .map(|(id, _)| *id)
            .collect();
        assert_eq!(db.edges_of_type(edge_type).expect("scan"), expected);
        // Every ordered pair is checked, exercising both hits and misses.
        for src in &node_ids {
            for dst in &node_ids {
                let expected = model.edges.values().any(|edge| {
                    edge.edge_type == edge_type && edge.src == *src && edge.dst == *dst
                });
                assert_eq!(
                    db.edge_exists(edge_type, *src, *dst).expect("exists"),
                    expected,
                    "edge_exists({edge_type}, {src}, {dst})"
                );
            }
        }
    }
    for value in 0..VALUES {
        let expected: Vec<u64> = model
            .nodes
            .iter()
            .filter(|(_, node)| node.labels.contains(&1) && node.value == Some(value))
            .map(|(id, _)| *id)
            .collect();
        let actual = db
            .index_lookup(schema.index, &Value::Int(value))
            .expect("seek");
        assert_eq!(actual, expected, "index seek for {value}");
    }
    let report = db.check_integrity().expect("check");
    assert!(report.is_clean(), "{report}");
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    /// Each operation commits on its own or, when `rollback` is drawn, is
    /// rolled back; the model only advances on commit.
    #[test]
    fn writes_match_the_reference_model(
        start in proptest::sample::select(&START_IDS[..]),
        seed in seed_strategy(),
        steps in proptest::collection::vec((op_strategy(), proptest::bool::weighted(0.2)), 1..60),
    ) {
        let (mut db, schema) = create_db(Arc::new(MemVfs::new()), "model.db", start);
        let mut model = Model::default();
        db.write(|db| seed.iter().try_for_each(|op| apply(db, schema, &mut model, op)))
            .expect("seed");
        for (op, rollback) in &steps {
            if *rollback {
                let mut scratch = model.clone();
                db.begin_write().expect("begin");
                apply(&mut db, schema, &mut scratch, op).expect("apply");
                db.rollback().expect("rollback");
            } else {
                db.write(|db| apply(db, schema, &mut model, op)).expect("apply");
            }
            verify(&mut db, schema, &model);
        }
    }

    /// A commit interrupted at a random I/O operation recovers, on reopen,
    /// to either the old or the new state, and the checker is clean.
    #[test]
    fn interrupted_commits_recover_to_a_clean_state(
        start in proptest::sample::select(&START_IDS[..]),
        seed in seed_strategy(),
        before in proptest::collection::vec(op_strategy(), 1..20),
        during in proptest::collection::vec(op_strategy(), 1..20),
        fail_after in 0u64..60,
        torn in any::<bool>(),
    ) {
        let store = new_store();
        let mut model = Model::default();
        let mut next_model = Model::default();
        let schema = {
            let vfs = Arc::new(MemVfs::from_store(store.clone()));
            let (mut db, schema) = create_db(vfs.clone(), "crash.db", start);
            for op in seed.iter().chain(&before) {
                db.write(|db| apply(db, schema, &mut model, op)).expect("apply");
            }
            next_model.clone_from(&model);
            db.begin_write().expect("begin");
            for op in &during {
                apply(&mut db, schema, &mut next_model, op).expect("apply");
            }
            vfs.inject_fault(FaultConfig { fail_after_ops: Some(fail_after), torn_write: torn });
            let committed = db.commit().is_ok();
            if committed {
                model.clone_from(&next_model);
            }
            schema
        };
        let vfs = Arc::new(MemVfs::from_store(store));
        let mut db = Database::open(vfs, Path::new("crash.db"), OpenMode::ReadWrite)
            .expect("reopen recovers");
        let recovered = snapshot(&mut db, schema);
        prop_assert!(
            recovered == model || recovered == next_model,
            "recovered a mixed state"
        );
        verify(&mut db, schema, &recovered);
    }
}

/// The model a database's records describe.
fn snapshot(db: &mut Database, schema: Schema) -> Model {
    let mut model = Model::default();
    for id in db.all_node_ids().expect("nodes") {
        let record = db.get_node(id).expect("get").expect("listed node");
        let value = match record.get(schema.key) {
            Some(Value::Int(value)) => Some(*value),
            _ => None,
        };
        let labels = record.labels.iter().copied().collect();
        model.nodes.insert(id, ModelNode { labels, value });
    }
    for id in db.all_edge_ids().expect("edges") {
        let record = db.get_edge(id).expect("get").expect("listed edge");
        let edge = ModelEdge {
            edge_type: record.edge_type,
            src: record.src,
            dst: record.dst,
        };
        model.edges.insert(id, edge);
    }
    model
}

/// Create nodes and edges whose ids straddle `boundary`, then delete every
/// other one, verifying against the model throughout.
fn sweep(boundary: u64) {
    let start = boundary - 3;
    let (mut db, schema) = create_db(Arc::new(MemVfs::new()), "sweep.db", start);
    let mut model = Model::default();
    let mut ops: Vec<Op> = (0..7)
        .map(|index| Op::CreateNode {
            labels: vec![1 + index as u32 % LABELS],
        })
        .collect();
    // A star, a chain and a self-loop, so rows on both sides of the band
    // boundary carry edges.
    ops.extend((0..7).flat_map(|index| {
        [
            Op::CreateEdge {
                src: 0,
                dst: index,
                edge_type: 1,
            },
            Op::CreateEdge {
                src: index,
                dst: index + 1,
                edge_type: 2,
            },
        ]
    }));
    ops.push(Op::CreateEdge {
        src: 3,
        dst: 3,
        edge_type: 3,
    });
    ops.extend((0..4).map(|index| Op::SetValue {
        node: index * 2,
        value: index as i64 % VALUES,
    }));
    for op in &ops {
        db.write(|db| apply(db, schema, &mut model, op))
            .expect("apply");
    }
    verify(&mut db, schema, &model);
    for index in [6, 4, 2] {
        let op = Op::DeleteNode {
            node: index,
            detach: true,
        };
        db.write(|db| apply(db, schema, &mut model, &op))
            .expect("delete");
        verify(&mut db, schema, &model);
    }
}

#[test]
fn incidence_band_boundaries() {
    for boundary in [256, 512, 65_536 + 256] {
        sweep(boundary);
    }
}

#[test]
fn label_and_type_band_boundaries() {
    for boundary in [65_536, 131_072, 1 << 24] {
        sweep(boundary);
    }
}

/// Label and edge-type fragments cross `SPARSE_MAX` (4095 bits) into the
/// dense representation and back.
#[test]
fn fragments_cross_the_sparse_dense_threshold() {
    let (mut db, schema) = create_db(Arc::new(MemVfs::new()), "dense.db", 1);
    let mut model = Model::default();
    db.write(|db| {
        for _ in 0..4200 {
            apply(db, schema, &mut model, &Op::CreateNode { labels: vec![1] })?;
        }
        // Each node keeps a self-loop, so the T1 bitmap also turns dense.
        for index in 0..4200 {
            let op = Op::CreateEdge {
                src: index,
                dst: index,
                edge_type: 1,
            };
            apply(db, schema, &mut model, &op)?;
        }
        Ok(())
    })
    .expect("populate");
    let report = db.check_integrity().expect("check");
    assert!(report.is_clean(), "{report}");
    assert_eq!(db.label_nodes(1).expect("scan").len(), 4200);
    // Back below the threshold; dense fragments stay dense but shrink.
    db.write(|db| {
        for _ in 0..300 {
            apply(
                db,
                schema,
                &mut model,
                &Op::DeleteNode {
                    node: 0,
                    detach: true,
                },
            )?;
        }
        Ok(())
    })
    .expect("delete");
    assert_eq!(db.edges_of_type(1).expect("scan").len(), 3900);
    let report = db.check_integrity().expect("check");
    assert!(report.is_clean(), "{report}");
}

/// Enough records to split every tree several levels deep, then deletes
/// that leave stale dividers and empty leaves behind.
#[test]
fn deep_trees_with_holes_stay_consistent() {
    let (mut db, schema) = create_db(Arc::new(MemVfs::new()), "deep.db", 250);
    let mut model = Model::default();
    db.write(|db| {
        for index in 0..3000usize {
            apply(
                db,
                schema,
                &mut model,
                &Op::CreateNode { labels: vec![1, 2] },
            )?;
            let op = Op::SetValue {
                node: index,
                value: (index % 4) as i64,
            };
            apply(db, schema, &mut model, &op)?;
        }
        for index in 0..3000usize {
            let op = Op::CreateEdge {
                src: index,
                dst: index * 7,
                edge_type: 1 + (index % 3) as u32,
            };
            apply(db, schema, &mut model, &op)?;
        }
        Ok(())
    })
    .expect("populate");
    db.write(|db| {
        for index in (0..2000usize).rev().step_by(2) {
            let op = Op::DeleteNode {
                node: index,
                detach: true,
            };
            apply(db, schema, &mut model, &op)?;
        }
        Ok(())
    })
    .expect("delete");
    let report = db.check_integrity().expect("check");
    assert!(report.is_clean(), "{report}");
    assert!(report.stats().free_pages > 0 || report.stats().pages > 100);
    let nodes: Vec<u64> = model.nodes.keys().copied().collect();
    assert_eq!(db.all_node_ids().expect("nodes"), nodes);
    assert_eq!(
        db.index_lookup(schema.index, &Value::Int(2))
            .expect("seek")
            .len(),
        model
            .nodes
            .values()
            .filter(|node| node.value == Some(2))
            .count()
    );
}
