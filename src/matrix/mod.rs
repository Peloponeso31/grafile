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

use hashbrown::HashMap;
use smallvec::SmallVec;

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
///
/// Sparse fragments (the common case) store sorted bit offsets and never
/// touch the full 8 KiB word array; dense fragments fall back to the word
/// representation once a fragment exceeds [`SPARSE_MAX`] bits.
#[derive(Clone)]
pub struct Fragment {
    repr: Repr,
}

#[derive(Clone)]
enum Repr {
    /// Sorted, unique bit offsets.
    Sparse(SmallVec<[u16; 16]>),
    /// Full 64 KiB bitmap as 1024 words.
    Dense(Box<[u64; FRAGMENT_WORDS]>),
}

/// Sparse fragments switch to the dense representation above this many bits.
const SPARSE_MAX: usize = 4095;

impl Default for Fragment {
    fn default() -> Self {
        Self::new()
    }
}

impl Fragment {
    /// Create an empty fragment.
    pub fn new() -> Self {
        Self {
            repr: Repr::Sparse(SmallVec::new()),
        }
    }

    /// Set a bit.
    pub fn set(&mut self, bit: u16) {
        match &mut self.repr {
            Repr::Sparse(bits) => {
                if let Err(index) = bits.binary_search(&bit) {
                    bits.insert(index, bit);
                    if bits.len() > SPARSE_MAX {
                        let mut words = Box::new([0u64; FRAGMENT_WORDS]);
                        for stored in bits.iter() {
                            words[*stored as usize / 64] |= 1u64 << (*stored as usize % 64);
                        }
                        self.repr = Repr::Dense(words);
                    }
                }
            }
            Repr::Dense(words) => {
                words[bit as usize / 64] |= 1u64 << (bit as usize % 64);
            }
        }
    }

    /// Clear a bit.
    pub fn clear(&mut self, bit: u16) {
        match &mut self.repr {
            Repr::Sparse(bits) => {
                if let Ok(index) = bits.binary_search(&bit) {
                    bits.remove(index);
                }
            }
            Repr::Dense(words) => {
                words[bit as usize / 64] &= !(1u64 << (bit as usize % 64));
            }
        }
    }

    /// Test a bit.
    pub fn test(&self, bit: u16) -> bool {
        match &self.repr {
            Repr::Sparse(bits) => bits.binary_search(&bit).is_ok(),
            Repr::Dense(words) => words[bit as usize / 64] & (1u64 << (bit as usize % 64)) != 0,
        }
    }

    /// Whether no bits are set.
    pub fn is_empty(&self) -> bool {
        match &self.repr {
            Repr::Sparse(bits) => bits.is_empty(),
            Repr::Dense(words) => words.iter().all(|word| *word == 0),
        }
    }

    /// Number of set bits.
    pub fn cardinality(&self) -> u32 {
        match &self.repr {
            Repr::Sparse(bits) => bits.len() as u32,
            Repr::Dense(words) => words.iter().map(|word| word.count_ones()).sum(),
        }
    }

    /// Iterate the set bit positions in ascending order.
    pub fn iter_bits(&self) -> impl Iterator<Item = u16> + '_ {
        match &self.repr {
            Repr::Sparse(bits) => Bits::sparse(bits.iter()),
            Repr::Dense(words) => Bits::dense(&words[..]),
        }
    }

    /// Iterate set bit positions within a 256-bit row, as offsets `0..=255`.
    pub fn row_bits(&self, row: u8) -> impl Iterator<Item = u16> + '_ {
        let row_start = row as u16 * 256;
        match &self.repr {
            Repr::Sparse(bits) => Bits::sparse_row(bits.iter(), row_start),
            Repr::Dense(words) => {
                let base = row as usize * 4;
                Bits::dense(&words[base..base + 4])
            }
        }
    }

    /// Whether the 256-bit row has any bit set.
    pub fn row_nonempty(&self, row: u8) -> bool {
        match &self.repr {
            Repr::Sparse(bits) => {
                let start = row as u16 * 256;
                let end = start + 256;
                let index = bits.partition_point(|bit| *bit < start);
                bits.get(index).is_some_and(|bit| *bit < end)
            }
            Repr::Dense(words) => {
                let base = row as usize * 4;
                words[base..base + 4].iter().any(|word| *word != 0)
            }
        }
    }

    /// Whether rows `row_a` of `self` and `row_b` of `other` share a set bit.
    ///
    /// This is the incidence-matrix edge existence test: intersect an out-row
    /// with an in-row and check for a common edge.
    pub fn row_and_nonempty(&self, row_a: u8, other: &Fragment, row_b: u8) -> bool {
        let left = self.row_words(row_a);
        let right = other.row_words(row_b);
        (0..4).any(|index| left[index] & right[index] != 0)
    }

    fn row_words(&self, row: u8) -> [u64; 4] {
        match &self.repr {
            Repr::Sparse(bits) => {
                let mut words = [0u64; 4];
                let start = row as u16 * 256;
                let end = start + 256;
                let mut index = bits.partition_point(|bit| *bit < start);
                while let Some(bit) = bits.get(index) {
                    if *bit >= end {
                        break;
                    }
                    let offset = (*bit - start) as usize;
                    words[offset / 64] |= 1u64 << (offset % 64);
                    index += 1;
                }
                words
            }
            Repr::Dense(words) => {
                let base = row as usize * 4;
                words[base..base + 4].try_into().expect("row slice length")
            }
        }
    }

    /// Encode as sparse (tag 0) or dense (tag 1) bytes.
    pub fn encode(&self) -> Vec<u8> {
        match &self.repr {
            Repr::Sparse(bits) if bits.len() * 2 < FRAGMENT_BITS / 8 => {
                let mut out = Vec::with_capacity(1 + bits.len() * 2);
                out.push(0);
                for bit in bits.iter() {
                    out.extend_from_slice(&bit.to_le_bytes());
                }
                out
            }
            Repr::Sparse(bits) => {
                // Defensive: a sparse fragment past the encoding threshold is
                // written in dense form.
                let mut words = [0u64; FRAGMENT_WORDS];
                for bit in bits.iter() {
                    words[*bit as usize / 64] |= 1u64 << (*bit as usize % 64);
                }
                encode_dense(&words)
            }
            Repr::Dense(words) => encode_dense(words),
        }
    }

    /// Decode from fragment bytes.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let (tag, payload) = bytes
            .split_first()
            .ok_or_else(|| Error::corrupt("empty fragment"))?;
        match tag {
            0 => {
                if payload.len() % 2 != 0 {
                    return Err(Error::corrupt("sparse fragment has odd length"));
                }
                let mut bits = SmallVec::with_capacity(payload.len() / 2);
                let mut previous: Option<u16> = None;
                for chunk in payload.as_chunks::<2>().0 {
                    let bit = u16::from_le_bytes(*chunk);
                    if let Some(previous) = previous
                        && bit <= previous
                    {
                        return Err(Error::corrupt("sparse fragment bits out of order"));
                    }
                    previous = Some(bit);
                    bits.push(bit);
                }
                Ok(Self {
                    repr: Repr::Sparse(bits),
                })
            }
            1 => {
                if payload.len() != FRAGMENT_WORDS * 8 {
                    return Err(Error::corrupt("dense fragment has wrong length"));
                }
                let mut words = Box::new([0u64; FRAGMENT_WORDS]);
                for (index, chunk) in payload.as_chunks::<8>().0.iter().enumerate() {
                    words[index] = u64::from_le_bytes(*chunk);
                }
                Ok(Self {
                    repr: Repr::Dense(words),
                })
            }
            other => Err(Error::corrupt(format!("unknown fragment tag {other}"))),
        }
    }
}

fn encode_dense(words: &[u64; FRAGMENT_WORDS]) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + FRAGMENT_WORDS * 8);
    out.push(1);
    for word in words.iter() {
        out.extend_from_slice(&word.to_le_bytes());
    }
    out
}

/// Iterator over set bit positions, handling both fragment representations.
struct Bits<'a> {
    sparse: Option<std::slice::Iter<'a, u16>>,
    row_start: u16,
    dense: Option<DenseBits<'a>>,
}

impl<'a> Bits<'a> {
    fn sparse(iter: std::slice::Iter<'a, u16>) -> Self {
        Self {
            sparse: Some(iter),
            row_start: 0,
            dense: None,
        }
    }

    fn sparse_row(iter: std::slice::Iter<'a, u16>, row_start: u16) -> Self {
        Self {
            sparse: Some(iter),
            row_start,
            dense: None,
        }
    }

    fn dense(words: &'a [u64]) -> Self {
        Self {
            sparse: None,
            row_start: 0,
            dense: Some(DenseBits {
                words,
                next_index: 0,
                current: 0,
                current_base: 0,
            }),
        }
    }
}

impl Iterator for Bits<'_> {
    type Item = u16;

    fn next(&mut self) -> Option<u16> {
        if let Some(sparse) = &mut self.sparse {
            for bit in sparse.by_ref() {
                if *bit < self.row_start {
                    continue;
                }
                if self.row_start != 0 {
                    // Row iteration: stop at the end of the row.
                    let offset = *bit - self.row_start;
                    if offset >= 256 {
                        return None;
                    }
                    return Some(offset);
                }
                return Some(*bit);
            }
            return None;
        }
        self.dense.as_mut()?.next()
    }
}

struct DenseBits<'a> {
    words: &'a [u64],
    next_index: usize,
    current: u64,
    current_base: usize,
}

impl Iterator for DenseBits<'_> {
    type Item = u16;

    fn next(&mut self) -> Option<u16> {
        loop {
            if self.current != 0 {
                let bit = self.current.trailing_zeros() as usize;
                self.current &= self.current - 1;
                return Some((self.current_base + bit) as u16);
            }
            if self.next_index >= self.words.len() {
                return None;
            }
            self.current = self.words[self.next_index];
            self.current_base = self.next_index * 64;
            self.next_index += 1;
        }
    }
}

/// Address of a fragment in the store.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
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

/// How many modified fragments may accumulate before the store flushes them
/// to the B+tree mid-transaction.
const DEFAULT_CACHE_CAPACITY: usize = 16_384;

/// B+tree-backed fragment store with a write-back cache.
///
/// Modified fragments stay in memory for the duration of a write transaction
/// and are encoded once when the transaction commits: dense fragments (label
/// bitmaps, edge-type bitmaps, high-degree nodes) would otherwise be decoded
/// and re-encoded on every single insert.
pub struct FragmentStore {
    tree: Btree,
    cache: HashMap<FragmentKey, Fragment>,
    cache_capacity: usize,
    generation: u64,
}

impl FragmentStore {
    /// Create an empty store, allocating its B+tree root.
    pub fn create(pager: &mut Pager) -> Result<Self> {
        Ok(Self {
            tree: Btree::create(pager, TreeKind::Index)?,
            cache: HashMap::new(),
            cache_capacity: DEFAULT_CACHE_CAPACITY,
            generation: pager.txn_generation(),
        })
    }

    /// Open an existing store at `root`.
    pub fn open(root: PageNo) -> Self {
        Self {
            tree: Btree::from_root(root, TreeKind::Index),
            cache: HashMap::new(),
            cache_capacity: DEFAULT_CACHE_CAPACITY,
            generation: 0,
        }
    }

    /// Configure how many modified fragments may accumulate before an
    /// automatic mid-transaction flush.
    pub fn set_cache_capacity(&mut self, fragments: usize) {
        self.cache_capacity = fragments.max(1);
    }

    /// Drop cached fragments when the pager's transaction changed (a commit
    /// flushes them first; a rollback invalidates them).
    fn sync_generation(&mut self, pager: &Pager) {
        let generation = pager.txn_generation();
        if self.generation != generation {
            self.cache.clear();
            self.generation = generation;
        }
    }

    /// Store B+tree root (persist in the database header).
    pub fn root(&self) -> PageNo {
        self.tree.root()
    }

    /// Read a fragment; `None` means all-zero (missing chunks are empty).
    pub fn get(&mut self, pager: &mut Pager, key: &FragmentKey) -> Result<Option<Fragment>> {
        self.sync_generation(pager);
        if let Some(fragment) = self.cache.get(key) {
            return Ok(if fragment.is_empty() {
                None
            } else {
                Some(fragment.clone())
            });
        }
        match self.tree.get(pager, &key.encode())? {
            Some(bytes) => Ok(Some(Fragment::decode(&bytes)?)),
            None => Ok(None),
        }
    }

    /// Set one bit, updating (or creating) the fragment.
    pub fn set_bit(&mut self, pager: &mut Pager, key: &FragmentKey, bit: u16) -> Result<()> {
        self.sync_generation(pager);
        if let Some(fragment) = self.cache.get_mut(key) {
            fragment.set(bit);
            return Ok(());
        }
        let mut fragment = self.load(pager, key)?.unwrap_or_default();
        fragment.set(bit);
        self.cache.insert(*key, fragment);
        self.flush_if_full(pager)
    }

    /// Clear one bit, deleting the fragment when it becomes empty.
    pub fn clear_bit(&mut self, pager: &mut Pager, key: &FragmentKey, bit: u16) -> Result<()> {
        self.sync_generation(pager);
        if let Some(fragment) = self.cache.get_mut(key) {
            fragment.clear(bit);
            return Ok(());
        }
        let Some(mut fragment) = self.load(pager, key)? else {
            return Ok(());
        };
        fragment.clear(bit);
        self.cache.insert(*key, fragment);
        self.flush_if_full(pager)
    }

    fn load(&self, pager: &mut Pager, key: &FragmentKey) -> Result<Option<Fragment>> {
        match self.tree.get(pager, &key.encode())? {
            Some(bytes) => Ok(Some(Fragment::decode(&bytes)?)),
            None => Ok(None),
        }
    }

    fn flush_if_full(&mut self, pager: &mut Pager) -> Result<()> {
        if self.cache.len() >= self.cache_capacity {
            self.flush(pager)?;
        }
        Ok(())
    }

    /// Encode cached fragments into the B+tree. Must be called before the
    /// pager commits; [`crate::engine::Database::commit`] does this
    /// automatically.
    pub fn flush(&mut self, pager: &mut Pager) -> Result<()> {
        self.sync_generation(pager);
        if self.cache.is_empty() {
            return Ok(());
        }
        let pending: Vec<(FragmentKey, Fragment)> = self.cache.drain().collect();
        for (key, fragment) in pending {
            let encoded_key = key.encode();
            if fragment.is_empty() {
                self.tree.delete(pager, &encoded_key)?;
            } else {
                self.tree.replace(pager, &encoded_key, &fragment.encode())?;
            }
        }
        Ok(())
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

    /// Non-empty fragments under `prefix`, merging the tree with unflushed
    /// cache entries (cache entries win; emptied ones suppress tree entries).
    fn merged_prefix(
        &mut self,
        pager: &mut Pager,
        prefix: &[u8],
    ) -> Result<Vec<(FragmentKey, Fragment)>> {
        self.sync_generation(pager);
        let mut merged: Vec<(FragmentKey, Fragment)> = Vec::new();
        {
            let mut cursor = self.tree.cursor();
            let mut ok = cursor.seek_ge(pager, prefix)?;
            while ok && cursor.key().starts_with(prefix) {
                let key = FragmentKey::decode(cursor.key())?;
                let fragment = Fragment::decode(&cursor.value(pager)?)?;
                if !fragment.is_empty() {
                    merged.push((key, fragment));
                }
                ok = cursor.next(pager)?;
            }
        }
        for (key, fragment) in &self.cache {
            if !key.encode().starts_with(prefix) {
                continue;
            }
            merged.retain(|(existing, _)| existing != key);
            if !fragment.is_empty() {
                merged.push((*key, fragment.clone()));
            }
        }
        merged.sort_by_key(|(key, _)| key.encode());
        Ok(merged)
    }

    /// All non-empty fragments with a `(family, family_id, band0)` prefix.
    pub fn fragments_for_band(
        &mut self,
        pager: &mut Pager,
        family: u8,
        family_id: u32,
        band0: u32,
    ) -> Result<Vec<(FragmentKey, Fragment)>> {
        let prefix = FragmentKey::prefix(family, family_id, band0);
        self.merged_prefix(pager, &prefix)
    }

    /// Node ids carrying a label.
    pub fn label_nodes(&mut self, pager: &mut Pager, label_id: u32) -> Result<Vec<u64>> {
        let prefix = FragmentKey::prefix(FAMILY_LABEL, label_id, 0);
        let mut out = Vec::new();
        for (key, fragment) in self.merged_prefix(pager, &prefix[..5])? {
            for bit in fragment.iter_bits() {
                out.push((u64::from(key.band0) << 16) | u64::from(bit));
            }
        }
        Ok(out)
    }

    /// Edge ids of a given type.
    pub fn edges_of_type(&mut self, pager: &mut Pager, type_id: u32) -> Result<Vec<u64>> {
        let prefix = FragmentKey::prefix(FAMILY_EDGE_TYPE, type_id, 0);
        let mut out = Vec::new();
        for (key, fragment) in self.merged_prefix(pager, &prefix[..5])? {
            for bit in fragment.iter_bits() {
                out.push((u64::from(key.band0) << 16) | u64::from(bit));
            }
        }
        Ok(out)
    }

    /// Whether an edge `(u -> v)` of `type_id` exists, tested by intersecting
    /// the out-row of `u` with the in-row of `v` in each column band.
    pub fn incidence_exists(
        &mut self,
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
    #[test]
    fn sparse_to_dense_transition_preserves_bits() {
        let mut fragment = Fragment::new();
        for bit in 0..4095u32 {
            fragment.set(bit as u16);
        }
        assert_eq!(fragment.cardinality(), 4095);
        assert_eq!(fragment.encode()[0], 0, "still sparse at the threshold");
        fragment.set(4095);
        assert_eq!(fragment.cardinality(), 4096);
        assert_eq!(fragment.encode()[0], 1, "dense past the threshold");
        assert!(fragment.test(0) && fragment.test(4094) && fragment.test(4095));
        assert!(!fragment.test(4096));
        let decoded = Fragment::decode(&fragment.encode()).expect("decode");
        assert_eq!(decoded.cardinality(), 4096);
        assert!(decoded.test(4095));
        // Clearing back below the threshold keeps working in dense form.
        fragment.clear(4095);
        assert_eq!(fragment.cardinality(), 4095);
        assert!(!fragment.test(4095));
    }

    #[test]
    fn mixed_representation_row_operations() {
        let mut sparse = Fragment::new();
        sparse.set(FragmentKey::incidence_bit(1, 3));
        let mut dense = Fragment::new();
        for bit in 0..5000u32 {
            dense.set(bit as u16);
        }
        // Row 0 of the dense fragment contains bit 3 (offset 3).
        assert!(dense.row_nonempty(0));
        assert!(sparse.row_and_nonempty(1, &dense, 0));
        assert!(!sparse.row_and_nonempty(2, &dense, 0));
        assert_eq!(dense.row_bits(0).count(), 256);
        assert_eq!(dense.row_bits(0).next(), Some(0));
        assert_eq!(sparse.row_bits(1).collect::<Vec<_>>(), vec![3]);
        // Row 19 still has bits (4864..=4999); row 20 is beyond them.
        assert!(dense.row_nonempty(19));
        assert!(!dense.row_nonempty(20));
    }
    #[test]
    fn write_back_cache_is_visible_and_flushed() {
        let mut pager = test_pager();
        pager.begin_write().expect("begin");
        let mut store = FragmentStore::create(&mut pager).expect("store");
        let key = FragmentKey::label(1, 5);
        store
            .set_bit(&mut pager, &key, FragmentKey::label_bit(5))
            .expect("set");
        // Visible before the flush, from the cache.
        assert_eq!(store.label_nodes(&mut pager, 1).expect("scan"), vec![5]);
        store.flush(&mut pager).expect("flush");
        pager.commit().expect("commit");
        // And after, from the tree.
        assert_eq!(store.label_nodes(&mut pager, 1).expect("scan"), vec![5]);
        assert!(store.get(&mut pager, &key).expect("get").is_some());
    }

    #[test]
    fn rollback_discards_cached_fragments() {
        let mut pager = test_pager();
        pager.begin_write().expect("begin");
        let mut store = FragmentStore::create(&mut pager).expect("store");
        store.flush(&mut pager).expect("flush");
        pager.commit().expect("commit");

        // A second transaction adds a bit and rolls back.
        pager.begin_write().expect("begin");
        let key = FragmentKey::label(1, 7);
        store
            .set_bit(&mut pager, &key, FragmentKey::label_bit(7))
            .expect("set");
        pager.rollback().expect("rollback");

        // The generation change invalidated the cached fragment.
        assert!(store.label_nodes(&mut pager, 1).expect("scan").is_empty());
        assert!(store.get(&mut pager, &key).expect("get").is_none());
    }

    #[test]
    fn cache_flushes_when_full_and_stays_consistent() {
        let mut pager = test_pager();
        pager.begin_write().expect("begin");
        let mut store = FragmentStore::create(&mut pager).expect("store");
        store.set_cache_capacity(2);
        for node in [1u64, 2, 3, 4, 5] {
            let key = FragmentKey::label(1, node);
            store
                .set_bit(&mut pager, &key, FragmentKey::label_bit(node))
                .expect("set");
        }
        // Mid-transaction the flushed entries are read back from the tree and
        // the rest from the cache.
        assert_eq!(
            store.label_nodes(&mut pager, 1).expect("scan"),
            vec![1, 2, 3, 4, 5]
        );
        store.flush(&mut pager).expect("flush");
        pager.commit().expect("commit");
        assert_eq!(
            store.label_nodes(&mut pager, 1).expect("scan"),
            vec![1, 2, 3, 4, 5]
        );
    }
}
