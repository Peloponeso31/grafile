//! Catalog: schema registries stored in a table B+tree.
//!
//! Entries are keyed by `(kind u8, id u32)` packed into a `u64` B+tree key.
//! The in-memory [`Schema`] mirrors the on-disk catalog and is rebuilt on
//! open; mutations go through `ensure_*`/`create_*` methods that keep both in
//! sync and persist the catalog root in the database header.

use hashbrown::HashMap;

use crate::btree::{Btree, TreeKind};
use crate::error::{Error, ErrorCode, Result};
use crate::pager::{PageNo, Pager};
use crate::record::{decode_varint, encode_varint};

mod kind {
    pub const LABEL: u8 = 1;
    pub const EDGE_TYPE: u8 = 2;
    pub const PROPERTY_KEY: u8 = 3;
    /// Anonymous index entry (format minor 2).
    pub const NODE_INDEX: u8 = 4;
    /// Anonymous edge index entry (format minor 2).
    pub const EDGE_INDEX: u8 = 5;
    /// Named node index entry (format major 0, minor 3).
    pub const NODE_INDEX_V2: u8 = 6;
    /// Named edge index entry (format minor 3).
    pub const EDGE_INDEX_V2: u8 = 7;
    /// Node constraint without a backing index (`IS NOT NULL`).
    pub const NODE_CONSTRAINT: u8 = 8;
    /// Edge constraint without a backing index (`IS NOT NULL`).
    pub const EDGE_CONSTRAINT: u8 = 9;
}

/// Kinds of index objects. Unique and node-key constraints are index-backed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexKind {
    /// Plain property index.
    Index,
    /// Unique property constraint.
    Unique,
    /// Node key constraint: existence plus tuple uniqueness.
    NodeKey,
}

impl IndexKind {
    const fn to_byte(self) -> u8 {
        match self {
            Self::Index => 0,
            Self::Unique => 1,
            Self::NodeKey => 2,
        }
    }

    fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            0 => Some(Self::Index),
            1 => Some(Self::Unique),
            2 => Some(Self::NodeKey),
            _ => None,
        }
    }
}

/// Definition of a property index over nodes.
///
/// Entries written before format minor 3 (and by the legacy Rust API) are
/// anonymous and single-key; the readers accept both encodings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeIndexDef {
    /// Index id (its catalog entry id).
    pub id: u32,
    /// User-visible name; `None` for anonymous legacy indexes.
    pub name: Option<String>,
    /// Index kind.
    pub kind: IndexKind,
    /// Indexed property key ids, in key order.
    pub key_ids: Vec<u32>,
    /// Restrict to this label id, or 0 for all nodes.
    pub label_id: u32,
    /// B+tree root page.
    pub root: PageNo,
}

impl NodeIndexDef {
    /// First (v1: only) indexed property key id.
    pub fn key_id(&self) -> u32 {
        self.key_ids[0]
    }

    /// Whether the index enforces uniqueness.
    pub fn unique(&self) -> bool {
        self.kind != IndexKind::Index
    }

    /// Whether the index can serve property seeks (single-key only).
    pub fn seekable(&self) -> bool {
        self.key_ids.len() == 1
    }

    /// Human-readable kind for introspection.
    pub fn kind_name(&self) -> &'static str {
        match self.kind {
            IndexKind::Index => "index",
            IndexKind::Unique => "unique",
            IndexKind::NodeKey => "node key",
        }
    }
}

/// Definition of a property index over edges.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EdgeIndexDef {
    /// Index id (its catalog entry id).
    pub id: u32,
    /// User-visible name; `None` for anonymous legacy indexes.
    pub name: Option<String>,
    /// Index kind (`Index` or `Unique`; node keys are node-only).
    pub kind: IndexKind,
    /// Indexed property key ids, in key order.
    pub key_ids: Vec<u32>,
    /// Restrict to this edge type id, or 0 for all edges.
    pub type_id: u32,
    /// B+tree root page.
    pub root: PageNo,
}

impl EdgeIndexDef {
    /// First (v1: only) indexed property key id.
    pub fn key_id(&self) -> u32 {
        self.key_ids[0]
    }

    /// Whether the index enforces uniqueness.
    pub fn unique(&self) -> bool {
        self.kind != IndexKind::Index
    }

    /// Whether the index can serve property seeks (single-key only).
    pub fn seekable(&self) -> bool {
        self.key_ids.len() == 1
    }

    /// Human-readable kind for introspection.
    pub fn kind_name(&self) -> &'static str {
        match self.kind {
            IndexKind::Index => "index",
            IndexKind::Unique => "unique",
            IndexKind::NodeKey => "node key",
        }
    }
}

/// A node constraint without a backing index (`IS NOT NULL`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeConstraintDef {
    /// Constraint id (its catalog entry id, shared with index ids).
    pub id: u32,
    /// User-visible name.
    pub name: String,
    /// Restrict to this label id, or 0 for all nodes.
    pub label_id: u32,
    /// Property key id that must exist.
    pub key_id: u32,
}

/// An edge constraint without a backing index (`IS NOT NULL`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EdgeConstraintDef {
    /// Constraint id (its catalog entry id, shared with index ids).
    pub id: u32,
    /// User-visible name.
    pub name: String,
    /// Restrict to this edge type id, or 0 for all edges.
    pub type_id: u32,
    /// Property key id that must exist.
    pub key_id: u32,
}

/// In-memory schema: registries and index definitions.
#[derive(Debug, Clone, Default)]
pub struct Schema {
    /// Label names, indexed by `id - 1`.
    pub labels: Vec<String>,
    /// Label name to id.
    pub label_ids: HashMap<String, u32>,
    /// Edge type names, indexed by `id - 1`.
    pub edge_types: Vec<String>,
    /// Edge type name to id.
    pub edge_type_ids: HashMap<String, u32>,
    /// Property key names, indexed by `id - 1`.
    pub property_keys: Vec<String>,
    /// Property key name to id.
    pub property_key_ids: HashMap<String, u32>,
    /// Node property indexes (including unique and node-key constraints).
    pub node_indexes: Vec<NodeIndexDef>,
    /// Edge property indexes (including unique constraints).
    pub edge_indexes: Vec<EdgeIndexDef>,
    /// Node constraints without a backing index.
    pub node_constraints: Vec<NodeConstraintDef>,
    /// Edge constraints without a backing index.
    pub edge_constraints: Vec<EdgeConstraintDef>,
}

impl Schema {
    /// Label name for an id.
    pub fn label_name(&self, id: u32) -> Option<&str> {
        self.labels
            .get(id.checked_sub(1)? as usize)
            .map(String::as_str)
    }

    /// Edge type name for an id.
    pub fn edge_type_name(&self, id: u32) -> Option<&str> {
        self.edge_types
            .get(id.checked_sub(1)? as usize)
            .map(String::as_str)
    }

    /// Property key name for an id.
    pub fn property_key_name(&self, id: u32) -> Option<&str> {
        self.property_keys
            .get(id.checked_sub(1)? as usize)
            .map(String::as_str)
    }

    /// Node indexes maintained when a property key changes.
    pub fn node_indexes_for_key(&self, key_id: u32) -> impl Iterator<Item = &NodeIndexDef> {
        self.node_indexes
            .iter()
            .filter(move |index| index.key_ids.contains(&key_id))
    }

    /// Edge indexes maintained when a property key changes.
    pub fn edge_indexes_for_key(&self, key_id: u32) -> impl Iterator<Item = &EdgeIndexDef> {
        self.edge_indexes
            .iter()
            .filter(move |index| index.key_ids.contains(&key_id))
    }
}

/// The catalog: a B+tree of schema entries plus the decoded schema.
pub struct Catalog {
    tree: Btree,
    schema: Schema,
    next_index_id: u32,
}

impl Catalog {
    /// Create an empty catalog, allocating its B+tree root.
    pub fn create(pager: &mut Pager) -> Result<Self> {
        let tree = Btree::create(pager, TreeKind::Table)?;
        Ok(Self {
            tree,
            schema: Schema::default(),
            next_index_id: 1,
        })
    }

    /// Open an existing catalog and decode its schema.
    pub fn open(pager: &mut Pager, root: PageNo) -> Result<Self> {
        let tree = Btree::from_root(root, TreeKind::Table);
        let mut schema = Schema::default();
        let mut next_index_id = 1u32;
        let mut cursor = tree.cursor();
        let mut ok = cursor.first(pager)?;
        while ok {
            let key_bytes: [u8; 8] = cursor
                .key()
                .try_into()
                .map_err(|_| Error::corrupt("catalog key is not 8 bytes"))?;
            let key = u64::from_be_bytes(key_bytes);
            let entry_kind = (key >> 32) as u8;
            let id = key as u32;
            let value = cursor.value(pager)?;
            match entry_kind {
                kind::LABEL => {
                    let name = decode_name(&value)?;
                    if id as usize != schema.labels.len() + 1 {
                        return Err(Error::corrupt(format!(
                            "label ids are not dense (id {id}, {} present)",
                            schema.labels.len()
                        )));
                    }
                    schema.label_ids.insert(name.clone(), id);
                    schema.labels.push(name);
                }
                kind::EDGE_TYPE => {
                    let name = decode_name(&value)?;
                    if id as usize != schema.edge_types.len() + 1 {
                        return Err(Error::corrupt("edge type ids are not dense"));
                    }
                    schema.edge_type_ids.insert(name.clone(), id);
                    schema.edge_types.push(name);
                }
                kind::PROPERTY_KEY => {
                    let name = decode_name(&value)?;
                    if id as usize != schema.property_keys.len() + 1 {
                        return Err(Error::corrupt("property key ids are not dense"));
                    }
                    schema.property_key_ids.insert(name.clone(), id);
                    schema.property_keys.push(name);
                }
                kind::NODE_INDEX => {
                    let def = decode_node_index_v1(id, &value)?;
                    next_index_id = next_index_id.max(id + 1);
                    schema.node_indexes.push(def);
                }
                kind::EDGE_INDEX => {
                    let def = decode_edge_index_v1(id, &value)?;
                    next_index_id = next_index_id.max(id + 1);
                    schema.edge_indexes.push(def);
                }
                kind::NODE_INDEX_V2 => {
                    let def = decode_node_index_v2(id, &value)?;
                    next_index_id = next_index_id.max(id + 1);
                    schema.node_indexes.push(def);
                }
                kind::EDGE_INDEX_V2 => {
                    let def = decode_edge_index_v2(id, &value)?;
                    next_index_id = next_index_id.max(id + 1);
                    schema.edge_indexes.push(def);
                }
                kind::NODE_CONSTRAINT => {
                    let def = decode_node_constraint(id, &value)?;
                    next_index_id = next_index_id.max(id + 1);
                    schema.node_constraints.push(def);
                }
                kind::EDGE_CONSTRAINT => {
                    let def = decode_edge_constraint(id, &value)?;
                    next_index_id = next_index_id.max(id + 1);
                    schema.edge_constraints.push(def);
                }
                other => {
                    return Err(Error::corrupt(format!(
                        "unknown catalog entry kind {other}"
                    )));
                }
            }
            ok = cursor.next(pager)?;
        }
        Ok(Self {
            tree,
            schema,
            next_index_id,
        })
    }

    /// Catalog B+tree root (persist in the database header).
    pub fn root(&self) -> PageNo {
        self.tree.root()
    }

    /// The decoded schema.
    pub fn schema(&self) -> &Schema {
        &self.schema
    }

    /// Mutable access to the decoded schema (root pointers after splits).
    pub fn schema_mut(&mut self) -> &mut Schema {
        &mut self.schema
    }

    /// Label id for a name, if registered.
    pub fn label_id(&self, name: &str) -> Option<u32> {
        self.schema.label_ids.get(name).copied()
    }

    /// Edge type id for a name, if registered.
    pub fn edge_type_id(&self, name: &str) -> Option<u32> {
        self.schema.edge_type_ids.get(name).copied()
    }

    /// Property key id for a name, if registered.
    pub fn property_key_id(&self, name: &str) -> Option<u32> {
        self.schema.property_key_ids.get(name).copied()
    }

    /// Register a label if needed and return its id.
    pub fn ensure_label(&mut self, pager: &mut Pager, name: &str) -> Result<u32> {
        if let Some(id) = self.label_id(name) {
            return Ok(id);
        }
        let id = pager.update_header(|header| {
            let id = header.next_label_id;
            header.next_label_id += 1;
            id
        });
        self.insert_entry(pager, kind::LABEL, id, name.as_bytes())?;
        self.schema.label_ids.insert(name.to_owned(), id);
        self.schema.labels.push(name.to_owned());
        Ok(id)
    }

    /// Register an edge type if needed and return its id.
    pub fn ensure_edge_type(&mut self, pager: &mut Pager, name: &str) -> Result<u32> {
        if let Some(id) = self.edge_type_id(name) {
            return Ok(id);
        }
        let id = pager.update_header(|header| {
            let id = header.next_type_id;
            header.next_type_id += 1;
            id
        });
        self.insert_entry(pager, kind::EDGE_TYPE, id, name.as_bytes())?;
        self.schema.edge_type_ids.insert(name.to_owned(), id);
        self.schema.edge_types.push(name.to_owned());
        Ok(id)
    }

    /// Register a property key if needed and return its id.
    pub fn ensure_property_key(&mut self, pager: &mut Pager, name: &str) -> Result<u32> {
        if let Some(id) = self.property_key_id(name) {
            return Ok(id);
        }
        let id = pager.update_header(|header| {
            let id = header.next_key_id;
            header.next_key_id += 1;
            id
        });
        self.insert_entry(pager, kind::PROPERTY_KEY, id, name.as_bytes())?;
        self.schema.property_key_ids.insert(name.to_owned(), id);
        self.schema.property_keys.push(name.to_owned());
        Ok(id)
    }

    /// Create an anonymous node property index and return its id.
    pub fn create_node_index(
        &mut self,
        pager: &mut Pager,
        key_id: u32,
        label_id: u32,
        unique: bool,
    ) -> Result<u32> {
        self.create_node_index_def(
            pager,
            NodeIndexDef {
                id: 0,
                name: None,
                kind: if unique {
                    IndexKind::Unique
                } else {
                    IndexKind::Index
                },
                key_ids: vec![key_id],
                label_id,
                root: 0,
            },
        )
    }

    /// Create a named node index (or index-backed constraint).
    pub fn create_node_index_named(
        &mut self,
        pager: &mut Pager,
        name: &str,
        kind: IndexKind,
        key_ids: &[u32],
        label_id: u32,
    ) -> Result<u32> {
        self.create_node_index_def(
            pager,
            NodeIndexDef {
                id: 0,
                name: Some(name.to_owned()),
                kind,
                key_ids: key_ids.to_vec(),
                label_id,
                root: 0,
            },
        )
    }

    /// Create an anonymous edge property index and return its id.
    pub fn create_edge_index(
        &mut self,
        pager: &mut Pager,
        key_id: u32,
        type_id: u32,
        unique: bool,
    ) -> Result<u32> {
        self.create_edge_index_def(
            pager,
            EdgeIndexDef {
                id: 0,
                name: None,
                kind: if unique {
                    IndexKind::Unique
                } else {
                    IndexKind::Index
                },
                key_ids: vec![key_id],
                type_id,
                root: 0,
            },
        )
    }

    /// Create a named edge index (or index-backed constraint).
    pub fn create_edge_index_named(
        &mut self,
        pager: &mut Pager,
        name: &str,
        kind: IndexKind,
        key_ids: &[u32],
        type_id: u32,
    ) -> Result<u32> {
        self.create_edge_index_def(
            pager,
            EdgeIndexDef {
                id: 0,
                name: Some(name.to_owned()),
                kind,
                key_ids: key_ids.to_vec(),
                type_id,
                root: 0,
            },
        )
    }

    /// Create a node constraint without a backing index.
    pub fn create_node_constraint(
        &mut self,
        pager: &mut Pager,
        name: &str,
        label_id: u32,
        key_id: u32,
    ) -> Result<u32> {
        let id = self.next_object_id();
        let def = NodeConstraintDef {
            id,
            name: name.to_owned(),
            label_id,
            key_id,
        };
        self.insert_entry(
            pager,
            kind::NODE_CONSTRAINT,
            id,
            &encode_constraint(def.label_id, def.key_id, &def.name),
        )?;
        self.schema.node_constraints.push(def);
        pager.update_header(|header| header.schema_generation += 1);
        Ok(id)
    }

    /// Create an edge constraint without a backing index.
    pub fn create_edge_constraint(
        &mut self,
        pager: &mut Pager,
        name: &str,
        type_id: u32,
        key_id: u32,
    ) -> Result<u32> {
        let id = self.next_object_id();
        let def = EdgeConstraintDef {
            id,
            name: name.to_owned(),
            type_id,
            key_id,
        };
        self.insert_entry(
            pager,
            kind::EDGE_CONSTRAINT,
            id,
            &encode_constraint(def.type_id, def.key_id, &def.name),
        )?;
        self.schema.edge_constraints.push(def);
        pager.update_header(|header| header.schema_generation += 1);
        Ok(id)
    }

    /// Remove a node index (or index-backed constraint).
    ///
    /// # Errors
    ///
    /// Returns `NotFound` when no node index has this id.
    pub fn remove_node_index(&mut self, pager: &mut Pager, id: u32) -> Result<()> {
        let index = self
            .schema
            .node_indexes
            .iter()
            .position(|def| def.id == id)
            .ok_or_else(|| {
                Error::new(
                    ErrorCode::NotFound,
                    format!("node index {id} does not exist"),
                )
            })?;
        let def = self.schema.node_indexes.remove(index);
        self.remove_entry(pager, node_index_kind(&def), id)?;
        pager.update_header(|header| header.schema_generation += 1);
        Ok(())
    }

    /// Remove an edge index (or index-backed constraint).
    ///
    /// # Errors
    ///
    /// Returns `NotFound` when no edge index has this id.
    pub fn remove_edge_index(&mut self, pager: &mut Pager, id: u32) -> Result<()> {
        let index = self
            .schema
            .edge_indexes
            .iter()
            .position(|def| def.id == id)
            .ok_or_else(|| {
                Error::new(
                    ErrorCode::NotFound,
                    format!("edge index {id} does not exist"),
                )
            })?;
        let def = self.schema.edge_indexes.remove(index);
        self.remove_entry(pager, edge_index_kind(&def), id)?;
        pager.update_header(|header| header.schema_generation += 1);
        Ok(())
    }

    /// Remove a node constraint without a backing index.
    ///
    /// # Errors
    ///
    /// Returns `NotFound` when no node constraint has this id.
    pub fn remove_node_constraint(&mut self, pager: &mut Pager, id: u32) -> Result<()> {
        let index = self
            .schema
            .node_constraints
            .iter()
            .position(|def| def.id == id)
            .ok_or_else(|| {
                Error::new(
                    ErrorCode::NotFound,
                    format!("node constraint {id} does not exist"),
                )
            })?;
        self.schema.node_constraints.remove(index);
        self.remove_entry(pager, kind::NODE_CONSTRAINT, id)?;
        pager.update_header(|header| header.schema_generation += 1);
        Ok(())
    }

    /// Remove an edge constraint without a backing index.
    ///
    /// # Errors
    ///
    /// Returns `NotFound` when no edge constraint has this id.
    pub fn remove_edge_constraint(&mut self, pager: &mut Pager, id: u32) -> Result<()> {
        let index = self
            .schema
            .edge_constraints
            .iter()
            .position(|def| def.id == id)
            .ok_or_else(|| {
                Error::new(
                    ErrorCode::NotFound,
                    format!("edge constraint {id} does not exist"),
                )
            })?;
        self.schema.edge_constraints.remove(index);
        self.remove_entry(pager, kind::EDGE_CONSTRAINT, id)?;
        pager.update_header(|header| header.schema_generation += 1);
        Ok(())
    }

    /// Rewrite the catalog entry for a node index (root may have moved).
    pub fn update_node_index(&mut self, pager: &mut Pager, def: &NodeIndexDef) -> Result<()> {
        let (entry_kind, value) = encode_node_index(def);
        self.insert_entry(pager, entry_kind, def.id, &value)
    }

    /// Rewrite the catalog entry for an edge index (root may have moved).
    pub fn update_edge_index(&mut self, pager: &mut Pager, def: &EdgeIndexDef) -> Result<()> {
        let (entry_kind, value) = encode_edge_index(def);
        self.insert_entry(pager, entry_kind, def.id, &value)
    }

    fn next_object_id(&mut self) -> u32 {
        let id = self.next_index_id;
        self.next_index_id += 1;
        id
    }

    fn create_node_index_def(&mut self, pager: &mut Pager, mut def: NodeIndexDef) -> Result<u32> {
        def.id = self.next_object_id();
        def.root = Btree::create(pager, TreeKind::Index)?.root();
        let (entry_kind, value) = encode_node_index(&def);
        self.insert_entry(pager, entry_kind, def.id, &value)?;
        self.schema.node_indexes.push(def.clone());
        pager.update_header(|header| header.schema_generation += 1);
        Ok(def.id)
    }

    fn create_edge_index_def(&mut self, pager: &mut Pager, mut def: EdgeIndexDef) -> Result<u32> {
        def.id = self.next_object_id();
        def.root = Btree::create(pager, TreeKind::Index)?.root();
        let (entry_kind, value) = encode_edge_index(&def);
        self.insert_entry(pager, entry_kind, def.id, &value)?;
        self.schema.edge_indexes.push(def.clone());
        pager.update_header(|header| header.schema_generation += 1);
        Ok(def.id)
    }

    fn insert_entry(
        &mut self,
        pager: &mut Pager,
        entry_kind: u8,
        id: u32,
        value: &[u8],
    ) -> Result<()> {
        let key = ((entry_kind as u64) << 32) | u64::from(id);
        let key_bytes = key.to_be_bytes();
        // Upsert: rewriting an entry (e.g. an index root) must not conflict.
        self.tree.delete(pager, &key_bytes)?;
        self.tree.insert(pager, &key_bytes, value)?;
        let root = self.tree.root();
        pager.update_header(|header| header.catalog_root = root);
        Ok(())
    }

    fn remove_entry(&mut self, pager: &mut Pager, entry_kind: u8, id: u32) -> Result<()> {
        let key = ((entry_kind as u64) << 32) | u64::from(id);
        self.tree.delete(pager, &key.to_be_bytes())?;
        let root = self.tree.root();
        pager.update_header(|header| header.catalog_root = root);
        Ok(())
    }
}

fn decode_name(value: &[u8]) -> Result<String> {
    std::str::from_utf8(value)
        .map(str::to_owned)
        .map_err(|_| Error::corrupt("catalog name is not valid UTF-8"))
}

/// Catalog entry kind an index is written under: anonymous single-key
/// indexes keep the format-minor-2 encoding so older builds can still read
/// them; everything else uses the v2 encoding.
fn node_index_kind(def: &NodeIndexDef) -> u8 {
    if def.name.is_none() && def.key_ids.len() == 1 && def.kind != IndexKind::NodeKey {
        kind::NODE_INDEX
    } else {
        kind::NODE_INDEX_V2
    }
}

fn edge_index_kind(def: &EdgeIndexDef) -> u8 {
    if def.name.is_none() && def.key_ids.len() == 1 && def.kind != IndexKind::NodeKey {
        kind::EDGE_INDEX
    } else {
        kind::EDGE_INDEX_V2
    }
}

fn encode_node_index(def: &NodeIndexDef) -> (u8, Vec<u8>) {
    let entry_kind = node_index_kind(def);
    let value = if entry_kind == kind::NODE_INDEX {
        encode_node_index_v1(def)
    } else {
        encode_node_index_v2(def)
    };
    (entry_kind, value)
}

fn encode_edge_index(def: &EdgeIndexDef) -> (u8, Vec<u8>) {
    let entry_kind = edge_index_kind(def);
    let value = if entry_kind == kind::EDGE_INDEX {
        encode_edge_index_v1(def)
    } else {
        encode_edge_index_v2(def)
    };
    (entry_kind, value)
}

fn encode_node_index_v1(def: &NodeIndexDef) -> Vec<u8> {
    let mut out = Vec::with_capacity(16);
    encode_varint(&mut out, u64::from(def.key_id()));
    encode_varint(&mut out, u64::from(def.label_id));
    out.push(u8::from(def.unique()));
    out.extend_from_slice(&def.root.to_le_bytes());
    out
}

fn decode_node_index_v1(id: u32, value: &[u8]) -> Result<NodeIndexDef> {
    let (key_id, label_id, unique, root) = decode_index_prefix("node", value)?;
    Ok(NodeIndexDef {
        id,
        name: None,
        kind: if unique {
            IndexKind::Unique
        } else {
            IndexKind::Index
        },
        key_ids: vec![key_id],
        label_id,
        root,
    })
}

fn encode_node_index_v2(def: &NodeIndexDef) -> Vec<u8> {
    let mut out = Vec::with_capacity(24);
    encode_key_ids(&mut out, &def.key_ids);
    encode_varint(&mut out, u64::from(def.label_id));
    out.push(def.kind.to_byte());
    encode_optional_name(&mut out, def.name.as_deref());
    out.extend_from_slice(&def.root.to_le_bytes());
    out
}

fn decode_node_index_v2(id: u32, value: &[u8]) -> Result<NodeIndexDef> {
    let mut pos = 0usize;
    let key_ids = decode_key_ids(value, &mut pos)?;
    let label_id = u32::try_from(decode_varint(value, &mut pos)?)
        .map_err(|_| Error::corrupt("index label id out of range"))?;
    let kind = decode_kind(value, &mut pos, true)?;
    let name = decode_optional_name(value, &mut pos)?;
    let root = decode_root(value, pos)?;
    Ok(NodeIndexDef {
        id,
        name,
        kind,
        key_ids,
        label_id,
        root,
    })
}

fn encode_edge_index_v1(def: &EdgeIndexDef) -> Vec<u8> {
    let mut out = Vec::with_capacity(16);
    encode_varint(&mut out, u64::from(def.key_id()));
    encode_varint(&mut out, u64::from(def.type_id));
    out.push(u8::from(def.unique()));
    out.extend_from_slice(&def.root.to_le_bytes());
    out
}

fn decode_edge_index_v1(id: u32, value: &[u8]) -> Result<EdgeIndexDef> {
    let (key_id, type_id, unique, root) = decode_index_prefix("edge", value)?;
    Ok(EdgeIndexDef {
        id,
        name: None,
        kind: if unique {
            IndexKind::Unique
        } else {
            IndexKind::Index
        },
        key_ids: vec![key_id],
        type_id,
        root,
    })
}

fn encode_edge_index_v2(def: &EdgeIndexDef) -> Vec<u8> {
    let mut out = Vec::with_capacity(24);
    encode_key_ids(&mut out, &def.key_ids);
    encode_varint(&mut out, u64::from(def.type_id));
    out.push(def.kind.to_byte());
    encode_optional_name(&mut out, def.name.as_deref());
    out.extend_from_slice(&def.root.to_le_bytes());
    out
}

fn decode_edge_index_v2(id: u32, value: &[u8]) -> Result<EdgeIndexDef> {
    let mut pos = 0usize;
    let key_ids = decode_key_ids(value, &mut pos)?;
    let type_id = u32::try_from(decode_varint(value, &mut pos)?)
        .map_err(|_| Error::corrupt("index type id out of range"))?;
    let kind = decode_kind(value, &mut pos, false)?;
    let name = decode_optional_name(value, &mut pos)?;
    let root = decode_root(value, pos)?;
    Ok(EdgeIndexDef {
        id,
        name,
        kind,
        key_ids,
        type_id,
        root,
    })
}

fn decode_kind(value: &[u8], pos: &mut usize, node: bool) -> Result<IndexKind> {
    let byte = *value
        .get(*pos)
        .ok_or_else(|| Error::corrupt("truncated index entry"))?;
    *pos += 1;
    let kind = IndexKind::from_byte(byte).ok_or_else(|| Error::corrupt("unknown index kind"))?;
    if !node && kind == IndexKind::NodeKey {
        return Err(Error::corrupt("edge index cannot be a node key"));
    }
    Ok(kind)
}

fn decode_index_prefix(what: &str, value: &[u8]) -> Result<(u32, u32, bool, PageNo)> {
    let mut pos = 0usize;
    let key_id = u32::try_from(decode_varint(value, &mut pos)?)
        .map_err(|_| Error::corrupt("index key id out of range"))?;
    let target_id = u32::try_from(decode_varint(value, &mut pos)?)
        .map_err(|_| Error::corrupt("index target id out of range"))?;
    let unique = *value
        .get(pos)
        .ok_or_else(|| Error::corrupt(format!("truncated {what} index entry")))?
        != 0;
    pos += 1;
    let root = decode_root(value, pos)?;
    Ok((key_id, target_id, unique, root))
}

fn decode_root(value: &[u8], pos: usize) -> Result<PageNo> {
    Ok(u32::from_le_bytes(
        value
            .get(pos..pos + 4)
            .ok_or_else(|| Error::corrupt("truncated index entry"))?
            .try_into()
            .expect("slice length"),
    ))
}

fn encode_key_ids(out: &mut Vec<u8>, key_ids: &[u32]) {
    encode_varint(out, key_ids.len() as u64);
    for key_id in key_ids {
        encode_varint(out, u64::from(*key_id));
    }
}

fn decode_key_ids(value: &[u8], pos: &mut usize) -> Result<Vec<u32>> {
    let count = usize::try_from(decode_varint(value, pos)?)
        .map_err(|_| Error::corrupt("index key count out of range"))?;
    if count == 0 {
        return Err(Error::corrupt("index has no key columns"));
    }
    let mut key_ids = Vec::with_capacity(count);
    for _ in 0..count {
        key_ids.push(
            u32::try_from(decode_varint(value, pos)?)
                .map_err(|_| Error::corrupt("index key id out of range"))?,
        );
    }
    Ok(key_ids)
}

fn encode_optional_name(out: &mut Vec<u8>, name: Option<&str>) {
    match name {
        Some(name) => {
            encode_varint(out, name.len() as u64 + 1);
            out.extend_from_slice(name.as_bytes());
        }
        None => encode_varint(out, 0),
    }
}

fn decode_optional_name(value: &[u8], pos: &mut usize) -> Result<Option<String>> {
    let len = usize::try_from(decode_varint(value, pos)?)
        .map_err(|_| Error::corrupt("index name length out of range"))?;
    if len == 0 {
        return Ok(None);
    }
    let bytes = value
        .get(*pos..*pos + len - 1)
        .ok_or_else(|| Error::corrupt("truncated index name"))?;
    *pos += len - 1;
    let name =
        std::str::from_utf8(bytes).map_err(|_| Error::corrupt("index name is not valid UTF-8"))?;
    Ok(Some(name.to_owned()))
}

fn encode_constraint(target_id: u32, key_id: u32, name: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(16 + name.len());
    encode_varint(&mut out, u64::from(target_id));
    encode_varint(&mut out, u64::from(key_id));
    encode_varint(&mut out, name.len() as u64);
    out.extend_from_slice(name.as_bytes());
    out
}

fn decode_constraint(name_kind: &str, value: &[u8]) -> Result<(u32, u32, String)> {
    let mut pos = 0usize;
    let target_id = u32::try_from(decode_varint(value, &mut pos)?)
        .map_err(|_| Error::corrupt(format!("{name_kind} constraint target out of range")))?;
    let key_id = u32::try_from(decode_varint(value, &mut pos)?)
        .map_err(|_| Error::corrupt(format!("{name_kind} constraint key out of range")))?;
    let len = usize::try_from(decode_varint(value, &mut pos)?)
        .map_err(|_| Error::corrupt("constraint name length out of range"))?;
    let bytes = value
        .get(pos..pos + len)
        .ok_or_else(|| Error::corrupt("truncated constraint name"))?;
    let name = std::str::from_utf8(bytes)
        .map_err(|_| Error::corrupt("constraint name is not valid UTF-8"))?
        .to_owned();
    Ok((target_id, key_id, name))
}

fn decode_node_constraint(id: u32, value: &[u8]) -> Result<NodeConstraintDef> {
    let (label_id, key_id, name) = decode_constraint("node", value)?;
    Ok(NodeConstraintDef {
        id,
        name,
        label_id,
        key_id,
    })
}

fn decode_edge_constraint(id: u32, value: &[u8]) -> Result<EdgeConstraintDef> {
    let (type_id, key_id, name) = decode_constraint("edge", value)?;
    Ok(EdgeConstraintDef {
        id,
        name,
        type_id,
        key_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::mem::MemVfs;
    use std::sync::Arc;

    fn test_pager() -> Pager {
        Pager::create(
            Arc::new(MemVfs::new()),
            std::path::Path::new("catalog.db"),
            4096,
        )
        .expect("pager")
    }

    #[test]
    fn registries_are_idempotent_and_dense() {
        let mut pager = test_pager();
        pager.begin_write().expect("begin");
        let mut catalog = Catalog::create(&mut pager).expect("create");
        let person = catalog.ensure_label(&mut pager, "Person").expect("label");
        let person2 = catalog.ensure_label(&mut pager, "Person").expect("label");
        assert_eq!(person, person2);
        let knows = catalog.ensure_edge_type(&mut pager, "KNOWS").expect("type");
        let name = catalog
            .ensure_property_key(&mut pager, "name")
            .expect("key");
        assert_eq!((person, knows, name), (1, 1, 1));
        let other = catalog.ensure_label(&mut pager, "Movie").expect("label");
        assert_eq!(other, 2);
        pager.commit().expect("commit");
    }

    #[test]
    fn catalog_reopens_with_same_schema() {
        let store = crate::vfs::mem::new_store();
        let root;
        {
            let vfs = Arc::new(MemVfs::from_store(store.clone()));
            let mut pager =
                Pager::create(vfs, std::path::Path::new("cat.db"), 4096).expect("pager");
            pager.begin_write().expect("begin");
            let mut catalog = Catalog::create(&mut pager).expect("create");
            catalog.ensure_label(&mut pager, "Person").expect("label");
            catalog.ensure_edge_type(&mut pager, "KNOWS").expect("type");
            catalog.ensure_property_key(&mut pager, "age").expect("key");
            let age = catalog.property_key_id("age").expect("id");
            catalog
                .create_node_index(&mut pager, age, 1, true)
                .expect("index");
            root = catalog.root();
            pager.update_header(|header| header.catalog_root = root);
            pager.commit().expect("commit");
        }
        let vfs = Arc::new(MemVfs::from_store(store));
        let mut pager = Pager::open(
            vfs,
            std::path::Path::new("cat.db"),
            crate::vfs::OpenMode::ReadWrite,
        )
        .expect("reopen");
        let catalog = Catalog::open(&mut pager, root).expect("open");
        assert_eq!(catalog.label_id("Person"), Some(1));
        assert_eq!(catalog.edge_type_id("KNOWS"), Some(1));
        assert_eq!(catalog.property_key_id("age"), Some(1));
        assert_eq!(catalog.schema().node_indexes.len(), 1);
        let index = &catalog.schema().node_indexes[0];
        assert!(index.unique());
        assert_eq!(index.label_id, 1);
        assert_eq!(catalog.schema().label_name(1), Some("Person"));
    }

    #[test]
    fn named_objects_roundtrip_through_the_catalog() {
        let store = crate::vfs::mem::new_store();
        let root;
        {
            let vfs = Arc::new(MemVfs::from_store(store.clone()));
            let mut pager =
                Pager::create(vfs, std::path::Path::new("cat.db"), 4096).expect("pager");
            pager.begin_write().expect("begin");
            let mut catalog = Catalog::create(&mut pager).expect("create");
            let person = catalog.ensure_label(&mut pager, "Person").expect("label");
            let knows = catalog.ensure_edge_type(&mut pager, "KNOWS").expect("type");
            let name = catalog
                .ensure_property_key(&mut pager, "name")
                .expect("key");
            let surname = catalog
                .ensure_property_key(&mut pager, "surname")
                .expect("key");
            let weight = catalog
                .ensure_property_key(&mut pager, "weight")
                .expect("key");
            catalog
                .create_node_index_named(
                    &mut pager,
                    "person_name",
                    IndexKind::Unique,
                    &[name],
                    person,
                )
                .expect("named index");
            catalog
                .create_node_index_named(
                    &mut pager,
                    "person_key",
                    IndexKind::NodeKey,
                    &[name, surname],
                    person,
                )
                .expect("node key");
            catalog
                .create_edge_index_named(
                    &mut pager,
                    "knows_weight",
                    IndexKind::Index,
                    &[weight],
                    knows,
                )
                .expect("edge index");
            catalog
                .create_node_constraint(&mut pager, "person_surname", person, surname)
                .expect("node constraint");
            catalog
                .create_edge_constraint(&mut pager, "knows_weight_required", knows, weight)
                .expect("edge constraint");
            root = catalog.root();
            pager.update_header(|header| header.catalog_root = root);
            pager.commit().expect("commit");
        }
        let vfs = Arc::new(MemVfs::from_store(store));
        let mut pager = Pager::open(
            vfs,
            std::path::Path::new("cat.db"),
            crate::vfs::OpenMode::ReadWrite,
        )
        .expect("reopen");
        let catalog = Catalog::open(&mut pager, root).expect("open");
        let schema = catalog.schema();
        let named = |name: &str| {
            schema
                .node_indexes
                .iter()
                .find(|index| index.name.as_deref() == Some(name))
                .expect("named index")
        };
        let unique = named("person_name");
        assert_eq!(unique.kind, IndexKind::Unique);
        assert_eq!(unique.key_ids, vec![1]);
        assert!(unique.seekable());
        let node_key = named("person_key");
        assert_eq!(node_key.kind, IndexKind::NodeKey);
        assert_eq!(node_key.key_ids, vec![1, 2]);
        assert!(!node_key.seekable());
        let edge = schema
            .edge_indexes
            .iter()
            .find(|index| index.name.as_deref() == Some("knows_weight"))
            .expect("edge index");
        assert_eq!(edge.kind, IndexKind::Index);
        assert_eq!(catalog.schema().node_constraints[0].name, "person_surname");
        assert_eq!(
            catalog.schema().edge_constraints[0].name,
            "knows_weight_required"
        );
    }

    #[test]
    fn anonymous_indexes_keep_the_v1_encoding() {
        let mut pager = test_pager();
        pager.begin_write().expect("begin");
        let mut catalog = Catalog::create(&mut pager).expect("create");
        let name = catalog
            .ensure_property_key(&mut pager, "name")
            .expect("key");
        let id = catalog
            .create_node_index(&mut pager, name, 0, true)
            .expect("index");
        let def = catalog
            .schema()
            .node_indexes
            .iter()
            .find(|index| index.id == id)
            .expect("def")
            .clone();
        assert_eq!(node_index_kind(&def), kind::NODE_INDEX);
        // The v1 payload decodes to the same definition.
        let decoded = decode_node_index_v1(id, &encode_node_index_v1(&def)).expect("decode");
        assert_eq!(decoded, def);
        // Named or composite objects use the v2 encoding.
        let named = NodeIndexDef {
            name: Some("idx".to_owned()),
            kind: IndexKind::NodeKey,
            key_ids: vec![name, name],
            ..def.clone()
        };
        let (entry_kind, value) = encode_node_index(&named);
        assert_eq!(entry_kind, kind::NODE_INDEX_V2);
        let decoded = decode_node_index_v2(id, &value).expect("decode v2");
        assert_eq!(decoded, named);
        pager.commit().expect("commit");
    }

    #[test]
    fn dropping_entries_rewrites_the_catalog() {
        let store = crate::vfs::mem::new_store();
        let root;
        {
            let vfs = Arc::new(MemVfs::from_store(store.clone()));
            let mut pager =
                Pager::create(vfs, std::path::Path::new("cat.db"), 4096).expect("pager");
            pager.begin_write().expect("begin");
            let mut catalog = Catalog::create(&mut pager).expect("create");
            let person = catalog.ensure_label(&mut pager, "Person").expect("label");
            let name = catalog
                .ensure_property_key(&mut pager, "name")
                .expect("key");
            let index = catalog
                .create_node_index_named(&mut pager, "idx", IndexKind::Index, &[name], person)
                .expect("index");
            let constraint = catalog
                .create_node_constraint(&mut pager, "req", person, name)
                .expect("constraint");
            catalog
                .remove_node_index(&mut pager, index)
                .expect("drop index");
            assert!(catalog.schema().node_indexes.is_empty());
            let missing = catalog
                .remove_node_index(&mut pager, index)
                .expect_err("already dropped");
            assert_eq!(missing.code(), ErrorCode::NotFound);
            catalog
                .remove_node_constraint(&mut pager, constraint)
                .expect("drop constraint");
            assert!(catalog.schema().node_constraints.is_empty());
            root = catalog.root();
            pager.update_header(|header| header.catalog_root = root);
            pager.commit().expect("commit");
        }
        let vfs = Arc::new(MemVfs::from_store(store));
        let mut pager = Pager::open(
            vfs,
            std::path::Path::new("cat.db"),
            crate::vfs::OpenMode::ReadWrite,
        )
        .expect("reopen");
        let catalog = Catalog::open(&mut pager, root).expect("open");
        assert!(catalog.schema().node_indexes.is_empty());
        assert!(catalog.schema().node_constraints.is_empty());
    }
}
