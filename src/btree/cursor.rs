//! B+tree cursors: forward/backward iteration and range seeks.
//!
//! A cursor remembers its `(page, cell index)` position plus a copy of the
//! current key. Page transitions re-descend from the root (`find_gt`/`find_lt`),
//! which costs O(log n) once per page instead of once per row, and avoids
//! parent-pointer bookkeeping in the tree.

use super::TreeKind;
use super::page::{cell_child, cell_key, cell_pointer, read_cell_value};
use crate::error::{Error, Result};
use crate::pager::{PageHeader, PageNo, Pager};

/// A positioned iterator over a B+tree.
///
/// Mutating the tree while a cursor is live invalidates the cursor; re-seek
/// after writes (the same contract SQLite gives for its write cursors).
pub struct Cursor {
    root: PageNo,
    kind: TreeKind,
    page_no: PageNo,
    index: usize,
    key: Vec<u8>,
    valid: bool,
}

impl Cursor {
    pub(crate) fn new(root: PageNo, kind: TreeKind) -> Self {
        Self {
            root,
            kind,
            page_no: 0,
            index: 0,
            key: Vec::new(),
            valid: false,
        }
    }

    /// Whether the cursor currently points at a cell.
    pub fn is_valid(&self) -> bool {
        self.valid
    }

    /// Current key (empty when invalid).
    pub fn key(&self) -> &[u8] {
        &self.key
    }

    /// Current leaf page number (0 when invalid).
    pub fn page_no(&self) -> PageNo {
        self.page_no
    }

    /// Current cell index within the leaf page.
    pub fn index(&self) -> usize {
        self.index
    }

    /// Drop the position; iteration methods return `false` until re-seeked.
    pub fn invalidate(&mut self) {
        self.valid = false;
        self.key.clear();
    }

    /// Position on the smallest key.
    pub fn first(&mut self, pager: &mut Pager) -> Result<bool> {
        let found = find_first(pager, self.root, self.kind)?;
        self.position(pager, found)
    }

    /// Position on the largest key.
    pub fn last(&mut self, pager: &mut Pager) -> Result<bool> {
        let found = find_last(pager, self.root, self.kind)?;
        self.position(pager, found)
    }

    /// Position on the first key `>= key`.
    pub fn seek_ge(&mut self, pager: &mut Pager, key: &[u8]) -> Result<bool> {
        let found = find_ge(pager, self.root, self.kind, key)?;
        self.position(pager, found)
    }

    /// Position on the last key `<= key`.
    pub fn seek_le(&mut self, pager: &mut Pager, key: &[u8]) -> Result<bool> {
        let found = find_le(pager, self.root, self.kind, key)?;
        self.position(pager, found)
    }

    /// Advance to the next key.
    pub fn next(&mut self, pager: &mut Pager) -> Result<bool> {
        if !self.valid {
            return Ok(false);
        }
        let count = self.cell_count(pager)?;
        if self.index + 1 < count {
            let (page_no, index) = (self.page_no, self.index + 1);
            return self.position(pager, Some((page_no, index)));
        }
        let key = self.key.clone();
        let found = find_gt(pager, self.root, self.kind, &key)?;
        self.position(pager, found)
    }

    /// Step back to the previous key.
    pub fn prev(&mut self, pager: &mut Pager) -> Result<bool> {
        if !self.valid {
            return Ok(false);
        }
        if self.index > 0 {
            let (page_no, index) = (self.page_no, self.index - 1);
            return self.position(pager, Some((page_no, index)));
        }
        let key = self.key.clone();
        let found = find_lt(pager, self.root, self.kind, &key)?;
        self.position(pager, found)
    }

    /// Value at the current position (overflow chains resolved).
    pub fn value(&self, pager: &mut Pager) -> Result<Vec<u8>> {
        if !self.valid {
            return Err(Error::misuse("cursor is not positioned"));
        }
        let ptr = {
            let page = pager.page(self.page_no)?;
            cell_pointer(page.data(), self.index)?
        };
        read_cell_value(pager, self.page_no, self.kind, ptr)
    }

    fn cell_count(&self, pager: &mut Pager) -> Result<usize> {
        let page = pager.page(self.page_no)?;
        Ok(PageHeader::parse(page.data())?.cell_count as usize)
    }

    fn position(&mut self, pager: &mut Pager, found: Option<(PageNo, usize)>) -> Result<bool> {
        let Some((page_no, index)) = found else {
            self.invalidate();
            return Ok(false);
        };
        let key = {
            let page = pager.page(page_no)?;
            let data = page.data();
            let header = PageHeader::parse(data)?;
            if index >= header.cell_count as usize {
                None
            } else {
                let ptr = cell_pointer(data, index)?;
                Some(cell_key(data, self.kind, ptr)?.to_vec())
            }
        };
        match key {
            Some(key) => {
                self.page_no = page_no;
                self.index = index;
                self.key = key;
                self.valid = true;
                Ok(true)
            }
            None => {
                self.invalidate();
                Ok(false)
            }
        }
    }
}

/// Interior layout for searches: children and their upper-bound dividers.
struct Interior {
    rightmost: PageNo,
    children: Vec<PageNo>,
    dividers: Vec<Vec<u8>>,
}

/// Fetch a page and classify it; interior layouts gather children/dividers.
enum Node {
    Leaf { count: usize },
    Interior(Interior),
}

fn fetch_node(pager: &mut Pager, page_no: PageNo, kind: TreeKind) -> Result<Node> {
    let page = pager.page(page_no)?;
    let data = page.data();
    let header = PageHeader::parse(data)?;
    match header.page_type {
        t if t == kind.leaf_type() => Ok(Node::Leaf {
            count: header.cell_count as usize,
        }),
        t if t == kind.interior_type() => {
            let count = header.cell_count as usize;
            let mut children = Vec::with_capacity(count);
            let mut dividers = Vec::with_capacity(count);
            for idx in 0..count {
                let ptr = cell_pointer(data, idx)?;
                dividers.push(cell_key(data, kind, ptr)?.to_vec());
                children.push(cell_child(data, ptr)?);
            }
            Ok(Node::Interior(Interior {
                rightmost: header.rightmost,
                children,
                dividers,
            }))
        }
        other => Err(Error::corrupt(format!(
            "page {page_no}: unexpected page type {other:?}"
        ))),
    }
}

/// Binary search within a leaf: first index whose key satisfies `pred`
/// (keys are ascending, so the predicate must be monotone false-then-true).
fn leaf_bound<F>(pager: &mut Pager, page_no: PageNo, kind: TreeKind, pred: F) -> Result<usize>
where
    F: Fn(&[u8]) -> bool,
{
    let page = pager.page(page_no)?;
    let data = page.data();
    let header = PageHeader::parse(data)?;
    let count = header.cell_count as usize;
    let mut lo = 0usize;
    let mut hi = count;
    while lo < hi {
        let mid = (lo + hi) / 2;
        let ptr = cell_pointer(data, mid)?;
        if pred(cell_key(data, kind, ptr)?) {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }
    Ok(lo)
}

fn find_first(
    pager: &mut Pager,
    page_no: PageNo,
    kind: TreeKind,
) -> Result<Option<(PageNo, usize)>> {
    match fetch_node(pager, page_no, kind)? {
        Node::Leaf { count } => Ok(if count > 0 { Some((page_no, 0)) } else { None }),
        Node::Interior(interior) => {
            for child in &interior.children {
                if let Some(found) = find_first(pager, *child, kind)? {
                    return Ok(Some(found));
                }
            }
            find_first(pager, interior.rightmost, kind)
        }
    }
}

fn find_last(
    pager: &mut Pager,
    page_no: PageNo,
    kind: TreeKind,
) -> Result<Option<(PageNo, usize)>> {
    match fetch_node(pager, page_no, kind)? {
        Node::Leaf { count } => Ok(if count > 0 {
            Some((page_no, count - 1))
        } else {
            None
        }),
        Node::Interior(interior) => {
            if let Some(found) = find_last(pager, interior.rightmost, kind)? {
                return Ok(Some(found));
            }
            for child in interior.children.iter().rev() {
                if let Some(found) = find_last(pager, *child, kind)? {
                    return Ok(Some(found));
                }
            }
            Ok(None)
        }
    }
}

fn find_ge(
    pager: &mut Pager,
    page_no: PageNo,
    kind: TreeKind,
    target: &[u8],
) -> Result<Option<(PageNo, usize)>> {
    match fetch_node(pager, page_no, kind)? {
        Node::Leaf { count } => {
            let pos = leaf_bound(pager, page_no, kind, |key| key >= target)?;
            Ok(if pos < count {
                Some((page_no, pos))
            } else {
                None
            })
        }
        Node::Interior(interior) => {
            let start = interior.dividers.partition_point(|d| d.as_slice() < target);
            for child in interior.children.iter().skip(start) {
                if let Some(found) = find_ge(pager, *child, kind, target)? {
                    return Ok(Some(found));
                }
            }
            find_ge(pager, interior.rightmost, kind, target)
        }
    }
}

fn find_gt(
    pager: &mut Pager,
    page_no: PageNo,
    kind: TreeKind,
    target: &[u8],
) -> Result<Option<(PageNo, usize)>> {
    match fetch_node(pager, page_no, kind)? {
        Node::Leaf { count } => {
            let pos = leaf_bound(pager, page_no, kind, |key| key > target)?;
            Ok(if pos < count {
                Some((page_no, pos))
            } else {
                None
            })
        }
        Node::Interior(interior) => {
            let start = interior.dividers.partition_point(|d| d.as_slice() < target);
            for child in interior.children.iter().skip(start) {
                if let Some(found) = find_gt(pager, *child, kind, target)? {
                    return Ok(Some(found));
                }
            }
            find_gt(pager, interior.rightmost, kind, target)
        }
    }
}

fn find_lt(
    pager: &mut Pager,
    page_no: PageNo,
    kind: TreeKind,
    target: &[u8],
) -> Result<Option<(PageNo, usize)>> {
    match fetch_node(pager, page_no, kind)? {
        Node::Leaf { count } => {
            let pos = leaf_bound(pager, page_no, kind, |key| key >= target)?;
            Ok(if pos > 0 && count > 0 {
                Some((page_no, pos - 1))
            } else {
                None
            })
        }
        Node::Interior(interior) => {
            let start = interior.dividers.partition_point(|d| d.as_slice() < target);
            if start == interior.children.len()
                && let Some(found) = find_lt(pager, interior.rightmost, kind, target)?
            {
                return Ok(Some(found));
            }
            for child in interior.children.iter().take(start).rev() {
                if let Some(found) = find_lt(pager, *child, kind, target)? {
                    return Ok(Some(found));
                }
            }
            Ok(None)
        }
    }
}

fn find_le(
    pager: &mut Pager,
    page_no: PageNo,
    kind: TreeKind,
    target: &[u8],
) -> Result<Option<(PageNo, usize)>> {
    match fetch_node(pager, page_no, kind)? {
        Node::Leaf { count } => {
            let pos = leaf_bound(pager, page_no, kind, |key| key > target)?;
            Ok(if pos > 0 && count > 0 {
                Some((page_no, pos - 1))
            } else {
                None
            })
        }
        Node::Interior(interior) => {
            let start = interior.dividers.partition_point(|d| d.as_slice() < target);
            if start == interior.children.len()
                && let Some(found) = find_le(pager, interior.rightmost, kind, target)?
            {
                return Ok(Some(found));
            }
            for child in interior.children.iter().take(start + 1).rev() {
                if let Some(found) = find_le(pager, *child, kind, target)? {
                    return Ok(Some(found));
                }
            }
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use super::*;
    use crate::btree::Btree;
    use crate::vfs::mem::MemVfs;

    fn test_pager() -> Pager {
        Pager::create(
            Arc::new(MemVfs::new()),
            std::path::Path::new("cursor.db"),
            4096,
        )
        .expect("pager")
    }

    fn key_of(cursor: &Cursor) -> u64 {
        u64::from_be_bytes(cursor.key().try_into().expect("key"))
    }

    /// Insert `keys` with 200-byte values, forcing several leaf pages.
    fn wide_model(
        pager: &mut Pager,
        keys: impl IntoIterator<Item = u64>,
    ) -> (Btree, BTreeMap<u64, Vec<u8>>) {
        let mut tree = Btree::create(pager, TreeKind::Table).expect("create");
        let mut model = BTreeMap::new();
        for key in keys {
            let value = vec![(key % 251) as u8; 200];
            tree.insert(pager, &key.to_be_bytes(), &value)
                .expect("insert");
            model.insert(key, value);
        }
        (tree, model)
    }

    #[test]
    fn empty_tree_cursor_reports_no_position() {
        let mut pager = test_pager();
        pager.begin_write().expect("begin");
        let tree = Btree::create(&mut pager, TreeKind::Table).expect("create");
        let mut cursor = tree.cursor();
        assert!(!cursor.first(&mut pager).expect("first"));
        assert!(!cursor.is_valid());
        assert!(cursor.key().is_empty());
        assert!(!cursor.last(&mut pager).expect("last"));
        assert!(!cursor.seek_ge(&mut pager, &1u64.to_be_bytes()).expect("ge"));
        assert!(!cursor.seek_le(&mut pager, &1u64.to_be_bytes()).expect("le"));
        assert!(!cursor.next(&mut pager).expect("next"));
        assert!(!cursor.prev(&mut pager).expect("prev"));
        assert!(cursor.value(&mut pager).is_err());
        pager.commit().expect("commit");
    }

    #[test]
    fn invalidate_clears_position_and_stops_iteration() {
        let mut pager = test_pager();
        pager.begin_write().expect("begin");
        let (tree, _) = wide_model(&mut pager, [1, 2, 3]);
        let mut cursor = tree.cursor();
        assert!(cursor.first(&mut pager).expect("first"));
        assert!(cursor.is_valid());
        cursor.invalidate();
        assert!(!cursor.is_valid());
        assert!(cursor.key().is_empty());
        assert!(!cursor.next(&mut pager).expect("next"));
        assert!(!cursor.prev(&mut pager).expect("prev"));
        assert!(cursor.first(&mut pager).expect("re-seek"));
        assert!(cursor.is_valid());
        pager.commit().expect("commit");
    }

    /// Seek sweeps must agree with a `BTreeMap` model across leaf pages,
    /// including targets greater than every divider of an interior node.
    #[test]
    fn seeks_match_the_model_across_pages() {
        let mut pager = test_pager();
        pager.begin_write().expect("begin");
        let (tree, model) = wide_model(&mut pager, (0..500u64).map(|i| i * 7));
        for target in (0..3500u64).step_by(13) {
            let mut cursor = tree.cursor();
            let expected_ge = model.range(target..).next().map(|(key, _)| *key);
            let found = cursor
                .seek_ge(&mut pager, &target.to_be_bytes())
                .expect("seek_ge");
            assert_eq!(found, expected_ge.is_some(), "seek_ge({target})");
            if let Some(key) = expected_ge {
                assert_eq!(key_of(&cursor), key, "seek_ge({target})");
                assert_eq!(
                    cursor.value(&mut pager).expect("value"),
                    model[&key],
                    "seek_ge({target}) value"
                );
            }

            let expected_le = model.range(..=target).next_back().map(|(key, _)| *key);
            let found = cursor
                .seek_le(&mut pager, &target.to_be_bytes())
                .expect("seek_le");
            assert_eq!(found, expected_le.is_some(), "seek_le({target})");
            if let Some(key) = expected_le {
                assert_eq!(key_of(&cursor), key, "seek_le({target})");
            }
        }
        pager.commit().expect("commit");
    }

    #[test]
    fn reverse_scan_visits_every_key_across_pages() {
        let mut pager = test_pager();
        pager.begin_write().expect("begin");
        let (tree, model) = wide_model(&mut pager, (0..300u64).map(|i| i * 3));
        let mut cursor = tree.cursor();
        let mut seen = Vec::with_capacity(model.len());
        let mut ok = cursor.last(&mut pager).expect("last");
        while ok {
            seen.push(key_of(&cursor));
            ok = cursor.prev(&mut pager).expect("prev");
        }
        let expected: Vec<u64> = model.keys().rev().copied().collect();
        assert_eq!(seen, expected);
        pager.commit().expect("commit");
    }

    #[test]
    fn cursor_value_resolves_overflow_chains() {
        let mut pager = test_pager();
        pager.begin_write().expect("begin");
        let mut tree = Btree::create(&mut pager, TreeKind::Table).expect("create");
        let big = vec![0x5Au8; 30_000];
        tree.insert(&mut pager, &1u64.to_be_bytes(), &big)
            .expect("insert");
        tree.insert(&mut pager, &2u64.to_be_bytes(), b"small")
            .expect("insert");
        let mut cursor = tree.cursor();
        assert!(cursor.first(&mut pager).expect("first"));
        assert_eq!(cursor.value(&mut pager).expect("value"), big);
        assert!(cursor.next(&mut pager).expect("next"));
        assert_eq!(cursor.value(&mut pager).expect("value"), b"small");
        pager.commit().expect("commit");
    }
}
