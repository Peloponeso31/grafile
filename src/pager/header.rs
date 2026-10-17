//! Database and page header structures, plus shared format constants.

use crate::error::{Error, Result};

/// File magic at the start of the database header.
pub const MAGIC: [u8; 8] = *b"GRAFILE\0";
/// Format version written by this build.
pub const FORMAT_MAJOR: u16 = 0;
/// Format minor version written by this build. Minor 2 adds the temporal
/// and point record tags; minor 3 adds named catalog objects (indexes with
/// names, key lists and kinds, plus NOT NULL constraints). Older minors load
/// unchanged; builds from before minor 3 reject files that contain the new
/// catalog entry kinds.
pub const FORMAT_MINOR: u16 = 3;

/// Size of the common page header in bytes.
pub const PAGE_HEADER_SIZE: usize = 20;
/// Size of the database header inside page 1.
pub const DB_HEADER_SIZE: usize = 128;
/// Offset of the database header inside page 1.
pub const DB_HEADER_OFFSET: usize = PAGE_HEADER_SIZE;
/// End offset of the database header inside page 1.
pub const DB_HEADER_END: usize = DB_HEADER_OFFSET + DB_HEADER_SIZE;

/// Default page size used when creating databases.
pub const DEFAULT_PAGE_SIZE: u32 = 8192;
/// Smallest supported page size.
pub const MIN_PAGE_SIZE: u32 = 4096;
/// Largest supported page size.
pub const MAX_PAGE_SIZE: u32 = 65536;

/// Text encoding marker for UTF-8 (the only encoding).
pub const ENCODING_UTF8: u8 = 0;

/// Sentinel page number meaning "none".
pub const INVALID_PAGE: u32 = 0;

/// Page type stored in byte 0 of every page header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PageType {
    /// Metadata page (page 1 only).
    Header = 0x00,
    /// Table B+tree interior page.
    TableInterior = 0x01,
    /// Table B+tree leaf page.
    TableLeaf = 0x02,
    /// Index B+tree interior page.
    IndexInterior = 0x03,
    /// Index B+tree leaf page.
    IndexLeaf = 0x04,
    /// Overflow page chain member.
    Overflow = 0x05,
    /// Freelist trunk page.
    FreelistTrunk = 0x06,
    /// Freelist leaf page (unused content).
    FreelistLeaf = 0x07,
}

impl PageType {
    /// Parse a raw byte into a page type.
    pub fn from_byte(byte: u8) -> Result<Self> {
        match byte {
            0x00 => Ok(Self::Header),
            0x01 => Ok(Self::TableInterior),
            0x02 => Ok(Self::TableLeaf),
            0x03 => Ok(Self::IndexInterior),
            0x04 => Ok(Self::IndexLeaf),
            0x05 => Ok(Self::Overflow),
            0x06 => Ok(Self::FreelistTrunk),
            0x07 => Ok(Self::FreelistLeaf),
            other => Err(Error::corrupt(format!("unknown page type {other:#04x}"))),
        }
    }

    /// Whether this page type belongs to a B+tree.
    pub const fn is_btree(self) -> bool {
        matches!(
            self,
            Self::TableInterior | Self::TableLeaf | Self::IndexInterior | Self::IndexLeaf
        )
    }
}

/// Parsed view of the 20-byte page header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageHeader {
    /// Type of this page.
    pub page_type: PageType,
    /// Reserved flags; must be zero in this format version.
    pub flags: u8,
    /// Number of cells on the page.
    pub cell_count: u16,
    /// Offset where the cell content area starts.
    pub content_start: u16,
    /// Bytes of free space lost to fragmentation.
    pub fragmented_free: u16,
    /// This page's own number (1-based).
    pub page_no: u32,
    /// Rightmost child of an interior page (0 for other page types).
    pub rightmost: u32,
}

impl PageHeader {
    /// Parse the header from the first [`PAGE_HEADER_SIZE`] bytes of a page.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < PAGE_HEADER_SIZE {
            return Err(Error::corrupt("page shorter than page header"));
        }
        Ok(Self {
            page_type: PageType::from_byte(bytes[0])?,
            flags: bytes[1],
            cell_count: u16::from_le_bytes([bytes[2], bytes[3]]),
            content_start: u16::from_le_bytes([bytes[4], bytes[5]]),
            fragmented_free: u16::from_le_bytes([bytes[6], bytes[7]]),
            page_no: u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]),
            rightmost: u32::from_le_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]),
        })
    }

    /// Serialize the header (checksum excluded) into the first 16 bytes.
    pub fn write_to(&self, bytes: &mut [u8]) {
        bytes[0] = self.page_type as u8;
        bytes[1] = self.flags;
        bytes[2..4].copy_from_slice(&self.cell_count.to_le_bytes());
        bytes[4..6].copy_from_slice(&self.content_start.to_le_bytes());
        bytes[6..8].copy_from_slice(&self.fragmented_free.to_le_bytes());
        bytes[8..12].copy_from_slice(&self.page_no.to_le_bytes());
        bytes[12..16].copy_from_slice(&self.rightmost.to_le_bytes());
    }
}

/// Page number of an allocated structure, or [`INVALID_PAGE`].
pub type PageNo = u32;

/// Parsed database header (page 1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatabaseHeader {
    /// Format major version.
    pub format_major: u16,
    /// Format minor version.
    pub format_minor: u16,
    /// Page size in bytes.
    pub page_size: u32,
    /// Bytes reserved at the end of every page.
    pub reserved_per_page: u8,
    /// Text encoding (0 = UTF-8).
    pub encoding: u8,
    /// Highest allocated page number.
    pub page_count: u64,
    /// Head of the freelist, or 0.
    pub freelist_trunk: PageNo,
    /// Number of free pages on the freelist.
    pub freelist_count: u32,
    /// Root of the catalog B+tree.
    pub catalog_root: PageNo,
    /// Root of the node table B+tree.
    pub node_root: PageNo,
    /// Root of the edge table B+tree.
    pub edge_root: PageNo,
    /// Root of the fragment store B+tree.
    pub fragment_root: PageNo,
    /// Next node id to allocate.
    pub next_node_id: u64,
    /// Next edge id to allocate.
    pub next_edge_id: u64,
    /// Next label id to allocate.
    pub next_label_id: u32,
    /// Next edge type id to allocate.
    pub next_type_id: u32,
    /// Next property key id to allocate.
    pub next_key_id: u32,
    /// Incremented once per committed write transaction.
    pub change_counter: u32,
    /// Incremented by schema changes.
    pub schema_generation: u32,
    /// Creation time in Unix seconds.
    pub created_at: i64,
    /// Random database identifier.
    pub uuid: [u8; 16],
    /// Application-managed version number.
    pub user_version: u32,
    /// Application-managed identifier.
    pub application_id: u32,
}

impl DatabaseHeader {
    /// Create a fresh header for a new database.
    pub fn new(page_size: u32) -> Result<Self> {
        if !(MIN_PAGE_SIZE..=MAX_PAGE_SIZE).contains(&page_size) || !page_size.is_power_of_two() {
            return Err(Error::new(
                crate::error::ErrorCode::Range,
                format!("invalid page size {page_size}; expected a power of two in 4096..=65536"),
            ));
        }
        let created_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        Ok(Self {
            format_major: FORMAT_MAJOR,
            format_minor: FORMAT_MINOR,
            page_size,
            reserved_per_page: 0,
            encoding: ENCODING_UTF8,
            page_count: 1,
            freelist_trunk: INVALID_PAGE,
            freelist_count: 0,
            catalog_root: INVALID_PAGE,
            node_root: INVALID_PAGE,
            edge_root: INVALID_PAGE,
            fragment_root: INVALID_PAGE,
            next_node_id: 1,
            next_edge_id: 1,
            next_label_id: 1,
            next_type_id: 1,
            next_key_id: 1,
            change_counter: 0,
            schema_generation: 0,
            created_at,
            uuid: *uuid::Uuid::new_v4().as_bytes(),
            user_version: 0,
            application_id: 0,
        })
    }

    /// Parse the header from the bytes at [`DB_HEADER_OFFSET`] onward.
    pub fn parse(page: &[u8]) -> Result<Self> {
        if page.len() < DB_HEADER_END {
            return Err(Error::corrupt("page 1 too short for database header"));
        }
        let bytes = &page[DB_HEADER_OFFSET..DB_HEADER_END];
        if bytes[0..8] != MAGIC {
            return Err(Error::new(
                crate::error::ErrorCode::NotADb,
                "file is not a grafile database (bad magic)",
            ));
        }
        let stored_checksum = u32::from_le_bytes([
            bytes[DB_HEADER_SIZE - 4],
            bytes[DB_HEADER_SIZE - 3],
            bytes[DB_HEADER_SIZE - 2],
            bytes[DB_HEADER_SIZE - 1],
        ]);
        let computed = crc32c::crc32c(&bytes[..DB_HEADER_SIZE - 4]);
        if stored_checksum != computed {
            return Err(Error::corrupt("database header checksum mismatch"));
        }
        let header = Self {
            format_major: u16::from_le_bytes([bytes[8], bytes[9]]),
            format_minor: u16::from_le_bytes([bytes[10], bytes[11]]),
            page_size: u32::from_le_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]),
            reserved_per_page: bytes[16],
            encoding: bytes[17],
            page_count: u64::from_le_bytes(bytes[24..32].try_into().expect("slice length")),
            freelist_trunk: u32::from_le_bytes(bytes[32..36].try_into().expect("slice length")),
            freelist_count: u32::from_le_bytes(bytes[36..40].try_into().expect("slice length")),
            catalog_root: u32::from_le_bytes(bytes[40..44].try_into().expect("slice length")),
            node_root: u32::from_le_bytes(bytes[44..48].try_into().expect("slice length")),
            edge_root: u32::from_le_bytes(bytes[48..52].try_into().expect("slice length")),
            fragment_root: u32::from_le_bytes(bytes[52..56].try_into().expect("slice length")),
            next_node_id: u64::from_le_bytes(bytes[56..64].try_into().expect("slice length")),
            next_edge_id: u64::from_le_bytes(bytes[64..72].try_into().expect("slice length")),
            next_label_id: u32::from_le_bytes(bytes[72..76].try_into().expect("slice length")),
            next_type_id: u32::from_le_bytes(bytes[76..80].try_into().expect("slice length")),
            next_key_id: u32::from_le_bytes(bytes[80..84].try_into().expect("slice length")),
            change_counter: u32::from_le_bytes(bytes[84..88].try_into().expect("slice length")),
            schema_generation: u32::from_le_bytes(bytes[88..92].try_into().expect("slice length")),
            created_at: i64::from_le_bytes(bytes[92..100].try_into().expect("slice length")),
            uuid: bytes[100..116].try_into().expect("slice length"),
            user_version: u32::from_le_bytes(bytes[116..120].try_into().expect("slice length")),
            application_id: u32::from_le_bytes(bytes[120..124].try_into().expect("slice length")),
        };
        if !header.page_size.is_power_of_two()
            || !(MIN_PAGE_SIZE..=MAX_PAGE_SIZE).contains(&header.page_size)
        {
            return Err(Error::corrupt(format!(
                "invalid page size {} in database header",
                header.page_size
            )));
        }
        if header.format_major > FORMAT_MAJOR {
            return Err(Error::new(
                crate::error::ErrorCode::NotADb,
                format!(
                    "database format {}.{} is newer than supported {}.{}",
                    header.format_major, header.format_minor, FORMAT_MAJOR, FORMAT_MINOR
                ),
            ));
        }
        Ok(header)
    }

    /// Serialize into a page-sized buffer starting at [`DB_HEADER_OFFSET`].
    pub fn write_to(&self, page: &mut [u8]) {
        let bytes = &mut page[DB_HEADER_OFFSET..DB_HEADER_END];
        bytes[0..8].copy_from_slice(&MAGIC);
        bytes[8..10].copy_from_slice(&self.format_major.to_le_bytes());
        bytes[10..12].copy_from_slice(&self.format_minor.to_le_bytes());
        bytes[12..16].copy_from_slice(&self.page_size.to_le_bytes());
        bytes[16] = self.reserved_per_page;
        bytes[17] = self.encoding;
        bytes[18..20].fill(0);
        bytes[20..24].fill(0);
        bytes[24..32].copy_from_slice(&self.page_count.to_le_bytes());
        bytes[32..36].copy_from_slice(&self.freelist_trunk.to_le_bytes());
        bytes[36..40].copy_from_slice(&self.freelist_count.to_le_bytes());
        bytes[40..44].copy_from_slice(&self.catalog_root.to_le_bytes());
        bytes[44..48].copy_from_slice(&self.node_root.to_le_bytes());
        bytes[48..52].copy_from_slice(&self.edge_root.to_le_bytes());
        bytes[52..56].copy_from_slice(&self.fragment_root.to_le_bytes());
        bytes[56..64].copy_from_slice(&self.next_node_id.to_le_bytes());
        bytes[64..72].copy_from_slice(&self.next_edge_id.to_le_bytes());
        bytes[72..76].copy_from_slice(&self.next_label_id.to_le_bytes());
        bytes[76..80].copy_from_slice(&self.next_type_id.to_le_bytes());
        bytes[80..84].copy_from_slice(&self.next_key_id.to_le_bytes());
        bytes[84..88].copy_from_slice(&self.change_counter.to_le_bytes());
        bytes[88..92].copy_from_slice(&self.schema_generation.to_le_bytes());
        bytes[92..100].copy_from_slice(&self.created_at.to_le_bytes());
        bytes[100..116].copy_from_slice(&self.uuid);
        bytes[116..120].copy_from_slice(&self.user_version.to_le_bytes());
        bytes[120..124].copy_from_slice(&self.application_id.to_le_bytes());
        let checksum = crc32c::crc32c(&bytes[..DB_HEADER_SIZE - 4]);
        bytes[DB_HEADER_SIZE - 4..DB_HEADER_SIZE].copy_from_slice(&checksum.to_le_bytes());
    }
}

/// Recompute and store the page checksum in bytes `[16..20]`.
///
/// The checksum covers the header bytes before it and all content after it.
pub fn write_page_checksum(page: &mut [u8]) {
    debug_assert!(page.len() >= PAGE_HEADER_SIZE);
    let checksum = page_checksum(page);
    page[16..20].copy_from_slice(&checksum.to_le_bytes());
}

/// Compute the CRC32C of a page, excluding the checksum field itself.
pub fn page_checksum(page: &[u8]) -> u32 {
    crc32c::crc32c_append(crc32c::crc32c(&page[..16]), &page[PAGE_HEADER_SIZE..])
}

/// Verify the page checksum; returns an error describing the page on failure.
pub fn verify_page_checksum(page: &[u8], page_no: u32) -> Result<()> {
    if page.len() < PAGE_HEADER_SIZE {
        return Err(Error::corrupt(format!(
            "page {page_no}: shorter than header"
        )));
    }
    let stored = u32::from_le_bytes([page[16], page[17], page[18], page[19]]);
    let computed = page_checksum(page);
    if stored != computed {
        return Err(Error::corrupt(format!(
            "page {page_no}: checksum mismatch (stored {stored:#010x}, computed {computed:#010x})"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn database_header_roundtrip() {
        let mut header = DatabaseHeader::new(8192).expect("header");
        header.next_node_id = 42;
        header.page_count = 7;
        header.freelist_trunk = 3;
        header.catalog_root = 2;
        header.user_version = 9;
        let mut page = vec![0u8; 8192];
        header.write_to(&mut page);
        let parsed = DatabaseHeader::parse(&page).expect("parse");
        assert_eq!(parsed, header);
    }

    #[test]
    fn database_header_detects_corruption() {
        let header = DatabaseHeader::new(4096).expect("header");
        let mut page = vec![0u8; 4096];
        header.write_to(&mut page);
        page[DB_HEADER_OFFSET + 60] ^= 0xFF;
        let err = DatabaseHeader::parse(&page).expect_err("corrupt");
        assert!(err.code().is_corruption());
    }

    #[test]
    fn database_header_rejects_bad_magic() {
        let mut page = vec![0u8; 4096];
        page[DB_HEADER_OFFSET] = b'X';
        let err = DatabaseHeader::parse(&page).expect_err("not a db");
        assert_eq!(err.code(), crate::error::ErrorCode::NotADb);
    }

    #[test]
    fn database_header_rejects_bad_page_size() {
        let header = DatabaseHeader::new(3000);
        assert!(header.is_err());
        let header = DatabaseHeader::new(131072);
        assert!(header.is_err());
    }

    #[test]
    fn page_header_roundtrip() {
        let mut bytes = vec![0u8; PAGE_HEADER_SIZE];
        let header = PageHeader {
            page_type: PageType::TableLeaf,
            flags: 0,
            cell_count: 5,
            content_start: 4000,
            fragmented_free: 3,
            page_no: 17,
            rightmost: 0,
        };
        header.write_to(&mut bytes);
        let parsed = PageHeader::parse(&bytes).expect("parse");
        assert_eq!(parsed, header);
    }

    #[test]
    fn page_checksum_detects_flip() {
        let mut page = vec![7u8; 4096];
        write_page_checksum(&mut page);
        verify_page_checksum(&page, 1).expect("valid");
        page[100] ^= 1;
        assert!(verify_page_checksum(&page, 1).is_err());
    }

    #[test]
    fn unknown_page_type_is_corruption() {
        let mut bytes = vec![0u8; PAGE_HEADER_SIZE];
        bytes[0] = 0xEE;
        let err = PageHeader::parse(&bytes).expect_err("bad type");
        assert!(err.code().is_corruption());
    }
}
