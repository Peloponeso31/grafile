//! B+tree: sorted key/value storage on top of the pager.
//!
//! Two tree kinds share one implementation: **table** trees use fixed 8-byte
//! big-endian `u64` keys (node, edge and catalog records), while **index**
//! trees use arbitrary byte-string keys compared with `memcmp` (property
//! indexes and the fragment store).
//!
//! Interior pages store divider cells `(child, key)` where `key` is an upper
//! bound of the child's subtree. Descent picks the first divider `>= target`,
//! or the rightmost child when no divider qualifies. Deletion removes leaf
//! cells without rebalancing: dividers may become stale (larger than the
//! subtree maximum) but remain valid routing hints, and empty pages are
//! tolerated. Rebalancing is planned as a later optimization.

pub mod cursor;
pub mod page;

use std::collections::HashSet;

use crate::error::{Error, Result};
use crate::pager::{PageHeader, PageNo, PageType, Pager};

use page::{
    build_interior_cell, build_leaf_cell, cell_child, cell_key, cell_pointer, cell_size,
    free_cell_overflow, insert_cell_bytes, raw_cell_key, read_all_cells, read_cell_value,
    rebuild_page, remove_cell_at,
};

pub use cursor::Cursor;

/// Which flavor of B+tree this is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TreeKind {
    /// Fixed 8-byte keys; values are records.
    Table,
    /// Arbitrary byte keys; values are payloads.
    Index,
}

impl TreeKind {
    fn leaf_type(self) -> PageType {
        match self {
            Self::Table => PageType::TableLeaf,
            Self::Index => PageType::IndexLeaf,
        }
    }

    fn interior_type(self) -> PageType {
        match self {
            Self::Table => PageType::TableInterior,
            Self::Index => PageType::IndexInterior,
        }
    }
}

/// A handle to a B+tree. Cheap to copy; the root page may change on splits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Btree {
    root: PageNo,
    kind: TreeKind,
}

/// Result of a child-page split, propagated upward during insertion.
struct Split {
    /// Upper bound (max key) of the left half that stayed in place.
    max_left: Vec<u8>,
    /// Newly allocated page holding the right half.
    right_page: PageNo,
}

impl Btree {
    /// Create an empty tree, allocating its root leaf page.
    pub fn create(pager: &mut Pager, kind: TreeKind) -> Result<Self> {
        if !pager.in_write_txn() {
            return Err(Error::misuse("Btree::create outside a write transaction"));
        }
        let root = pager.alloc_page()?;
        rebuild_page(pager, root, kind.leaf_type(), 0, &[])?;
        Ok(Self { root, kind })
    }

    /// Reopen an existing tree at `root`.
    pub fn from_root(root: PageNo, kind: TreeKind) -> Self {
        Self { root, kind }
    }

    /// Root page number.
    pub fn root(&self) -> PageNo {
        self.root
    }

    /// Tree flavor.
    pub fn kind(&self) -> TreeKind {
        self.kind
    }

    /// Insert `key`/`value`; duplicate keys are rejected.
    pub fn insert(&mut self, pager: &mut Pager, key: &[u8], value: &[u8]) -> Result<()> {
        if !pager.in_write_txn() {
            return Err(Error::misuse("Btree::insert outside a write transaction"));
        }
        match self.insert_rec(pager, self.root, key, value, false)? {
            None => Ok(()),
            Some(split) => {
                let cell = build_interior_cell(self.kind, self.root, &split.max_left)?;
                let new_root = pager.alloc_page()?;
                rebuild_page(
                    pager,
                    new_root,
                    self.kind.interior_type(),
                    split.right_page,
                    &[cell],
                )?;
                self.root = new_root;
                Ok(())
            }
        }
    }

    /// Insert `key`/`value`, replacing any existing value.
    ///
    /// Replacing costs a single descent: the old cell is removed and the new
    /// one written back into the same leaf (overflow chains are freed and
    /// rewritten as needed), falling back to a split only when the new cell
    /// does not fit.
    pub fn replace(&mut self, pager: &mut Pager, key: &[u8], value: &[u8]) -> Result<()> {
        if !pager.in_write_txn() {
            return Err(Error::misuse("Btree::replace outside a write transaction"));
        }
        match self.insert_rec(pager, self.root, key, value, true)? {
            None => Ok(()),
            Some(split) => {
                let cell = build_interior_cell(self.kind, self.root, &split.max_left)?;
                let new_root = pager.alloc_page()?;
                rebuild_page(
                    pager,
                    new_root,
                    self.kind.interior_type(),
                    split.right_page,
                    &[cell],
                )?;
                self.root = new_root;
                Ok(())
            }
        }
    }

    fn insert_rec(
        &self,
        pager: &mut Pager,
        page_no: PageNo,
        key: &[u8],
        value: &[u8],
        replace: bool,
    ) -> Result<Option<Split>> {
        let (page_type, rightmost) = {
            let page = pager.page(page_no)?;
            let header = page.header()?;
            (header.page_type, header.rightmost)
        };
        if page_type == self.kind.leaf_type() {
            let pos = leaf_position(pager, page_no, self.kind, key)?;
            let duplicate = leaf_key_at(pager, page_no, self.kind, pos)? == Some(key.to_vec());
            if duplicate && !replace {
                return Err(Error::constraint(format!(
                    "duplicate key {}",
                    hex_preview(key)
                )));
            }
            let cell = build_leaf_cell(pager, self.kind, key, value)?;
            if duplicate {
                // Free the old cell and its overflow chain, then reuse the
                // freed space in the same leaf.
                remove_cell_at(pager, page_no, self.kind, pos)?;
            }
            if insert_cell_bytes(pager, page_no, self.kind, pos, &cell)? {
                return Ok(None);
            }
            let mut cells = read_all_cells(pager, page_no, self.kind)?;
            cells.insert(pos, cell);
            let split_at = balance_point(&cells);
            let max_left = raw_cell_key(&cells[split_at - 1], self.kind, page_type)?;
            let right_page = pager.alloc_page()?;
            rebuild_page(pager, page_no, page_type, 0, &cells[..split_at])?;
            rebuild_page(pager, right_page, page_type, 0, &cells[split_at..])?;
            return Ok(Some(Split {
                max_left,
                right_page,
            }));
        }

        let (child, is_rightmost, child_idx, divider) = find_child(pager, page_no, self.kind, key)?;
        let Some(split) = self.insert_rec(pager, child, key, value, replace)? else {
            return Ok(None);
        };
        // Materialize the parent cells with the child update applied, then try
        // to rebuild in place. Rebuilding from a cell list keeps this simple
        // and correct even when a replacement cell changes size.
        let mut cells = read_all_cells(pager, page_no, self.kind)?;
        let (new_cell, new_rightmost) = if is_rightmost {
            let new_cell = build_interior_cell(self.kind, child, &split.max_left)?;
            cells.push(new_cell);
            (None, Some(split.right_page))
        } else {
            cells[child_idx] = build_interior_cell(self.kind, child, &split.max_left)?;
            let new_cell = build_interior_cell(self.kind, split.right_page, &divider)?;
            (Some((child_idx + 1, new_cell)), None)
        };
        let rightmost_now = new_rightmost.unwrap_or(rightmost);
        if let Some((at, cell)) = new_cell {
            cells.insert(at, cell);
        }
        if cells_fit(pager.page_size(), &cells) {
            rebuild_page(pager, page_no, page_type, rightmost_now, &cells)?;
            return Ok(None);
        }
        split_interior(pager, page_no, self.kind, &cells, rightmost_now)
    }

    /// Look up a key, returning its value.
    pub fn get(&self, pager: &mut Pager, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let leaf = descend_leaf(pager, self.root, self.kind, key)?;
        let pos = leaf_position(pager, leaf, self.kind, key)?;
        if leaf_key_at(pager, leaf, self.kind, pos)? != Some(key.to_vec()) {
            return Ok(None);
        }
        let ptr = {
            let page = pager.page(leaf)?;
            cell_pointer(page.data(), pos)?
        };
        Ok(Some(read_cell_value(pager, leaf, self.kind, ptr)?))
    }

    /// Whether `key` exists.
    pub fn contains(&self, pager: &mut Pager, key: &[u8]) -> Result<bool> {
        Ok(self.get(pager, key)?.is_some())
    }

    /// Delete `key`; returns whether it existed. Overflow chains are freed.
    pub fn delete(&mut self, pager: &mut Pager, key: &[u8]) -> Result<bool> {
        if !pager.in_write_txn() {
            return Err(Error::misuse("Btree::delete outside a write transaction"));
        }
        let leaf = descend_leaf(pager, self.root, self.kind, key)?;
        let pos = leaf_position(pager, leaf, self.kind, key)?;
        if leaf_key_at(pager, leaf, self.kind, pos)? != Some(key.to_vec()) {
            return Ok(false);
        }
        remove_cell_at(pager, leaf, self.kind, pos)?;
        maybe_defragment(pager, leaf, self.kind)?;
        Ok(true)
    }

    /// Create a cursor positioned before the first key.
    pub fn cursor(&self) -> Cursor {
        Cursor::new(self.root, self.kind)
    }

    /// Free every page of this tree (interior, leaf and overflow pages).
    /// The tree handle is unusable afterwards.
    pub fn destroy(&self, pager: &mut Pager) -> Result<()> {
        if !pager.in_write_txn() {
            return Err(Error::misuse("Btree::destroy outside a write transaction"));
        }
        let mut stack = vec![self.root];
        while let Some(page_no) = stack.pop() {
            let (page_type, cell_count, rightmost) = {
                let page = pager.page(page_no)?;
                let header = PageHeader::parse(page.data())?;
                (header.page_type, header.cell_count, header.rightmost)
            };
            if page_type == self.kind.interior_type() {
                let children = {
                    let page = pager.page(page_no)?;
                    let data = page.data();
                    let mut children = Vec::with_capacity(cell_count as usize + 1);
                    for idx in 0..cell_count as usize {
                        children.push(cell_child(data, cell_pointer(data, idx)?)?);
                    }
                    children
                };
                stack.extend(children);
                if rightmost != 0 {
                    stack.push(rightmost);
                }
            } else if page_type == self.kind.leaf_type() {
                let cells = {
                    let page = pager.page(page_no)?;
                    let data = page.data();
                    let mut cells = Vec::with_capacity(cell_count as usize);
                    for idx in 0..cell_count as usize {
                        cells.push(cell_pointer(data, idx)?);
                    }
                    cells
                };
                // Overflow chains are read through the page, so they must be
                // freed before the page joins the freelist.
                for ptr in cells {
                    free_cell_overflow(pager, page_no, self.kind, ptr)?;
                }
            } else {
                return Err(Error::corrupt(format!(
                    "tree page {page_no} has unexpected type {page_type:?}"
                )));
            }
            pager.free_page(page_no)?;
        }
        Ok(())
    }

    /// Walk the whole tree validating structure, ordering and dividers.
    pub fn integrity_check(&self, pager: &mut Pager) -> Result<CheckStats> {
        let mut visited = HashSet::new();
        let mut stats = CheckStats::default();
        let bounds = check_node(pager, self.root, self.kind, 0, &mut visited, &mut stats)?;
        stats.root_min = bounds.0;
        stats.root_max = bounds.1;
        Ok(stats)
    }
}

/// Statistics returned by [`Btree::integrity_check`].
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CheckStats {
    /// Number of B+tree pages reachable from the root.
    pub pages: u64,
    /// Number of leaf cells.
    pub keys: u64,
    /// Tree height (leaf level = 0).
    pub height: u32,
    /// Smallest key in the tree, if any.
    pub root_min: Option<Vec<u8>>,
    /// Largest key in the tree, if any.
    pub root_max: Option<Vec<u8>>,
}

fn hex_preview(key: &[u8]) -> String {
    let shown = key.len().min(16);
    let mut out = String::with_capacity(shown * 2);
    for byte in &key[..shown] {
        out.push_str(&format!("{byte:02x}"));
    }
    if key.len() > shown {
        out.push_str("..");
    }
    out
}

/// Split an interior page in place, returning the propagated split.
///
/// Left keeps cells `[0..m)`, with the child of cell `m` as rightmost. Right
/// gets cells `[m+1..)` and the page's rightmost child. The propagated upper
/// bound is the divider of cell `m`.
fn split_interior(
    pager: &mut Pager,
    page_no: PageNo,
    kind: TreeKind,
    cells: &[Vec<u8>],
    rightmost: PageNo,
) -> Result<Option<Split>> {
    let n = cells.len();
    if n < 2 {
        return Err(Error::internal("interior split with fewer than two cells"));
    }
    let m = n / 2;
    let max_left = raw_cell_key(&cells[m], kind, kind.interior_type())?;
    let left_rightmost = cell_child(&cells[m], 0)?;
    let right_page = pager.alloc_page()?;
    rebuild_page(
        pager,
        page_no,
        kind.interior_type(),
        left_rightmost,
        &cells[..m],
    )?;
    rebuild_page(
        pager,
        right_page,
        kind.interior_type(),
        rightmost,
        &cells[m + 1..],
    )?;
    Ok(Some(Split {
        max_left,
        right_page,
    }))
}

/// Whether a cell list fits on one page.
fn cells_fit(page_size: usize, cells: &[Vec<u8>]) -> bool {
    let content: usize = cells.iter().map(|cell| cell.len()).sum();
    crate::pager::header::PAGE_HEADER_SIZE + 2 * cells.len() + content <= page_size
}

/// Choose how many cells stay in the left page (byte-balanced).
fn balance_point(cells: &[Vec<u8>]) -> usize {
    let total: usize = cells.iter().map(|cell| cell.len()).sum();
    let mut accumulated = 0usize;
    for (idx, cell) in cells.iter().enumerate() {
        accumulated += cell.len();
        if accumulated * 2 >= total {
            return (idx + 1).min(cells.len().saturating_sub(1)).max(1);
        }
    }
    1
}

/// Position of the first cell with key `>= key`.
fn leaf_position(pager: &mut Pager, page_no: PageNo, kind: TreeKind, key: &[u8]) -> Result<usize> {
    let page = pager.page(page_no)?;
    let data = page.data();
    let header = PageHeader::parse(data)?;
    let count = header.cell_count as usize;
    let mut lo = 0usize;
    let mut hi = count;
    while lo < hi {
        let mid = (lo + hi) / 2;
        let ptr = cell_pointer(data, mid)?;
        if cell_key(data, kind, ptr)? < key {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    Ok(lo)
}

/// Key of the cell at `idx`, or `None` when past the end.
fn leaf_key_at(
    pager: &mut Pager,
    page_no: PageNo,
    kind: TreeKind,
    idx: usize,
) -> Result<Option<Vec<u8>>> {
    let page = pager.page(page_no)?;
    let data = page.data();
    let header = PageHeader::parse(data)?;
    if idx >= header.cell_count as usize {
        return Ok(None);
    }
    let ptr = cell_pointer(data, idx)?;
    Ok(Some(cell_key(data, kind, ptr)?.to_vec()))
}

/// Descend to the leaf that would contain `key`.
fn descend_leaf(pager: &mut Pager, root: PageNo, kind: TreeKind, key: &[u8]) -> Result<PageNo> {
    let mut page_no = root;
    loop {
        let (page_type, rightmost) = {
            let page = pager.page(page_no)?;
            let header = page.header()?;
            (header.page_type, header.rightmost)
        };
        if page_type == kind.leaf_type() {
            return Ok(page_no);
        }
        if page_type != kind.interior_type() {
            return Err(Error::corrupt(format!(
                "expected interior page, found {page_type:?} at page {page_no}"
            )));
        }
        let (child, is_rightmost, _, _) = find_child(pager, page_no, kind, key)?;
        page_no = if is_rightmost { rightmost } else { child };
    }
}

/// Find the child to descend into for `key`:
/// `(child, is_rightmost, cell_index, divider_key)`.
fn find_child(
    pager: &mut Pager,
    page_no: PageNo,
    kind: TreeKind,
    key: &[u8],
) -> Result<(PageNo, bool, usize, Vec<u8>)> {
    let page = pager.page(page_no)?;
    let data = page.data();
    let header = PageHeader::parse(data)?;
    let count = header.cell_count as usize;
    let mut lo = 0usize;
    let mut hi = count;
    while lo < hi {
        let mid = (lo + hi) / 2;
        let ptr = cell_pointer(data, mid)?;
        if cell_key(data, kind, ptr)? < key {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    if lo < count {
        let ptr = cell_pointer(data, lo)?;
        let child = cell_child(data, ptr)?;
        let divider = cell_key(data, kind, ptr)?.to_vec();
        Ok((child, false, lo, divider))
    } else {
        Ok((header.rightmost, true, count, Vec::new()))
    }
}

fn maybe_defragment(pager: &mut Pager, page_no: PageNo, kind: TreeKind) -> Result<()> {
    let (fragmented, page_size) = {
        let page = pager.page(page_no)?;
        let header = page.header()?;
        (header.fragmented_free as usize, pager.page_size())
    };
    if fragmented > page_size / 8 {
        page::defragment(pager, page_no, kind)?;
    }
    Ok(())
}

/// A recursive integrity-check result: `(min key, max key)` of the subtree.
type KeyBounds = (Option<Vec<u8>>, Option<Vec<u8>>);

fn check_node(
    pager: &mut Pager,
    page_no: PageNo,
    kind: TreeKind,
    depth: u32,
    visited: &mut HashSet<PageNo>,
    stats: &mut CheckStats,
) -> Result<KeyBounds> {
    if !visited.insert(page_no) {
        return Err(Error::corrupt(format!(
            "page {page_no} reachable more than once (cycle or shared child)"
        )));
    }
    stats.pages += 1;
    let page_size = pager.page_size();
    let page = pager.page(page_no)?;
    let data = page.data();
    let header = PageHeader::parse(data)?;
    if header.page_no != page_no {
        return Err(Error::corrupt(format!(
            "page {page_no} header claims page {}",
            header.page_no
        )));
    }
    let content_start = page::content_start_offset(header.content_start, page_size);
    let count = header.cell_count as usize;
    let pointer_end = crate::pager::header::PAGE_HEADER_SIZE + 2 * count;
    if content_start < pointer_end || content_start > page_size {
        return Err(Error::corrupt(format!(
            "page {page_no}: content start {content_start} outside cell area"
        )));
    }
    // Cell ranges: in bounds and non-overlapping.
    let mut ranges = Vec::with_capacity(count);
    for idx in 0..count {
        let ptr = cell_pointer(data, idx)?;
        let size = cell_size(data, kind, ptr)?;
        if ptr < content_start || ptr + size > page_size {
            return Err(Error::corrupt(format!(
                "page {page_no}: cell {idx} outside content area"
            )));
        }
        ranges.push((ptr, ptr + size));
    }
    let mut sorted = ranges.clone();
    sorted.sort_unstable();
    for pair in sorted.windows(2) {
        if pair[0].1 > pair[1].0 {
            return Err(Error::corrupt(format!("page {page_no}: overlapping cells")));
        }
    }

    match header.page_type {
        t if t == kind.leaf_type() => {
            let mut min = None;
            let mut max: Option<Vec<u8>> = None;
            for idx in 0..count {
                let ptr = cell_pointer(data, idx)?;
                let key = cell_key(data, kind, ptr)?.to_vec();
                if let Some(prev) = &max
                    && key.as_slice() <= prev.as_slice()
                {
                    return Err(Error::corrupt(format!(
                        "page {page_no}: keys out of order at cell {idx}"
                    )));
                }
                if min.is_none() {
                    min = Some(key.clone());
                }
                max = Some(key);
                stats.keys += 1;
            }
            stats.height = stats.height.max(depth);
            Ok((min, max))
        }
        t if t == kind.interior_type() => {
            if count == 0 && header.rightmost == 0 {
                return Err(Error::corrupt(format!(
                    "page {page_no}: empty interior page"
                )));
            }
            let mut prev_divider: Option<Vec<u8>> = None;
            let mut min: Option<Vec<u8>> = None;
            let mut max: Option<Vec<u8>> = None;
            let mut dividers: Vec<Vec<u8>> = Vec::with_capacity(count);
            let mut children: Vec<PageNo> = Vec::with_capacity(count + 1);
            for idx in 0..count {
                let ptr = cell_pointer(data, idx)?;
                let divider = cell_key(data, kind, ptr)?.to_vec();
                if let Some(prev) = &prev_divider
                    && divider.as_slice() <= prev.as_slice()
                {
                    return Err(Error::corrupt(format!(
                        "page {page_no}: dividers out of order at cell {idx}"
                    )));
                }
                prev_divider = Some(divider.clone());
                dividers.push(divider);
                children.push(cell_child(data, ptr)?);
            }
            if header.rightmost == 0 {
                return Err(Error::corrupt(format!(
                    "page {page_no}: interior page without rightmost child"
                )));
            }
            children.push(header.rightmost);
            for (idx, child) in children.iter().enumerate() {
                let (child_min, child_max) =
                    check_node(pager, *child, kind, depth + 1, visited, stats)?;
                if let Some(child_max) = &child_max
                    && idx < dividers.len()
                    && child_max.as_slice() > dividers[idx].as_slice()
                {
                    return Err(Error::corrupt(format!(
                        "page {page_no}: child {child} exceeds divider"
                    )));
                }
                if let Some(child_min) = &child_min {
                    if idx > 0 && child_min.as_slice() <= dividers[idx - 1].as_slice() {
                        return Err(Error::corrupt(format!(
                            "page {page_no}: child {child} below previous divider"
                        )));
                    }
                    if min.is_none() {
                        min = Some(child_min.clone());
                    }
                }
                if let Some(child_max) = child_max {
                    max = Some(child_max);
                }
            }
            stats.height = stats.height.max(depth);
            Ok((min, max))
        }
        other => Err(Error::corrupt(format!(
            "page {page_no}: unexpected page type {other:?} in B+tree"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::mem::MemVfs;
    use proptest::prelude::*;
    use std::collections::BTreeMap;
    use std::sync::Arc;

    fn test_pager() -> Pager {
        Pager::create(
            Arc::new(MemVfs::new()),
            std::path::Path::new("btree.db"),
            4096,
        )
        .expect("pager")
    }

    #[test]
    fn table_tree_crud_roundtrip() {
        let mut pager = test_pager();
        pager.begin_write().expect("begin");
        let mut tree = Btree::create(&mut pager, TreeKind::Table).expect("create");
        for key in 0..100u64 {
            tree.insert(&mut pager, &key.to_be_bytes(), format!("v{key}").as_bytes())
                .expect("insert");
        }
        for key in 0..100u64 {
            let value = tree.get(&mut pager, &key.to_be_bytes()).expect("get");
            assert_eq!(value, Some(format!("v{key}").into_bytes()));
        }
        for key in (0..100u64).step_by(3) {
            assert!(tree.delete(&mut pager, &key.to_be_bytes()).expect("delete"));
        }
        for key in (0..100u64).step_by(3) {
            assert_eq!(tree.get(&mut pager, &key.to_be_bytes()).expect("get"), None);
        }
        for key in (1..100u64).step_by(3) {
            assert!(
                tree.contains(&mut pager, &key.to_be_bytes())
                    .expect("contains")
            );
        }
        let stats = tree.integrity_check(&mut pager).expect("integrity");
        assert_eq!(stats.keys, 66);
        pager.commit().expect("commit");
    }

    #[test]
    fn duplicate_insert_is_rejected() {
        let mut pager = test_pager();
        pager.begin_write().expect("begin");
        let mut tree = Btree::create(&mut pager, TreeKind::Table).expect("create");
        tree.insert(&mut pager, &1u64.to_be_bytes(), b"a")
            .expect("insert");
        let err = tree
            .insert(&mut pager, &1u64.to_be_bytes(), b"b")
            .expect_err("duplicate");
        assert_eq!(err.code(), crate::error::ErrorCode::Constraint);
        pager.commit().expect("commit");
    }

    #[test]
    fn sequential_inserts_split_and_scan_in_order() {
        let mut pager = test_pager();
        pager.begin_write().expect("begin");
        let mut tree = Btree::create(&mut pager, TreeKind::Table).expect("create");
        const N: u64 = 3000;
        for key in 0..N {
            tree.insert(&mut pager, &key.to_be_bytes(), &(key as u32).to_le_bytes())
                .expect("insert");
        }
        let stats = tree.integrity_check(&mut pager).expect("integrity");
        assert_eq!(stats.keys, N);
        assert!(stats.height >= 1, "expected at least two levels");
        let mut cursor = tree.cursor();
        let mut count = 0u64;
        let mut ok = cursor.first(&mut pager).expect("first");
        while ok {
            let key = u64::from_be_bytes(cursor.key().try_into().expect("key"));
            assert_eq!(key, count);
            count += 1;
            ok = cursor.next(&mut pager).expect("next");
        }
        assert_eq!(count, N);
        // Reverse scan.
        let mut ok = cursor.last(&mut pager).expect("last");
        let mut expected = N;
        while ok {
            expected -= 1;
            let key = u64::from_be_bytes(cursor.key().try_into().expect("key"));
            assert_eq!(key, expected);
            ok = cursor.prev(&mut pager).expect("prev");
        }
        assert_eq!(expected, 0);
        pager.commit().expect("commit");
    }

    #[test]
    fn descending_inserts_are_valid_too() {
        let mut pager = test_pager();
        pager.begin_write().expect("begin");
        let mut tree = Btree::create(&mut pager, TreeKind::Table).expect("create");
        for key in (0..1500u64).rev() {
            tree.insert(&mut pager, &key.to_be_bytes(), b"x")
                .expect("insert");
        }
        let stats = tree.integrity_check(&mut pager).expect("integrity");
        assert_eq!(stats.keys, 1500);
        pager.commit().expect("commit");
    }

    #[test]
    fn cursor_seek_ge_and_le() {
        let mut pager = test_pager();
        pager.begin_write().expect("begin");
        let mut tree = Btree::create(&mut pager, TreeKind::Table).expect("create");
        for key in [10u64, 20, 30, 40, 50] {
            tree.insert(&mut pager, &key.to_be_bytes(), b"v")
                .expect("insert");
        }
        let mut cursor = tree.cursor();
        assert!(
            cursor
                .seek_ge(&mut pager, &25u64.to_be_bytes())
                .expect("seek")
        );
        assert_eq!(
            u64::from_be_bytes(cursor.key().try_into().expect("key")),
            30
        );
        assert!(
            cursor
                .seek_le(&mut pager, &25u64.to_be_bytes())
                .expect("seek")
        );
        assert_eq!(
            u64::from_be_bytes(cursor.key().try_into().expect("key")),
            20
        );
        assert!(
            !cursor
                .seek_ge(&mut pager, &51u64.to_be_bytes())
                .expect("seek")
        );
        assert!(
            !cursor
                .seek_le(&mut pager, &9u64.to_be_bytes())
                .expect("seek")
        );
        assert!(
            cursor
                .seek_le(&mut pager, &10u64.to_be_bytes())
                .expect("seek")
        );
        assert_eq!(
            u64::from_be_bytes(cursor.key().try_into().expect("key")),
            10
        );
        pager.commit().expect("commit");
    }

    #[test]
    fn overflow_values_roundtrip_and_free() {
        let mut pager = test_pager();
        pager.begin_write().expect("begin");
        let mut tree = Btree::create(&mut pager, TreeKind::Table).expect("create");
        let big = vec![0xA5u8; 30_000];
        tree.insert(&mut pager, &1u64.to_be_bytes(), &big)
            .expect("insert");
        tree.insert(&mut pager, &2u64.to_be_bytes(), b"small")
            .expect("insert");
        assert_eq!(
            tree.get(&mut pager, &1u64.to_be_bytes()).expect("get"),
            Some(big)
        );
        let pages_with_big = pager.header().page_count;
        tree.delete(&mut pager, &1u64.to_be_bytes())
            .expect("delete");
        tree.insert(&mut pager, &3u64.to_be_bytes(), b"small")
            .expect("insert");
        let pages_after = pager.header().page_count;
        assert!(
            pages_after <= pages_with_big,
            "overflow pages should be reused ({pages_after} > {pages_with_big})"
        );
        pager.commit().expect("commit");
    }

    #[test]
    fn destroy_frees_every_page_and_overflow() {
        let mut pager = test_pager();
        pager.begin_write().expect("begin");
        let mut tree = Btree::create(&mut pager, TreeKind::Index).expect("create");
        // Enough keys for interior pages and a value that overflows.
        for i in 0..200u32 {
            let key = format!("key{i:04}");
            tree.insert(&mut pager, key.as_bytes(), &[])
                .expect("insert");
        }
        let big = vec![7u8; 20_000];
        tree.insert(&mut pager, b"overflow", &big)
            .expect("insert big");
        let pages = pager.header().page_count;
        let before_free = pager.header().freelist_count;
        tree.destroy(&mut pager).expect("destroy");
        let freed = pager.header().freelist_count - before_free;
        assert!(
            freed >= 3,
            "expected interior, leaf and overflow pages, freed {freed}"
        );
        // Every freed page is reusable without growing the file.
        for _ in 0..freed {
            pager.alloc_page().expect("alloc");
        }
        assert_eq!(pager.header().page_count, pages);
        pager.commit().expect("commit");
    }

    #[test]
    fn index_tree_with_variable_keys() {
        let mut pager = test_pager();
        pager.begin_write().expect("begin");
        let mut tree = Btree::create(&mut pager, TreeKind::Index).expect("create");
        let mut model: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
        for i in 0..500u32 {
            let mut key = b"prefix/".to_vec();
            key.extend_from_slice(&i.to_be_bytes());
            let value = vec![i as u8; (i % 7) as usize];
            tree.insert(&mut pager, &key, &value).expect("insert");
            model.insert(key, value);
        }
        for (key, value) in &model {
            assert_eq!(tree.get(&mut pager, key).expect("get"), Some(value.clone()));
        }
        let mut cursor = tree.cursor();
        let mut seen = 0usize;
        let mut ok = cursor.first(&mut pager).expect("first");
        while ok {
            assert_eq!(cursor.value(&mut pager).expect("value").len(), seen % 7);
            seen += 1;
            ok = cursor.next(&mut pager).expect("next");
        }
        assert_eq!(seen, model.len());
        tree.integrity_check(&mut pager).expect("integrity");
        pager.commit().expect("commit");
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(48))]

        #[test]
        fn table_tree_matches_btreemap(
            ops in prop::collection::vec(
                (0u8..3, any::<u64>(), prop::collection::vec(any::<u8>(), 0..48)),
                1..400,
            ),
        ) {
            let mut pager = test_pager();
            pager.begin_write().expect("begin");
            let mut tree = Btree::create(&mut pager, TreeKind::Table).expect("create");
            let mut model: BTreeMap<u64, Vec<u8>> = BTreeMap::new();
            for (op, key, value) in ops {
                let key_bytes = key.to_be_bytes();
                match op % 3 {
                    0 => {
                        let result = tree.insert(&mut pager, &key_bytes, &value);
                        if let std::collections::btree_map::Entry::Vacant(entry) = model.entry(key) {
                            result.expect("insert");
                            entry.insert(value);
                        } else {
                            prop_assert!(result.is_err());
                        }
                    }
                    1 => {
                        let deleted = tree.delete(&mut pager, &key_bytes).expect("delete");
                        prop_assert_eq!(deleted, model.remove(&key).is_some());
                    }
                    _ => {
                        let got = tree.get(&mut pager, &key_bytes).expect("get");
                        prop_assert_eq!(got, model.get(&key).cloned());
                    }
                }
            }
            let mut cursor = tree.cursor();
            let mut found = Vec::new();
            let mut ok = cursor.first(&mut pager).expect("first");
            while ok {
                let key = u64::from_be_bytes(cursor.key().try_into().expect("key len"));
                let value = cursor.value(&mut pager).expect("value");
                found.push((key, value));
                ok = cursor.next(&mut pager).expect("next");
            }
            prop_assert_eq!(found, model.into_iter().collect::<Vec<_>>());
            let stats = tree.integrity_check(&mut pager).expect("integrity");
            prop_assert_eq!(stats.keys, cursor_count(&mut pager, &tree));
            pager.commit().expect("commit");
        }

        #[test]
        fn index_tree_matches_btreemap(
            ops in prop::collection::vec(
                (0u8..3, prop::collection::vec(any::<u8>(), 1..12), prop::collection::vec(any::<u8>(), 0..24)),
                1..300,
            ),
        ) {
            let mut pager = test_pager();
            pager.begin_write().expect("begin");
            let mut tree = Btree::create(&mut pager, TreeKind::Index).expect("create");
            let mut model: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
            for (op, key, value) in ops {
                match op % 3 {
                    0 => {
                        let result = tree.insert(&mut pager, &key, &value);
                        if let std::collections::btree_map::Entry::Vacant(entry) = model.entry(key) {
                            result.expect("insert");
                            entry.insert(value);
                        } else {
                            prop_assert!(result.is_err());
                        }
                    }
                    1 => {
                        let deleted = tree.delete(&mut pager, &key).expect("delete");
                        prop_assert_eq!(deleted, model.remove(&key).is_some());
                    }
                    _ => {
                        let got = tree.get(&mut pager, &key).expect("get");
                        prop_assert_eq!(got, model.get(&key).cloned());
                    }
                }
            }
            let mut cursor = tree.cursor();
            let mut found = Vec::new();
            let mut ok = cursor.first(&mut pager).expect("first");
            while ok {
                found.push((cursor.key().to_vec(), cursor.value(&mut pager).expect("value")));
                ok = cursor.next(&mut pager).expect("next");
            }
            prop_assert_eq!(found, model.into_iter().collect::<Vec<_>>());
            tree.integrity_check(&mut pager).expect("integrity");
            pager.commit().expect("commit");
        }
    }

    fn cursor_count(pager: &mut Pager, tree: &Btree) -> u64 {
        let mut cursor = tree.cursor();
        let mut count = 0u64;
        let mut ok = cursor.first(pager).expect("first");
        while ok {
            count += 1;
            ok = cursor.next(pager).expect("next");
        }
        count
    }
    #[test]
    fn replace_overwrites_and_inserts() {
        let mut pager = test_pager();
        pager.begin_write().expect("begin");
        let mut tree = Btree::create(&mut pager, TreeKind::Table).expect("create");
        tree.insert(&mut pager, &1u64.to_be_bytes(), b"one")
            .expect("insert");
        tree.insert(&mut pager, &2u64.to_be_bytes(), b"two")
            .expect("insert");
        // Replace an existing key with a longer value.
        tree.replace(&mut pager, &1u64.to_be_bytes(), b"one-longer")
            .expect("replace");
        assert_eq!(
            tree.get(&mut pager, &1u64.to_be_bytes()).expect("get"),
            Some(b"one-longer".to_vec())
        );
        // Replace with an overflowing value and back again.
        let big = vec![0x5A; 30_000];
        tree.replace(&mut pager, &1u64.to_be_bytes(), &big)
            .expect("replace big");
        assert_eq!(
            tree.get(&mut pager, &1u64.to_be_bytes()).expect("get"),
            Some(big)
        );
        tree.replace(&mut pager, &1u64.to_be_bytes(), b"small")
            .expect("replace small");
        assert_eq!(
            tree.get(&mut pager, &1u64.to_be_bytes()).expect("get"),
            Some(b"small".to_vec())
        );
        // Replace a missing key inserts it.
        tree.replace(&mut pager, &3u64.to_be_bytes(), b"three")
            .expect("replace missing");
        assert_eq!(
            tree.get(&mut pager, &3u64.to_be_bytes()).expect("get"),
            Some(b"three".to_vec())
        );
        // Other keys are untouched and the tree stays valid.
        assert_eq!(
            tree.get(&mut pager, &2u64.to_be_bytes()).expect("get"),
            Some(b"two".to_vec())
        );
        let stats = tree.integrity_check(&mut pager).expect("integrity");
        assert_eq!(stats.keys, 3);
        pager.commit().expect("commit");
    }

    #[test]
    fn replace_works_across_splits() {
        let mut pager = test_pager();
        pager.begin_write().expect("begin");
        let mut tree = Btree::create(&mut pager, TreeKind::Table).expect("create");
        for key in 0..3000u64 {
            tree.insert(&mut pager, &key.to_be_bytes(), b"x")
                .expect("insert");
        }
        // Rewrite every key with a differently sized value, exercising leaves
        // and interior routing.
        for key in (0..3000u64).step_by(2) {
            let value = vec![key as u8; 1 + (key as usize % 64)];
            tree.replace(&mut pager, &key.to_be_bytes(), &value)
                .expect("replace");
        }
        for key in 0..3000u64 {
            let expected: Vec<u8> = if key % 2 == 0 {
                vec![key as u8; 1 + (key as usize % 64)]
            } else {
                b"x".to_vec()
            };
            assert_eq!(
                tree.get(&mut pager, &key.to_be_bytes()).expect("get"),
                Some(expected)
            );
        }
        tree.integrity_check(&mut pager).expect("integrity");
        pager.commit().expect("commit");
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(24))]

        #[test]
        fn replace_matches_btreemap(
            ops in prop::collection::vec(
                (0u8..3, any::<u64>(), prop::collection::vec(any::<u8>(), 0..64)),
                1..300,
            ),
        ) {
            let mut pager = test_pager();
            pager.begin_write().expect("begin");
            let mut tree = Btree::create(&mut pager, TreeKind::Table).expect("create");
            let mut model: BTreeMap<u64, Vec<u8>> = BTreeMap::new();
            for (op, key, value) in ops {
                let key_bytes = key.to_be_bytes();
                match op % 3 {
                    0 => {
                        let result = tree.insert(&mut pager, &key_bytes, &value);
                        if let std::collections::btree_map::Entry::Vacant(entry) =
                            model.entry(key)
                        {
                            result.expect("insert");
                            entry.insert(value);
                        } else {
                            prop_assert!(result.is_err());
                        }
                    }
                    1 => {
                        let deleted = tree.delete(&mut pager, &key_bytes).expect("delete");
                        prop_assert_eq!(deleted, model.remove(&key).is_some());
                    }
                    _ => {
                        tree.replace(&mut pager, &key_bytes, &value).expect("replace");
                        model.insert(key, value);
                    }
                }
            }
            let mut cursor = tree.cursor();
            let mut found = Vec::new();
            let mut ok = cursor.first(&mut pager).expect("first");
            while ok {
                found.push((
                    u64::from_be_bytes(cursor.key().try_into().expect("key")),
                    cursor.value(&mut pager).expect("value"),
                ));
                ok = cursor.next(&mut pager).expect("next");
            }
            prop_assert_eq!(found, model.into_iter().collect::<Vec<_>>());
            tree.integrity_check(&mut pager).expect("integrity");
            pager.commit().expect("commit");
        }
    }
}
