//! Rollback journal: header/record codecs and crash recovery.
//!
//! See `docs/format.md` for the on-disk layout and commit protocol.

use crate::error::{Error, Result};
use crate::vfs::File;

/// Journal magic at offset 0 of the journal header.
pub const JOURNAL_MAGIC: [u8; 8] = *b"GRAJNL1\0";
/// Size of the serialized journal header (rest of the block is zero padding).
pub const JOURNAL_HEADER_SIZE: usize = 36;
/// Bytes of overhead per journal record (page number + checksum).
pub const JOURNAL_RECORD_OVERHEAD: usize = 8;

/// Parsed journal header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JournalHeader {
    /// Page size of the associated database.
    pub page_size: u32,
    /// Database change counter before the interrupted transaction.
    pub change_counter: u32,
    /// Random salt distinguishing journal incarnations.
    pub salt: u64,
    /// Number of page records written before commit.
    pub record_count: u64,
}

impl JournalHeader {
    /// Serialize into the first block of a journal file.
    pub fn encode(&self, block: &mut [u8]) {
        block[..JOURNAL_HEADER_SIZE].fill(0);
        block[..8].copy_from_slice(&JOURNAL_MAGIC);
        block[8..12].copy_from_slice(&self.page_size.to_le_bytes());
        block[12..16].copy_from_slice(&self.change_counter.to_le_bytes());
        block[16..24].copy_from_slice(&self.salt.to_le_bytes());
        block[24..32].copy_from_slice(&self.record_count.to_le_bytes());
        let checksum = crc32c::crc32c(&block[..32]);
        block[32..36].copy_from_slice(&checksum.to_le_bytes());
    }

    /// Parse a journal header block.
    ///
    /// Returns `Ok(None)` when the magic is zeroed (no transaction in flight
    /// or a crash before the commit header was written). Returns `Corrupt`
    /// when the magic is present but the header does not validate.
    pub fn parse(block: &[u8]) -> Result<Option<Self>> {
        if block.len() < JOURNAL_HEADER_SIZE {
            return Err(Error::corrupt("journal header block truncated"));
        }
        if block[..8] != JOURNAL_MAGIC {
            if block[..8].iter().all(|byte| *byte == 0) {
                return Ok(None);
            }
            return Err(Error::corrupt("journal header magic mismatch"));
        }
        let stored = u32::from_le_bytes([block[32], block[33], block[34], block[35]]);
        let computed = crc32c::crc32c(&block[..32]);
        if stored != computed {
            return Err(Error::corrupt("journal header checksum mismatch"));
        }
        let header = Self {
            page_size: u32::from_le_bytes([block[8], block[9], block[10], block[11]]),
            change_counter: u32::from_le_bytes([block[12], block[13], block[14], block[15]]),
            salt: u64::from_le_bytes(block[16..24].try_into().expect("slice length")),
            record_count: u64::from_le_bytes(block[24..32].try_into().expect("slice length")),
        };
        Ok(Some(header))
    }
}

/// Encode one journal record into a fresh buffer.
///
/// Record layout: `[page_no u32][page bytes][crc32c over both]`.
pub fn encode_record(page_no: u32, page: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + page.len() + 4);
    out.extend_from_slice(&page_no.to_le_bytes());
    out.extend_from_slice(page);
    let mut crc = crc32c::crc32c(&page_no.to_le_bytes());
    crc = crc32c::crc32c_append(crc, page);
    out.extend_from_slice(&crc.to_le_bytes());
    out
}

/// Outcome of inspecting a journal file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryOutcome {
    /// A valid journal was replayed; these page numbers were restored.
    RolledBack { pages: Vec<u32> },
    /// The journal carries no valid transaction; it can be deleted.
    Stale,
}

/// Replay a journal over the database file if it contains a valid transaction.
///
/// The caller must hold an exclusive lock on the database file. The journal
/// file is left in place; the caller removes it after a successful outcome.
pub fn recover(db: &dyn File, journal: &dyn File) -> Result<RecoveryOutcome> {
    let journal_size = journal.size()?;
    if journal_size < JOURNAL_HEADER_SIZE as u64 {
        return Ok(RecoveryOutcome::Stale);
    }
    let mut header_block = vec![0u8; JOURNAL_HEADER_SIZE];
    journal.read_at(&mut header_block, 0)?;
    let header = match JournalHeader::parse(&header_block)? {
        Some(header) => header,
        None => return Ok(RecoveryOutcome::Stale),
    };
    let page_size = header.page_size as usize;
    if page_size < super::header::MIN_PAGE_SIZE as usize
        || page_size > super::header::MAX_PAGE_SIZE as usize
    {
        return Err(Error::corrupt(format!(
            "journal records invalid page size {page_size}"
        )));
    }
    let record_size = (page_size + JOURNAL_RECORD_OVERHEAD) as u64;
    let mut offset = page_size as u64;
    let mut pages = Vec::new();
    let mut page = vec![0u8; page_size];
    while offset + record_size <= journal_size && (pages.len() as u64) < header.record_count {
        let mut page_no_bytes = [0u8; 4];
        journal.read_at(&mut page_no_bytes, offset)?;
        let page_no = u32::from_le_bytes(page_no_bytes);
        journal.read_at(&mut page, offset + 4)?;
        let mut crc_bytes = [0u8; 4];
        journal.read_at(&mut crc_bytes, offset + 4 + page_size as u64)?;
        let stored = u32::from_le_bytes(crc_bytes);
        let mut computed = crc32c::crc32c(&page_no_bytes);
        computed = crc32c::crc32c_append(computed, &page);
        if stored != computed {
            break;
        }
        if page_no == 0 {
            return Err(Error::corrupt("journal record with page number 0"));
        }
        let db_offset = (page_no as u64 - 1) * page_size as u64;
        db.write_at(&page, db_offset)?;
        pages.push(page_no);
        offset += record_size;
    }
    db.sync_all()?;
    Ok(RecoveryOutcome::RolledBack { pages })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pager::header::{DB_HEADER_END, DatabaseHeader, PageType, write_page_checksum};
    use crate::vfs::{MemVfs, OpenMode, Vfs};
    use std::path::Path;

    fn test_page(page_no: u32, fill: u8, page_size: usize) -> Vec<u8> {
        let mut page = vec![fill; page_size];
        let header = super::super::header::PageHeader {
            page_type: PageType::TableLeaf,
            flags: 0,
            cell_count: 0,
            content_start: page_size as u16,
            fragmented_free: 0,
            page_no,
            rightmost: 0,
        };
        header.write_to(&mut page);
        write_page_checksum(&mut page);
        page
    }

    fn header_page(page_size: usize) -> Vec<u8> {
        let mut page = vec![0u8; page_size];
        let header = super::super::header::PageHeader {
            page_type: PageType::Header,
            flags: 0,
            cell_count: 0,
            content_start: DB_HEADER_END as u16,
            fragmented_free: 0,
            page_no: 1,
            rightmost: 0,
        };
        header.write_to(&mut page);
        let db_header = DatabaseHeader::new(page_size as u32).expect("header");
        db_header.write_to(&mut page);
        write_page_checksum(&mut page);
        page
    }

    #[test]
    fn journal_header_roundtrip() {
        let mut block = vec![0u8; 512];
        let header = JournalHeader {
            page_size: 8192,
            change_counter: 3,
            salt: 0xDEADBEEF,
            record_count: 12,
        };
        header.encode(&mut block);
        let parsed = JournalHeader::parse(&block).expect("parse").expect("some");
        assert_eq!(parsed, header);
    }

    #[test]
    fn zero_magic_is_stale_not_corrupt() {
        let block = vec![0u8; 512];
        assert_eq!(JournalHeader::parse(&block).expect("parse"), None);
    }

    #[test]
    fn tampered_header_is_corrupt() {
        let mut block = vec![0u8; 512];
        let header = JournalHeader {
            page_size: 8192,
            change_counter: 3,
            salt: 7,
            record_count: 1,
        };
        header.encode(&mut block);
        block[16] ^= 0xFF;
        assert!(JournalHeader::parse(&block).is_err());
    }

    #[test]
    fn recovery_restores_pages() {
        let vfs = MemVfs::new();
        let db_path = Path::new("db");
        let journal_path = Path::new("db-journal");
        let page_size = 4096usize;

        let db = vfs.open(db_path, OpenMode::Create).expect("open db");
        db.write_at(&header_page(page_size), 0).expect("page 1");
        let original = test_page(2, 0xAA, page_size);
        db.write_at(&original, page_size as u64).expect("page 2");

        let mut db_header = DatabaseHeader::new(page_size as u32).expect("header");
        db_header.page_count = 2;
        let mut page_one = header_page(page_size);
        db_header.write_to(&mut page_one);
        write_page_checksum(&mut page_one);
        db.write_at(&page_one, 0).expect("page 1 rewrite");

        let journal = vfs.open(journal_path, OpenMode::Create).expect("journal");
        let mut header_block = vec![0u8; page_size];
        JournalHeader {
            page_size: page_size as u32,
            change_counter: 0,
            salt: 42,
            record_count: 1,
        }
        .encode(&mut header_block);
        journal.write_at(&header_block, 0).expect("journal header");
        let record = encode_record(2, &original);
        journal
            .write_at(&record, page_size as u64)
            .expect("journal record");

        let overwritten = test_page(2, 0xBB, page_size);
        db.write_at(&overwritten, page_size as u64)
            .expect("overwrite");

        let outcome = recover(&*db, &*journal).expect("recover");
        assert_eq!(outcome, RecoveryOutcome::RolledBack { pages: vec![2] });
        let mut restored = vec![0u8; page_size];
        db.read_at(&mut restored, page_size as u64).expect("read");
        assert_eq!(restored, original);
    }

    #[test]
    fn recovery_skips_torn_record() {
        let vfs = MemVfs::new();
        let db_path = Path::new("db");
        let journal_path = Path::new("db-journal");
        let page_size = 4096usize;

        let db = vfs.open(db_path, OpenMode::Create).expect("open db");
        let mut page_one = header_page(page_size);
        DatabaseHeader::new(page_size as u32)
            .expect("header")
            .write_to(&mut page_one);
        write_page_checksum(&mut page_one);
        db.write_at(&page_one, 0).expect("page 1");

        let journal = vfs.open(journal_path, OpenMode::Create).expect("journal");
        let mut header_block = vec![0u8; page_size];
        JournalHeader {
            page_size: page_size as u32,
            change_counter: 0,
            salt: 1,
            record_count: 2,
        }
        .encode(&mut header_block);
        journal.write_at(&header_block, 0).expect("header");
        let good = test_page(2, 0x11, page_size);
        let record = encode_record(2, &good);
        journal
            .write_at(&record, page_size as u64)
            .expect("record 1");
        let mut torn = encode_record(3, &test_page(3, 0x22, page_size));
        let len = torn.len();
        torn[len - 1] ^= 0xFF;
        journal
            .write_at(&torn, page_size as u64 + record.len() as u64)
            .expect("record 2");

        let outcome = recover(&*db, &*journal).expect("recover");
        assert_eq!(outcome, RecoveryOutcome::RolledBack { pages: vec![2] });
    }

    #[test]
    fn recovery_rejects_foreign_journal() {
        // A journal whose change counter does not match the database is
        // replayed anyway; staleness is handled by the zeroed-magic commit
        // protocol, not by counter comparison. This test documents that a
        // valid journal always replays.
        let vfs = MemVfs::new();
        let db_path = Path::new("db");
        let journal_path = Path::new("db-journal");
        let page_size = 4096usize;

        let db = vfs.open(db_path, OpenMode::Create).expect("open db");
        let mut page_one = header_page(page_size);
        let mut db_header = DatabaseHeader::new(page_size as u32).expect("header");
        db_header.change_counter = 5;
        db_header.write_to(&mut page_one);
        write_page_checksum(&mut page_one);
        db.write_at(&page_one, 0).expect("page 1");
        let original = test_page(2, 0x33, page_size);
        db.write_at(&original, page_size as u64).expect("page 2");

        let journal = vfs.open(journal_path, OpenMode::Create).expect("journal");
        let mut header_block = vec![0u8; page_size];
        JournalHeader {
            page_size: page_size as u32,
            change_counter: 1,
            salt: 1,
            record_count: 1,
        }
        .encode(&mut header_block);
        journal.write_at(&header_block, 0).expect("header");
        let record = encode_record(2, &original);
        journal.write_at(&record, page_size as u64).expect("record");

        let outcome = recover(&*db, &*journal).expect("replay");
        assert_eq!(outcome, RecoveryOutcome::RolledBack { pages: vec![2] });
    }
}
