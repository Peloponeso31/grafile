//! Chunked incidence matrix: fixed-size bitmap fragments in a B+tree.
//!
//! The graph's adjacency structure is stored as a sparse blocked bitmap. Each
//! fragment covers 65536 bits; for incidence planes that is 256 node rows by
//! 256 edge columns. Fragments are addressed by a 13-byte key
//! `(family, family id, band 0, band 1)` and live as index-tree records, so
//! empty chunks simply do not exist on disk.
//!
//! Families:
//!
//! | family | meaning | family id | band 0 | band 1 |
//! |-------:|---------|-----------|--------|--------|
//! | 0 | incidence, out | edge type (0 = any) | node id >> 8 | edge id >> 8 |
//! | 1 | incidence, in | edge type (0 = any) | node id >> 8 | edge id >> 8 |
//! | 2 | label membership | label id | node id >> 16 | 0 |
//! | 3 | edge type membership | edge type | edge id >> 16 | 0 |
//!
//! Fragments are derived data: they can be rebuilt from node and edge
//! records at any time.

use crate::btree::{Btree, TreeKind};
use crate::error::{Error, Result};
use crate::pager::{PageNo, Pager};

/// Bits per fragment.
pub const FRAGMENT_BITS: usize = 65_536;
/// 64-bit words per fragment.
pub const FRAGMENT_WORDS: usize = FRAGMENT_BITS / 64;
/// Fragment family for out-incidence.
pub const FAMILY_OUT: u8 = 0;
/// Fragment family for in-incidence.
pub const FAMILY_IN: u8 = 1;
/// Fragment family for label membership bitmaps.
pub const FAMILY_LABEL: u8 = 2;
/// Fragment family for edge-type membership bitmaps.
pub const FAMILY_EDGE_TYPE: u8 = 3;
/// Sentinel family id meaning "any edge type".
pub const ANY_TYPE: u32 = 0;

/// Direction of an incidence plane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Edges leaving a node.
    Out,
    /// Edges entering a node.
    In,
}

impl Direction {
    /// Fragment family for this direction.
    pub const fn family(self) -> u8 {
        match self {
            Self::Out => FAMILY_OUT,
            Self::In => FAMILY_IN,
        }
    }

    /// Opposite direction.
    pub const fn reverse(self) -> Self {
        match self {
            Self::Out => Self::In,
            Self::In => Self::Out,
        }
    }
}

/// A 65536-bit bitmap fragment.
#[derive(Clone)]
pub struct Fragment {
    words: Box<[u64; FRAGMENT_WORDS]>,
}

impl Default for Fragment {
    fn default() -> Self {
        Self::new()
    }
}

impl Fragment {
    /// Create an empty fragment.
    pub fn new() -> Self {
        Self {
            words: Box::new([0u64; FRAGMENT_WORDS]),
        }
    }

    /// Set a bit.
    pub fn set(&mut self, bit: u16) {
        self.words[bit as usize / 64] |= 1u64 << (bit as usize % 64);
    }

    /// Clear a bit.
    pub fn clear(&mut self, bit: u16) {
        self.words[bit as usize / 64] &= !(1u64 << (bit as usize % 64));
    }

    /// Test a bit.
    pub fn test(&self, bit: u16) -> bool {
        self.words[bit as usize / 64] & (1u64 << (bit as usize % 64)) != 0
    }

    /// Whether no bits are set.
    pub fn is_empty(&self) -> bool {
        self.words.iter().all(|word| *word == 0)
    }

    /// Number of set bits.
    pub fn cardinality(&self) -> u32 {
        self.words.iter().map(|word| word.count_ones()).sum()
    }

    /// Iterate the set bit positions in ascending order.
    pub fn iter_bits(&self) -> impl Iterator<Item = u16> + '_ {
        self.words.iter().enumerate().flat_map(|(word_idx, word)| {
            let mut word = *word;
            std::iter::from_fn(move || {
                if word == 0 {
                    return None;
                }
                let bit = word.trailing_zeros() as usize;
                word &= word - 1;
                Some((word_idx * 64 + bit) as u16)
            })
        })
    }

    /// Iterate set bit positions within a 256-bit row.
    pub fn row_bits(&self, row: u8) -> impl Iterator<Item = u16> + '_ {
        let base = row as usize * 4;
        let words: [u64; 4] = self.words[base..base + 4]
            .try_into()
            .expect("row slice length");
        words.into_iter().enumerate().flat_map(|(word_idx, word)| {
            let mut word = word;
            std::iter::from_fn(move || {
                if word == 0 {
                    return None;
                }
                let bit = word.trailing_zeros() as usize;
                word &= word - 1;
                Some((word_idx * 64 + bit) as u16)
            })
        })
    }

    /// Whether the 256-bit row has any bit set.
    pub fn row_nonempty(&self, row: u8) -> bool {
        let base = row as usize * 4;
        self.words[base..base + 4].iter().any(|word| *word != 0)
    }

    /// Whether rows `row_a` of `self` and `row_b` of `other` share a set bit.
    ///
    /// This is the incidence-matrix edge existence test: intersect an out-row
    /// with an in-row and check for a common edge.
    pub fn row_and_nonempty(&self, row_a: u8, other: &Fragment, row_b: u8) -> bool {
        let base_a = row_a as usize * 4;
        let base_b = row_b as usize * 4;
        (0..4).any(|idx| self.words[base_a + idx] & other.words[base_b + idx] != 0)
    }

    /// Encode as sparse (tag 0) or dense (tag 1) bytes.
    pub fn encode(&self) -> Vec<u8> {
        let cardinality = self.cardinality() as usize;
        if cardinality * 2 < FRAGMENT_BITS / 8 {
            let mut out = Vec::with_capacity(1 + cardinality * 2);
            out.push(0);
            for bit in self.iter_bits() {
                out.extend_from_slice(&bit.to_le_bytes());
            }
            out
        } else {
            let mut out = Vec::with_capacity(1 + FRAGMENT_WORDS * 8);
            out.push(1);
            for word in self.words.iter() {
                out.extend_from_slice(&word.to_le_bytes());
            }
            out
        }
    }

    /// Decode from fragment bytes.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let (tag, payload) = bytes
            .split_first()
            .ok_or_else(|| Error::corrupt("empty fragment"))?;
        let mut fragment = Self::new();
        match tag {
            0 => {
                if payload.len() % 2 != 0 {
                    return Err(Error::corrupt("sparse fragment has odd length"));
                }
                let mut previous: Option<u16> = None;
                for chunk in payload.as_chunks::<2>().0 {
                    let bit = u16::from_le_bytes(*chunk);
                    if let Some(previous) = previous
                        && bit <= previous
                    {
                        return Err(Error::corrupt("sparse fragment bits out of order"));
                    }
                    previous = Some(bit);
                    fragment.set(bit);
                }
                Ok(fragment)
            }
            1 => {
                if payload.len() != FRAGMENT_WORDS * 8 {
                    return Err(Error::corrupt("dense fragment has wrong length"));
                }
                for (idx, chunk) in payload.as_chunks::<8>().0.iter().enumerate() {
                    fragment.words[idx] = u64::from_le_bytes(*chunk);
                }
                Ok(fragment)
            }
            other => Err(Error::corrupt(format!("unknown fragment tag {other}"))),
        }
    }
}

/// Address of a fragment in the store.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct FragmentKey {
    /// Fragment family (see module docs).
    pub family: u8,
    /// Family id (edge type, label id, or 0 for "any type").
    pub family_id: u32,
    /// First band coordinate.
    pub band0: u32,
    /// Second band coordinate.
    pub band1: u32,
}

impl FragmentKey {
    /// Incidence plane fragment for a node/edge pair.
    pub fn incidence(direction: Direction, type_id: u32, node_id: u64, edge_id: u64) -> Self {
        Self {
            family: direction.family(),
            family_id: type_id,
            band0: (node_id >> 8) as u32,
            band1: (edge_id >> 8) as u32,
        }
    }

    /// Bit position of `(node, edge)` inside an incidence fragment.
    pub fn incidence_bit(node_id: u64, edge_id: u64) -> u16 {
        (((node_id & 255) << 8) | (edge_id & 255)) as u16
    }

    /// Label bitmap fragment for a node.
    pub fn label(label_id: u32, node_id: u64) -> Self {
        Self {
            family: FAMILY_LABEL,
            family_id: label_id,
            band0: (node_id >> 16) as u32,
            band1: 0,
        }
    }

    /// Bit position of a node inside a label fragment.
    pub fn label_bit(node_id: u64) -> u16 {
        (node_id & 0xFFFF) as u16
    }

    /// Edge-type bitmap fragment for an edge.
    pub fn edge_type(type_id: u32, edge_id: u64) -> Self {
        Self {
            family: FAMILY_EDGE_TYPE,
            family_id: type_id,
            band0: (edge_id >> 16) as u32,
            band1: 0,
        }
    }

    /// Bit position of an edge inside an edge-type fragment.
    pub fn edge_type_bit(edge_id: u64) -> u16 {
        (edge_id & 0xFFFF) as u16
    }

    /// Encode to the 13-byte on-disk key (big-endian for sort order).
    pub fn encode(&self) -> [u8; 13] {
        let mut out = [0u8; 13];
        out[0] = self.family;
        out[1..5].copy_from_slice(&self.family_id.to_be_bytes());
        out[5..9].copy_from_slice(&self.band0.to_be_bytes());
        out[9..13].copy_from_slice(&self.band1.to_be_bytes());
        out
    }

    /// Decode from on-disk key bytes.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != 13 {
            return Err(Error::corrupt("fragment key must be 13 bytes"));
        }
        Ok(Self {
            family: bytes[0],
            family_id: u32::from_be_bytes(bytes[1..5].try_into().expect("slice length")),
            band0: u32::from_be_bytes(bytes[5..9].try_into().expect("slice length")),
            band1: u32::from_be_bytes(bytes[9..13].try_into().expect("slice length")),
        })
    }

    /// Encode the 9-byte prefix `(family, family_id, band0)`.
    pub fn prefix(family: u8, family_id: u32, band0: u32) -> [u8; 9] {
        let mut out = [0u8; 9];
        out[0] = family;
        out[1..5].copy_from_slice(&family_id.to_be_bytes());
        out[5..9].copy_from_slice(&band0.to_be_bytes());
        out
    }
}

/// B+tree-backed fragment store.
pub struct FragmentStore {
    tree: Btree,
}

impl FragmentStore {
    /// Create an empty store, allocating its B+tree root.
    pub fn create(pager: &mut Pager) -> Result<Self> {
        Ok(Self {
            tree: Btree::create(pager, TreeKind::Index)?,
        })
    }

    /// Open an existing store at `root`.
    pub fn open(root: PageNo) -> Self {
        Self {
            tree: Btree::from_root(root, TreeKind::Index),
        }
    }

    /// Store B+tree root (persist in the database header).
    pub fn root(&self) -> PageNo {
        self.tree.root()
    }

    /// Read a fragment; `None` means all-zero (missing chunks are empty).
    pub fn get(&self, pager: &mut Pager, key: &FragmentKey) -> Result<Option<Fragment>> {
        match self.tree.get(pager, &key.encode())? {
            Some(bytes) => Ok(Some(Fragment::decode(&bytes)?)),
            None => Ok(None),
        }
    }

    /// Set one bit, updating (or creating) the fragment.
    pub fn set_bit(&mut self, pager: &mut Pager, key: &FragmentKey, bit: u16) -> Result<()> {
        let mut fragment = self.get(pager, key)?.unwrap_or_default();
        fragment.set(bit);
        self.put(pager, key, &fragment)
    }

    /// Clear one bit, deleting the fragment when it becomes empty.
    pub fn clear_bit(&mut self, pager: &mut Pager, key: &FragmentKey, bit: u16) -> Result<()> {
        let Some(mut fragment) = self.get(pager, key)? else {
            return Ok(());
        };
        fragment.clear(bit);
        self.put(pager, key, &fragment)
    }

    /// Set several bits; keys may repeat (grouped updates are fine).
    pub fn set_bits(&mut self, pager: &mut Pager, bits: &[(FragmentKey, u16)]) -> Result<()> {
        for (key, bit) in bits {
            self.set_bit(pager, key, *bit)?;
        }
        Ok(())
    }

    /// Clear several bits.
    pub fn clear_bits(&mut self, pager: &mut Pager, bits: &[(FragmentKey, u16)]) -> Result<()> {
        for (key, bit) in bits {
            self.clear_bit(pager, key, *bit)?;
        }
        Ok(())
    }

    fn put(&mut self, pager: &mut Pager, key: &FragmentKey, fragment: &Fragment) -> Result<()> {
        let encoded_key = key.encode();
        let existed = self.tree.contains(pager, &encoded_key)?;
        if existed {
            self.tree.delete(pager, &encoded_key)?;
        }
        if !fragment.is_empty() {
            self.tree.insert(pager, &encoded_key, &fragment.encode())?;
        }
        Ok(())
    }

    /// All non-empty fragments with a `(family, family_id, band0)` prefix.
    pub fn fragments_for_band(
        &self,
        pager: &mut Pager,
        family: u8,
        family_id: u32,
        band0: u32,
    ) -> Result<Vec<(FragmentKey, Fragment)>> {
        let prefix = FragmentKey::prefix(family, family_id, band0);
        let mut cursor = self.tree.cursor();
        let mut out = Vec::new();
        let mut ok = cursor.seek_ge(pager, &prefix)?;
        while ok && cursor.key().starts_with(&prefix) {
            let key = FragmentKey::decode(cursor.key())?;
            let fragment = Fragment::decode(&cursor.value(pager)?)?;
            if !fragment.is_empty() {
                out.push((key, fragment));
            }
            ok = cursor.next(pager)?;
        }
        Ok(out)
    }

    /// Node ids carrying a label.
    pub fn label_nodes(&self, pager: &mut Pager, label_id: u32) -> Result<Vec<u64>> {
        let prefix = FragmentKey::prefix(FAMILY_LABEL, label_id, 0);
        let mut cursor = self.tree.cursor();
        let mut out = Vec::new();
        let mut ok = cursor.seek_ge(pager, &prefix)?;
        while ok && cursor.key().starts_with(&prefix[..5]) {
            let key = FragmentKey::decode(cursor.key())?;
            let fragment = Fragment::decode(&cursor.value(pager)?)?;
            for bit in fragment.iter_bits() {
                out.push((u64::from(key.band0) << 16) | u64::from(bit));
            }
            ok = cursor.next(pager)?;
        }
        Ok(out)
    }

    /// Edge ids of a given type.
    pub fn edges_of_type(&self, pager: &mut Pager, type_id: u32) -> Result<Vec<u64>> {
        let prefix = FragmentKey::prefix(FAMILY_EDGE_TYPE, type_id, 0);
        let mut cursor = self.tree.cursor();
        let mut out = Vec::new();
        let mut ok = cursor.seek_ge(pager, &prefix)?;
        while ok && cursor.key().starts_with(&prefix[..5]) {
            let key = FragmentKey::decode(cursor.key())?;
            let fragment = Fragment::decode(&cursor.value(pager)?)?;
            for bit in fragment.iter_bits() {
                out.push((u64::from(key.band0) << 16) | u64::from(bit));
            }
            ok = cursor.next(pager)?;
        }
        Ok(out)
    }

    /// Whether an edge `(u -> v)` of `type_id` exists, tested by intersecting
    /// the out-row of `u` with the in-row of `v` in each column band.
    pub fn incidence_exists(
        &self,
        pager: &mut Pager,
        type_id: u32,
        src: u64,
        dst: u64,
    ) -> Result<bool> {
        let out = self.fragments_for_band(pager, FAMILY_OUT, type_id, (src >> 8) as u32)?;
        let src_row = (src & 255) as u8;
        let dst_row = (dst & 255) as u8;
        let dst_band = (dst >> 8) as u32;
        for (key, fragment) in out {
            if !fragment.row_nonempty(src_row) {
                continue;
            }
            let in_key = FragmentKey {
                family: FAMILY_IN,
                family_id: type_id,
                band0: dst_band,
                band1: key.band1,
            };
            if let Some(in_fragment) = self.get(pager, &in_key)?
                && fragment.row_and_nonempty(src_row, &in_fragment, dst_row)
            {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::mem::MemVfs;
    use std::sync::Arc;

    fn test_pager() -> Pager {
        Pager::create(
            Arc::new(MemVfs::new()),
            std::path::Path::new("matrix.db"),
            4096,
        )
        .expect("pager")
    }

    #[test]
    fn fragment_sparse_dense_roundtrip() {
        let mut fragment = Fragment::new();
        fragment.set(0);
        fragment.set(65535);
        fragment.set(30000);
        assert_eq!(fragment.cardinality(), 3);
        assert!(fragment.test(30000));
        assert!(!fragment.test(30001));
        let encoded = fragment.encode();
        assert_eq!(encoded[0], 0, "should use sparse encoding");
        let decoded = Fragment::decode(&encoded).expect("decode");
        assert_eq!(decoded.cardinality(), 3);
        assert!(decoded.test(65535));

        let mut dense = Fragment::new();
        for bit in (0..65536u32).step_by(2) {
            dense.set(bit as u16);
        }
        let encoded = dense.encode();
        assert_eq!(encoded[0], 1, "should use dense encoding");
        let decoded = Fragment::decode(&encoded).expect("decode");
        assert_eq!(decoded.cardinality(), 32768);
        assert!(decoded.test(2) && !decoded.test(3));
    }

    #[test]
    fn fragment_rejects_bad_input() {
        assert!(Fragment::decode(&[]).is_err());
        assert!(Fragment::decode(&[9, 0]).is_err());
        assert!(Fragment::decode(&[0, 1, 0, 2]).is_err());
        assert!(Fragment::decode(&[0, 5, 0, 5, 0]).is_err());
        assert!(Fragment::decode(&[1, 0, 0]).is_err());
    }

    #[test]
    fn row_operations() {
        let mut fragment = Fragment::new();
        fragment.set(FragmentKey::incidence_bit(3, 7));
        fragment.set(FragmentKey::incidence_bit(3, 400));
        assert_eq!(fragment.row_bits(3).collect::<Vec<_>>(), vec![7, 400 % 256]);
        assert!(fragment.row_nonempty(3));
        assert!(!fragment.row_nonempty(4));

        let mut other = Fragment::new();
        other.set(FragmentKey::incidence_bit(9, 7));
        assert!(fragment.row_and_nonempty(3, &other, 9));
        assert!(!fragment.row_and_nonempty(3, &other, 8));
    }

    #[test]
    fn store_set_clear_and_empty_removal() {
        let mut pager = test_pager();
        pager.begin_write().expect("begin");
        let mut store = FragmentStore::create(&mut pager).expect("store");
        let key = FragmentKey::incidence(Direction::Out, 1, 100, 500);
        let bit = FragmentKey::incidence_bit(100, 500);
        store.set_bit(&mut pager, &key, bit).expect("set");
        assert!(store.get(&mut pager, &key).expect("get").is_some());
        store.clear_bit(&mut pager, &key, bit).expect("clear");
        assert!(store.get(&mut pager, &key).expect("get").is_none());
        pager.commit().expect("commit");
    }

    #[test]
    fn incidence_existence_via_row_intersection() {
        let mut pager = test_pager();
        pager.begin_write().expect("begin");
        let mut store = FragmentStore::create(&mut pager).expect("store");
        let (src, dst, edge, edge_type) = (1000u64, 20_000u64, 7u64, 1u32);
        let bits = [
            (
                FragmentKey::incidence(Direction::Out, edge_type, src, edge),
                FragmentKey::incidence_bit(src, edge),
            ),
            (
                FragmentKey::incidence(Direction::In, edge_type, dst, edge),
                FragmentKey::incidence_bit(dst, edge),
            ),
        ];
        store.set_bits(&mut pager, &bits).expect("set");
        assert!(
            store
                .incidence_exists(&mut pager, edge_type, src, dst)
                .expect("exists")
        );
        assert!(
            !store
                .incidence_exists(&mut pager, edge_type, dst, src)
                .expect("reverse")
        );
        assert!(
            !store
                .incidence_exists(&mut pager, 2, src, dst)
                .expect("other type")
        );
        pager.commit().expect("commit");
    }

    #[test]
    fn label_scan_returns_nodes() {
        let mut pager = test_pager();
        pager.begin_write().expect("begin");
        let mut store = FragmentStore::create(&mut pager).expect("store");
        for node in [5u64, 70_000, 200_000] {
            let key = FragmentKey::label(1, node);
            store
                .set_bit(&mut pager, &key, FragmentKey::label_bit(node))
                .expect("set");
        }
        let nodes = store.label_nodes(&mut pager, 1).expect("scan");
        assert_eq!(nodes, vec![5, 70_000, 200_000]);
        assert!(store.label_nodes(&mut pager, 2).expect("scan").is_empty());
        pager.commit().expect("commit");
    }

    #[test]
    fn band_fragments_iterate_in_order() {
        let mut pager = test_pager();
        pager.begin_write().expect("begin");
        let mut store = FragmentStore::create(&mut pager).expect("store");
        let node = 300u64;
        for edge in [10u64, 300, 70_000] {
            let key = FragmentKey::incidence(Direction::Out, 0, node, edge);
            store
                .set_bit(&mut pager, &key, FragmentKey::incidence_bit(node, edge))
                .expect("set");
        }
        let fragments = store
            .fragments_for_band(&mut pager, FAMILY_OUT, 0, (node >> 8) as u32)
            .expect("band");
        assert_eq!(fragments.len(), 3);
        assert_eq!(fragments[0].0.band1, 0);
        assert_eq!(fragments[1].0.band1, (300u64 >> 8) as u32);
        assert_eq!(fragments[2].0.band1, (70_000u64 >> 8) as u32);
        let edges: Vec<u64> = fragments
            .iter()
            .flat_map(|(key, fragment)| {
                fragment
                    .row_bits((node & 255) as u8)
                    .map(move |bit| (u64::from(key.band1) << 8) | u64::from(bit))
            })
            .collect();
        assert_eq!(edges, vec![10, 300, 70_000]);
        pager.commit().expect("commit");
    }
}
