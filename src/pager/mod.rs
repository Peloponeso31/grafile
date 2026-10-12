//! Pager: page cache, transactions, rollback journal, freelist.
//!
//! The pager is the only layer that knows about file offsets and page
//! allocation. All higher layers access pages through it, which guarantees
//! that every modified page is journaled before it is touched and that
//! checksums are verified on every read.

pub mod header;
pub mod journal;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use hashlink::LinkedHashMap;

use crate::error::{Error, ErrorCode, Result};
use crate::vfs::{File, LockLevel, OpenMode, Vfs, journal_path};

pub use header::{
    DB_HEADER_END, DEFAULT_PAGE_SIZE, DatabaseHeader, INVALID_PAGE, MAX_PAGE_SIZE, MIN_PAGE_SIZE,
    PAGE_HEADER_SIZE, PageHeader, PageNo, PageType, verify_page_checksum, write_page_checksum,
};

/// Default number of pages kept in the cache (16 MiB at 8 KiB pages).
pub const DEFAULT_CACHE_PAGES: usize = 2048;

/// Offset of the freelist trunk's next pointer within the page.
const FREELIST_NEXT_OFFSET: usize = header::PAGE_HEADER_SIZE;
/// Offset of the freelist trunk's entry count within the page.
const FREELIST_COUNT_OFFSET: usize = header::PAGE_HEADER_SIZE + 4;
/// Offset of the freelist trunk's first page-number entry.
const FREELIST_ENTRIES_OFFSET: usize = header::PAGE_HEADER_SIZE + 8;

/// Which lock the pager currently holds on the database file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LockState {
    None,
    Shared,
    Exclusive,
}

struct Frame {
    data: Box<[u8]>,
    dirty: bool,
    /// Bumped on every mutation so cursors can detect stale cached metadata.
    generation: u64,
}

/// State for an in-flight write transaction.
struct WriteTxn {
    journal: Box<dyn File>,
    /// Database header as it was before the transaction, restored on rollback.
    header_snapshot: DatabaseHeader,
    salt: u64,
    /// Pages whose original image is already in the journal.
    journaled: HashSet<u32>,
    journal_offset: u64,
    record_count: u64,
}

/// Borrowed page contents plus the frame generation.
#[derive(Debug)]
pub struct PageRef<'a> {
    data: &'a [u8],
    generation: u64,
}

impl<'a> PageRef<'a> {
    /// The raw page bytes.
    pub fn data(&self) -> &'a [u8] {
        self.data
    }

    /// Generation of the cached frame; changes on every mutation.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Parsed page header.
    pub fn header(&self) -> Result<PageHeader> {
        PageHeader::parse(self.data)
    }
}

/// The pager: caches pages, journals writes, and manages the freelist.
pub struct Pager {
    vfs: Arc<dyn Vfs>,
    file: Box<dyn File>,
    path: PathBuf,
    journal_path: PathBuf,
    header: DatabaseHeader,
    header_dirty: bool,
    page_size: usize,
    cache: LinkedHashMap<u32, Frame>,
    cache_capacity: usize,
    writable: bool,
    txn: Option<WriteTxn>,
    lock: LockState,
}

impl Pager {
    /// Create a new database file with the given page size.
    pub fn create(vfs: Arc<dyn Vfs>, path: &Path, page_size: u32) -> Result<Self> {
        let header = DatabaseHeader::new(page_size)?;
        if vfs.exists(path)? {
            return Err(Error::new(
                ErrorCode::Exists,
                format!("refusing to overwrite existing file {}", path.display()),
            ));
        }
        vfs.create_dir_all(path)?;
        let file = vfs.open(path, OpenMode::Create)?;
        let mut page_one = vec![0u8; header.page_size as usize];
        write_header_page(&mut page_one, &header)?;
        file.write_at(&page_one, 0)?;
        file.sync_all()?;
        let mut cache = LinkedHashMap::new();
        cache.insert(
            1,
            Frame {
                data: page_one.into_boxed_slice(),
                dirty: false,
                generation: 1,
            },
        );
        Ok(Self {
            vfs,
            file,
            path: path.to_path_buf(),
            journal_path: journal_path(path),
            page_size: header.page_size as usize,
            header,
            header_dirty: false,
            cache,
            cache_capacity: DEFAULT_CACHE_PAGES,
            writable: true,
            txn: None,
            lock: LockState::None,
        })
    }

    /// Open an existing database file, recovering a hot journal if present.
    pub fn open(vfs: Arc<dyn Vfs>, path: &Path, mode: OpenMode) -> Result<Self> {
        let file = vfs.open(path, mode)?;
        let size = file.size()?;
        if size < DB_HEADER_END as u64 {
            return Err(Error::new(
                ErrorCode::NotADb,
                format!("{} is too small to be a grafile database", path.display()),
            ));
        }
        let mut header_bytes = vec![0u8; DB_HEADER_END];
        file.read_at(&mut header_bytes, 0)?;
        let header = DatabaseHeader::parse(&header_bytes)?;
        let page_size = header.page_size as usize;
        if size < page_size as u64 {
            return Err(Error::corrupt(format!(
                "{} is shorter than one page",
                path.display()
            )));
        }
        let jpath = journal_path(path);
        if vfs.exists(&jpath)? {
            Self::recover_journal(vfs.as_ref(), &*file, &jpath, mode)?;
        }
        let mut page_one = vec![0u8; page_size];
        file.read_at(&mut page_one, 0)?;
        verify_page_checksum(&page_one, 1)?;
        let header = DatabaseHeader::parse(&page_one)?;
        if header.page_count < 1 {
            return Err(Error::corrupt("database header declares zero pages"));
        }
        let mut cache = LinkedHashMap::new();
        cache.insert(
            1,
            Frame {
                data: page_one.into_boxed_slice(),
                dirty: false,
                generation: 1,
            },
        );
        Ok(Self {
            vfs,
            file,
            path: path.to_path_buf(),
            journal_path: jpath,
            page_size,
            header,
            header_dirty: false,
            cache,
            cache_capacity: DEFAULT_CACHE_PAGES,
            writable: mode.is_writable(),
            txn: None,
            lock: LockState::None,
        })
    }

    fn recover_journal(vfs: &dyn Vfs, file: &dyn File, jpath: &Path, mode: OpenMode) -> Result<()> {
        if !mode.is_writable() {
            let journal_file = vfs.open(jpath, OpenMode::ReadOnly)?;
            let mut block = vec![0u8; journal::JOURNAL_HEADER_SIZE];
            journal_file.read_at(&mut block, 0)?;
            if journal::JournalHeader::parse(&block)?.is_some() {
                return Err(Error::new(
                    ErrorCode::ReadOnly,
                    "database needs journal recovery but was opened read-only",
                ));
            }
            return Ok(());
        }
        file.lock(LockLevel::Exclusive)?;
        let result = (|| {
            let journal_file = vfs.open(jpath, OpenMode::ReadWrite)?;
            let outcome = journal::recover(file, &*journal_file)?;
            drop(journal_file);
            vfs.remove(jpath)?;
            Ok::<_, Error>(outcome)
        })();
        let unlock = file.unlock(LockLevel::Exclusive);
        result?;
        unlock?;
        Ok(())
    }

    /// Database path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Page size in bytes.
    pub fn page_size(&self) -> usize {
        self.page_size
    }

    /// Current database header.
    pub fn header(&self) -> &DatabaseHeader {
        &self.header
    }

    /// Mutate the database header; changes are persisted on commit.
    pub fn update_header<R>(&mut self, f: impl FnOnce(&mut DatabaseHeader) -> R) -> R {
        let result = f(&mut self.header);
        self.header_dirty = true;
        result
    }

    /// Whether a write transaction is active.
    pub fn in_write_txn(&self) -> bool {
        self.txn.is_some()
    }

    /// Configure the page cache capacity in pages.
    pub fn set_cache_capacity(&mut self, pages: usize) {
        self.cache_capacity = pages.max(8);
        self.evict_if_needed();
    }

    /// Acquire the shared lock for a read snapshot (idempotent).
    pub fn begin_read(&mut self) -> Result<()> {
        match self.lock {
            LockState::None => {
                self.file.lock(LockLevel::Shared)?;
                self.lock = LockState::Shared;
            }
            LockState::Shared | LockState::Exclusive => {}
        }
        Ok(())
    }

    /// Release a shared read lock if held.
    pub fn end_read(&mut self) -> Result<()> {
        if self.lock == LockState::Shared {
            self.file.unlock(LockLevel::Shared)?;
            self.lock = LockState::None;
        }
        Ok(())
    }

    /// Begin a write transaction, acquiring the exclusive lock.
    pub fn begin_write(&mut self) -> Result<()> {
        if self.txn.is_some() {
            return Err(Error::misuse("nested write transactions are not supported"));
        }
        if !self.writable {
            return Err(Error::new(
                ErrorCode::ReadOnly,
                "database was opened read-only",
            ));
        }
        match self.lock {
            LockState::None => {
                self.file.lock(LockLevel::Exclusive)?;
                self.lock = LockState::Exclusive;
            }
            LockState::Shared => {
                self.file.unlock(LockLevel::Shared)?;
                self.file.lock(LockLevel::Exclusive)?;
                self.lock = LockState::Exclusive;
            }
            LockState::Exclusive => {}
        }
        let journal = match self.vfs.open(&self.journal_path, OpenMode::Create) {
            Ok(file) => file,
            Err(err) => {
                let _ = self.file.unlock(LockLevel::Exclusive);
                self.lock = LockState::None;
                return Err(err);
            }
        };
        if let Err(err) = journal.set_len(0) {
            let _ = self.file.unlock(LockLevel::Exclusive);
            self.lock = LockState::None;
            return Err(err);
        }
        let salt = uuid::Uuid::new_v4().as_u64_pair().0;
        self.txn = Some(WriteTxn {
            journal,
            header_snapshot: self.header.clone(),
            salt,
            journaled: HashSet::new(),
            journal_offset: self.page_size as u64,
            record_count: 0,
        });
        Ok(())
    }

    /// Commit the active write transaction.
    pub fn commit(&mut self) -> Result<()> {
        if self.txn.is_none() {
            return Err(Error::misuse("no write transaction to commit"));
        }
        let any_dirty = self.header_dirty || self.cache.values().any(|frame| frame.dirty);
        if !any_dirty {
            return self.finish_txn(true);
        }
        // Page 1 must reflect the new change counter; serialize it now so the
        // journal captures the original bytes first.
        self.update_header(|header| header.change_counter = header.change_counter.wrapping_add(1));
        let header = self.header.clone();
        {
            let data = self.page_mut(1)?;
            write_header_page(data, &header)?;
        }
        let txn = self.txn.as_mut().expect("txn present");
        let mut header_block = vec![0u8; self.page_size];
        journal::JournalHeader {
            page_size: self.page_size as u32,
            change_counter: txn.header_snapshot.change_counter,
            salt: txn.salt,
            record_count: txn.record_count,
        }
        .encode(&mut header_block);
        txn.journal.write_at(&header_block, 0)?;
        txn.journal.sync_all()?;
        let dirty_pages: Vec<u32> = {
            let mut pages: Vec<u32> = self
                .cache
                .iter()
                .filter(|(_, frame)| frame.dirty)
                .map(|(page_no, _)| *page_no)
                .collect();
            pages.sort_unstable();
            pages
        };
        for page_no in dirty_pages {
            let frame = self.cache.get(&page_no).expect("dirty page in cache");
            let offset = (page_no as u64 - 1) * self.page_size as u64;
            self.file.write_at(&frame.data, offset)?;
        }
        self.file.sync_all()?;
        self.finish_txn(true)
    }

    /// Roll back the active write transaction.
    pub fn rollback(&mut self) -> Result<()> {
        if self.txn.is_none() {
            return Err(Error::misuse("no write transaction to roll back"));
        }
        self.finish_txn(false)
    }

    fn finish_txn(&mut self, committed: bool) -> Result<()> {
        let txn = self.txn.take().expect("txn present");
        if committed {
            // Invalidate the journal so a resurrected file is ignored, then
            // delete it.
            txn.journal.write_at(&[0u8; 8], 0)?;
            txn.journal.sync_all()?;
        }
        self.vfs.remove(&self.journal_path)?;
        if committed {
            for frame in self.cache.values_mut() {
                frame.dirty = false;
            }
        } else {
            self.header = txn.header_snapshot;
            self.cache.retain(|_, frame| !frame.dirty);
            self.header_dirty = false;
        }
        self.file.unlock(LockLevel::Exclusive)?;
        self.lock = LockState::None;
        Ok(())
    }

    /// Fetch a page for reading, verifying its checksum.
    pub fn page(&mut self, page_no: u32) -> Result<PageRef<'_>> {
        self.fetch(page_no)?;
        self.cache.to_back(&page_no);
        let frame = self.cache.get(&page_no).expect("fetched");
        Ok(PageRef {
            data: &frame.data,
            generation: frame.generation,
        })
    }

    /// Fetch a page for mutation. Requires an active write transaction and
    /// journals the original image on first touch.
    pub fn page_mut(&mut self, page_no: u32) -> Result<&mut [u8]> {
        self.fetch(page_no)?;
        self.cache.to_back(&page_no);
        let txn = self
            .txn
            .as_mut()
            .ok_or_else(|| Error::misuse("page_mut called outside a write transaction"))?;
        let frame = self.cache.get_mut(&page_no).expect("fetched");
        if !frame.dirty {
            if !txn.journaled.contains(&page_no) {
                let record = journal::encode_record(page_no, &frame.data);
                txn.journal.write_at(&record, txn.journal_offset)?;
                txn.journal_offset += record.len() as u64;
                txn.record_count += 1;
                txn.journaled.insert(page_no);
            }
            frame.dirty = true;
            frame.generation += 1;
        }
        Ok(&mut frame.data)
    }

    fn fetch(&mut self, page_no: u32) -> Result<()> {
        if page_no == 0 || page_no as u64 > self.header.page_count {
            return Err(Error::corrupt(format!(
                "page {page_no} out of range (page count {})",
                self.header.page_count
            )));
        }
        if self.cache.contains_key(&page_no) {
            return Ok(());
        }
        let mut data = vec![0u8; self.page_size];
        let offset = (page_no as u64 - 1) * self.page_size as u64;
        self.file.read_at(&mut data, offset)?;
        verify_page_checksum(&data, page_no)?;
        let header = PageHeader::parse(&data)?;
        if header.page_no != page_no {
            return Err(Error::corrupt(format!(
                "page {page_no} claims to be page {}",
                header.page_no
            )));
        }
        self.cache.insert(
            page_no,
            Frame {
                data: data.into_boxed_slice(),
                dirty: false,
                generation: 1,
            },
        );
        self.evict_if_needed();
        Ok(())
    }

    fn evict_if_needed(&mut self) {
        while self.cache.len() > self.cache_capacity {
            let candidate = self
                .cache
                .iter()
                .find(|(_, frame)| !frame.dirty)
                .map(|(page_no, _)| *page_no);
            match candidate {
                Some(page_no) => {
                    self.cache.remove(&page_no);
                }
                None => break,
            }
        }
    }

    /// Allocate a page, reusing the freelist first.
    ///
    /// The returned page's contents are zeroed and its type header unset; the
    /// caller is responsible for initializing and checksumming it.
    pub fn alloc_page(&mut self) -> Result<u32> {
        if self.txn.is_none() {
            return Err(Error::misuse(
                "alloc_page called outside a write transaction",
            ));
        }
        if self.header.freelist_trunk != INVALID_PAGE {
            return self.alloc_from_freelist();
        }
        let next = self
            .header
            .page_count
            .checked_add(1)
            .filter(|count| *count <= u32::MAX as u64)
            .ok_or_else(|| Error::new(ErrorCode::Range, "database reached maximum size"))?;
        self.header.page_count = next;
        let page_no = next as u32;
        self.cache.insert(
            page_no,
            Frame {
                data: vec![0u8; self.page_size].into_boxed_slice(),
                dirty: true,
                generation: 1,
            },
        );
        self.evict_if_needed();
        Ok(page_no)
    }

    fn alloc_from_freelist(&mut self) -> Result<u32> {
        let trunk_no = self.header.freelist_trunk;
        let (next_trunk, count) = {
            let page = self.page(trunk_no)?;
            let header = page.header()?;
            if header.page_type != PageType::FreelistTrunk {
                return Err(Error::corrupt(format!(
                    "freelist trunk {trunk_no} has type {:?}",
                    header.page_type
                )));
            }
            let next = u32::from_le_bytes(
                page.data()[FREELIST_NEXT_OFFSET..FREELIST_NEXT_OFFSET + 4]
                    .try_into()
                    .expect("slice length"),
            );
            let count = u32::from_le_bytes(
                page.data()[FREELIST_COUNT_OFFSET..FREELIST_COUNT_OFFSET + 4]
                    .try_into()
                    .expect("slice length"),
            );
            (next, count)
        };
        if count == 0 {
            self.header.freelist_trunk = next_trunk;
            self.header.freelist_count = self.header.freelist_count.saturating_sub(1);
            let data = self.page_mut(trunk_no)?;
            data.fill(0);
            return Ok(trunk_no);
        }
        let entry_offset = FREELIST_ENTRIES_OFFSET + (count as usize - 1) * 4;
        let page_no = {
            let page = self.page(trunk_no)?;
            u32::from_le_bytes(
                page.data()[entry_offset..entry_offset + 4]
                    .try_into()
                    .expect("slice length"),
            )
        };
        {
            let data = self.page_mut(trunk_no)?;
            data[FREELIST_COUNT_OFFSET..FREELIST_COUNT_OFFSET + 4]
                .copy_from_slice(&(count - 1).to_le_bytes());
        }
        self.header.freelist_count = self.header.freelist_count.saturating_sub(1);
        let data = self.page_mut(page_no)?;
        data.fill(0);
        Ok(page_no)
    }

    /// Return a page to the freelist.
    pub fn free_page(&mut self, page_no: u32) -> Result<()> {
        if self.txn.is_none() {
            return Err(Error::misuse(
                "free_page called outside a write transaction",
            ));
        }
        if page_no <= 1 || page_no as u64 > self.header.page_count {
            return Err(Error::corrupt(format!("cannot free page {page_no}")));
        }
        let capacity = (self.page_size - FREELIST_ENTRIES_OFFSET) / 4;
        let head = self.header.freelist_trunk;
        if head != INVALID_PAGE {
            let count = {
                let page = self.page(head)?;
                u32::from_le_bytes(
                    page.data()[FREELIST_COUNT_OFFSET..FREELIST_COUNT_OFFSET + 4]
                        .try_into()
                        .expect("slice length"),
                ) as usize
            };
            if count < capacity {
                let entry_offset = FREELIST_ENTRIES_OFFSET + count * 4;
                let data = self.page_mut(head)?;
                data[entry_offset..entry_offset + 4].copy_from_slice(&page_no.to_le_bytes());
                data[FREELIST_COUNT_OFFSET..FREELIST_COUNT_OFFSET + 4]
                    .copy_from_slice(&((count as u32) + 1).to_le_bytes());
                self.mark_freelist_leaf(page_no)?;
                self.header.freelist_count += 1;
                return Ok(());
            }
        }
        // `page_no` becomes a new trunk pointing at the previous head.
        let page_size = self.page_size;
        let data = self.page_mut(page_no)?;
        data.fill(0);
        let header = PageHeader {
            page_type: PageType::FreelistTrunk,
            flags: 0,
            cell_count: 0,
            content_start: page_size as u16,
            fragmented_free: 0,
            page_no,
            rightmost: 0,
        };
        header.write_to(data);
        data[FREELIST_NEXT_OFFSET..FREELIST_NEXT_OFFSET + 4].copy_from_slice(&head.to_le_bytes());
        data[FREELIST_COUNT_OFFSET..FREELIST_COUNT_OFFSET + 4].copy_from_slice(&0u32.to_le_bytes());
        write_page_checksum(data);
        self.header.freelist_trunk = page_no;
        self.header.freelist_count += 1;
        Ok(())
    }

    fn mark_freelist_leaf(&mut self, page_no: u32) -> Result<()> {
        let page_size = self.page_size;
        let data = self.page_mut(page_no)?;
        data.fill(0);
        let header = PageHeader {
            page_type: PageType::FreelistLeaf,
            flags: 0,
            cell_count: 0,
            content_start: page_size as u16,
            fragmented_free: 0,
            page_no,
            rightmost: 0,
        };
        header.write_to(data);
        write_page_checksum(data);
        Ok(())
    }
}

/// Serialize the database header into a page-sized buffer (page 1).
fn write_header_page(page: &mut [u8], header: &DatabaseHeader) -> Result<()> {
    if page.len() < DB_HEADER_END {
        return Err(Error::internal("page buffer too small for database header"));
    }
    let page_header = PageHeader {
        page_type: PageType::Header,
        flags: 0,
        cell_count: 0,
        content_start: DB_HEADER_END as u16,
        fragmented_free: 0,
        page_no: 1,
        rightmost: 0,
    };
    page_header.write_to(page);
    header.write_to(page);
    write_page_checksum(page);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::mem::{FaultConfig, MemVfs, new_store};

    const PAGE_SIZE: u32 = 4096;

    fn init_page(pager: &mut Pager, page_no: u32, page_type: PageType) -> Result<()> {
        let page_size = pager.page_size();
        let data = pager.page_mut(page_no)?;
        data.fill(0);
        let header = PageHeader {
            page_type,
            flags: 0,
            cell_count: 0,
            content_start: page_size as u16,
            fragmented_free: 0,
            page_no,
            rightmost: 0,
        };
        header.write_to(data);
        write_page_checksum(data);
        Ok(())
    }

    fn create_pager(vfs: Arc<dyn Vfs>, path: &Path) -> Pager {
        Pager::create(vfs, path, PAGE_SIZE).expect("create pager")
    }

    #[test]
    fn create_commit_reopen_roundtrip() {
        let store = new_store();
        let path = Path::new("roundtrip.db");
        {
            let vfs = Arc::new(MemVfs::from_store(store.clone()));
            let mut pager = create_pager(vfs, path);
            assert_eq!(pager.header().change_counter, 0);
            pager.begin_write().expect("begin");
            pager.update_header(|header| header.user_version = 7);
            pager.commit().expect("commit");
            assert_eq!(pager.header().change_counter, 1);
        }
        let vfs = Arc::new(MemVfs::from_store(store));
        let pager = Pager::open(vfs, path, OpenMode::ReadWrite).expect("reopen");
        assert_eq!(pager.header().user_version, 7);
        assert_eq!(pager.header().change_counter, 1);
        assert_eq!(pager.header().page_size, PAGE_SIZE);
    }

    #[test]
    fn alloc_and_free_reuse_pages() {
        let store = new_store();
        let path = Path::new("alloc.db");
        let vfs = Arc::new(MemVfs::from_store(store));
        let mut pager = create_pager(vfs, path);

        pager.begin_write().expect("begin");
        let p1 = pager.alloc_page().expect("alloc");
        init_page(&mut pager, p1, PageType::TableLeaf).expect("init");
        let p2 = pager.alloc_page().expect("alloc");
        init_page(&mut pager, p2, PageType::TableLeaf).expect("init");
        pager.commit().expect("commit");
        assert_eq!(pager.header().page_count, 3);
        assert_eq!((p1, p2), (2, 3));

        pager.begin_write().expect("begin");
        pager.free_page(p2).expect("free");
        pager.commit().expect("commit");
        assert_eq!(pager.header().freelist_count, 1);

        pager.begin_write().expect("begin");
        let reused = pager.alloc_page().expect("alloc");
        pager.commit().expect("commit");
        assert_eq!(reused, p2);
        assert_eq!(pager.header().freelist_count, 0);
    }

    #[test]
    fn rollback_restores_header_and_pages() {
        let store = new_store();
        let path = Path::new("rollback.db");
        let vfs = Arc::new(MemVfs::from_store(store));
        let mut pager = create_pager(vfs, path);

        pager.begin_write().expect("begin");
        let p1 = pager.alloc_page().expect("alloc");
        init_page(&mut pager, p1, PageType::TableLeaf).expect("init");
        pager.commit().expect("commit");
        let original = pager.page(p1).expect("page").data().to_vec();

        pager.begin_write().expect("begin");
        {
            let data = pager.page_mut(p1).expect("mutate");
            data[100] = 0xAB;
            write_page_checksum(data);
        }
        pager.update_header(|header| header.user_version = 99);
        pager.rollback().expect("rollback");

        assert_eq!(pager.header().user_version, 0);
        assert_eq!(pager.header().page_count, 2);
        assert_eq!(pager.page(p1).expect("page").data(), original.as_slice());
    }

    #[test]
    fn commit_failure_after_db_writes_recovers_on_reopen() {
        let store = new_store();
        let path = Path::new("crash-replay.db");
        let original;
        {
            let vfs = MemVfs::from_store(store.clone());
            let shared: Arc<dyn Vfs> = Arc::new(vfs.clone());
            let mut pager = create_pager(shared, path);
            pager.begin_write().expect("begin");
            let p1 = pager.alloc_page().expect("alloc");
            init_page(&mut pager, p1, PageType::TableLeaf).expect("init");
            pager.commit().expect("commit");
            original = pager.page(p1).expect("page").data().to_vec();

            pager.begin_write().expect("begin");
            {
                let data = pager.page_mut(p1).expect("mutate");
                data[200] = 0xFF;
                write_page_checksum(data);
            }
            // Op sequence after arming: 1 page-1 journal record, 2 journal
            // header, 3 journal sync, 4 db page 1, 5 db page 2, 6 db sync,
            // 7 journal magic zeroing (fails here).
            vfs.inject_fault(FaultConfig {
                fail_after_ops: Some(6),
                torn_write: false,
            });
            let err = pager.commit().expect_err("commit fails");
            assert_eq!(err.code(), ErrorCode::Io);
        }
        let vfs = Arc::new(MemVfs::from_store(store));
        let mut pager = Pager::open(vfs, path, OpenMode::ReadWrite).expect("reopen recovers");
        assert_eq!(pager.page(2).expect("page").data(), original.as_slice());
        assert_eq!(pager.header().change_counter, 1);
    }

    #[test]
    fn torn_write_during_commit_recovers_on_reopen() {
        let store = new_store();
        let path = Path::new("crash-torn.db");
        let original;
        {
            let vfs = MemVfs::from_store(store.clone());
            let shared: Arc<dyn Vfs> = Arc::new(vfs.clone());
            let mut pager = create_pager(shared, path);
            pager.begin_write().expect("begin");
            let p1 = pager.alloc_page().expect("alloc");
            init_page(&mut pager, p1, PageType::TableLeaf).expect("init");
            pager.commit().expect("commit");
            original = pager.page(p1).expect("page").data().to_vec();

            pager.begin_write().expect("begin");
            {
                let data = pager.page_mut(p1).expect("mutate");
                data[300] = 0xEE;
                write_page_checksum(data);
            }
            // Op 5 is the db write of page 2; make it a torn write.
            vfs.inject_fault(FaultConfig {
                fail_after_ops: Some(4),
                torn_write: true,
            });
            pager.commit().expect_err("commit fails");
        }
        let vfs = Arc::new(MemVfs::from_store(store));
        let mut pager = Pager::open(vfs, path, OpenMode::ReadWrite).expect("reopen recovers");
        assert_eq!(pager.page(2).expect("page").data(), original.as_slice());
    }

    #[test]
    fn crash_before_journal_header_leaves_db_untouched() {
        let store = new_store();
        let path = Path::new("crash-early.db");
        let original;
        {
            let vfs = MemVfs::from_store(store.clone());
            let shared: Arc<dyn Vfs> = Arc::new(vfs.clone());
            let mut pager = create_pager(shared, path);
            pager.begin_write().expect("begin");
            let p1 = pager.alloc_page().expect("alloc");
            init_page(&mut pager, p1, PageType::TableLeaf).expect("init");
            pager.commit().expect("commit");
            original = pager.page(p1).expect("page").data().to_vec();

            pager.begin_write().expect("begin");
            {
                let data = pager.page_mut(p1).expect("mutate");
                data[400] = 0x11;
                write_page_checksum(data);
            }
            // Op 2 is the journal header write; failing there leaves a journal
            // with zeroed magic, so the database is never modified.
            vfs.inject_fault(FaultConfig {
                fail_after_ops: Some(1),
                torn_write: false,
            });
            pager.commit().expect_err("commit fails");
        }
        let vfs = Arc::new(MemVfs::from_store(store));
        let mut pager = Pager::open(vfs, path, OpenMode::ReadWrite).expect("reopen");
        assert_eq!(pager.page(2).expect("page").data(), original.as_slice());
        assert_eq!(pager.header().change_counter, 1);
    }

    #[test]
    fn checksum_corruption_is_detected_on_read() {
        let store = new_store();
        let path = Path::new("corrupt.db");
        {
            let vfs = Arc::new(MemVfs::from_store(store.clone()));
            let mut pager = create_pager(vfs, path);
            pager.begin_write().expect("begin");
            let p1 = pager.alloc_page().expect("alloc");
            init_page(&mut pager, p1, PageType::TableLeaf).expect("init");
            pager.commit().expect("commit");
        }
        {
            let mut guard = store.lock().expect("store");
            let bytes = guard.get_mut(path).expect("file");
            bytes[PAGE_SIZE as usize + 100] ^= 0xFF;
        }
        let vfs = Arc::new(MemVfs::from_store(store));
        let mut pager = Pager::open(vfs, path, OpenMode::ReadWrite).expect("open");
        let err = pager.page(2).expect_err("corrupt page");
        assert!(err.code().is_corruption());
    }

    #[test]
    fn read_only_open_rejects_writes() {
        let store = new_store();
        let path = Path::new("readonly.db");
        {
            let vfs = Arc::new(MemVfs::from_store(store.clone()));
            let pager = create_pager(vfs, path);
            drop(pager);
        }
        let vfs = Arc::new(MemVfs::from_store(store));
        let mut pager = Pager::open(vfs, path, OpenMode::ReadOnly).expect("open");
        let err = pager.begin_write().expect_err("read-only");
        assert_eq!(err.code(), ErrorCode::ReadOnly);
    }

    #[test]
    fn header_only_commit_persists_change_counter() {
        let store = new_store();
        let path = Path::new("headeronly.db");
        {
            let vfs = Arc::new(MemVfs::from_store(store.clone()));
            let mut pager = create_pager(vfs, path);
            pager.begin_write().expect("begin");
            pager.update_header(|header| header.user_version = 3);
            pager.commit().expect("commit");
        }
        let vfs = Arc::new(MemVfs::from_store(store));
        let pager = Pager::open(vfs, path, OpenMode::ReadWrite).expect("reopen");
        assert_eq!(pager.header().user_version, 3);
        assert_eq!(pager.header().change_counter, 1);
    }
}
