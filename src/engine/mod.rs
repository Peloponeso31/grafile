//! Database engine: nodes, edges, indexes and traversals on top of the
//! storage layers.
//!
//! [`Database`] owns the pager and the four B+trees (catalog, nodes, edges,
//! fragments) and keeps their roots synchronized with the database header.
//! All mutations must run inside a write transaction; [`Database::write`]
//! wraps a closure in begin/commit with automatic rollback on error.

use std::path::Path;
use std::sync::Arc;

use crate::btree::{Btree, TreeKind};
use crate::catalog::{Catalog, Schema};
use crate::error::{Error, ErrorCode, Result};
use crate::matrix::{ANY_TYPE, Direction, FragmentKey, FragmentStore};
use crate::pager::{DatabaseHeader, Pager};
use crate::record::{EdgeRecord, NodeRecord, Value, encode_index_key};
use crate::vfs::{OpenMode, Vfs};

/// An embedded graph database.
pub struct Database {
    pager: Pager,
    catalog: Catalog,
    nodes: Btree,
    edges: Btree,
    fragments: FragmentStore,
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

    /// Commit the active write transaction, persisting tree roots.
    pub fn commit(&mut self) -> Result<()> {
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
        Ok(true)
    }

    /// Replace a node property (or remove it when `value` is null).
    pub fn set_node_property(&mut self, node_id: u64, key_id: u32, value: Value) -> Result<bool> {
        self.require_write()?;
        let Some(mut record) = self.get_node(node_id)? else {
            return Err(Error::new(
                ErrorCode::NotFound,
                format!("node {node_id} does not exist"),
            ));
        };
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
        let Some(mut record) = self.get_edge(edge_id)? else {
            return Err(Error::new(
                ErrorCode::NotFound,
                format!("edge {edge_id} does not exist"),
            ));
        };
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

    /// Edge ids incident to `node_id` in `direction`, optionally filtered by
    /// edge types. An empty `types` slice uses the combined "any" plane.
    /// Enumerate every trail from `src` whose length is within `min..=max`
    /// (`max` of `None` is unbounded), returning `(edge ids, end node)`.
    /// Relationships never repeat within a trail; nodes may.
    ///
    /// # Errors
    ///
    /// Propagates storage errors while expanding.
    pub fn var_expand(
        &mut self,
        src: u64,
        directions: &[Direction],
        types: &[u32],
        props: &[(String, Value)],
        min: u32,
        max: Option<u32>,
    ) -> Result<Vec<(Vec<u64>, u64)>> {
        // Property key names are resolved per call, so keys created by the
        // same statement are seen. An unknown key matches nothing.
        let mut keyed = Vec::with_capacity(props.len());
        for (name, value) in props {
            match self.property_key_id(name) {
                Some(key_id) => keyed.push((key_id, value.clone())),
                None => return Ok(Vec::new()),
            }
        }
        let mut out = Vec::new();
        if min == 0 {
            out.push((Vec::new(), src));
        }
        let mut trail = Vec::new();
        self.var_expand_rec(
            src, directions, types, &keyed, min, max, &mut trail, &mut out,
        )?;
        Ok(out)
    }

    /// Shortest trails from `src`, breadth-first by hop count. With a target,
    /// returns one trail to it (or every trail of minimal length when `all`);
    /// without a target, returns one trail per reachable node (or every trail
    /// of minimal length per node).
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
    ) -> Result<Vec<(Vec<u64>, u64)>> {
        // Property key names are resolved per call, so keys created by the
        // same statement are seen. An unknown key matches nothing.
        let mut keyed = Vec::with_capacity(props.len());
        for (name, value) in props {
            match self.property_key_id(name) {
                Some(key_id) => keyed.push((key_id, value.clone())),
                None => return Ok(Vec::new()),
            }
        }
        // Shortest paths are simple: a node may not repeat. Each frontier
        // entry carries its edge list and visited nodes.
        let mut out: Vec<(Vec<u64>, u64)> = Vec::new();
        let mut frontier: Vec<(u64, Vec<u64>, Vec<u64>)> = vec![(src, Vec::new(), vec![src])];
        let mut length = 0u32;
        let mut emitted: Vec<u64> = Vec::new();
        loop {
            if length >= min {
                match dst {
                    Some(target) => {
                        let mut found: Vec<(Vec<u64>, u64)> = frontier
                            .iter()
                            .filter(|(node, _, _)| *node == target)
                            .map(|(node, trail, _)| (trail.clone(), *node))
                            .collect();
                        if !found.is_empty() {
                            if !all {
                                found.truncate(1);
                            }
                            return Ok(found);
                        }
                    }
                    None => {
                        let mut found: Vec<(Vec<u64>, u64)> = frontier
                            .iter()
                            .filter(|(node, _, _)| !emitted.contains(node))
                            .map(|(node, trail, _)| (trail.clone(), *node))
                            .collect();
                        if !found.is_empty() {
                            found.sort_unstable_by_key(|(_, node)| *node);
                            if !all {
                                found.dedup_by_key(|(_, node)| *node);
                            }
                            emitted.extend(found.iter().map(|(_, node)| *node));
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
                        if nodes.contains(&neighbour) || !self.edge_matches(edge, &keyed)? {
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

    #[allow(clippy::too_many_arguments)]
    fn var_expand_rec(
        &mut self,
        node: u64,
        directions: &[Direction],
        types: &[u32],
        props: &[(u32, Value)],
        min: u32,
        max: Option<u32>,
        trail: &mut Vec<u64>,
        out: &mut Vec<(Vec<u64>, u64)>,
    ) -> Result<()> {
        if let Some(max) = max
            && trail.len() as u32 >= max
        {
            return Ok(());
        }
        let mut neighbours = Vec::new();
        for direction in directions {
            neighbours.extend(self.expand(node, *direction, types)?);
        }
        neighbours.sort_unstable();
        neighbours.dedup();
        for (edge, next) in neighbours {
            if trail.contains(&edge) || !self.edge_matches(edge, props)? {
                continue;
            }
            trail.push(edge);
            if trail.len() as u32 >= min {
                out.push((trail.clone(), next));
            }
            self.var_expand_rec(next, directions, types, props, min, max, trail, out)?;
            trail.pop();
        }
        Ok(())
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

    /// All edge ids of `edge_type` (bitmap scan).
    pub fn edges_of_type(&mut self, edge_type: u32) -> Result<Vec<u64>> {
        self.fragments.edges_of_type(&mut self.pager, edge_type)
    }

    /// Whether an edge `src -[type]-> dst` exists (bitmap intersection).
    pub fn edge_exists(&mut self, edge_type: u32, src: u64, dst: u64) -> Result<bool> {
        self.fragments
            .incidence_exists(&mut self.pager, edge_type, src, dst)
    }

    /// Look up nodes by an exact property value using index `index_id`.
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
        let prefix = index_value_prefix(value);
        let tree = Btree::from_root(def.root, TreeKind::Index);
        let mut cursor = tree.cursor();
        let mut out = Vec::new();
        let mut ok = cursor.seek_ge(&mut self.pager, &prefix)?;
        while ok && cursor.key().starts_with(&prefix) {
            let key = cursor.key();
            let id_bytes: [u8; 8] = key[key.len() - 8..]
                .try_into()
                .map_err(|_| Error::corrupt("index key too short"))?;
            out.push(u64::from_be_bytes(id_bytes));
            ok = cursor.next(&mut self.pager)?;
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
        // Backfill.
        let mut cursor = self.nodes.cursor();
        let mut ok = cursor.first(&mut self.pager)?;
        while ok {
            let id_bytes: [u8; 8] = cursor
                .key()
                .try_into()
                .map_err(|_| Error::corrupt("node key is not 8 bytes"))?;
            let node_id = u64::from_be_bytes(id_bytes);
            let record = NodeRecord::decode(&cursor.value(&mut self.pager)?)?;
            if label_id == 0 || record.labels.contains(&label_id) {
                self.insert_index_entry(index_id, &record, node_id, true)?;
            }
            ok = cursor.next(&mut self.pager)?;
        }
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
        let mut cursor = self.edges.cursor();
        let mut ok = cursor.first(&mut self.pager)?;
        while ok {
            let id_bytes: [u8; 8] = cursor
                .key()
                .try_into()
                .map_err(|_| Error::corrupt("edge key is not 8 bytes"))?;
            let edge_id = u64::from_be_bytes(id_bytes);
            let record = EdgeRecord::decode(&cursor.value(&mut self.pager)?)?;
            if type_id == 0 || record.edge_type == type_id {
                self.insert_edge_index_entry(index_id, &record, edge_id, true)?;
            }
            ok = cursor.next(&mut self.pager)?;
        }
        Ok(index_id)
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
                    index.key_id == *key_id
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
        let key_bytes = edge_id.to_be_bytes();
        self.edges.delete(&mut self.pager, &key_bytes)?;
        record.props = normalize_props(props)?;
        self.edges
            .insert(&mut self.pager, &key_bytes, &record.encode())?;
        Ok(())
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
        let indexes: Vec<_> = self.schema().node_indexes.clone();
        for index in indexes {
            if index.label_id == 0 || record.labels.contains(&index.label_id) {
                self.insert_index_entry(index.id, record, node_id, false)?;
            }
        }
        Ok(())
    }

    fn unindex_node(&mut self, record: &NodeRecord, node_id: u64) -> Result<()> {
        let indexes: Vec<_> = self.schema().node_indexes.clone();
        for index in indexes {
            self.delete_index_entry(&index, record.get(index.key_id), node_id)?;
        }
        Ok(())
    }

    fn index_node_property(
        &mut self,
        record: &NodeRecord,
        node_id: u64,
        key_id: u32,
    ) -> Result<()> {
        let indexes: Vec<_> = self
            .schema()
            .node_indexes_for_key(key_id)
            .cloned()
            .collect();
        for index in indexes {
            if index.label_id == 0 || record.labels.contains(&index.label_id) {
                self.insert_index_entry(index.id, record, node_id, false)?;
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
        let indexes: Vec<_> = self
            .schema()
            .node_indexes_for_key(key_id)
            .cloned()
            .collect();
        for index in indexes {
            self.delete_index_entry(&index, record.get(key_id), node_id)?;
        }
        Ok(())
    }

    fn insert_index_entry(
        &mut self,
        index_id: u32,
        record: &NodeRecord,
        node_id: u64,
        backfill: bool,
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
        let Some(value) = record.get(def.key_id) else {
            return Ok(());
        };
        let mut tree = Btree::from_root(def.root, TreeKind::Index);
        let key = node_index_key(value, node_id);
        if def.unique {
            // Index keys embed the entity id, so key uniqueness cannot
            // enforce value uniqueness: probe the value prefix instead.
            let prefix = index_value_prefix(value);
            let mut cursor = tree.cursor();
            if cursor.seek_ge(&mut self.pager, &prefix)? && cursor.key().starts_with(&prefix) {
                // Backfill may re-encounter the entry being created.
                let existing: [u8; 8] = cursor.key()[cursor.key().len() - 8..]
                    .try_into()
                    .map_err(|_| Error::corrupt("index key too short"))?;
                if u64::from_be_bytes(existing) != node_id || !backfill {
                    return Err(Error::constraint(format!(
                        "unique index on property {} violated by node {node_id}",
                        def.key_id
                    )));
                }
            }
        }
        tree.insert(&mut self.pager, &key, &[])?;
        self.replace_node_index_root(index_id, tree.root())
    }

    fn delete_index_entry(
        &mut self,
        def: &crate::catalog::NodeIndexDef,
        value: Option<&Value>,
        node_id: u64,
    ) -> Result<()> {
        let Some(value) = value else {
            return Ok(());
        };
        let mut tree = Btree::from_root(def.root, TreeKind::Index);
        tree.delete(&mut self.pager, &node_index_key(value, node_id))?;
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
                self.insert_edge_index_entry(index.id, record, edge_id, false)?;
            }
        }
        Ok(())
    }

    fn unindex_edge(&mut self, record: &EdgeRecord, edge_id: u64) -> Result<()> {
        let indexes: Vec<_> = self.schema().edge_indexes.clone();
        for index in indexes {
            self.delete_edge_index_entry(&index, record.get(index.key_id), edge_id)?;
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
                self.insert_edge_index_entry(index.id, record, edge_id, false)?;
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
            self.delete_edge_index_entry(&index, record.get(key_id), edge_id)?;
        }
        Ok(())
    }

    fn insert_edge_index_entry(
        &mut self,
        index_id: u32,
        record: &EdgeRecord,
        edge_id: u64,
        backfill: bool,
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
        let Some(value) = record.get(def.key_id) else {
            return Ok(());
        };
        let mut tree = Btree::from_root(def.root, TreeKind::Index);
        let key = edge_index_key(value, edge_id);
        if def.unique {
            let prefix = index_value_prefix(value);
            let mut cursor = tree.cursor();
            if cursor.seek_ge(&mut self.pager, &prefix)? && cursor.key().starts_with(&prefix) {
                let existing: [u8; 8] = cursor.key()[cursor.key().len() - 8..]
                    .try_into()
                    .map_err(|_| Error::corrupt("index key too short"))?;
                if u64::from_be_bytes(existing) != edge_id || !backfill {
                    return Err(Error::constraint(format!(
                        "unique index on property {} violated by edge {edge_id}",
                        def.key_id
                    )));
                }
            }
        }
        tree.insert(&mut self.pager, &key, &[])?;
        self.replace_edge_index_root(index_id, tree.root())
    }

    fn delete_edge_index_entry(
        &mut self,
        def: &crate::catalog::EdgeIndexDef,
        value: Option<&Value>,
        edge_id: u64,
    ) -> Result<()> {
        let Some(value) = value else {
            return Ok(());
        };
        let mut tree = Btree::from_root(def.root, TreeKind::Index);
        tree.delete(&mut self.pager, &edge_index_key(value, edge_id))?;
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

/// Index key for a node: order-preserving value encoding plus node id.
fn node_index_key(value: &Value, node_id: u64) -> Vec<u8> {
    let mut key = Vec::with_capacity(16);
    encode_index_key(&mut key, value);
    key.extend_from_slice(&node_id.to_be_bytes());
    key
}

/// Index key for an edge: order-preserving value encoding plus edge id.
fn edge_index_key(value: &Value, edge_id: u64) -> Vec<u8> {
    let mut key = Vec::with_capacity(16);
    encode_index_key(&mut key, value);
    key.extend_from_slice(&edge_id.to_be_bytes());
    key
}

/// Prefix shared by all index entries with `value`.
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
}
