//! Slotted-page mechanics: cell codecs, insertion/removal, defragmentation,
//! overflow chains.
//!
//! Cell layouts are specified in `docs/format.md`. All keys are compared with
//! `memcmp`; table trees use fixed 8-byte big-endian `u64` keys so byte order
//! equals numeric order.

use crate::error::{Error, Result};
use crate::pager::{PAGE_HEADER_SIZE, PageHeader, PageNo, PageType, Pager};

use super::TreeKind;

/// Bit 31 of a payload length marks an overflow record.
pub(crate) const OVERFLOW_FLAG: u32 = 0x8000_0000;
/// Bytes of overflow-page overhead (page header + next pointer).
pub(crate) const OVERFLOW_OVERHEAD: usize = PAGE_HEADER_SIZE + 4;
/// Length of a fixed table-tree key.
pub(crate) const TABLE_KEY_LEN: usize = 8;

/// Convert a stored `content_start` value into a byte offset.
///
/// A stored zero means `page_size` (65536 does not fit in `u16`).
#[inline]
pub(crate) fn content_start_offset(stored: u16, page_size: usize) -> usize {
    if stored == 0 {
        page_size
    } else {
        stored as usize
    }
}

/// Convert a byte offset into the stored `content_start` representation.
#[inline]
pub(crate) fn content_start_stored(offset: usize) -> u16 {
    if offset >= 65536 { 0 } else { offset as u16 }
}

/// Read the cell pointer at `idx`.
pub(crate) fn cell_pointer(data: &[u8], idx: usize) -> Result<usize> {
    let offset = PAGE_HEADER_SIZE + idx * 2;
    if offset + 2 > data.len() {
        return Err(Error::corrupt("cell pointer out of page bounds"));
    }
    Ok(u16::from_le_bytes([data[offset], data[offset + 1]]) as usize)
}

/// Size in bytes of the cell at `ptr`, depending on tree kind.
pub(crate) fn cell_size(data: &[u8], kind: TreeKind, ptr: usize) -> Result<usize> {
    let page_header = PageHeader::parse(data)?;
    let size = match (kind, page_header.page_type) {
        (TreeKind::Table, PageType::TableInterior) => 12,
        (TreeKind::Table, PageType::TableLeaf) => {
            let len_offset = ptr + TABLE_KEY_LEN;
            let len = read_u32(data, len_offset)?;
            if len & OVERFLOW_FLAG != 0 {
                let local = read_u16(data, len_offset + 4)? as usize;
                TABLE_KEY_LEN + 4 + 2 + 4 + local
            } else {
                TABLE_KEY_LEN + 4 + len as usize
            }
        }
        (TreeKind::Index, PageType::IndexInterior) => {
            let key_len = read_u16(data, ptr + 4)? as usize;
            4 + 2 + key_len
        }
        (TreeKind::Index, PageType::IndexLeaf) => {
            let key_len = read_u16(data, ptr)? as usize;
            let value_base = ptr + 2 + key_len;
            let len = read_u32(data, value_base)?;
            if len & OVERFLOW_FLAG != 0 {
                let local = read_u16(data, value_base + 4)? as usize;
                2 + key_len + 4 + 2 + 4 + local
            } else {
                2 + key_len + 4 + len as usize
            }
        }
        (kind, page_type) => {
            return Err(Error::corrupt(format!(
                "cell_size mismatch: {kind:?} cell on {page_type:?} page"
            )));
        }
    };
    if ptr + size > data.len() {
        return Err(Error::corrupt("cell extends past end of page"));
    }
    Ok(size)
}

fn read_u16(data: &[u8], offset: usize) -> Result<u16> {
    let bytes = data
        .get(offset..offset + 2)
        .ok_or_else(|| Error::corrupt("cell field out of page bounds"))?;
    Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
}

pub(crate) fn read_u32(data: &[u8], offset: usize) -> Result<u32> {
    let bytes = data
        .get(offset..offset + 4)
        .ok_or_else(|| Error::corrupt("cell field out of page bounds"))?;
    Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

/// Copy every cell of a page in key order.
pub(crate) fn read_all_cells(
    pager: &mut Pager,
    page_no: PageNo,
    kind: TreeKind,
) -> Result<Vec<Vec<u8>>> {
    let page = pager.page(page_no)?;
    let data = page.data();
    let header = PageHeader::parse(data)?;
    let mut cells = Vec::with_capacity(header.cell_count as usize);
    for idx in 0..header.cell_count as usize {
        let ptr = cell_pointer(data, idx)?;
        let size = cell_size(data, kind, ptr)?;
        cells.push(data[ptr..ptr + size].to_vec());
    }
    Ok(cells)
}

/// Key bytes of an interior or leaf cell.
pub(crate) fn cell_key(data: &[u8], kind: TreeKind, ptr: usize) -> Result<&[u8]> {
    let page_header = PageHeader::parse(data)?;
    match (kind, page_header.page_type) {
        (TreeKind::Table, PageType::TableInterior) => data
            .get(ptr + 4..ptr + 4 + TABLE_KEY_LEN)
            .ok_or_else(|| Error::corrupt("table key out of page bounds")),
        (TreeKind::Table, PageType::TableLeaf) => data
            .get(ptr..ptr + TABLE_KEY_LEN)
            .ok_or_else(|| Error::corrupt("table key out of page bounds")),
        (TreeKind::Index, PageType::IndexInterior) => {
            let key_len = read_u16(data, ptr + 4)? as usize;
            data.get(ptr + 6..ptr + 6 + key_len)
                .ok_or_else(|| Error::corrupt("index key out of page bounds"))
        }
        (TreeKind::Index, PageType::IndexLeaf) => {
            let key_len = read_u16(data, ptr)? as usize;
            data.get(ptr + 2..ptr + 2 + key_len)
                .ok_or_else(|| Error::corrupt("index key out of page bounds"))
        }
        (kind, page_type) => Err(Error::corrupt(format!(
            "cell_key mismatch: {kind:?} cell on {page_type:?} page"
        ))),
    }
}

/// Child page number of an interior cell.
pub(crate) fn cell_child(data: &[u8], ptr: usize) -> Result<PageNo> {
    read_u32(data, ptr)
}

/// Key of a raw cell buffer (used during splits and rebuilds).
pub(crate) fn raw_cell_key(cell: &[u8], kind: TreeKind, page_type: PageType) -> Result<Vec<u8>> {
    match (kind, page_type) {
        (TreeKind::Table, PageType::TableInterior) => Ok(cell[4..4 + TABLE_KEY_LEN].to_vec()),
        (TreeKind::Table, PageType::TableLeaf) => Ok(cell[..TABLE_KEY_LEN].to_vec()),
        (TreeKind::Index, PageType::IndexInterior) => {
            let key_len = u16::from_le_bytes([cell[4], cell[5]]) as usize;
            Ok(cell[6..6 + key_len].to_vec())
        }
        (TreeKind::Index, PageType::IndexLeaf) => {
            let key_len = u16::from_le_bytes([cell[0], cell[1]]) as usize;
            Ok(cell[2..2 + key_len].to_vec())
        }
        (kind, page_type) => Err(Error::internal(format!(
            "raw_cell_key mismatch: {kind:?} cell on {page_type:?} page"
        ))),
    }
}

/// Read the value payload of a leaf cell, following overflow chains.
pub(crate) fn read_cell_value(
    pager: &mut Pager,
    page_no: PageNo,
    kind: TreeKind,
    ptr: usize,
) -> Result<Vec<u8>> {
    let (local, total, overflow) = {
        let page = pager.page(page_no)?;
        let data = page.data();
        let page_type = PageHeader::parse(data)?.page_type;
        let value_base = match (kind, page_type) {
            (TreeKind::Table, PageType::TableLeaf) => ptr + TABLE_KEY_LEN,
            (TreeKind::Index, PageType::IndexLeaf) => {
                let key_len = read_u16(data, ptr)? as usize;
                ptr + 2 + key_len
            }
            _ => {
                return Err(Error::internal("read_cell_value called on a non-leaf cell"));
            }
        };
        let len = read_u32(data, value_base)?;
        if len & OVERFLOW_FLAG != 0 {
            let local_len = read_u16(data, value_base + 4)? as usize;
            let overflow = read_u32(data, value_base + 6)?;
            let start = value_base + 10;
            (
                data[start..start + local_len].to_vec(),
                (len & !OVERFLOW_FLAG) as usize,
                Some(overflow),
            )
        } else {
            let start = value_base + 4;
            let len = len as usize;
            (data[start..start + len].to_vec(), len, None)
        }
    };
    match overflow {
        None => Ok(local),
        Some(first) => {
            let mut out = local;
            let mut page_no = first;
            while out.len() < total {
                if page_no == 0 {
                    return Err(Error::corrupt("overflow chain ended early"));
                }
                let page = pager.page(page_no)?;
                let data = page.data();
                let header = PageHeader::parse(data)?;
                if header.page_type != PageType::Overflow {
                    return Err(Error::corrupt(format!(
                        "overflow chain reached non-overflow page {page_no}"
                    )));
                }
                let next = read_u32(data, PAGE_HEADER_SIZE)?;
                let capacity = data.len() - OVERFLOW_OVERHEAD;
                let remaining = total - out.len();
                let take = remaining.min(capacity);
                out.extend_from_slice(&data[OVERFLOW_OVERHEAD..OVERFLOW_OVERHEAD + take]);
                page_no = next;
            }
            Ok(out)
        }
    }
}

/// Build a leaf cell for `key`/`value`, writing an overflow chain if needed.
pub(crate) fn build_leaf_cell(
    pager: &mut Pager,
    kind: TreeKind,
    key: &[u8],
    value: &[u8],
) -> Result<Vec<u8>> {
    let page_size = pager.page_size();
    let max_local = max_local_payload(page_size);
    if kind == TreeKind::Table && key.len() != TABLE_KEY_LEN {
        return Err(Error::internal("table keys must be exactly 8 bytes"));
    }
    if key.len() > max_local {
        return Err(Error::new(
            crate::error::ErrorCode::Range,
            format!("key of {} bytes is too large for this page size", key.len()),
        ));
    }
    let mut cell = Vec::with_capacity(key.len() + value.len() + 18);
    match kind {
        TreeKind::Table => cell.extend_from_slice(key),
        TreeKind::Index => {
            cell.extend_from_slice(&(key.len() as u16).to_le_bytes());
            cell.extend_from_slice(key);
        }
    }
    if value.len() <= max_local {
        cell.extend_from_slice(&(value.len() as u32).to_le_bytes());
        cell.extend_from_slice(value);
    } else {
        let local = max_local;
        let first_overflow = write_overflow_chain(pager, &value[local..])?;
        cell.extend_from_slice(&((value.len() as u32) | OVERFLOW_FLAG).to_le_bytes());
        cell.extend_from_slice(&(local as u16).to_le_bytes());
        cell.extend_from_slice(&first_overflow.to_le_bytes());
        cell.extend_from_slice(&value[..local]);
    }
    Ok(cell)
}

/// Build an interior cell pointing at `child` with divider `key`.
pub(crate) fn build_interior_cell(kind: TreeKind, child: PageNo, key: &[u8]) -> Result<Vec<u8>> {
    let mut cell = Vec::with_capacity(key.len() + 10);
    cell.extend_from_slice(&child.to_le_bytes());
    match kind {
        TreeKind::Table => {
            if key.len() != TABLE_KEY_LEN {
                return Err(Error::internal("table divider keys must be 8 bytes"));
            }
            cell.extend_from_slice(key);
        }
        TreeKind::Index => {
            cell.extend_from_slice(&(key.len() as u16).to_le_bytes());
            cell.extend_from_slice(key);
        }
    }
    Ok(cell)
}

/// Maximum bytes of a payload stored inline in a cell.
pub(crate) fn max_local_payload(page_size: usize) -> usize {
    (page_size - PAGE_HEADER_SIZE) / 4
}

/// Write `bytes` as an overflow chain; returns the first page number.
pub(crate) fn write_overflow_chain(pager: &mut Pager, bytes: &[u8]) -> Result<PageNo> {
    let page_size = pager.page_size();
    let capacity = page_size - OVERFLOW_OVERHEAD;
    if bytes.is_empty() {
        return Err(Error::internal("empty overflow chain requested"));
    }
    let chunks: Vec<&[u8]> = bytes.chunks(capacity).collect();
    let mut pages = Vec::with_capacity(chunks.len());
    for _ in &chunks {
        pages.push(pager.alloc_page()?);
    }
    for (i, chunk) in chunks.iter().enumerate() {
        let next = pages.get(i + 1).copied().unwrap_or(0);
        let page_no = pages[i];
        let data = pager.page_mut(page_no)?;
        data.fill(0);
        let header = PageHeader {
            page_type: PageType::Overflow,
            flags: 0,
            cell_count: 0,
            content_start: content_start_stored(page_size),
            fragmented_free: 0,
            page_no,
            rightmost: 0,
        };
        header.write_to(data);
        data[PAGE_HEADER_SIZE..PAGE_HEADER_SIZE + 4].copy_from_slice(&next.to_le_bytes());
        data[OVERFLOW_OVERHEAD..OVERFLOW_OVERHEAD + chunk.len()].copy_from_slice(chunk);
    }
    Ok(pages[0])
}

/// Free every page of an overflow chain.
pub(crate) fn free_overflow_chain(pager: &mut Pager, first: PageNo) -> Result<()> {
    let mut page_no = first;
    while page_no != 0 {
        let next = {
            let page = pager.page(page_no)?;
            let data = page.data();
            let header = PageHeader::parse(data)?;
            if header.page_type != PageType::Overflow {
                return Err(Error::corrupt(format!(
                    "overflow chain reached non-overflow page {page_no}"
                )));
            }
            read_u32(data, PAGE_HEADER_SIZE)?
        };
        pager.free_page(page_no)?;
        page_no = next;
    }
    Ok(())
}

/// Free the overflow chain referenced by a leaf cell, if any.
pub(crate) fn free_cell_overflow(
    pager: &mut Pager,
    page_no: PageNo,
    kind: TreeKind,
    ptr: usize,
) -> Result<()> {
    let first = {
        let page = pager.page(page_no)?;
        let data = page.data();
        let page_type = PageHeader::parse(data)?.page_type;
        let value_base = match (kind, page_type) {
            (TreeKind::Table, PageType::TableLeaf) => ptr + TABLE_KEY_LEN,
            (TreeKind::Index, PageType::IndexLeaf) => {
                let key_len = read_u16(data, ptr)? as usize;
                ptr + 2 + key_len
            }
            _ => return Err(Error::internal("free_cell_overflow on non-leaf cell")),
        };
        let len = read_u32(data, value_base)?;
        if len & OVERFLOW_FLAG != 0 {
            read_u32(data, value_base + 6)?
        } else {
            0
        }
    };
    if first != 0 {
        free_overflow_chain(pager, first)?;
    }
    Ok(())
}

/// Try to insert a raw cell at pointer position `insert_idx`.
///
/// Returns `false` when the page has no room even after defragmentation; the
/// caller then splits the page.
pub(crate) fn insert_cell_bytes(
    pager: &mut Pager,
    page_no: PageNo,
    kind: TreeKind,
    insert_idx: usize,
    cell: &[u8],
) -> Result<bool> {
    if try_place_cell(pager, page_no, insert_idx, cell)? {
        return Ok(true);
    }
    defragment(pager, page_no, kind)?;
    try_place_cell(pager, page_no, insert_idx, cell)
}

fn try_place_cell(
    pager: &mut Pager,
    page_no: PageNo,
    insert_idx: usize,
    cell: &[u8],
) -> Result<bool> {
    let page_size = pager.page_size();
    let data = pager.page_mut(page_no)?;
    let mut header = PageHeader::parse(data)?;
    let count = header.cell_count as usize;
    let content_start = content_start_offset(header.content_start, page_size);
    let used = PAGE_HEADER_SIZE + 2 * count;
    if content_start < used {
        return Err(Error::corrupt("cell area overlaps pointer array"));
    }
    if content_start - used < cell.len() + 2 {
        return Ok(false);
    }
    let new_start = content_start - cell.len();
    data[new_start..new_start + cell.len()].copy_from_slice(cell);
    let src = PAGE_HEADER_SIZE + 2 * insert_idx;
    let end = PAGE_HEADER_SIZE + 2 * count;
    data.copy_within(src..end, src + 2);
    data[src..src + 2].copy_from_slice(&(new_start as u16).to_le_bytes());
    header.cell_count += 1;
    header.content_start = content_start_stored(new_start);
    header.write_to(data);
    Ok(true)
}

/// Remove the cell at pointer position `idx`, freeing its overflow chain.
pub(crate) fn remove_cell_at(
    pager: &mut Pager,
    page_no: PageNo,
    kind: TreeKind,
    idx: usize,
) -> Result<()> {
    let ptr = {
        let page = pager.page(page_no)?;
        cell_pointer(page.data(), idx)?
    };
    free_cell_overflow(pager, page_no, kind, ptr)?;
    let data = pager.page_mut(page_no)?;
    let mut header = PageHeader::parse(data)?;
    let count = header.cell_count as usize;
    if idx >= count {
        return Err(Error::internal("remove_cell_at index out of range"));
    }
    let ptr = cell_pointer(data, idx)?;
    let size = cell_size(data, kind, ptr)?;
    let src = PAGE_HEADER_SIZE + 2 * (idx + 1);
    let end = PAGE_HEADER_SIZE + 2 * count;
    data.copy_within(src..end, src - 2);
    header.cell_count -= 1;
    header.fragmented_free = header
        .fragmented_free
        .saturating_add((size + 2).min(u16::MAX as usize) as u16);
    header.write_to(data);
    Ok(())
}

/// Compact all live cells to the top of the page, clearing fragmentation.
pub(crate) fn defragment(pager: &mut Pager, page_no: PageNo, kind: TreeKind) -> Result<()> {
    let page_size = pager.page_size();
    let data = pager.page_mut(page_no)?;
    let mut header = PageHeader::parse(data)?;
    let count = header.cell_count as usize;
    // Copy all cells to a temporary buffer first: cell positions are
    // arbitrary, so in-place compaction could overwrite unmoved cells.
    let mut temp = Vec::with_capacity(page_size);
    let mut sizes = Vec::with_capacity(count);
    for idx in 0..count {
        let ptr = cell_pointer(data, idx)?;
        let size = cell_size(data, kind, ptr)?;
        sizes.push(size);
        temp.extend_from_slice(&data[ptr..ptr + size]);
    }
    let mut write = page_size;
    let mut temp_offset = temp.len();
    for (idx, size) in sizes.iter().enumerate().rev() {
        write -= size;
        temp_offset -= size;
        data[write..write + size].copy_from_slice(&temp[temp_offset..temp_offset + size]);
        let ptr_offset = PAGE_HEADER_SIZE + 2 * idx;
        data[ptr_offset..ptr_offset + 2].copy_from_slice(&(write as u16).to_le_bytes());
    }
    header.content_start = content_start_stored(write);
    header.fragmented_free = 0;
    header.write_to(data);
    Ok(())
}

/// Rewrite a page from a list of raw cells (in key order).
pub(crate) fn rebuild_page(
    pager: &mut Pager,
    page_no: PageNo,
    page_type: PageType,
    rightmost: PageNo,
    cells: &[Vec<u8>],
) -> Result<()> {
    let page_size = pager.page_size();
    let data = pager.page_mut(page_no)?;
    data.fill(0);
    let mut write = page_size;
    let mut ptrs = Vec::with_capacity(cells.len());
    for cell in cells {
        write -= cell.len();
        data[write..write + cell.len()].copy_from_slice(cell);
        ptrs.push(write as u16);
    }
    let header = PageHeader {
        page_type,
        flags: 0,
        cell_count: cells.len() as u16,
        content_start: content_start_stored(write),
        fragmented_free: 0,
        page_no,
        rightmost,
    };
    header.write_to(data);
    for (idx, ptr) in ptrs.iter().enumerate() {
        let offset = PAGE_HEADER_SIZE + 2 * idx;
        data[offset..offset + 2].copy_from_slice(&ptr.to_le_bytes());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pager::header::PageType;
    use crate::vfs::mem::MemVfs;
    use std::sync::Arc;

    fn test_pager() -> Pager {
        Pager::create(
            Arc::new(MemVfs::new()),
            std::path::Path::new("btree-page.db"),
            4096,
        )
        .expect("pager")
    }

    #[test]
    fn leaf_cell_roundtrip_inline() {
        let mut pager = test_pager();
        pager.begin_write().expect("begin");
        let page = pager.alloc_page().expect("alloc");
        rebuild_page(&mut pager, page, PageType::TableLeaf, 0, &[]).expect("rebuild");
        let key = 42u64.to_be_bytes();
        let cell = build_leaf_cell(&mut pager, TreeKind::Table, &key, b"payload").expect("cell");
        assert!(insert_cell_bytes(&mut pager, page, TreeKind::Table, 0, &cell).expect("insert"));
        let ptr = {
            let page_ref = pager.page(page).expect("page");
            cell_pointer(page_ref.data(), 0).expect("ptr")
        };
        let value = read_cell_value(&mut pager, page, TreeKind::Table, ptr).expect("value");
        assert_eq!(value, b"payload");
    }

    #[test]
    fn leaf_cell_roundtrip_overflow() {
        let mut pager = test_pager();
        pager.begin_write().expect("begin");
        let page = pager.alloc_page().expect("alloc");
        rebuild_page(&mut pager, page, PageType::TableLeaf, 0, &[]).expect("rebuild");
        let key = 7u64.to_be_bytes();
        let big = vec![0x5A; 10_000];
        let cell = build_leaf_cell(&mut pager, TreeKind::Table, &key, &big).expect("cell");
        assert!(insert_cell_bytes(&mut pager, page, TreeKind::Table, 0, &cell).expect("insert"));
        let ptr = {
            let page_ref = pager.page(page).expect("page");
            cell_pointer(page_ref.data(), 0).expect("ptr")
        };
        let value = read_cell_value(&mut pager, page, TreeKind::Table, ptr).expect("value");
        assert_eq!(value, big);
    }

    #[test]
    fn defragment_preserves_cells() {
        let mut pager = test_pager();
        pager.begin_write().expect("begin");
        let page = pager.alloc_page().expect("alloc");
        rebuild_page(&mut pager, page, PageType::TableLeaf, 0, &[]).expect("rebuild");
        for i in 0..20u64 {
            let key = i.to_be_bytes();
            let cell = build_leaf_cell(&mut pager, TreeKind::Table, &key, b"x").expect("cell");
            assert!(
                insert_cell_bytes(&mut pager, page, TreeKind::Table, i as usize, &cell)
                    .expect("insert")
            );
        }
        remove_cell_at(&mut pager, page, TreeKind::Table, 5).expect("remove");
        remove_cell_at(&mut pager, page, TreeKind::Table, 5).expect("remove");
        defragment(&mut pager, page, TreeKind::Table).expect("defrag");
        let cells = read_all_cells(&mut pager, page, TreeKind::Table).expect("cells");
        assert_eq!(cells.len(), 18);
        for (i, cell) in cells.iter().enumerate() {
            let expected = (if i < 5 { i } else { i + 2 }) as u64;
            assert_eq!(
                raw_cell_key(cell, TreeKind::Table, PageType::TableLeaf).expect("key"),
                expected.to_be_bytes()
            );
        }
    }

    #[test]
    fn index_key_too_large_is_range_error() {
        let mut pager = test_pager();
        pager.begin_write().expect("begin");
        let big_key = vec![0u8; 5000];
        let err = build_leaf_cell(&mut pager, TreeKind::Index, &big_key, b"").expect_err("too big");
        assert_eq!(err.code(), crate::error::ErrorCode::Range);
    }
}
