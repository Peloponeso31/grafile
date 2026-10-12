//! Catalog: schema registries stored in a table B+tree.
//!
//! Entries are keyed by `(kind u8, id u32)` packed into a `u64` B+tree key.
//! The in-memory [`Schema`] mirrors the on-disk catalog and is rebuilt on
//! open; mutations go through `ensure_*`/`create_*` methods that keep both in
//! sync and persist the catalog root in the database header.

use hashbrown::HashMap;

use crate::btree::{Btree, TreeKind};
use crate::error::{Error, Result};
use crate::pager::{PageNo, Pager};
use crate::record::{decode_varint, encode_varint};

mod kind {
    pub const LABEL: u8 = 1;
    pub const EDGE_TYPE: u8 = 2;
    pub const PROPERTY_KEY: u8 = 3;
    pub const NODE_INDEX: u8 = 4;
    pub const EDGE_INDEX: u8 = 5;
}

/// Definition of a property index over nodes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeIndexDef {
    /// Index id (its catalog entry id).
    pub id: u32,
    /// Indexed property key id.
    pub key_id: u32,
    /// Restrict to this label id, or 0 for all nodes.
    pub label_id: u32,
    /// Whether the indexed property must be unique.
    pub unique: bool,
    /// B+tree root page.
    pub root: PageNo,
}

/// Definition of a property index over edges.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EdgeIndexDef {
    /// Index id (its catalog entry id).
    pub id: u32,
    /// Indexed property key id.
    pub key_id: u32,
    /// Restrict to this edge type id, or 0 for all edges.
    pub type_id: u32,
    /// Whether the indexed property must be unique.
    pub unique: bool,
    /// B+tree root page.
    pub root: PageNo,
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
    /// Node property indexes.
    pub node_indexes: Vec<NodeIndexDef>,
    /// Edge property indexes.
    pub edge_indexes: Vec<EdgeIndexDef>,
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

    /// Node indexes matching a property key (any label).
    pub fn node_indexes_for_key(&self, key_id: u32) -> impl Iterator<Item = &NodeIndexDef> {
        self.node_indexes
            .iter()
            .filter(move |index| index.key_id == key_id)
    }

    /// Edge indexes matching a property key (any type).
    pub fn edge_indexes_for_key(&self, key_id: u32) -> impl Iterator<Item = &EdgeIndexDef> {
        self.edge_indexes
            .iter()
            .filter(move |index| index.key_id == key_id)
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
                    let def = decode_node_index(id, &value)?;
                    next_index_id = next_index_id.max(id + 1);
                    schema.node_indexes.push(def);
                }
                kind::EDGE_INDEX => {
                    let def = decode_edge_index(id, &value)?;
                    next_index_id = next_index_id.max(id + 1);
                    schema.edge_indexes.push(def);
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

    /// Create a node property index and return its id.
    pub fn create_node_index(
        &mut self,
        pager: &mut Pager,
        key_id: u32,
        label_id: u32,
        unique: bool,
    ) -> Result<u32> {
        let id = self.next_index_id;
        self.next_index_id += 1;
        let tree = Btree::create(pager, TreeKind::Index)?;
        let def = NodeIndexDef {
            id,
            key_id,
            label_id,
            unique,
            root: tree.root(),
        };
        self.insert_entry(pager, kind::NODE_INDEX, id, &encode_node_index(&def))?;
        self.schema.node_indexes.push(def);
        pager.update_header(|header| header.schema_generation += 1);
        Ok(id)
    }

    /// Create an edge property index and return its id.
    pub fn create_edge_index(
        &mut self,
        pager: &mut Pager,
        key_id: u32,
        type_id: u32,
        unique: bool,
    ) -> Result<u32> {
        let id = self.next_index_id;
        self.next_index_id += 1;
        let tree = Btree::create(pager, TreeKind::Index)?;
        let def = EdgeIndexDef {
            id,
            key_id,
            type_id,
            unique,
            root: tree.root(),
        };
        self.insert_entry(pager, kind::EDGE_INDEX, id, &encode_edge_index(&def))?;
        self.schema.edge_indexes.push(def);
        pager.update_header(|header| header.schema_generation += 1);
        Ok(id)
    }

    /// Rewrite the catalog entry for a node index (root may have moved).
    pub fn update_node_index(&mut self, pager: &mut Pager, def: &NodeIndexDef) -> Result<()> {
        self.insert_entry(pager, kind::NODE_INDEX, def.id, &encode_node_index(def))
    }

    /// Rewrite the catalog entry for an edge index (root may have moved).
    pub fn update_edge_index(&mut self, pager: &mut Pager, def: &EdgeIndexDef) -> Result<()> {
        self.insert_entry(pager, kind::EDGE_INDEX, def.id, &encode_edge_index(def))
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
}

fn decode_name(value: &[u8]) -> Result<String> {
    std::str::from_utf8(value)
        .map(str::to_owned)
        .map_err(|_| Error::corrupt("catalog name is not valid UTF-8"))
}

fn encode_node_index(def: &NodeIndexDef) -> Vec<u8> {
    let mut out = Vec::with_capacity(16);
    encode_varint(&mut out, u64::from(def.key_id));
    encode_varint(&mut out, u64::from(def.label_id));
    out.push(u8::from(def.unique));
    out.extend_from_slice(&def.root.to_le_bytes());
    out
}

fn decode_node_index(id: u32, value: &[u8]) -> Result<NodeIndexDef> {
    let mut pos = 0usize;
    let key_id = u32::try_from(decode_varint(value, &mut pos)?)
        .map_err(|_| Error::corrupt("index key id out of range"))?;
    let label_id = u32::try_from(decode_varint(value, &mut pos)?)
        .map_err(|_| Error::corrupt("index label id out of range"))?;
    let unique = *value
        .get(pos)
        .ok_or_else(|| Error::corrupt("truncated node index entry"))?
        != 0;
    pos += 1;
    let root = u32::from_le_bytes(
        value
            .get(pos..pos + 4)
            .ok_or_else(|| Error::corrupt("truncated node index entry"))?
            .try_into()
            .expect("slice length"),
    );
    Ok(NodeIndexDef {
        id,
        key_id,
        label_id,
        unique,
        root,
    })
}

fn encode_edge_index(def: &EdgeIndexDef) -> Vec<u8> {
    let mut out = Vec::with_capacity(16);
    encode_varint(&mut out, u64::from(def.key_id));
    encode_varint(&mut out, u64::from(def.type_id));
    out.push(u8::from(def.unique));
    out.extend_from_slice(&def.root.to_le_bytes());
    out
}

fn decode_edge_index(id: u32, value: &[u8]) -> Result<EdgeIndexDef> {
    let mut pos = 0usize;
    let key_id = u32::try_from(decode_varint(value, &mut pos)?)
        .map_err(|_| Error::corrupt("index key id out of range"))?;
    let type_id = u32::try_from(decode_varint(value, &mut pos)?)
        .map_err(|_| Error::corrupt("index type id out of range"))?;
    let unique = *value
        .get(pos)
        .ok_or_else(|| Error::corrupt("truncated edge index entry"))?
        != 0;
    pos += 1;
    let root = u32::from_le_bytes(
        value
            .get(pos..pos + 4)
            .ok_or_else(|| Error::corrupt("truncated edge index entry"))?
            .try_into()
            .expect("slice length"),
    );
    Ok(EdgeIndexDef {
        id,
        key_id,
        type_id,
        unique,
        root,
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
        assert!(index.unique);
        assert_eq!(index.label_id, 1);
        assert_eq!(catalog.schema().label_name(1), Some("Person"));
    }
}
