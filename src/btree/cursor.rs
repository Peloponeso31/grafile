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
            find_first(pager, interior.rightmost, kind)
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
