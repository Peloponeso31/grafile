//! Database engine: nodes, edges, indexes and traversals on top of the
//! storage layers.
//!
//! [`Database`] owns the pager and the four B+trees (catalog, nodes, edges,
//! fragments) and keeps their roots synchronized with the database header.
//! All mutations must run inside a write transaction; [`Database::write`]
//! wraps a closure in begin/commit with automatic rollback on error.

mod integrity;
#[cfg(test)]
mod model_tests;

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use hashbrown::{HashMap, HashSet};
use indicatif::{ProgressBar, ProgressStyle};

use smallvec::SmallVec;

use crate::btree::{Btree, TreeKind};

use crate::catalog::{Catalog, FulltextIndexDef, IndexKind, NodeIndexDef, Schema};
use crate::cypher::{DdlConstraintKind, DdlEntity, DdlStatement};
use crate::error::{Error, ErrorCode, Result};
use crate::fulltext::{
    FulltextQuery, Posting, decode_positions, document_terms, encode_positions, posting_key,
    posting_node, term_prefix,
};
use crate::matrix::{ANY_TYPE, Direction, FragmentKey, FragmentStore};
use crate::pager::{DatabaseHeader, Pager};
use crate::record::types::CypherType;
use crate::record::{EdgeRecord, NodeRecord, Value, encode_index_key};
use crate::vfs::{OpenMode, Vfs};

pub use integrity::{Finding, FindingKind, IntegrityReport, IntegrityStats, RebuildSummary};

/// An embedded graph database.
pub struct Database {
    pager: Pager,
    catalog: Catalog,
    nodes: Btree,
    edges: Btree,
    fragments: FragmentStore,
    /// Nodes deleted by the statement in progress. Their records are gone,
    /// but accessing their properties or labels is an error rather than a
    /// miss while the statement still references them.
    deleted_nodes: std::collections::HashSet<u64>,
    /// Edges deleted by the statement in progress, with their type so
    /// `type()` can still answer.
    deleted_edges: std::collections::HashMap<u64, u32>,
    /// Bytes an unbounded sort or `DISTINCT` buffers before spilling a run.
    sort_memory: usize,
}

/// Default sort budget: 256 MiB of buffered rows before spilling.
pub const DEFAULT_SORT_MEMORY: usize = 256 << 20;

/// One variable-length expansion: the relationships in traversal order and
/// the nodes they connect (one more node than edges).
#[derive(Debug, Clone, PartialEq)]
pub struct Trail {
    /// Relationships in traversal order.
    pub edges: Vec<u64>,
    /// Nodes in traversal order; `nodes[i]` and `nodes[i + 1]` are the
    /// endpoints of `edges[i]`.
    pub nodes: Vec<u64>,
}

/// A depth-first trail enumeration in progress (see
/// [`Database::var_expansion`]).
#[derive(Debug, Clone)]
pub struct VarExpansion {
    directions: SmallVec<[Direction; 2]>,
    types: Vec<u32>,
    props: Vec<(u32, Value)>,
    min: u32,
    max: Option<u32>,
    /// Per depth: the sorted neighbours of that trail node and the index of
    /// the next one to try.
    frames: Vec<(Vec<(u64, u64)>, usize)>,
    edges: Vec<u64>,
    nodes: Vec<u64>,
    state: ExpansionState,
}

impl VarExpansion {
    /// Relationships of the current trail, in traversal order.
    pub fn edges(&self) -> &[u64] {
        &self.edges
    }

    /// Nodes of the current trail; one more than its relationships.
    pub fn nodes(&self) -> &[u64] {
        &self.nodes
    }
}

/// The traversal restrictions shared by both directions of a path search.
#[derive(Debug, Clone, Copy)]
struct PathSearch<'a> {
    directions: &'a [Direction],
    types: &'a [u32],
    props: &'a [(u32, Value)],
}

/// Breadth-first distances from one node, kept by layer.
#[derive(Debug)]
struct DistanceBall {
    distances: HashMap<u64, u32>,
    layers: Vec<Vec<u64>>,
}

impl DistanceBall {
    fn new(start: u64) -> Self {
        Self {
            distances: HashMap::from([(start, 0)]),
            layers: vec![vec![start]],
        }
    }

    fn radius(&self) -> u32 {
        self.layers.len() as u32 - 1
    }

    fn frontier(&self) -> &[u64] {
        self.layers.last().expect("a ball has its start layer")
    }

    fn distance(&self, node: u64) -> Option<u32> {
        self.distances.get(&node).copied()
    }
}

/// What [`Database::next_trail`] does next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExpansionState {
    /// Yield the zero-length trail (`min == 0`).
    EmitStart,
    /// Push a frame for the current trail's last node.
    Expand,
    /// Try the next neighbour of the deepest frame.
    Advance,
    /// Every trail was produced.
    Done,
}

impl Trail {
    /// The final node of the trail.
    pub fn end(&self) -> u64 {
        *self.nodes.last().expect("a trail always has a start node")
    }
}

impl Database {
    /// Create a new database file.
    pub fn create(vfs: Arc<dyn Vfs>, path: &Path, page_size: u32) -> Result<Self> {
        let mut pager = Pager::create(vfs, path, page_size)?;
        pager.begin_write()?;
        let catalog = Catalog::create(&mut pager)?;
        let nodes = Btree::create(&mut pager, TreeKind::Table)?;
        let edges = Btree::create(&mut pager, TreeKind::Table)?;
        let fragments = FragmentStore::create(&mut pager)?;
        pager.update_header(|header| {
            header.catalog_root = catalog.root();
            header.node_root = nodes.root();
            header.edge_root = edges.root();
            header.fragment_root = fragments.root();
        });
        pager.commit()?;
        Ok(Self {
            pager,
            catalog,
            nodes,
            edges,
            fragments,
            deleted_nodes: std::collections::HashSet::new(),
            deleted_edges: std::collections::HashMap::new(),
            sort_memory: DEFAULT_SORT_MEMORY,
        })
    }

    /// Open an existing database file.
    pub fn open(vfs: Arc<dyn Vfs>, path: &Path, mode: OpenMode) -> Result<Self> {
        let mut pager = Pager::open(vfs, path, mode)?;
        let header = pager.header().clone();
        if header.catalog_root == 0
            || header.node_root == 0
            || header.edge_root == 0
            || header.fragment_root == 0
        {
            return Err(Error::new(
                ErrorCode::NotADb,
                "database is missing its storage roots",
            ));
        }
        let catalog = Catalog::open(&mut pager, header.catalog_root)?;
        let nodes = Btree::from_root(header.node_root, TreeKind::Table);
        let edges = Btree::from_root(header.edge_root, TreeKind::Table);
        let fragments = FragmentStore::open(header.fragment_root);
        Ok(Self {
            pager,
            catalog,
            nodes,
            edges,
            fragments,
            deleted_nodes: std::collections::HashSet::new(),
            deleted_edges: std::collections::HashMap::new(),
            sort_memory: DEFAULT_SORT_MEMORY,
        })
    }

    /// The database schema.
    pub fn schema(&self) -> &Schema {
        self.catalog.schema()
    }

    /// The current database header (the in-memory authoritative copy).
    pub fn header(&self) -> &DatabaseHeader {
        self.pager.header()
    }

    /// Path of the database file.
    pub fn path(&self) -> &Path {
        self.pager.path()
    }

    /// The VFS the database file was opened through; sort spill files use
    /// it too.
    pub fn vfs(&self) -> &Arc<dyn Vfs> {
        self.pager.vfs()
    }

    /// Bytes an unbounded `ORDER BY` or `DISTINCT` buffers before it writes
    /// a sorted run to a spill file next to the database.
    pub fn sort_memory(&self) -> usize {
        self.sort_memory
    }

    /// Set the sort budget (see [`Database::sort_memory`]).
    ///
    /// # Arguments
    ///
    /// * `bytes` - Approximate bytes of buffered rows per sort before
    ///   spilling; values below one row still spill one row at a time
    pub fn set_sort_memory(&mut self, bytes: usize) {
        self.sort_memory = bytes;
    }

    /// The catalog (for id lookups and DDL).
    pub fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    /// Register a label if needed (write transaction required).
    pub fn ensure_label(&mut self, name: &str) -> Result<u32> {
        self.catalog.ensure_label(&mut self.pager, name)
    }

    /// Register an edge type if needed (write transaction required).
    pub fn ensure_edge_type(&mut self, name: &str) -> Result<u32> {
        self.catalog.ensure_edge_type(&mut self.pager, name)
    }

    /// Register a property key if needed (write transaction required).
    pub fn ensure_property_key(&mut self, name: &str) -> Result<u32> {
        self.catalog.ensure_property_key(&mut self.pager, name)
    }

    /// Label id for a name.
    pub fn label_id(&self, name: &str) -> Option<u32> {
        self.catalog.label_id(name)
    }

    /// Edge type id for a name.
    pub fn edge_type_id(&self, name: &str) -> Option<u32> {
        self.catalog.edge_type_id(name)
    }

    /// Property key id for a name.
    pub fn property_key_id(&self, name: &str) -> Option<u32> {
        self.catalog.property_key_id(name)
    }

    /// Whether a write transaction is active.
    pub fn in_write_txn(&self) -> bool {
        self.pager.in_write_txn()
    }

    /// Begin a write transaction.
    pub fn begin_write(&mut self) -> Result<()> {
        self.pager.begin_write()
    }

    /// Acquire the shared read lock for one statement (idempotent).
    ///
    /// On the first acquisition the pager recovers a hot journal left by a
    /// crashed writer and refreshes stale cache entries after another
    /// connection's commit.
    pub fn begin_read(&mut self) -> Result<()> {
        self.pager.begin_read()
    }

    /// Release the shared read lock if held.
    pub fn end_read(&mut self) -> Result<()> {
        self.pager.end_read()
    }

    /// Commit the active write transaction, persisting tree roots. A failed
    /// commit rolls the transaction back before returning its error, so the
    /// file and the lock are never left mid-transaction.
    pub fn commit(&mut self) -> Result<()> {
        let committed = self.commit_inner();
        if committed.is_err() && self.pager.in_write_txn() {
            let _ = self.rollback();
        }
        committed
    }

    fn commit_inner(&mut self) -> Result<()> {
        // Flush cached fragments first: the flush may split the fragment
        // B+tree and move its root.
        self.fragments.flush(&mut self.pager)?;
        let (catalog_root, node_root, edge_root, fragment_root) = (
            self.catalog.root(),
            self.nodes.root(),
            self.edges.root(),
            self.fragments.root(),
        );
        self.pager.update_header(|header| {
            header.catalog_root = catalog_root;
            header.node_root = node_root;
            header.edge_root = edge_root;
            header.fragment_root = fragment_root;
        });
        self.pager.commit()
    }

    /// Roll back the active write transaction and reload in-memory state.
    pub fn rollback(&mut self) -> Result<()> {
        self.pager.rollback()?;
        self.reload()
    }

    /// Run `f` inside a write transaction, committing on success.
    pub fn write<R>(&mut self, f: impl FnOnce(&mut Self) -> Result<R>) -> Result<R> {
        self.begin_write()?;
        match f(self) {
            Ok(result) => {
                self.commit()?;
                Ok(result)
            }
            Err(err) => {
                let _ = self.rollback();
                Err(err)
            }
        }
    }

    fn reload(&mut self) -> Result<()> {
        let header = self.pager.header().clone();
        self.catalog = Catalog::open(&mut self.pager, header.catalog_root)?;
        self.nodes = Btree::from_root(header.node_root, TreeKind::Table);
        self.edges = Btree::from_root(header.edge_root, TreeKind::Table);
        self.fragments = FragmentStore::open(header.fragment_root);
        Ok(())
    }

    /// Read a node record.
    pub fn get_node(&mut self, node_id: u64) -> Result<Option<NodeRecord>> {
        match self.nodes.get(&mut self.pager, &node_id.to_be_bytes())? {
            Some(bytes) => Ok(Some(NodeRecord::decode(&bytes)?)),
            None => Ok(None),
        }
    }

    /// Read an edge record.
    pub fn get_edge(&mut self, edge_id: u64) -> Result<Option<EdgeRecord>> {
        match self.edges.get(&mut self.pager, &edge_id.to_be_bytes())? {
            Some(bytes) => Ok(Some(EdgeRecord::decode(&bytes)?)),
            None => Ok(None),
        }
    }

    /// Whether a node exists.
    pub fn node_exists(&mut self, node_id: u64) -> Result<bool> {
        self.nodes.contains(&mut self.pager, &node_id.to_be_bytes())
    }

    /// Create a node with the given label ids and properties.
    ///
    /// `labels` and `props` are sorted and deduplicated internally. Null
    /// properties are dropped (Cypher treats them as absent).
    pub fn create_node(&mut self, labels: &[u32], props: Vec<(u32, Value)>) -> Result<u64> {
        self.require_write()?;
        let mut labels = labels.to_vec();
        labels.sort_unstable();
        labels.dedup();
        check_node_existence(self.schema(), &labels, &props)?;
        let record = NodeRecord {
            labels,
            props: normalize_props(props)?,
        };
        let id = self.pager.update_header(|header| {
            let id = header.next_node_id;
            header.next_node_id += 1;
            id
        });
        self.nodes
            .insert(&mut self.pager, &id.to_be_bytes(), &record.encode())?;
        for label in &record.labels {
            let key = FragmentKey::label(*label, id);
            self.fragments
                .set_bit(&mut self.pager, &key, FragmentKey::label_bit(id))?;
        }
        self.index_node(&record, id)?;
        Ok(id)
    }

    /// Create an edge between two existing nodes.
    pub fn create_edge(
        &mut self,
        edge_type: u32,
        src: u64,
        dst: u64,
        props: Vec<(u32, Value)>,
    ) -> Result<u64> {
        self.require_write()?;
        if !self.node_exists(src)? {
            return Err(Error::new(
                ErrorCode::NotFound,
                format!("source node {src} does not exist"),
            ));
        }
        if !self.node_exists(dst)? {
            return Err(Error::new(
                ErrorCode::NotFound,
                format!("destination node {dst} does not exist"),
            ));
        }
        self.create_edge_inner(edge_type, src, dst, props)
    }

    /// Create an edge without verifying that the endpoints exist.
    ///
    /// Compiled Cypher patterns always bind existing nodes, so the VDBE uses
    /// this to skip two B+tree lookups per edge; callers that cannot
    /// guarantee endpoint existence must use [`Self::create_edge`].
    pub fn create_edge_unchecked(
        &mut self,
        edge_type: u32,
        src: u64,
        dst: u64,
        props: Vec<(u32, Value)>,
    ) -> Result<u64> {
        self.require_write()?;
        self.create_edge_inner(edge_type, src, dst, props)
    }

    fn create_edge_inner(
        &mut self,
        edge_type: u32,
        src: u64,
        dst: u64,
        props: Vec<(u32, Value)>,
    ) -> Result<u64> {
        check_edge_existence(self.schema(), edge_type, &props)?;
        let record = EdgeRecord {
            edge_type,
            src,
            dst,
            props: normalize_props(props)?,
        };
        let id = self.pager.update_header(|header| {
            let id = header.next_edge_id;
            header.next_edge_id += 1;
            id
        });
        self.edges
            .insert(&mut self.pager, &id.to_be_bytes(), &record.encode())?;
        self.set_incidence(&record, id, true)?;
        self.index_edge(&record, id)?;
        Ok(id)
    }

    fn set_incidence(&mut self, record: &EdgeRecord, edge_id: u64, set: bool) -> Result<()> {
        let mut bits = Vec::with_capacity(4);
        for type_id in [record.edge_type, ANY_TYPE] {
            bits.push((
                FragmentKey::incidence(Direction::Out, type_id, record.src, edge_id),
                FragmentKey::incidence_bit(record.src, edge_id),
            ));
            bits.push((
                FragmentKey::incidence(Direction::In, type_id, record.dst, edge_id),
                FragmentKey::incidence_bit(record.dst, edge_id),
            ));
        }
        if set {
            self.fragments.set_bits(&mut self.pager, &bits)?;
            let key = FragmentKey::edge_type(record.edge_type, edge_id);
            self.fragments
                .set_bit(&mut self.pager, &key, FragmentKey::edge_type_bit(edge_id))?;
        } else {
            self.fragments.clear_bits(&mut self.pager, &bits)?;
            let key = FragmentKey::edge_type(record.edge_type, edge_id);
            self.fragments
                .clear_bit(&mut self.pager, &key, FragmentKey::edge_type_bit(edge_id))?;
        }
        Ok(())
    }

    /// Delete an edge, clearing incidence, type bitmaps and indexes.
    pub fn delete_edge(&mut self, edge_id: u64) -> Result<bool> {
        self.require_write()?;
        let Some(bytes) = self.edges.get(&mut self.pager, &edge_id.to_be_bytes())? else {
            return Ok(false);
        };
        let record = EdgeRecord::decode(&bytes)?;
        self.deleted_edges.insert(edge_id, record.edge_type);
        self.edges.delete(&mut self.pager, &edge_id.to_be_bytes())?;
        self.set_incidence(&record, edge_id, false)?;
        self.unindex_edge(&record, edge_id)?;
        Ok(true)
    }

    /// Delete a node. With `detach`, incident edges are deleted first;
    /// otherwise a node with edges raises a constraint error.
    pub fn delete_node(&mut self, node_id: u64, detach: bool) -> Result<bool> {
        self.require_write()?;
        let Some(record) = self.get_node(node_id)? else {
            return Ok(false);
        };
        let outgoing = self.node_edges(node_id, Direction::Out, &[])?;
        let incoming = self.node_edges(node_id, Direction::In, &[])?;
        if !outgoing.is_empty() || !incoming.is_empty() {
            if !detach {
                return Err(Error::constraint(format!(
                    "cannot delete node {node_id}: it still has edges"
                )));
            }
            for edge_id in outgoing.into_iter().chain(incoming) {
                self.delete_edge(edge_id)?;
            }
        }
        self.nodes.delete(&mut self.pager, &node_id.to_be_bytes())?;
        for label in &record.labels {
            let key = FragmentKey::label(*label, node_id);
            self.fragments
                .clear_bit(&mut self.pager, &key, FragmentKey::label_bit(node_id))?;
        }
        self.unindex_node(&record, node_id)?;
        self.deleted_nodes.insert(node_id);
        Ok(true)
    }

    /// Forget the deletions of the previous statement.
    pub fn clear_deleted(&mut self) {
        self.deleted_nodes.clear();
        self.deleted_edges.clear();
    }

    /// Whether the node was deleted by the statement in progress.
    pub fn node_deleted(&self, node_id: u64) -> bool {
        self.deleted_nodes.contains(&node_id)
    }

    /// The type of an edge deleted by the statement in progress.
    pub fn deleted_edge_type(&self, edge_id: u64) -> Option<u32> {
        self.deleted_edges.get(&edge_id).copied()
    }

    /// Reject property values outside the value model: maps and entities,
    /// lists of lists and lists containing nulls or entities.
    fn validate_property_value(value: &Value) -> Result<()> {
        match value {
            Value::List(items) => {
                for item in items.iter() {
                    match item {
                        Value::Null
                        | Value::Map(_)
                        | Value::List(_)
                        | Value::Node(_)
                        | Value::Edge(_) => {
                            return Err(Error::type_error(
                                "InvalidPropertyType: a property list may only contain \
                                 primitive values",
                            ));
                        }
                        _ => {}
                    }
                }
                Ok(())
            }
            Value::Map(_) | Value::Node(_) | Value::Edge(_) => Err(Error::type_error(
                "InvalidPropertyType: a property may not be a map or an entity",
            )),
            _ => Ok(()),
        }
    }

    /// Replace a node property (or remove it when `value` is null).
    pub fn set_node_property(&mut self, node_id: u64, key_id: u32, value: Value) -> Result<bool> {
        self.require_write()?;
        if !value.is_null() {
            Self::validate_property_value(&value)?;
        }
        let Some(mut record) = self.get_node(node_id)? else {
            return Err(Error::new(
                ErrorCode::NotFound,
                format!("node {node_id} does not exist"),
            ));
        };
        if value.is_null() {
            check_node_removal(self.schema(), &record.labels, key_id)?;
        }
        self.unindex_node_property(&record, node_id, key_id)?;
        if value.is_null() {
            record.remove(key_id);
        } else {
            record.set(key_id, value);
        }
        let key_bytes = node_id.to_be_bytes();
        self.nodes.delete(&mut self.pager, &key_bytes)?;
        self.nodes
            .insert(&mut self.pager, &key_bytes, &record.encode())?;
        self.index_node_property(&record, node_id, key_id)?;
        Ok(true)
    }

    /// Replace an edge property (or remove it when `value` is null).
    pub fn set_edge_property(&mut self, edge_id: u64, key_id: u32, value: Value) -> Result<bool> {
        self.require_write()?;
        if !value.is_null() {
            Self::validate_property_value(&value)?;
        }
        let Some(mut record) = self.get_edge(edge_id)? else {
            return Err(Error::new(
                ErrorCode::NotFound,
                format!("edge {edge_id} does not exist"),
            ));
        };
        if value.is_null() {
            check_edge_removal(self.schema(), record.edge_type, key_id)?;
        }
        self.unindex_edge_property(&record, edge_id, key_id)?;
        if value.is_null() {
            record.remove(key_id);
        } else {
            record.set(key_id, value);
        }
        let key_bytes = edge_id.to_be_bytes();
        self.edges.delete(&mut self.pager, &key_bytes)?;
        self.edges
            .insert(&mut self.pager, &key_bytes, &record.encode())?;
        self.index_edge_property(&record, edge_id, key_id)?;
        Ok(true)
    }

    /// Start a depth-first enumeration of every trail from `src` whose length
    /// is within `min..=max` (`max` of `None` is unbounded); advance it with
    /// [`Database::next_trail`]. Relationships never repeat within a trail;
    /// nodes may. Memory is bounded by the trail depth times the degree.
    ///
    /// # Arguments
    ///
    /// * `src` - Start node
    /// * `directions` - Directions to follow at every hop
    /// * `types` - Edge types to follow; empty follows every type
    /// * `props` - Literal properties every relationship must carry
    /// * `bounds` - Minimum and optional maximum trail length
    ///
    /// # Returns
    ///
    /// The expansion, positioned before its first trail
    pub fn var_expansion(
        &mut self,
        src: u64,
        directions: &[Direction],
        types: &[u32],
        props: &[(String, Value)],
        bounds: (u32, Option<u32>),
    ) -> VarExpansion {
        // Property key names are resolved per call, so keys created by the
        // same statement are seen. An unknown key matches nothing.
        let keyed: Option<Vec<(u32, Value)>> = props
            .iter()
            .map(|(name, value)| Some((self.property_key_id(name)?, value.clone())))
            .collect();
        let (min, max) = bounds;
        let state = match keyed {
            Some(_) if min == 0 => ExpansionState::EmitStart,
            Some(_) => ExpansionState::Expand,
            None => ExpansionState::Done,
        };
        VarExpansion {
            directions: directions.iter().copied().collect(),
            types: types.to_vec(),
            props: keyed.unwrap_or_default(),
            min,
            max,
            frames: Vec::new(),
            edges: Vec::new(),
            nodes: vec![src],
            state,
        }
    }

    /// Advance `expansion` to its next trail, in the order a recursive
    /// depth-first search over ascending `(edge, node)` neighbours visits
    /// them.
    ///
    /// # Returns
    ///
    /// `true` when [`VarExpansion::edges`] and [`VarExpansion::nodes`] hold
    /// the next trail, `false` once every trail was produced
    ///
    /// # Errors
    ///
    /// Propagates storage errors while expanding.
    pub fn next_trail(&mut self, expansion: &mut VarExpansion) -> Result<bool> {
        loop {
            match expansion.state {
                ExpansionState::Done => return Ok(false),
                ExpansionState::EmitStart => {
                    expansion.state = ExpansionState::Expand;
                    return Ok(true);
                }
                ExpansionState::Expand => {
                    expansion.state = ExpansionState::Advance;
                    let depth = expansion.edges.len() as u32;
                    let mut neighbours = Vec::new();
                    if expansion.max.is_none_or(|max| depth < max) {
                        let node = *expansion.nodes.last().expect("a trail has a start node");
                        for direction in &expansion.directions {
                            neighbours.extend(self.expand(node, *direction, &expansion.types)?);
                        }
                        neighbours.sort_unstable();
                        neighbours.dedup();
                    }
                    expansion.frames.push((neighbours, 0));
                }
                ExpansionState::Advance => {
                    let Some((neighbours, next)) = expansion.frames.last_mut() else {
                        expansion.state = ExpansionState::Done;
                        return Ok(false);
                    };
                    let Some(&(edge, node)) = neighbours.get(*next) else {
                        // Exhausted: back out of the hop that led here.
                        expansion.frames.pop();
                        if expansion.frames.is_empty() {
                            expansion.state = ExpansionState::Done;
                            return Ok(false);
                        }
                        expansion.edges.pop();
                        expansion.nodes.pop();
                        continue;
                    };
                    *next += 1;
                    if expansion.edges.contains(&edge)
                        || !self.edge_matches(edge, &expansion.props)?
                    {
                        continue;
                    }
                    expansion.edges.push(edge);
                    expansion.nodes.push(node);
                    expansion.state = ExpansionState::Expand;
                    if expansion.edges.len() as u32 >= expansion.min {
                        return Ok(true);
                    }
                }
            }
        }
    }

    /// Shortest trails from `src`. With a target, returns one trail to it
    /// (or every trail of minimal length when `all`); without a target,
    /// returns one trail per reachable node (or every trail of minimal
    /// length per node). Trails come in the order a breadth-first
    /// enumeration over expansion order produces them.
    ///
    /// A target is found with a bidirectional search over node distances
    /// (`bidirectional_shortest`); the path enumeration remains
    /// for an unbound target and for a minimum length above the distance.
    ///
    /// # Errors
    ///
    /// Propagates storage errors while expanding.
    #[allow(clippy::too_many_arguments)]
    pub fn var_shortest_paths(
        &mut self,
        src: u64,
        dst: Option<u64>,
        directions: &[Direction],
        types: &[u32],
        props: &[(String, Value)],
        min: u32,
        max: Option<u32>,
        all: bool,
    ) -> Result<Vec<Trail>> {
        // Property key names are resolved per call, so keys created by the
        // same statement are seen. An unknown key matches nothing.
        let mut keyed = Vec::with_capacity(props.len());
        for (name, value) in props {
            match self.property_key_id(name) {
                Some(key_id) => keyed.push((key_id, value.clone())),
                None => return Ok(Vec::new()),
            }
        }
        let search = PathSearch {
            directions,
            types,
            props: &keyed,
        };
        if let Some(target) = dst {
            match self.bidirectional_shortest(&search, src, target, max, all)? {
                None => return Ok(Vec::new()),
                Some((distance, trails)) if distance >= min => return Ok(trails),
                Some(_) => {}
            }
        }
        self.shortest_by_enumeration(&search, src, dst, min, max, all)
    }

    /// Shortest trails by breadth-first enumeration of simple paths. Exact
    /// for every bound, but the frontier holds every simple path of the
    /// current length, so it is reserved for the cases the distance search
    /// cannot answer.
    fn shortest_by_enumeration(
        &mut self,
        search: &PathSearch<'_>,
        src: u64,
        dst: Option<u64>,
        min: u32,
        max: Option<u32>,
        all: bool,
    ) -> Result<Vec<Trail>> {
        let PathSearch {
            directions,
            types,
            props: keyed,
        } = *search;
        // Shortest paths are simple: a node may not repeat. Each frontier
        // entry carries its edge list and visited nodes.
        let mut out: Vec<Trail> = Vec::new();
        let mut frontier: Vec<(u64, Vec<u64>, Vec<u64>)> = vec![(src, Vec::new(), vec![src])];
        let mut length = 0u32;
        let mut emitted: Vec<u64> = Vec::new();
        loop {
            if length >= min {
                match dst {
                    Some(target) => {
                        let mut found: Vec<Trail> = frontier
                            .iter()
                            .filter(|(node, _, _)| *node == target)
                            .map(|(_, trail, nodes)| Trail {
                                edges: trail.clone(),
                                nodes: nodes.clone(),
                            })
                            .collect();
                        if !found.is_empty() {
                            if !all {
                                found.truncate(1);
                            }
                            return Ok(found);
                        }
                    }
                    None => {
                        let mut found: Vec<Trail> = frontier
                            .iter()
                            .filter(|(node, _, _)| !emitted.contains(node))
                            .map(|(_, trail, nodes)| Trail {
                                edges: trail.clone(),
                                nodes: nodes.clone(),
                            })
                            .collect();
                        if !found.is_empty() {
                            found.sort_unstable_by_key(Trail::end);
                            if !all {
                                found.dedup_by_key(|trail| trail.end());
                            }
                            emitted.extend(found.iter().map(Trail::end));
                            out.extend(found);
                        }
                    }
                }
            }
            if max.is_some_and(|max| length >= max) {
                return Ok(out);
            }
            let mut next: Vec<(u64, Vec<u64>, Vec<u64>)> = Vec::new();
            for (node, trail, nodes) in &frontier {
                for direction in directions {
                    for (edge, neighbour) in self.expand(*node, *direction, types)? {
                        if nodes.contains(&neighbour) || !self.edge_matches(edge, keyed)? {
                            continue;
                        }
                        let mut extended = trail.clone();
                        extended.push(edge);
                        let mut extended_nodes = nodes.clone();
                        extended_nodes.push(neighbour);
                        next.push((neighbour, extended, extended_nodes));
                    }
                }
            }
            if next.is_empty() {
                return Ok(out);
            }
            frontier = next;
            length += 1;
        }
    }

    /// Shortest trails from `src` to `dst` within `max` hops: the distance
    /// `D` from a bidirectional breadth-first search over node distances
    /// (expanding the smaller frontier a layer at a time), then the trails of
    /// length `D` in enumeration order, found by a depth-first walk that only
    /// steps onto nodes lying on some shortest path.
    ///
    /// # Returns
    ///
    /// `None` when no trail of at most `max` hops exists; otherwise `D` and
    /// the first trail (or all of them when `all`)
    fn bidirectional_shortest(
        &mut self,
        search: &PathSearch<'_>,
        src: u64,
        dst: u64,
        max: Option<u32>,
        all: bool,
    ) -> Result<Option<(u32, Vec<Trail>)>> {
        let backward: SmallVec<[Direction; 2]> = search
            .directions
            .iter()
            .map(|direction| direction.reverse())
            .collect();
        let backward = PathSearch {
            directions: &backward,
            ..*search
        };
        let mut forward_ball = DistanceBall::new(src);
        let mut backward_ball = DistanceBall::new(dst);
        if src != dst {
            loop {
                if max.is_some_and(|max| forward_ball.radius() + backward_ball.radius() >= max) {
                    return Ok(None);
                }
                // No node is in both balls yet, so the first one that is
                // fixes the distance at the sum of the radii.
                let met = if forward_ball.frontier().len() <= backward_ball.frontier().len() {
                    self.grow_ball(search, &mut forward_ball, &backward_ball)?
                } else {
                    self.grow_ball(&backward, &mut backward_ball, &forward_ball)?
                };
                match met {
                    Some(true) => break,
                    Some(false) => {}
                    None => return Ok(None),
                }
            }
        }
        let distance = forward_ball.radius() + backward_ball.radius();
        let on_path = self.forward_on_path(search, &forward_ball, &backward_ball, distance)?;
        let trails = self.walk_shortest(search, src, distance, all, |depth, node| {
            if depth <= forward_ball.radius() {
                forward_ball.distance(node) == Some(depth) && on_path.contains(&node)
            } else {
                backward_ball.distance(node) == Some(distance - depth)
            }
        })?;
        Ok(Some((distance, trails)))
    }

    /// Expand the frontier of `ball` by one layer. Returns whether a new
    /// node is also in `other`, or `None` when the frontier died out.
    fn grow_ball(
        &mut self,
        search: &PathSearch<'_>,
        ball: &mut DistanceBall,
        other: &DistanceBall,
    ) -> Result<Option<bool>> {
        let depth = ball.radius() + 1;
        let mut layer = Vec::new();
        let mut met = false;
        // Borrow the layer list and the distance map separately.
        let frontier = ball.layers.last().expect("a ball has its start layer");
        for &node in frontier {
            for &direction in search.directions {
                for (edge, next) in self.expand(node, direction, search.types)? {
                    if ball.distances.contains_key(&next)
                        || !self.edge_matches(edge, search.props)?
                    {
                        continue;
                    }
                    ball.distances.insert(next, depth);
                    met |= other.distances.contains_key(&next);
                    layer.push(next);
                }
            }
        }
        if layer.is_empty() {
            return Ok(None);
        }
        ball.layers.push(layer);
        Ok(Some(met))
    }

    /// Nodes of the forward ball that start a shortest continuation: those
    /// whose backward distance completes `distance`, and, layer by layer
    /// towards the source, those with a successor one layer further that
    /// does.
    fn forward_on_path(
        &mut self,
        search: &PathSearch<'_>,
        forward: &DistanceBall,
        backward: &DistanceBall,
        distance: u32,
    ) -> Result<HashSet<u64>> {
        let mut on_path = HashSet::new();
        for (depth, layer) in forward.layers.iter().enumerate().rev() {
            let depth = depth as u32;
            for &node in layer {
                let mut good = backward.distance(node) == Some(distance - depth);
                if !good && depth < forward.radius() {
                    'search: for &direction in search.directions {
                        for (edge, next) in self.expand(node, direction, search.types)? {
                            if forward.distance(next) == Some(depth + 1)
                                && on_path.contains(&next)
                                && self.edge_matches(edge, search.props)?
                            {
                                good = true;
                                break 'search;
                            }
                        }
                    }
                }
                if good {
                    on_path.insert(node);
                }
            }
        }
        Ok(on_path)
    }

    /// Trails of exactly `distance` hops from `src` whose node at each depth
    /// satisfies `on_path`, depth first in expansion order (the order a
    /// breadth-first enumeration lists them). Stops at the first unless
    /// `all`.
    fn walk_shortest(
        &mut self,
        search: &PathSearch<'_>,
        src: u64,
        distance: u32,
        all: bool,
        on_path: impl Fn(u32, u64) -> bool,
    ) -> Result<Vec<Trail>> {
        let mut out = Vec::new();
        let mut edges = Vec::new();
        let mut nodes = vec![src];
        let mut frames: Vec<(Vec<(u64, u64)>, usize)> = Vec::new();
        loop {
            let depth = edges.len() as u32;
            if depth == distance {
                out.push(Trail {
                    edges: edges.clone(),
                    nodes: nodes.clone(),
                });
                if !all {
                    return Ok(out);
                }
            } else {
                let node = *nodes.last().expect("a trail has a start node");
                let mut steps = Vec::new();
                for &direction in search.directions {
                    for (edge, next) in self.expand(node, direction, search.types)? {
                        if on_path(depth + 1, next) && self.edge_matches(edge, search.props)? {
                            steps.push((edge, next));
                        }
                    }
                }
                frames.push((steps, 0));
            }
            // Advance to the next untried step, backing out of exhausted
            // frames (and the trail of a completed one).
            loop {
                let Some((steps, next)) = frames.last_mut() else {
                    return Ok(out);
                };
                if let Some(&(edge, node)) = steps.get(*next) {
                    *next += 1;
                    if edges.len() as u32 == frames.len() as u32 {
                        edges.pop();
                        nodes.pop();
                    }
                    edges.push(edge);
                    nodes.push(node);
                    break;
                }
                frames.pop();
                if edges.len() as u32 > frames.len() as u32 {
                    edges.pop();
                    nodes.pop();
                }
            }
        }
    }

    /// Whether an edge carries all the literal property filters.
    fn edge_matches(&mut self, edge_id: u64, props: &[(u32, Value)]) -> Result<bool> {
        if props.is_empty() {
            return Ok(true);
        }
        let Some(record) = self.get_edge(edge_id)? else {
            return Ok(false);
        };
        Ok(props
            .iter()
            .all(|(key, value)| record.get(*key) == Some(value)))
    }

    pub fn node_edges(
        &mut self,
        node_id: u64,
        direction: Direction,
        types: &[u32],
    ) -> Result<Vec<u64>> {
        let family = direction.family();
        let type_ids: Vec<u32> = if types.is_empty() {
            vec![ANY_TYPE]
        } else {
            types.to_vec()
        };
        let mut edges = Vec::new();
        for type_id in type_ids {
            let fragments = self.fragments.fragments_for_band(
                &mut self.pager,
                family,
                type_id,
                (node_id >> 8) as u32,
            )?;
            let row = (node_id & 255) as u8;
            for (key, fragment) in fragments {
                for bit in fragment.row_bits(row) {
                    edges.push((u64::from(key.band1) << 8) | u64::from(bit));
                }
            }
        }
        Ok(edges)
    }

    /// Expand from `node_id`: returns `(edge_id, neighbor_id)` pairs.
    pub fn expand(
        &mut self,
        node_id: u64,
        direction: Direction,
        types: &[u32],
    ) -> Result<Vec<(u64, u64)>> {
        let edge_ids = self.node_edges(node_id, direction, types)?;
        let mut out = Vec::with_capacity(edge_ids.len());
        for edge_id in edge_ids {
            let Some(record) = self.get_edge(edge_id)? else {
                continue;
            };
            let neighbor = match direction {
                Direction::Out => record.dst,
                Direction::In => record.src,
            };
            out.push((edge_id, neighbor));
        }
        Ok(out)
    }

    /// All node ids carrying `label_id` (bitmap scan).
    pub fn label_nodes(&mut self, label_id: u32) -> Result<Vec<u64>> {
        self.fragments.label_nodes(&mut self.pager, label_id)
    }

    /// Node ids carrying every label in `label_ids`, ascending (bitmap
    /// intersection; see [`FragmentStore::nodes_with_labels`]).
    ///
    /// # Errors
    ///
    /// Returns an error when `label_ids` is empty or a fragment is unreadable
    pub fn nodes_with_labels(&mut self, label_ids: &[u32]) -> Result<Vec<u64>> {
        self.fragments.nodes_with_labels(&mut self.pager, label_ids)
    }

    /// All edge ids of `edge_type` (bitmap scan).
    pub fn edges_of_type(&mut self, edge_type: u32) -> Result<Vec<u64>> {
        self.fragments.edges_of_type(&mut self.pager, edge_type)
    }

    /// Whether an edge `src -[type]-> dst` exists (bitmap intersection).
    pub fn edge_exists(&mut self, edge_type: u32, src: u64, dst: u64) -> Result<bool> {
        self.fragments
            .incidence_exists(&mut self.pager, edge_type, src, dst)
    }

    /// Ids, ascending, of nodes whose value in index `index_id` equals
    /// `value` under Cypher equality (see
    /// [`equality_key_ranges`](crate::record::equality_key_ranges)); list
    /// values return a superset that callers re-check.
    pub fn index_lookup(&mut self, index_id: u32, value: &Value) -> Result<Vec<u64>> {
        let def = self
            .schema()
            .node_indexes
            .iter()
            .find(|index| index.id == index_id)
            .cloned()
            .ok_or_else(|| {
                Error::new(
                    ErrorCode::NotFound,
                    format!("node index {index_id} not found"),
                )
            })?;
        let ranges = crate::record::equality_key_ranges(value);
        let tree = Btree::from_root(def.root, TreeKind::Index);
        let mut cursor = tree.cursor();
        let mut out = Vec::new();
        for (start, end) in &ranges {
            let mut ok = cursor.seek_ge(&mut self.pager, start)?;
            while ok && end.as_deref().is_none_or(|end| cursor.key() < end) {
                let key = cursor.key();
                let id_bytes: [u8; 8] = key[key.len() - 8..]
                    .try_into()
                    .map_err(|_| Error::corrupt("index key too short"))?;
                out.push(u64::from_be_bytes(id_bytes));
                ok = cursor.next(&mut self.pager)?;
            }
        }
        // Ids ascend within one stored value only; numeric twins and list
        // ranges span several values and need a merge.
        if ranges.len() > 1 || matches!(value, Value::List(_)) {
            out.sort_unstable();
            out.dedup();
        }
        Ok(out)
    }

    /// Create a node property index, backfilling existing nodes.
    pub fn create_node_index(
        &mut self,
        key_name: &str,
        label_name: Option<&str>,
        unique: bool,
    ) -> Result<u32> {
        self.require_write()?;
        let key_id = self
            .catalog
            .ensure_property_key(&mut self.pager, key_name)?;
        let label_id = match label_name {
            Some(name) => self.catalog.ensure_label(&mut self.pager, name)?,
            None => 0,
        };
        let index_id = self
            .catalog
            .create_node_index(&mut self.pager, key_id, label_id, unique)?;
        self.backfill_node_index(index_id)?;
        Ok(index_id)
    }

    /// Create an edge property index, backfilling existing edges.
    pub fn create_edge_index(
        &mut self,
        key_name: &str,
        type_name: Option<&str>,
        unique: bool,
    ) -> Result<u32> {
        self.require_write()?;
        let key_id = self
            .catalog
            .ensure_property_key(&mut self.pager, key_name)?;
        let type_id = match type_name {
            Some(name) => self.catalog.ensure_edge_type(&mut self.pager, name)?,
            None => 0,
        };
        let index_id = self
            .catalog
            .create_edge_index(&mut self.pager, key_id, type_id, unique)?;
        self.backfill_edge_index(index_id)?;
        Ok(index_id)
    }

    /// Execute a schema DDL statement.
    ///
    /// # Errors
    ///
    /// Returns `Exists` when the name is taken (without `IF NOT EXISTS`),
    /// `NotFound` when a dropped object is missing (without `IF EXISTS`),
    /// `Misuse` when `DROP INDEX`/`DROP CONSTRAINT` names the other kind and
    /// `Constraint` when backfilling a new constraint fails.
    pub fn execute_ddl(&mut self, ddl: &DdlStatement) -> Result<()> {
        self.require_write()?;
        match ddl {
            DdlStatement::CreateIndex(index) => self.create_index_ddl(index),
            DdlStatement::CreateConstraint(constraint) => self.create_constraint_ddl(constraint),
            DdlStatement::DropIndex { name, if_exists } => self.drop_index_ddl(name, *if_exists),
            DdlStatement::DropConstraint { name, if_exists } => {
                self.drop_constraint_ddl(name, *if_exists)
            }
        }
    }

    fn ensure_ddl_name(&self, name: &str, if_not_exists: bool) -> Result<bool> {
        if !self.object_name_taken(name) {
            return Ok(false);
        }
        if if_not_exists {
            return Ok(true);
        }
        Err(Error::new(
            ErrorCode::Exists,
            format!("an index or constraint named `{name}` already exists"),
        ))
    }

    /// Whether any index or constraint carries this name.
    fn object_name_taken(&self, name: &str) -> bool {
        let schema = self.schema();
        schema
            .node_indexes
            .iter()
            .any(|def| def.name.as_deref() == Some(name))
            || schema
                .edge_indexes
                .iter()
                .any(|def| def.name.as_deref() == Some(name))
            || schema.node_constraints.iter().any(|def| def.name == name)
            || schema.edge_constraints.iter().any(|def| def.name == name)
            || schema.fulltext_indexes.iter().any(|def| def.name == name)
    }

    fn ensure_ddl_keys(&mut self, keys: &[String]) -> Result<Vec<u32>> {
        keys.iter()
            .map(|key| self.catalog.ensure_property_key(&mut self.pager, key))
            .collect()
    }

    fn create_index_ddl(&mut self, index: &crate::cypher::DdlIndex) -> Result<()> {
        if self.ensure_ddl_name(&index.name, index.if_not_exists)? {
            return Ok(());
        }
        let key_ids = self.ensure_ddl_keys(&index.keys)?;
        match &index.entity {
            DdlEntity::Node { label } if index.fulltext => {
                let label_id = self.catalog.ensure_label(&mut self.pager, label)?;
                let id = self.catalog.create_fulltext_index(
                    &mut self.pager,
                    &index.name,
                    label_id,
                    &key_ids,
                )?;
                self.backfill_fulltext_index(id)?;
            }
            DdlEntity::Edge { .. } if index.fulltext => {
                return Err(Error::unsupported(
                    "fulltext indexes on relationships are not supported",
                ));
            }
            DdlEntity::Node { label } => {
                let label_id = self.catalog.ensure_label(&mut self.pager, label)?;
                let id = self.catalog.create_node_index_named(
                    &mut self.pager,
                    &index.name,
                    IndexKind::Index,
                    &key_ids,
                    label_id,
                )?;
                self.backfill_node_index(id)?;
            }
            DdlEntity::Edge { edge_type } => {
                let type_id = self.catalog.ensure_edge_type(&mut self.pager, edge_type)?;
                let id = self.catalog.create_edge_index_named(
                    &mut self.pager,
                    &index.name,
                    IndexKind::Index,
                    &key_ids,
                    type_id,
                )?;
                self.backfill_edge_index(id)?;
            }
        }
        Ok(())
    }

    fn create_constraint_ddl(&mut self, constraint: &crate::cypher::DdlConstraint) -> Result<()> {
        if self.ensure_ddl_name(&constraint.name, constraint.if_not_exists)? {
            return Ok(());
        }
        let key_ids = self.ensure_ddl_keys(&constraint.keys)?;
        let kind = match constraint.kind {
            DdlConstraintKind::Unique => IndexKind::Unique,
            DdlConstraintKind::NodeKey => IndexKind::NodeKey,
            DdlConstraintKind::NotNull => {
                return self.create_not_null_constraint(constraint, key_ids[0]);
            }
        };
        match &constraint.entity {
            DdlEntity::Node { label } => {
                let label_id = self.catalog.ensure_label(&mut self.pager, label)?;
                let id = self.catalog.create_node_index_named(
                    &mut self.pager,
                    &constraint.name,
                    kind,
                    &key_ids,
                    label_id,
                )?;
                self.backfill_node_index(id)?;
            }
            DdlEntity::Edge { edge_type } => {
                let type_id = self.catalog.ensure_edge_type(&mut self.pager, edge_type)?;
                let id = self.catalog.create_edge_index_named(
                    &mut self.pager,
                    &constraint.name,
                    kind,
                    &key_ids,
                    type_id,
                )?;
                self.backfill_edge_index(id)?;
            }
        }
        Ok(())
    }

    fn create_not_null_constraint(
        &mut self,
        constraint: &crate::cypher::DdlConstraint,
        key_id: u32,
    ) -> Result<()> {
        match &constraint.entity {
            DdlEntity::Node { label } => {
                let label_id = self.catalog.ensure_label(&mut self.pager, label)?;
                self.catalog.create_node_constraint(
                    &mut self.pager,
                    &constraint.name,
                    label_id,
                    key_id,
                )?;
            }
            DdlEntity::Edge { edge_type } => {
                let type_id = self.catalog.ensure_edge_type(&mut self.pager, edge_type)?;
                self.catalog.create_edge_constraint(
                    &mut self.pager,
                    &constraint.name,
                    type_id,
                    key_id,
                )?;
            }
        }
        Ok(())
    }

    fn drop_index_ddl(&mut self, name: &str, if_exists: bool) -> Result<()> {
        // Index-backed constraints are dropped through DROP CONSTRAINT.
        if let Some(def) = self
            .schema()
            .node_indexes
            .iter()
            .find(|def| def.name.as_deref() == Some(name))
            .cloned()
        {
            if def.kind != IndexKind::Index {
                return Err(Error::misuse(format!(
                    "`{name}` is a constraint; use DROP CONSTRAINT"
                )));
            }
            return self.free_node_index(&def);
        }
        if let Some(def) = self
            .schema()
            .edge_indexes
            .iter()
            .find(|def| def.name.as_deref() == Some(name))
            .cloned()
        {
            if def.kind != IndexKind::Index {
                return Err(Error::misuse(format!(
                    "`{name}` is a constraint; use DROP CONSTRAINT"
                )));
            }
            return self.free_edge_index(&def);
        }
        if let Some(def) = self.fulltext_index_named(name) {
            Btree::from_root(def.root, TreeKind::Index).destroy(&mut self.pager)?;
            return self.catalog.remove_fulltext_index(&mut self.pager, def.id);
        }
        if if_exists {
            Ok(())
        } else {
            Err(Error::new(
                ErrorCode::NotFound,
                format!("index `{name}` does not exist"),
            ))
        }
    }

    fn drop_constraint_ddl(&mut self, name: &str, if_exists: bool) -> Result<()> {
        if let Some(def) = self
            .schema()
            .node_indexes
            .iter()
            .find(|def| def.name.as_deref() == Some(name))
            .cloned()
        {
            if def.kind == IndexKind::Index {
                return Err(Error::misuse(format!(
                    "`{name}` is an index; use DROP INDEX"
                )));
            }
            return self.free_node_index(&def);
        }
        if let Some(def) = self
            .schema()
            .edge_indexes
            .iter()
            .find(|def| def.name.as_deref() == Some(name))
            .cloned()
        {
            if def.kind == IndexKind::Index {
                return Err(Error::misuse(format!(
                    "`{name}` is an index; use DROP INDEX"
                )));
            }
            return self.free_edge_index(&def);
        }
        if let Some(def) = self
            .schema()
            .node_constraints
            .iter()
            .find(|def| def.name == name)
            .cloned()
        {
            return self.catalog.remove_node_constraint(&mut self.pager, def.id);
        }
        if let Some(def) = self
            .schema()
            .edge_constraints
            .iter()
            .find(|def| def.name == name)
            .cloned()
        {
            return self.catalog.remove_edge_constraint(&mut self.pager, def.id);
        }
        if self.fulltext_index_named(name).is_some() {
            return Err(Error::misuse(format!(
                "`{name}` is an index; use DROP INDEX"
            )));
        }
        if if_exists {
            Ok(())
        } else {
            Err(Error::new(
                ErrorCode::NotFound,
                format!("constraint `{name}` does not exist"),
            ))
        }
    }

    fn free_node_index(&mut self, def: &NodeIndexDef) -> Result<()> {
        Btree::from_root(def.root, TreeKind::Index).destroy(&mut self.pager)?;
        self.catalog.remove_node_index(&mut self.pager, def.id)
    }

    fn free_edge_index(&mut self, def: &crate::catalog::EdgeIndexDef) -> Result<()> {
        Btree::from_root(def.root, TreeKind::Index).destroy(&mut self.pager)?;
        self.catalog.remove_edge_index(&mut self.pager, def.id)
    }

    fn fulltext_index_named(&self, name: &str) -> Option<FulltextIndexDef> {
        self.schema()
            .fulltext_indexes
            .iter()
            .find(|def| def.name == name)
            .cloned()
    }

    /// Index every node carrying a new fulltext index's label.
    /// Fill an empty fulltext index from every node with its label: records
    /// are read serially (every page read goes through the pager), then
    /// analyzed and sorted on every core and bulk-built bottom-up.
    fn backfill_fulltext_index(&mut self, index_id: u32) -> Result<()> {
        use rayon::iter::{IntoParallelRefIterator, ParallelIterator};

        let Some(def) = self
            .schema()
            .fulltext_indexes
            .iter()
            .find(|def| def.id == index_id)
            .cloned()
        else {
            return Ok(());
        };
        let node_ids = self.label_nodes(def.label_id)?;
        let progress = ProgressBar::new(node_ids.len() as u64);
        progress.set_style(
            ProgressStyle::with_template("{spinner:.green} {msg} {pos}/{len} [{elapsed_precise}]")
                .expect("valid fulltext backfill template"),
        );
        progress.set_message(format!("reading nodes for fulltext index `{}`", def.name));
        let mut records = Vec::with_capacity(node_ids.len());
        for node_id in node_ids {
            if let Some(record) = self.get_node(node_id)? {
                records.push((node_id, record));
            }
            progress.inc(1);
        }
        progress.finish_and_clear();
        // `fold` builds one buffer per rayon task and `reduce` concatenates
        // them, so analysis runs on every core without shared state.
        let mut postings = records
            .par_iter()
            .fold(IndexKeys::default, |mut postings, (node_id, record)| {
                let texts = def
                    .key_ids
                    .iter()
                    .filter_map(|key_id| record.get(*key_id).and_then(Value::as_text));
                for (term, positions) in document_terms(texts) {
                    postings.push_entry(
                        &term_prefix(&term),
                        *node_id,
                        &encode_positions(&positions),
                    );
                }
                postings
            })
            .reduce(IndexKeys::default, IndexKeys::append);
        postings.sort();
        Btree::from_root(def.root, TreeKind::Index)
            .build_sorted(&mut self.pager, postings.entries())
    }

    /// Search a fulltext index.
    ///
    /// # Arguments
    ///
    /// * `index_name` - Name of the fulltext index
    /// * `query` - Query text: terms, `"phrases"`, `AND` and `OR`
    ///
    /// # Returns
    ///
    /// `(node id, score)` pairs ordered by descending score, then node id
    ///
    /// # Errors
    ///
    /// Returns `NotFound` when no fulltext index has that name and a syntax
    /// error when the query is malformed.
    pub fn fulltext_query(&mut self, index_name: &str, query: &str) -> Result<Vec<(u64, f64)>> {
        let def = self.fulltext_index_named(index_name).ok_or_else(|| {
            Error::new(
                ErrorCode::NotFound,
                format!("fulltext index `{index_name}` does not exist"),
            )
        })?;
        let parsed = FulltextQuery::parse(query)?;
        let tree = Btree::from_root(def.root, TreeKind::Index);
        let mut postings: HashMap<String, Vec<Posting>> = HashMap::new();
        for term in parsed.terms() {
            let prefix = term_prefix(term);
            let mut list = Vec::new();
            let mut cursor = tree.cursor();
            let mut ok = cursor.seek_ge(&mut self.pager, &prefix)?;
            while ok && cursor.key().starts_with(&prefix) {
                let node_id = posting_node(cursor.key())?;
                let positions = decode_positions(&cursor.value(&mut self.pager)?)?;
                list.push(Posting { node_id, positions });
                ok = cursor.next(&mut self.pager)?;
            }
            postings.insert(term.to_owned(), list);
        }
        let doc_count = self.fragments.label_count(&mut self.pager, def.label_id)?;
        Ok(parsed.search(&postings, doc_count))
    }

    /// Insert (`add`) or delete the postings of one node in a fulltext index.
    fn fulltext_update(
        &mut self,
        index_id: u32,
        record: &NodeRecord,
        node_id: u64,
        add: bool,
    ) -> Result<()> {
        let Some(def) = self
            .schema()
            .fulltext_indexes
            .iter()
            .find(|def| def.id == index_id)
        else {
            return Ok(());
        };
        let terms = document_terms(
            def.key_ids
                .iter()
                .filter_map(|key_id| record.get(*key_id).and_then(Value::as_text)),
        );
        let old_root = def.root;
        let mut tree = Btree::from_root(old_root, TreeKind::Index);
        for (term, positions) in &terms {
            let key = posting_key(term, node_id);
            if add {
                tree.insert(&mut self.pager, &key, &encode_positions(positions))?;
            } else {
                tree.delete(&mut self.pager, &key)?;
            }
        }
        if tree.root() != old_root {
            self.catalog
                .set_fulltext_root(&mut self.pager, index_id, tree.root())?;
        }
        Ok(())
    }

    /// Update every fulltext index covering `record`; with `key_id`, only
    /// those indexing that property.
    fn fulltext_sync(
        &mut self,
        record: &NodeRecord,
        node_id: u64,
        key_id: Option<u32>,
        add: bool,
    ) -> Result<()> {
        let ids: Vec<u32> = self
            .schema()
            .fulltext_indexes
            .iter()
            .filter(|def| {
                record.labels.contains(&def.label_id)
                    && key_id.is_none_or(|key_id| def.key_ids.contains(&key_id))
            })
            .map(|def| def.id)
            .collect();
        for id in ids {
            self.fulltext_update(id, record, node_id, add)?;
        }
        Ok(())
    }

    /// Index every node matching a node index's label: collect the keys,
    /// sort them in parallel and bulk-build the (empty) index tree.
    fn backfill_node_index(&mut self, index_id: u32) -> Result<()> {
        let Some(def) = self
            .schema()
            .node_indexes
            .iter()
            .find(|def| def.id == index_id)
            .cloned()
        else {
            return Ok(());
        };
        let mut keys = IndexKeys::default();
        let mut seen = UniqueValues::new(def.unique());
        let mut cursor = self.nodes.cursor();
        let mut ok = cursor.first(&mut self.pager)?;
        while ok {
            let node_id = table_key_id(cursor.key(), "node")?;
            let record = NodeRecord::decode(&cursor.value(&mut self.pager)?)?;
            if (def.label_id == 0 || record.labels.contains(&def.label_id))
                && let Some(prefix) = node_index_prefix(&record, &def)
            {
                if !seen.insert(def.key_ids.iter().filter_map(|key| record.get(*key))) {
                    return Err(Error::constraint(format!(
                        "unique index {} violated by node {node_id}",
                        def.id
                    )));
                }
                keys.push(&prefix, node_id);
            }
            ok = cursor.next(&mut self.pager)?;
        }
        keys.sort();
        Btree::from_root(def.root, TreeKind::Index).build_sorted(&mut self.pager, keys.entries())
    }

    /// Index every edge matching an edge index's type (see
    /// [`Self::backfill_node_index`]).
    fn backfill_edge_index(&mut self, index_id: u32) -> Result<()> {
        let Some(def) = self
            .schema()
            .edge_indexes
            .iter()
            .find(|def| def.id == index_id)
            .cloned()
        else {
            return Ok(());
        };
        let mut keys = IndexKeys::default();
        let mut seen = UniqueValues::new(def.unique());
        let mut cursor = self.edges.cursor();
        let mut ok = cursor.first(&mut self.pager)?;
        while ok {
            let edge_id = table_key_id(cursor.key(), "edge")?;
            let record = EdgeRecord::decode(&cursor.value(&mut self.pager)?)?;
            if (def.type_id == 0 || record.edge_type == def.type_id)
                && let Some(prefix) = edge_index_prefix(&record, &def)
            {
                if !seen.insert(def.key_ids.iter().filter_map(|key| record.get(*key))) {
                    return Err(Error::constraint(format!(
                        "unique index {} violated by edge {edge_id}",
                        def.id
                    )));
                }
                keys.push(&prefix, edge_id);
            }
            ok = cursor.next(&mut self.pager)?;
        }
        keys.sort();
        Btree::from_root(def.root, TreeKind::Index).build_sorted(&mut self.pager, keys.entries())
    }

    /// All node ids in ascending order.
    pub fn all_node_ids(&mut self) -> Result<Vec<u64>> {
        let mut cursor = self.nodes.cursor();
        let mut ids = Vec::new();
        let mut ok = cursor.first(&mut self.pager)?;
        while ok {
            let key: [u8; 8] = cursor
                .key()
                .try_into()
                .map_err(|_| Error::corrupt("node key is not 8 bytes"))?;
            ids.push(u64::from_be_bytes(key));
            ok = cursor.next(&mut self.pager)?;
        }
        Ok(ids)
    }

    /// Property statistics per label set, for
    /// `db.schema.nodeTypeProperties()`: one scan of the node table.
    ///
    /// # Errors
    ///
    /// Propagates storage and record decoding errors.
    pub fn node_type_properties(&mut self) -> Result<Vec<TypeProperty>> {
        let mut groups: BTreeMap<Vec<u32>, PropertyStats> = BTreeMap::new();
        let mut cursor = self.nodes.cursor();
        let mut ok = cursor.first(&mut self.pager)?;
        while ok {
            let record = NodeRecord::decode(&cursor.value(&mut self.pager)?)?;
            groups
                .entry(record.labels)
                .or_default()
                .observe(&record.props);
            ok = cursor.next(&mut self.pager)?;
        }
        let schema = self.schema();
        Ok(groups
            .into_iter()
            .flat_map(|(labels, stats)| {
                let names = labels
                    .iter()
                    .map(|label| schema.label_name(*label).unwrap_or("?").to_owned())
                    .collect();
                stats.rows(names, schema)
            })
            .collect())
    }

    /// Property statistics per relationship type, for
    /// `db.schema.relTypeProperties()`: one scan of the edge table.
    ///
    /// # Errors
    ///
    /// Propagates storage and record decoding errors.
    pub fn rel_type_properties(&mut self) -> Result<Vec<TypeProperty>> {
        let mut groups: BTreeMap<u32, PropertyStats> = BTreeMap::new();
        let mut cursor = self.edges.cursor();
        let mut ok = cursor.first(&mut self.pager)?;
        while ok {
            let record = EdgeRecord::decode(&cursor.value(&mut self.pager)?)?;
            groups
                .entry(record.edge_type)
                .or_default()
                .observe(&record.props);
            ok = cursor.next(&mut self.pager)?;
        }
        let schema = self.schema();
        Ok(groups
            .into_iter()
            .flat_map(|(edge_type, stats)| {
                let name = schema.edge_type_name(edge_type).unwrap_or("?").to_owned();
                stats.rows(vec![name], schema)
            })
            .collect())
    }

    /// All edge ids in ascending order.
    pub fn all_edge_ids(&mut self) -> Result<Vec<u64>> {
        let mut cursor = self.edges.cursor();
        let mut ids = Vec::new();
        let mut ok = cursor.first(&mut self.pager)?;
        while ok {
            let key: [u8; 8] = cursor
                .key()
                .try_into()
                .map_err(|_| Error::corrupt("edge key is not 8 bytes"))?;
            ids.push(u64::from_be_bytes(key));
            ok = cursor.next(&mut self.pager)?;
        }
        Ok(ids)
    }

    /// Find nodes matching all labels and properties (using an index when
    /// available), creating one when none exists.
    pub fn match_or_create_node(
        &mut self,
        labels: &[u32],
        props: Vec<(u32, Value)>,
    ) -> Result<(u64, bool)> {
        let props = normalize_props(props)?;
        let candidates = self.match_nodes(labels, &props)?;
        if let Some(id) = candidates.first() {
            return Ok((*id, false));
        }
        let id = self.create_node(labels, props)?;
        Ok((id, true))
    }

    /// Find edges matching `(type, src, dst, props)`, creating one when none
    /// exists.
    pub fn match_or_create_edge(
        &mut self,
        edge_type: u32,
        src: u64,
        dst: u64,
        props: Vec<(u32, Value)>,
    ) -> Result<(u64, bool)> {
        let props = normalize_props(props)?;
        let candidates: Vec<u64> = self
            .expand(src, Direction::Out, &[edge_type])?
            .into_iter()
            .filter(|(_, neighbor)| *neighbor == dst)
            .map(|(edge, _)| edge)
            .collect();
        for edge_id in candidates {
            if let Some(record) = self.get_edge(edge_id)?
                && props_match(&record.props, &props)
            {
                return Ok((edge_id, false));
            }
        }
        let id = self.create_edge(edge_type, src, dst, props)?;
        Ok((id, true))
    }

    /// Nodes matching all labels and properties.
    pub fn match_nodes(&mut self, labels: &[u32], props: &[(u32, Value)]) -> Result<Vec<u64>> {
        let mut candidates: Option<Vec<u64>> = None;
        for (key_id, value) in props {
            let index = self
                .schema()
                .node_indexes
                .iter()
                .find(|index| {
                    index.seekable()
                        && index.key_id() == *key_id
                        && (index.label_id == 0 || labels.contains(&index.label_id))
                })
                .cloned();
            if let Some(index) = index {
                let ids = self.index_lookup(index.id, value)?;
                candidates = Some(match candidates {
                    None => ids,
                    Some(previous) => intersect_ids(previous, ids),
                });
                break;
            }
        }
        let candidates = match candidates {
            Some(ids) => ids,
            None => match labels.first() {
                Some(label) => self.label_nodes(*label)?,
                None => self.all_node_ids()?,
            },
        };
        let mut out = Vec::new();
        for id in candidates {
            if let Some(record) = self.get_node(id)?
                && labels.iter().all(|label| record.labels.contains(label))
                && props_match(&record.props, props)
            {
                out.push(id);
            }
        }
        Ok(out)
    }

    /// Add labels to a node, updating bitmaps and indexes.
    pub fn add_node_labels(&mut self, node_id: u64, labels: &[u32]) -> Result<()> {
        self.require_write()?;
        let Some(mut record) = self.get_node(node_id)? else {
            return Err(Error::new(
                ErrorCode::NotFound,
                format!("node {node_id} does not exist"),
            ));
        };
        let mut combined = record.labels.clone();
        combined.extend_from_slice(labels);
        combined.sort_unstable();
        combined.dedup();
        check_node_existence(self.schema(), &combined, &record.props)?;
        self.unindex_node(&record, node_id)?;
        for label in labels {
            if !record.labels.contains(label) {
                record.labels.push(*label);
            }
        }
        record.labels.sort_unstable();
        record.labels.dedup();
        let key_bytes = node_id.to_be_bytes();
        self.nodes.delete(&mut self.pager, &key_bytes)?;
        self.nodes
            .insert(&mut self.pager, &key_bytes, &record.encode())?;
        for label in labels {
            let key = FragmentKey::label(*label, node_id);
            self.fragments
                .set_bit(&mut self.pager, &key, FragmentKey::label_bit(node_id))?;
        }
        self.index_node(&record, node_id)
    }

    /// Remove labels from a node, updating bitmaps and indexes.
    pub fn remove_node_labels(&mut self, node_id: u64, labels: &[u32]) -> Result<()> {
        self.require_write()?;
        let Some(mut record) = self.get_node(node_id)? else {
            return Err(Error::new(
                ErrorCode::NotFound,
                format!("node {node_id} does not exist"),
            ));
        };
        self.unindex_node(&record, node_id)?;
        record.labels.retain(|label| !labels.contains(label));
        let key_bytes = node_id.to_be_bytes();
        self.nodes.delete(&mut self.pager, &key_bytes)?;
        self.nodes
            .insert(&mut self.pager, &key_bytes, &record.encode())?;
        for label in labels {
            let key = FragmentKey::label(*label, node_id);
            self.fragments
                .clear_bit(&mut self.pager, &key, FragmentKey::label_bit(node_id))?;
        }
        self.index_node(&record, node_id)
    }

    /// Replace every property of a node.
    pub fn replace_node_properties(
        &mut self,
        node_id: u64,
        props: Vec<(u32, Value)>,
    ) -> Result<()> {
        self.require_write()?;
        let Some(mut record) = self.get_node(node_id)? else {
            return Err(Error::new(
                ErrorCode::NotFound,
                format!("node {node_id} does not exist"),
            ));
        };
        check_node_existence(self.schema(), &record.labels, &props)?;
        self.unindex_node(&record, node_id)?;
        record.props = normalize_props(props)?;
        let key_bytes = node_id.to_be_bytes();
        self.nodes.delete(&mut self.pager, &key_bytes)?;
        self.nodes
            .insert(&mut self.pager, &key_bytes, &record.encode())?;
        self.index_node(&record, node_id)
    }

    /// Replace every property of an edge.
    ///
    /// # Errors
    ///
    /// Returns `NotFound` when the edge does not exist.
    pub fn replace_edge_properties(
        &mut self,
        edge_id: u64,
        props: Vec<(u32, Value)>,
    ) -> Result<()> {
        self.require_write()?;
        let Some(mut record) = self.get_edge(edge_id)? else {
            return Err(Error::new(
                ErrorCode::NotFound,
                format!("edge {edge_id} does not exist"),
            ));
        };
        check_edge_existence(self.schema(), record.edge_type, &props)?;
        self.unindex_edge(&record, edge_id)?;
        record.props = normalize_props(props)?;
        let key_bytes = edge_id.to_be_bytes();
        self.edges.delete(&mut self.pager, &key_bytes)?;
        self.edges
            .insert(&mut self.pager, &key_bytes, &record.encode())?;
        self.index_edge(&record, edge_id)
    }

    fn require_write(&self) -> Result<()> {
        if !self.pager.in_write_txn() {
            return Err(Error::misuse(
                "mutation requires a write transaction (use Database::write)",
            ));
        }
        Ok(())
    }

    fn index_node(&mut self, record: &NodeRecord, node_id: u64) -> Result<()> {
        self.fulltext_sync(record, node_id, None, true)?;
        let indexes: Vec<_> = self.schema().node_indexes.clone();
        for index in indexes {
            if index.label_id == 0 || record.labels.contains(&index.label_id) {
                self.insert_index_entry(index.id, record, node_id)?;
            }
        }
        Ok(())
    }

    fn unindex_node(&mut self, record: &NodeRecord, node_id: u64) -> Result<()> {
        self.fulltext_sync(record, node_id, None, false)?;
        let indexes: Vec<_> = self.schema().node_indexes.clone();
        for index in indexes {
            self.delete_index_entry(&index, record, node_id)?;
        }
        Ok(())
    }

    fn index_node_property(
        &mut self,
        record: &NodeRecord,
        node_id: u64,
        key_id: u32,
    ) -> Result<()> {
        self.fulltext_sync(record, node_id, Some(key_id), true)?;
        let indexes: Vec<_> = self
            .schema()
            .node_indexes_for_key(key_id)
            .cloned()
            .collect();
        for index in indexes {
            if index.label_id == 0 || record.labels.contains(&index.label_id) {
                self.insert_index_entry(index.id, record, node_id)?;
            }
        }
        Ok(())
    }

    fn unindex_node_property(
        &mut self,
        record: &NodeRecord,
        node_id: u64,
        key_id: u32,
    ) -> Result<()> {
        self.fulltext_sync(record, node_id, Some(key_id), false)?;
        let indexes: Vec<_> = self
            .schema()
            .node_indexes_for_key(key_id)
            .cloned()
            .collect();
        for index in indexes {
            self.delete_index_entry(&index, record, node_id)?;
        }
        Ok(())
    }

    fn insert_index_entry(
        &mut self,
        index_id: u32,
        record: &NodeRecord,
        node_id: u64,
    ) -> Result<()> {
        let def = self
            .schema()
            .node_indexes
            .iter()
            .find(|index| index.id == index_id)
            .cloned();
        let Some(def) = def else {
            return Ok(());
        };
        let Some(prefix) = node_index_prefix(record, &def) else {
            return Ok(());
        };
        let mut tree = Btree::from_root(def.root, TreeKind::Index);
        // Index keys embed the entity id, so key uniqueness cannot enforce
        // value uniqueness: probe the value prefixes instead.
        if def.unique()
            && self
                .any_prefix_present(&tree, def.key_ids.iter().filter_map(|key| record.get(*key)))?
        {
            return Err(Error::constraint(format!(
                "unique index {} violated by node {node_id}",
                def.id
            )));
        }
        let mut key = prefix;
        key.extend_from_slice(&node_id.to_be_bytes());
        tree.insert(&mut self.pager, &key, &[])?;
        self.replace_node_index_root(index_id, tree.root())
    }

    fn delete_index_entry(
        &mut self,
        def: &NodeIndexDef,
        record: &NodeRecord,
        node_id: u64,
    ) -> Result<()> {
        let Some(prefix) = node_index_prefix(record, def) else {
            return Ok(());
        };
        let mut key = prefix;
        key.extend_from_slice(&node_id.to_be_bytes());
        let mut tree = Btree::from_root(def.root, TreeKind::Index);
        tree.delete(&mut self.pager, &key)?;
        self.replace_node_index_root(def.id, tree.root())
    }

    fn replace_node_index_root(&mut self, index_id: u32, root: u32) -> Result<()> {
        let unchanged = self
            .schema()
            .node_indexes
            .iter()
            .find(|index| index.id == index_id)
            .is_some_and(|index| index.root == root);
        if unchanged {
            // Roots only move on splits; rewriting the catalog entry on every
            // insert would cost two extra B+tree operations per row.
            return Ok(());
        }
        if let Some(def) = self
            .catalog
            .schema_mut()
            .node_indexes
            .iter_mut()
            .find(|index| index.id == index_id)
        {
            def.root = root;
        }
        // Persist the root through the catalog entry.
        let snapshot = self
            .schema()
            .node_indexes
            .iter()
            .find(|index| index.id == index_id)
            .cloned();
        if let Some(def) = snapshot {
            self.catalog.update_node_index(&mut self.pager, &def)?;
        }
        Ok(())
    }

    fn index_edge(&mut self, record: &EdgeRecord, edge_id: u64) -> Result<()> {
        let indexes: Vec<_> = self.schema().edge_indexes.clone();
        for index in indexes {
            if index.type_id == 0 || record.edge_type == index.type_id {
                self.insert_edge_index_entry(index.id, record, edge_id)?;
            }
        }
        Ok(())
    }

    fn unindex_edge(&mut self, record: &EdgeRecord, edge_id: u64) -> Result<()> {
        let indexes: Vec<_> = self.schema().edge_indexes.clone();
        for index in indexes {
            self.delete_edge_index_entry(&index, record, edge_id)?;
        }
        Ok(())
    }

    fn index_edge_property(
        &mut self,
        record: &EdgeRecord,
        edge_id: u64,
        key_id: u32,
    ) -> Result<()> {
        let indexes: Vec<_> = self
            .schema()
            .edge_indexes_for_key(key_id)
            .cloned()
            .collect();
        for index in indexes {
            if index.type_id == 0 || record.edge_type == index.type_id {
                self.insert_edge_index_entry(index.id, record, edge_id)?;
            }
        }
        Ok(())
    }

    fn unindex_edge_property(
        &mut self,
        record: &EdgeRecord,
        edge_id: u64,
        key_id: u32,
    ) -> Result<()> {
        let indexes: Vec<_> = self
            .schema()
            .edge_indexes_for_key(key_id)
            .cloned()
            .collect();
        for index in indexes {
            self.delete_edge_index_entry(&index, record, edge_id)?;
        }
        Ok(())
    }

    fn insert_edge_index_entry(
        &mut self,
        index_id: u32,
        record: &EdgeRecord,
        edge_id: u64,
    ) -> Result<()> {
        let def = self
            .schema()
            .edge_indexes
            .iter()
            .find(|index| index.id == index_id)
            .cloned();
        let Some(def) = def else {
            return Ok(());
        };
        let Some(prefix) = edge_index_prefix(record, &def) else {
            return Ok(());
        };
        let mut tree = Btree::from_root(def.root, TreeKind::Index);
        if def.unique()
            && self
                .any_prefix_present(&tree, def.key_ids.iter().filter_map(|key| record.get(*key)))?
        {
            return Err(Error::constraint(format!(
                "unique index {} violated by edge {edge_id}",
                def.id
            )));
        }
        let mut key = prefix;
        key.extend_from_slice(&edge_id.to_be_bytes());
        tree.insert(&mut self.pager, &key, &[])?;
        self.replace_edge_index_root(index_id, tree.root())
    }

    /// Whether `tree` holds an entry whose values equal `values` under
    /// Cypher equality (so `1` matches an entry stored for `1.0`).
    fn any_prefix_present<'v>(
        &mut self,
        tree: &Btree,
        values: impl Iterator<Item = &'v Value>,
    ) -> Result<bool> {
        let mut cursor = tree.cursor();
        for prefix in equal_value_prefixes(values) {
            if cursor.seek_ge(&mut self.pager, &prefix)? && cursor.key().starts_with(&prefix) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn delete_edge_index_entry(
        &mut self,
        def: &crate::catalog::EdgeIndexDef,
        record: &EdgeRecord,
        edge_id: u64,
    ) -> Result<()> {
        let Some(prefix) = edge_index_prefix(record, def) else {
            return Ok(());
        };
        let mut key = prefix;
        key.extend_from_slice(&edge_id.to_be_bytes());
        let mut tree = Btree::from_root(def.root, TreeKind::Index);
        tree.delete(&mut self.pager, &key)?;
        self.replace_edge_index_root(def.id, tree.root())
    }

    fn replace_edge_index_root(&mut self, index_id: u32, root: u32) -> Result<()> {
        let unchanged = self
            .schema()
            .edge_indexes
            .iter()
            .find(|index| index.id == index_id)
            .is_some_and(|index| index.root == root);
        if unchanged {
            return Ok(());
        }
        if let Some(def) = self
            .catalog
            .schema_mut()
            .edge_indexes
            .iter_mut()
            .find(|index| index.id == index_id)
        {
            def.root = root;
        }
        let snapshot = self
            .schema()
            .edge_indexes
            .iter()
            .find(|index| index.id == index_id)
            .cloned();
        if let Some(def) = snapshot {
            self.catalog.update_edge_index(&mut self.pager, &def)?;
        }
        Ok(())
    }
}

/// Enforce `IS NOT NULL` and NODE KEY existence when a node is written.
fn check_node_existence(schema: &Schema, labels: &[u32], props: &[(u32, Value)]) -> Result<()> {
    let has = |key_id: u32| {
        props
            .iter()
            .any(|(key, value)| *key == key_id && !value.is_null())
    };
    for constraint in &schema.node_constraints {
        if constraint.label_id != 0 && !labels.contains(&constraint.label_id) {
            continue;
        }
        if !has(constraint.key_id) {
            return Err(missing_property(
                schema,
                &constraint.name,
                constraint.key_id,
            ));
        }
    }
    for index in &schema.node_indexes {
        if index.kind != IndexKind::NodeKey {
            continue;
        }
        if index.label_id != 0 && !labels.contains(&index.label_id) {
            continue;
        }
        for key_id in &index.key_ids {
            if !has(*key_id) {
                return Err(missing_property(
                    schema,
                    index.name.as_deref().unwrap_or("-"),
                    *key_id,
                ));
            }
        }
    }
    Ok(())
}

/// Enforce `IS NOT NULL` and NODE KEY existence when one node property goes
/// away (`SET n.key = null`, `REMOVE n.key`).
fn check_node_removal(schema: &Schema, labels: &[u32], key_id: u32) -> Result<()> {
    for constraint in &schema.node_constraints {
        if constraint.key_id == key_id
            && (constraint.label_id == 0 || labels.contains(&constraint.label_id))
        {
            return Err(missing_property(schema, &constraint.name, key_id));
        }
    }
    for index in &schema.node_indexes {
        if index.kind == IndexKind::NodeKey
            && index.key_ids.contains(&key_id)
            && (index.label_id == 0 || labels.contains(&index.label_id))
        {
            return Err(missing_property(
                schema,
                index.name.as_deref().unwrap_or("-"),
                key_id,
            ));
        }
    }
    Ok(())
}

/// Enforce `IS NOT NULL` existence when an edge is written.
fn check_edge_existence(schema: &Schema, edge_type: u32, props: &[(u32, Value)]) -> Result<()> {
    let has = |key_id: u32| {
        props
            .iter()
            .any(|(key, value)| *key == key_id && !value.is_null())
    };
    for constraint in &schema.edge_constraints {
        if constraint.type_id != 0 && constraint.type_id != edge_type {
            continue;
        }
        if !has(constraint.key_id) {
            return Err(missing_property(
                schema,
                &constraint.name,
                constraint.key_id,
            ));
        }
    }
    Ok(())
}

/// Enforce `IS NOT NULL` existence when one edge property goes away.
fn check_edge_removal(schema: &Schema, edge_type: u32, key_id: u32) -> Result<()> {
    for constraint in &schema.edge_constraints {
        if constraint.key_id == key_id
            && (constraint.type_id == 0 || constraint.type_id == edge_type)
        {
            return Err(missing_property(schema, &constraint.name, key_id));
        }
    }
    Ok(())
}

fn missing_property(schema: &Schema, object: &str, key_id: u32) -> Error {
    Error::constraint(format!(
        "constraint `{object}` requires the property `{}` to exist",
        schema.property_key_name(key_id).unwrap_or("?")
    ))
}

fn normalize_props(props: Vec<(u32, Value)>) -> Result<Vec<(u32, Value)>> {
    for (_, value) in &props {
        validate_property_value(value)?;
    }
    let mut props: Vec<(u32, Value)> = props
        .into_iter()
        .filter(|(_, value)| !value.is_null())
        .collect();
    props.sort_by_key(|(key, _)| *key);
    Ok(props)
}

/// Property values may be primitives or flat lists of primitives; entity
/// references and maps are rejected, matching openCypher.
fn validate_property_value(value: &Value) -> Result<()> {
    match value {
        Value::Null
        | Value::Bool(_)
        | Value::Int(_)
        | Value::Float(_)
        | Value::Text(_)
        | Value::Bytes(_)
        | Value::Date(_)
        | Value::LocalTime(_)
        | Value::Time(_)
        | Value::LocalDateTime(_)
        | Value::DateTime(_)
        | Value::Duration(_)
        | Value::Point(_) => Ok(()),
        Value::List(items) => {
            for item in items.iter() {
                if matches!(
                    item,
                    Value::List(_) | Value::Map(_) | Value::Node(_) | Value::Edge(_)
                ) {
                    return Err(Error::type_error(
                        "lists stored as properties may only contain primitive values",
                    ));
                }
            }
            Ok(())
        }
        Value::Map(_) | Value::Node(_) | Value::Edge(_) => Err(Error::type_error(format!(
            "{} values cannot be stored as properties",
            value.type_name()
        ))),
    }
}

/// Index key columns for a node: the encoded values of every key column in
/// order. `None` when a keyed property is missing (the node has no entry).
fn node_index_prefix(record: &NodeRecord, def: &NodeIndexDef) -> Option<Vec<u8>> {
    let mut prefix = Vec::with_capacity(16 * def.key_ids.len());
    for key_id in &def.key_ids {
        encode_index_key(&mut prefix, record.get(*key_id)?);
    }
    Some(prefix)
}

/// Index key columns for an edge.
fn edge_index_prefix(record: &EdgeRecord, def: &crate::catalog::EdgeIndexDef) -> Option<Vec<u8>> {
    let mut prefix = Vec::with_capacity(16 * def.key_ids.len());
    for key_id in &def.key_ids {
        encode_index_key(&mut prefix, record.get(*key_id)?);
    }
    Some(prefix)
}

/// The other numeric value equal to `value` under Cypher equality: the
/// float of an integer it represents exactly, or the integer of an
/// integral float in range.
fn numeric_twin(value: &Value) -> Option<Value> {
    match value {
        Value::Int(integer) => {
            let float = *integer as f64;
            // Compare in i128: `i64::MAX as f64` rounds up to 2^63, which an
            // `as i64` cast would saturate back to `i64::MAX`.
            (float as i128 == i128::from(*integer)).then_some(Value::Float(float))
        }
        Value::Float(float) => {
            const TWO_POW_63: f64 = 9_223_372_036_854_775_808.0;
            (float.fract() == 0.0 && (-TWO_POW_63..TWO_POW_63).contains(float))
                .then_some(Value::Int(*float as i64))
        }
        _ => None,
    }
}

/// Every encoded index prefix whose values equal `values` under Cypher
/// equality: each numeric value also appears as its [`numeric_twin`].
fn equal_value_prefixes<'v>(values: impl Iterator<Item = &'v Value>) -> Vec<Vec<u8>> {
    let mut prefixes = vec![Vec::new()];
    for value in values {
        let twin = numeric_twin(value);
        let twin_prefixes = twin.as_ref().map(|_| prefixes.clone());
        for prefix in &mut prefixes {
            encode_index_key(prefix, value);
        }
        if let (Some(twin), Some(mut twin_prefixes)) = (twin, twin_prefixes) {
            for prefix in &mut twin_prefixes {
                encode_index_key(prefix, &twin);
            }
            prefixes.extend(twin_prefixes);
        }
    }
    prefixes
}

/// Value tuples seen while backfilling a unique index, compared under Cypher
/// equality by encoding integral floats as integers.
struct UniqueValues {
    seen: Option<HashSet<Vec<u8>>>,
}

impl UniqueValues {
    /// A tracker that checks values only when `unique` is set.
    fn new(unique: bool) -> Self {
        Self {
            seen: unique.then(HashSet::new),
        }
    }

    /// Record `values`; `false` when an equal tuple was already recorded.
    fn insert<'v>(&mut self, values: impl Iterator<Item = &'v Value>) -> bool {
        let Some(seen) = &mut self.seen else {
            return true;
        };
        let mut canonical = Vec::new();
        for value in values {
            match value {
                Value::Float(_) => match numeric_twin(value) {
                    Some(integer) => encode_index_key(&mut canonical, &integer),
                    None => encode_index_key(&mut canonical, value),
                },
                _ => encode_index_key(&mut canonical, value),
            }
        }
        seen.insert(canonical)
    }
}

/// The id in an 8-byte big-endian record table key.
fn table_key_id(key: &[u8], what: &str) -> Result<u64> {
    let bytes: [u8; 8] = key
        .try_into()
        .map_err(|_| Error::corrupt(format!("{what} key is not 8 bytes")))?;
    Ok(u64::from_be_bytes(bytes))
}

/// One row of `db.schema.nodeTypeProperties()` or `relTypeProperties()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeProperty {
    /// Sorted label names, or the relationship type name.
    pub type_names: Vec<String>,
    /// The property, or `None` for a label set or type without properties.
    pub property: Option<String>,
    /// Types the property's values have (`STRING`, `LIST<INTEGER>`, ...).
    pub types: Vec<String>,
    /// Whether every node or relationship of the group has the property.
    pub mandatory: bool,
}

/// Running property statistics for one label set or relationship type.
#[derive(Debug, Default)]
struct PropertyStats {
    entities: u64,
    /// Key id to (entities with the key, the value types seen).
    keys: BTreeMap<u32, (u64, std::collections::BTreeSet<String>)>,
}

impl PropertyStats {
    fn observe(&mut self, props: &[(u32, Value)]) {
        self.entities += 1;
        for (key_id, value) in props {
            let (count, types) = self.keys.entry(*key_id).or_default();
            *count += 1;
            // Stored values are never null, so the nullable spelling drops
            // the redundant `NOT NULL` (`mandatory` says it instead).
            types.insert(CypherType::of(value).nullable().to_string());
        }
    }

    fn rows(self, type_names: Vec<String>, schema: &Schema) -> Vec<TypeProperty> {
        if self.keys.is_empty() {
            return vec![TypeProperty {
                type_names,
                property: None,
                types: Vec::new(),
                mandatory: false,
            }];
        }
        self.keys
            .into_iter()
            .map(|(key_id, (count, types))| TypeProperty {
                type_names: type_names.clone(),
                property: Some(schema.property_key_name(key_id).unwrap_or("?").to_owned()),
                types: types.into_iter().collect(),
                mandatory: count == self.entities,
            })
            .collect()
    }
}

/// Index entries gathered for a bulk build: one flat byte buffer plus the
/// spans of each entry, so millions of entries cost two allocations rather
/// than one each. Every key is a value prefix followed by an 8-byte entity
/// id; property index entries carry an empty value, fulltext postings their
/// encoded positions.
#[derive(Debug, Default)]
struct IndexKeys {
    bytes: Vec<u8>,
    spans: Vec<EntrySpan>,
}

/// Where one entry lives in [`IndexKeys::bytes`]: the key, then its value
/// up to `value_end`.
#[derive(Debug, Clone)]
struct EntrySpan {
    key: std::ops::Range<usize>,
    value_end: usize,
}

impl IndexKeys {
    fn push(&mut self, prefix: &[u8], id: u64) {
        self.push_entry(prefix, id, &[]);
    }

    fn push_entry(&mut self, prefix: &[u8], id: u64, value: &[u8]) {
        let start = self.bytes.len();
        self.bytes.extend_from_slice(prefix);
        self.bytes.extend_from_slice(&id.to_be_bytes());
        let key_end = self.bytes.len();
        self.bytes.extend_from_slice(value);
        self.spans.push(EntrySpan {
            key: start..key_end,
            value_end: self.bytes.len(),
        });
    }

    /// Move every entry of `other` to the end of `self` (the reduce step of
    /// a parallel gather).
    fn append(mut self, other: Self) -> Self {
        let offset = self.bytes.len();
        self.bytes.extend_from_slice(&other.bytes);
        self.spans
            .extend(other.spans.into_iter().map(|span| EntrySpan {
                key: span.key.start + offset..span.key.end + offset,
                value_end: span.value_end + offset,
            }));
        self
    }

    fn key(&self, span: &EntrySpan) -> &[u8] {
        &self.bytes[span.key.clone()]
    }

    /// Order the entries by key, sorting on every core (keys are unique
    /// because they end in the entity id, so an unstable sort is
    /// deterministic).
    fn sort(&mut self) {
        use rayon::slice::ParallelSliceMut;
        let bytes = &self.bytes;
        self.spans
            .par_sort_unstable_by(|a, b| bytes[a.key.clone()].cmp(&bytes[b.key.clone()]));
    }

    fn entries(&self) -> impl Iterator<Item = (&[u8], &[u8])> {
        self.spans
            .iter()
            .map(|span| (self.key(span), &self.bytes[span.key.end..span.value_end]))
    }
}

#[cfg(test)]
fn index_value_prefix(value: &Value) -> Vec<u8> {
    let mut prefix = Vec::with_capacity(16);
    encode_index_key(&mut prefix, value);
    prefix
}

/// Intersection of two ascending id lists.
fn intersect_ids(a: Vec<u64>, b: Vec<u64>) -> Vec<u64> {
    let mut out = Vec::new();
    let (mut i, mut j) = (0usize, 0usize);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                out.push(a[i]);
                i += 1;
                j += 1;
            }
        }
    }
    out
}

/// Whether every expected `(key, value)` is present and equal in `actual`.
fn props_match(actual: &[(u32, Value)], expected: &[(u32, Value)]) -> bool {
    expected.iter().all(|(key, value)| {
        actual
            .binary_search_by_key(key, |(k, _)| *k)
            .ok()
            .is_some_and(|idx| crate::record::value_equals(&actual[idx].1, value) == Some(true))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::OpenMode;
    use crate::vfs::mem::{MemVfs, new_store};

    fn temp_db() -> Database {
        Database::create(Arc::new(MemVfs::new()), Path::new("engine.db"), 4096).expect("create db")
    }

    /// The recursive depth-first enumeration the streaming expansion
    /// replaced, kept as its oracle.
    fn reference_trails(
        db: &mut Database,
        directions: &[Direction],
        bounds: (u32, Option<u32>),
        trail: &mut Vec<u64>,
        nodes: &mut Vec<u64>,
        out: &mut Vec<(Vec<u64>, Vec<u64>)>,
    ) {
        let (min, max) = bounds;
        if max.is_some_and(|max| trail.len() as u32 >= max) {
            return;
        }
        let node = *nodes.last().expect("start");
        let mut neighbours = Vec::new();
        for direction in directions {
            neighbours.extend(db.expand(node, *direction, &[]).expect("expand"));
        }
        neighbours.sort_unstable();
        neighbours.dedup();
        for (edge, next) in neighbours {
            if trail.contains(&edge) {
                continue;
            }
            trail.push(edge);
            nodes.push(next);
            if trail.len() as u32 >= min {
                out.push((trail.clone(), nodes.clone()));
            }
            reference_trails(db, directions, bounds, trail, nodes, out);
            trail.pop();
            nodes.pop();
        }
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(48))]

        #[test]
        fn streaming_expansion_matches_recursive_enumeration(
            edges in proptest::collection::vec((0usize..6, 0usize..6), 0..10),
            min in 0u32..3,
            extra in proptest::option::of(0u32..4),
            direction in 0usize..3,
        ) {
            let mut db = temp_db();
            let ids = db
                .write(|db| {
                    let t = db.ensure_edge_type("T")?;
                    let ids: Vec<u64> =
                        (0..6).map(|_| db.create_node(&[], vec![])).collect::<Result<_>>()?;
                    for (src, dst) in &edges {
                        db.create_edge(t, ids[*src], ids[*dst], vec![])?;
                    }
                    Ok(ids)
                })
                .expect("graph");
            let directions: &[Direction] = match direction {
                0 => &[Direction::Out],
                1 => &[Direction::In],
                _ => &[Direction::Out, Direction::In],
            };
            let bounds = (min, extra.map(|extra| min + extra));
            for start in ids {
                let mut expected = Vec::new();
                if min == 0 {
                    expected.push((Vec::new(), vec![start]));
                }
                reference_trails(
                    &mut db, directions, bounds, &mut Vec::new(), &mut vec![start], &mut expected,
                );
                let mut expansion = db.var_expansion(start, directions, &[], &[], bounds);
                let mut actual = Vec::new();
                while db.next_trail(&mut expansion).expect("next") {
                    actual.push((expansion.edges().to_vec(), expansion.nodes().to_vec()));
                }
                proptest::prop_assert_eq!(actual, expected);
            }
        }
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(64))]

        /// The bidirectional search returns exactly the trails, in the same
        /// order, that the breadth-first path enumeration returns.
        #[test]
        fn bidirectional_shortest_matches_enumeration(
            edges in proptest::collection::vec((0usize..8, 0usize..8), 0..18),
            min in 0u32..3,
            max in proptest::option::of(0u32..6),
            direction in 0usize..3,
            all: bool,
        ) {
            let mut db = temp_db();
            let ids = db
                .write(|db| {
                    let t = db.ensure_edge_type("T")?;
                    let ids: Vec<u64> =
                        (0..8).map(|_| db.create_node(&[], vec![])).collect::<Result<_>>()?;
                    for (src, dst) in &edges {
                        db.create_edge(t, ids[*src], ids[*dst], vec![])?;
                    }
                    Ok(ids)
                })
                .expect("graph");
            let directions: &[Direction] = match direction {
                0 => &[Direction::Out],
                1 => &[Direction::In],
                _ => &[Direction::Out, Direction::In],
            };
            let search = PathSearch { directions, types: &[], props: &[] };
            for &src in &ids[..4] {
                for &dst in &ids[..4] {
                    let fast = db
                        .var_shortest_paths(src, Some(dst), directions, &[], &[], min, max, all)
                        .expect("fast");
                    let slow = db
                        .shortest_by_enumeration(&search, src, Some(dst), min, max, all)
                        .expect("slow");
                    proptest::prop_assert_eq!(&fast, &slow, "{} -> {}", src, dst);
                }
            }
        }
    }

    fn prop(key: u32, value: &str) -> (u32, Value) {
        (key, Value::from(value))
    }

    #[test]
    fn nodes_edges_and_traversal() {
        let mut db = temp_db();
        db.write(|db| {
            let person = db.ensure_label("Person")?;
            let knows = db.ensure_edge_type("KNOWS")?;
            let a = db.create_node(&[person], vec![])?;
            let b = db.create_node(&[person], vec![])?;
            let c = db.create_node(&[person], vec![])?;
            let e1 = db.create_edge(knows, a, b, vec![])?;
            let e2 = db.create_edge(knows, b, c, vec![])?;
            let e3 = db.create_edge(knows, a, c, vec![])?;

            let mut out = db.expand(a, Direction::Out, &[])?;
            out.sort_unstable();
            assert_eq!(out, vec![(e1, b), (e3, c)]);
            let into_c = db.expand(c, Direction::In, &[])?;
            let mut into_c_ids: Vec<u64> = into_c.iter().map(|(edge, _)| *edge).collect();
            into_c_ids.sort_unstable();
            assert_eq!(into_c_ids, vec![e2, e3]);
            assert_eq!(db.expand(a, Direction::In, &[])?.len(), 0);

            // Type-filtered traversal.
            let other = db.ensure_edge_type("LIKES")?;
            db.create_edge(other, a, b, vec![])?;
            assert_eq!(db.expand(a, Direction::Out, &[knows])?.len(), 2);
            assert_eq!(db.expand(a, Direction::Out, &[other])?.len(), 1);

            // Edge existence via incidence intersection.
            assert!(db.edge_exists(knows, a, b)?);
            assert!(!db.edge_exists(knows, b, a)?);
            assert!(!db.edge_exists(other, b, a)?);

            // Deleting a node with edges requires detach.
            let err = db.delete_node(c, false).expect_err("has edges");
            assert_eq!(err.code(), ErrorCode::Constraint);
            assert!(db.delete_node(c, true)?);
            assert!(!db.node_exists(c)?);
            // e1 (KNOWS a->b) and the LIKES edge remain.
            assert_eq!(db.expand(a, Direction::Out, &[])?.len(), 2);
            Ok(())
        })
        .expect("write");
    }

    #[test]
    fn reopen_persists_graph() {
        let store = new_store();
        let (a, b, knows, name_key) = {
            let vfs = Arc::new(MemVfs::from_store(store.clone()));
            let mut db = Database::create(vfs, Path::new("persist.db"), 4096).expect("create");
            db.write(|db| {
                let person = db.ensure_label("Person")?;
                let knows = db.ensure_edge_type("KNOWS")?;
                let name_key = db.ensure_property_key("name")?;
                let a = db.create_node(&[person], vec![prop(name_key, "alice")])?;
                let b = db.create_node(&[person], vec![prop(name_key, "bob")])?;
                db.create_edge(knows, a, b, vec![])?;
                Ok((a, b, knows, name_key))
            })
            .expect("write")
        };
        let vfs = Arc::new(MemVfs::from_store(store));
        let mut db =
            Database::open(vfs, Path::new("persist.db"), OpenMode::ReadWrite).expect("reopen");
        assert_eq!(db.label_id("Person"), Some(1));
        assert_eq!(db.edge_type_id("KNOWS"), Some(knows));
        let node = db.get_node(a).expect("get").expect("exists");
        assert_eq!(node.get(name_key), Some(&Value::from("alice")));
        assert_eq!(db.expand(a, Direction::Out, &[]).expect("expand").len(), 1);
        assert_eq!(db.label_nodes(1).expect("scan"), vec![a, b]);
    }

    #[test]
    fn rollback_discards_writes() {
        let mut db = temp_db();
        db.begin_write().expect("begin");
        let person = db.ensure_label("Person").expect("label");
        let node = db.create_node(&[person], vec![]).expect("node");
        assert!(db.node_exists(node).expect("exists"));
        db.rollback().expect("rollback");
        assert!(!db.node_exists(node).expect("exists"));
        // Schema registration was rolled back too.
        assert_eq!(db.label_id("Person"), None);
    }

    #[test]
    fn unique_index_is_enforced_and_backfilled() {
        let mut db = temp_db();
        db.write(|db| {
            let person = db.ensure_label("Person")?;
            let name = db.ensure_property_key("name")?;
            let a = db.create_node(&[person], vec![prop(name, "alice")])?;
            let b = db.create_node(&[person], vec![prop(name, "bob")])?;
            let index = db.create_node_index("name", Some("Person"), true)?;
            // Backfill finds both nodes.
            assert_eq!(db.index_lookup(index, &Value::from("alice"))?, vec![a]);
            assert_eq!(db.index_lookup(index, &Value::from("bob"))?, vec![b]);
            // Duplicate is rejected.
            let err = db.create_node(&[person], vec![prop(name, "alice")]);
            assert_eq!(err.expect_err("duplicate").code(), ErrorCode::Constraint);
            // Updating a property maintains the index.
            db.set_node_property(a, name, Value::from("carol"))?;
            assert!(db.index_lookup(index, &Value::from("alice"))?.is_empty());
            assert_eq!(db.index_lookup(index, &Value::from("carol"))?, vec![a]);
            // Null removes the property and its index entry.
            db.set_node_property(a, name, Value::Null)?;
            assert!(db.index_lookup(index, &Value::from("carol"))?.is_empty());
            let record = db.get_node(a)?.expect("node");
            assert_eq!(record.get(name), None);
            Ok(())
        })
        .expect("write");
    }

    #[test]
    fn index_created_after_data_is_backfilled() {
        let mut db = temp_db();
        db.write(|db| {
            let person = db.ensure_label("Person")?;
            let age = db.ensure_property_key("age")?;
            let a = db.create_node(&[person], vec![(age, Value::Int(30))])?;
            let b = db.create_node(&[person], vec![(age, Value::Int(40))])?;
            let index = db.create_node_index("age", None, false)?;
            assert_eq!(db.index_lookup(index, &Value::Int(30))?, vec![a]);
            assert_eq!(db.index_lookup(index, &Value::Int(40))?, vec![b]);
            assert!(db.index_lookup(index, &Value::Int(50))?.is_empty());
            Ok(())
        })
        .expect("write");
    }

    #[test]
    fn edge_index_and_type_scan() {
        let mut db = temp_db();
        db.write(|db| {
            let person = db.ensure_label("Person")?;
            let knows = db.ensure_edge_type("KNOWS")?;
            let weight = db.ensure_property_key("weight")?;
            let a = db.create_node(&[person], vec![])?;
            let b = db.create_node(&[person], vec![])?;
            let e1 = db.create_edge(knows, a, b, vec![(weight, Value::Int(1))])?;
            let e2 = db.create_edge(knows, b, a, vec![(weight, Value::Int(2))])?;
            let index = db.create_edge_index("weight", Some("KNOWS"), false)?;
            // Edge index lookup shares the node-index implementation.
            let prefix = index_value_prefix(&Value::Int(1));
            let def = db
                .schema()
                .edge_indexes
                .iter()
                .find(|index_def| index_def.id == index)
                .cloned()
                .expect("def");
            let tree = Btree::from_root(def.root, TreeKind::Index);
            let mut cursor = tree.cursor();
            let mut found = Vec::new();
            let mut ok = cursor.seek_ge(&mut db.pager, &prefix)?;
            while ok && cursor.key().starts_with(&prefix) {
                let key = cursor.key();
                found.push(u64::from_be_bytes(
                    key[key.len() - 8..].try_into().expect("id"),
                ));
                ok = cursor.next(&mut db.pager)?;
            }
            assert_eq!(found, vec![e1]);
            let mut typed = db.edges_of_type(knows)?;
            typed.sort_unstable();
            assert_eq!(typed, vec![e1, e2]);
            Ok(())
        })
        .expect("write");
    }

    /// Entity ids recorded in the single edge index of the test schema for a
    /// property value.
    fn edge_index_entries(db: &mut Database, value: &Value) -> Vec<u64> {
        let def = db
            .schema()
            .edge_indexes
            .first()
            .cloned()
            .expect("edge index");
        let tree = Btree::from_root(def.root, TreeKind::Index);
        let mut cursor = tree.cursor();
        let prefix = index_value_prefix(value);
        let mut found = Vec::new();
        let mut ok = cursor.seek_ge(&mut db.pager, &prefix).expect("seek");
        while ok && cursor.key().starts_with(&prefix) {
            let key = cursor.key();
            found.push(u64::from_be_bytes(
                key[key.len() - 8..].try_into().expect("id"),
            ));
            ok = cursor.next(&mut db.pager).expect("next");
        }
        found
    }

    #[test]
    fn replace_edge_properties_maintains_indexes() {
        let mut db = temp_db();
        let (e2, weight) = db
            .write(|db| {
                let person = db.ensure_label("Person")?;
                let knows = db.ensure_edge_type("KNOWS")?;
                let weight = db.ensure_property_key("weight")?;
                let a = db.create_node(&[person], vec![])?;
                let b = db.create_node(&[person], vec![])?;
                let e1 = db.create_edge(knows, a, b, vec![(weight, Value::Int(1))])?;
                let e2 = db.create_edge(knows, a, b, vec![(weight, Value::Int(2))])?;
                db.create_edge_index("weight", Some("KNOWS"), true)?;
                assert_eq!(edge_index_entries(db, &Value::Int(1)), vec![e1]);
                assert_eq!(edge_index_entries(db, &Value::Int(2)), vec![e2]);
                // Whole-map replacement rewrites the index entries in place.
                db.replace_edge_properties(e1, vec![(weight, Value::Int(3))])?;
                assert!(edge_index_entries(db, &Value::Int(1)).is_empty());
                assert_eq!(edge_index_entries(db, &Value::Int(3)), vec![e1]);
                Ok((e2, weight))
            })
            .expect("write");
        // Uniqueness is enforced on whole-map replacement too, and the
        // failure rolls the transaction back.
        let err = db
            .write(|db| db.replace_edge_properties(e2, vec![(weight, Value::Int(3))]))
            .expect_err("duplicate");
        assert_eq!(err.code(), ErrorCode::Constraint);
        db.write(|db| {
            assert_eq!(edge_index_entries(db, &Value::Int(2)), vec![e2]);
            let record = db.get_edge(e2)?.expect("edge");
            assert_eq!(record.get(weight), Some(&Value::Int(2)));
            Ok(())
        })
        .expect("verify");
    }

    #[test]
    fn label_bitmaps_follow_label_add_and_remove() {
        let mut db = temp_db();
        db.write(|db| {
            let person = db.ensure_label("Person")?;
            let robot = db.ensure_label("Robot")?;
            let a = db.create_node(&[person], vec![])?;
            let b = db.create_node(&[], vec![])?;

            db.add_node_labels(b, &[robot])?;
            assert_eq!(db.label_nodes(person)?, vec![a]);
            assert_eq!(db.label_nodes(robot)?, vec![b]);

            db.remove_node_labels(a, &[person])?;
            assert!(db.label_nodes(person)?.is_empty());
            assert!(db.node_exists(a)?, "the node survives label removal");
            assert_eq!(db.get_node(a)?.expect("node").labels, Vec::<u32>::new());

            db.add_node_labels(a, &[person])?;
            assert_eq!(db.label_nodes(person)?, vec![a]);
            Ok(())
        })
        .expect("write");
    }

    #[test]
    fn edge_deletion_removes_its_index_entries() {
        let mut db = temp_db();
        db.write(|db| {
            let person = db.ensure_label("Person")?;
            let knows = db.ensure_edge_type("KNOWS")?;
            let weight = db.ensure_property_key("weight")?;
            let a = db.create_node(&[person], vec![])?;
            let b = db.create_node(&[person], vec![])?;
            let e = db.create_edge(knows, a, b, vec![(weight, Value::Int(1))])?;
            db.create_edge_index("weight", Some("KNOWS"), false)?;
            assert_eq!(edge_index_entries(db, &Value::Int(1)), vec![e]);
            assert_eq!(db.node_edges(a, Direction::Out, &[])?.len(), 1);

            assert!(db.delete_edge(e)?);
            assert!(edge_index_entries(db, &Value::Int(1)).is_empty());
            assert!(db.edges_of_type(knows)?.is_empty());
            assert_eq!(db.node_edges(a, Direction::Out, &[])?.len(), 0);
            assert_eq!(
                db.node_edges(b, Direction::In, &[])?.len(),
                0,
                "the in-side bitmap is cleared too"
            );
            Ok(())
        })
        .expect("write");
    }

    #[test]
    fn node_index_entries_follow_property_changes() {
        let mut db = temp_db();
        db.write(|db| {
            let person = db.ensure_label("Person")?;
            let age = db.ensure_property_key("age")?;
            let n = db.create_node(&[person], vec![(age, Value::Int(30))])?;
            let index = db.create_node_index("age", Some("Person"), false)?;
            assert_eq!(db.index_lookup(index, &Value::Int(30))?, vec![n]);

            db.set_node_property(n, age, Value::Int(40))?;
            assert!(db.index_lookup(index, &Value::Int(30))?.is_empty());
            assert_eq!(db.index_lookup(index, &Value::Int(40))?, vec![n]);

            // Null removes the property and its index entry.
            db.set_node_property(n, age, Value::Null)?;
            assert!(db.index_lookup(index, &Value::Int(40))?.is_empty());
            assert_eq!(db.get_node(n)?.expect("node").get(age), None);

            db.set_node_property(n, age, Value::Int(50))?;
            assert_eq!(db.index_lookup(index, &Value::Int(50))?, vec![n]);
            db.replace_node_properties(n, vec![])?;
            assert!(db.index_lookup(index, &Value::Int(50))?.is_empty());
            Ok(())
        })
        .expect("write");
    }
}
