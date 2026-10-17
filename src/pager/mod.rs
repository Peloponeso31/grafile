//! Pager: page cache, transactions, rollback journal, freelist.
//!
//! The pager is the only layer that knows about file offsets and page
//! allocation. All higher layers access pages through it, which guarantees
//! that every modified page is journaled before it is touched and that
//! checksums are verified on every read.

pub mod header;
pub mod journal;
mod writer_lock;

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
    /// Process-global writer slot held while a write transaction is active.
    writer_claim: Option<std::sync::Arc<writer_lock::PathClaim>>,
    /// Bumped whenever a write transaction begins or ends; caches layered on
    /// top of the pager use it to invalidate themselves after rollback.
    txn_generation: u64,
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
        write_page_checksum(&mut page_one);
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
            writer_claim: None,
            txn_generation: 0,
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
            writer_claim: None,
            txn_generation: 0,
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

    /// Counter bumped when a write transaction begins or ends.
    pub fn txn_generation(&self) -> u64 {
        self.txn_generation
    }

    /// Configure the page cache capacity in pages.
    pub fn set_cache_capacity(&mut self, pages: usize) {
        self.cache_capacity = pages.max(8);
        self.evict_if_needed();
    }

    /// Acquire the shared lock for a read snapshot (idempotent).
    ///
    /// On the first acquisition the pager recovers a hot journal left by a
    /// crashed writer and drops its page cache if another connection has
    /// committed since the cache was filled, so readers never observe stale
    /// or partially committed state.
    pub fn begin_read(&mut self) -> Result<()> {
        match self.lock {
            LockState::None => {
                self.file.lock(LockLevel::Shared)?;
                self.lock = LockState::Shared;
                self.ensure_no_hot_journal()?;
                self.refresh_if_changed()?;
            }
            LockState::Shared | LockState::Exclusive => {}
        }
        Ok(())
    }

    /// Recover a hot journal before reading. Assumes the shared lock is held
    /// and leaves it held afterwards.
    fn ensure_no_hot_journal(&mut self) -> Result<()> {
        if !self.vfs.exists(&self.journal_path)? {
            return Ok(());
        }
        if !self.writable {
            if Self::journal_is_hot(self.vfs.as_ref(), &self.journal_path)? {
                return Err(Error::new(
                    ErrorCode::ReadOnly,
                    "database needs journal recovery but was opened read-only",
                ));
            }
            return Ok(());
        }
        // Replay requires the exclusive lock.
        self.file.unlock(LockLevel::Shared)?;
        self.lock = LockState::None;
        self.file.lock(LockLevel::Exclusive)?;
        let result: Result<bool> = (|| {
            if !self.vfs.exists(&self.journal_path)? {
                // Another process recovered and removed it first.
                return Ok(false);
            }
            let journal_file = self.vfs.open(&self.journal_path, OpenMode::ReadWrite)?;
            journal::recover(&*self.file, &*journal_file)?;
            drop(journal_file);
            self.vfs.remove(&self.journal_path)?;
            Ok(true)
        })();
        let unlock = self.file.unlock(LockLevel::Exclusive);
        self.file.lock(LockLevel::Shared)?;
        self.lock = LockState::Shared;
        let recovered = result?;
        unlock?;
        if recovered {
            self.invalidate_cache()?;
        }
        Ok(())
    }

    /// Whether the journal file carries a valid transaction header.
    fn journal_is_hot(vfs: &dyn Vfs, path: &Path) -> Result<bool> {
        let journal_file = vfs.open(path, OpenMode::ReadOnly)?;
        let mut block = vec![0u8; journal::JOURNAL_HEADER_SIZE];
        journal_file.read_at(&mut block, 0)?;
        Ok(journal::JournalHeader::parse(&block)?.is_some())
    }

    /// Drop the cache when the on-disk change counter has moved past the
    /// cached header (another connection committed).
    fn refresh_if_changed(&mut self) -> Result<()> {
        if self.txn.is_some() {
            return Ok(());
        }
        let mut header_bytes = vec![0u8; DB_HEADER_END];
        self.file.read_at(&mut header_bytes, 0)?;
        let on_disk = DatabaseHeader::parse(&header_bytes)?;
        if on_disk.change_counter != self.header.change_counter {
            self.invalidate_cache()?;
        }
        Ok(())
    }

    /// Discard every cached page and reload page 1 from disk.
    fn invalidate_cache(&mut self) -> Result<()> {
        self.cache.clear();
        self.header_dirty = false;
        let mut page_one = vec![0u8; self.page_size];
        self.file.read_at(&mut page_one, 0)?;
        verify_page_checksum(&page_one, 1)?;
        self.header = DatabaseHeader::parse(&page_one)?;
        self.cache.insert(
            1,
            Frame {
                data: page_one.into_boxed_slice(),
                dirty: false,
                generation: 1,
            },
        );
        Ok(())
    }

    fn release_writer_claim(&mut self) {
        if let Some(claim) = self.writer_claim.take() {
            writer_lock::release(&claim);
        }
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
    ///
    /// Also claims the process-global writer slot for this path: advisory
    /// file locks cannot stop two handles in the same process from both
    /// writing.
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
        let claim = writer_lock::claim(self.vfs.namespace_id(), &self.path)?;
        self.writer_claim = Some(claim);
        self.txn_generation = self.txn_generation.wrapping_add(1);
        match self.start_write_txn() {
            Ok(()) => Ok(()),
            Err(err) => {
                self.release_writer_claim();
                Err(err)
            }
        }
    }

    fn start_write_txn(&mut self) -> Result<()> {
        match self.lock {
            LockState::None => {
                self.file.lock(LockLevel::Exclusive)?;
                self.lock = LockState::Exclusive;
            }
            LockState::Shared => {
                self.file.unlock(LockLevel::Shared)?;
                self.lock = LockState::None;
                self.file.lock(LockLevel::Exclusive)?;
                self.lock = LockState::Exclusive;
            }
            LockState::Exclusive => {}
        }
        // A crashed writer may have left a hot journal; replay it before we
        // build on top of a partially committed state. Otherwise, pick up
        // commits made by other connections.
        if self.vfs.exists(&self.journal_path)? {
            let journal_file = self.vfs.open(&self.journal_path, OpenMode::ReadWrite)?;
            journal::recover(&*self.file, &*journal_file)?;
            drop(journal_file);
            self.vfs.remove(&self.journal_path)?;
            self.invalidate_cache()?;
        } else {
            self.refresh_if_changed()?;
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
        // Checksums are computed once per page here rather than on every
        // mutation: intermediate page states never reach the disk.
        for page_no in &dirty_pages {
            let frame = self.cache.get_mut(page_no).expect("dirty page in cache");
            write_page_checksum(&mut frame.data);
        }
        for page_no in &dirty_pages {
            let frame = self.cache.get(page_no).expect("dirty page in cache");
            let offset = (*page_no as u64 - 1) * self.page_size as u64;
            self.file.write_at(&frame.data, offset)?;
        }
        // Normal sync: the fully flushed journal is what guarantees
        // crash-consistency, as in SQLite.
        self.file.sync_data()?;
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
        let result: Result<()> = (|| {
            if committed {
                // Invalidate the journal so a resurrected file is ignored,
                // then delete it. A normal sync is enough: if the zeroing is
                // durable the resurrected journal is stale, and if it is not
                // the unlink was not durable either.
                txn.journal.write_at(&[0u8; 8], 0)?;
                txn.journal.sync_data()?;
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
            Ok(())
        })();
        // Release the writer slot and file lock even when cleanup failed.
        self.release_writer_claim();
        self.txn_generation = self.txn_generation.wrapping_add(1);
        let unlock = self.file.unlock(LockLevel::Exclusive);
        self.lock = LockState::None;
        result?;
        unlock?;
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
                let written = journal::write_record(
                    txn.journal.as_ref(),
                    page_no,
                    &frame.data,
                    txn.journal_offset,
                )?;
                txn.journal_offset += written;
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
        Ok(())
    }
}

impl Drop for Pager {
    fn drop(&mut self) {
        // A handle dropped mid-transaction must not leave its writer slot
        // claimed; the journal is recovered by the next writer or reader.
        self.release_writer_claim();
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
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::mem::{FaultConfig, MemVfs, new_store};
    use proptest::prelude::*;

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
    #[test]
    fn reader_refreshes_cache_after_other_handle_commits() {
        let store = new_store();
        let path = Path::new("refresh.db");
        {
            let vfs = Arc::new(MemVfs::from_store(store.clone()));
            let mut pager = create_pager(vfs, path);
            pager.begin_write().expect("begin");
            let page = pager.alloc_page().expect("alloc");
            init_page(&mut pager, page, PageType::TableLeaf).expect("init");
            pager.commit().expect("commit");
        }
        let vfs = Arc::new(MemVfs::from_store(store.clone()));
        let mut reader = Pager::open(vfs, path, OpenMode::ReadWrite).expect("reader");
        reader.begin_read().expect("read");
        assert_eq!(reader.page(2).expect("page").data()[100], 0);
        reader.end_read().expect("end read");

        {
            let vfs = Arc::new(MemVfs::from_store(store.clone()));
            let mut writer = Pager::open(vfs, path, OpenMode::ReadWrite).expect("writer");
            writer.begin_write().expect("begin");
            {
                let data = writer.page_mut(2).expect("mutate");
                data[100] = 0xAB;
                write_page_checksum(data);
            }
            writer.commit().expect("commit");
        }

        // Stale until the reader takes a read lock again.
        reader.begin_read().expect("refresh");
        assert_eq!(reader.page(2).expect("page").data()[100], 0xAB);
        reader.end_read().expect("end read");
    }

    #[test]
    fn reader_recovers_hot_journal_on_begin_read() {
        let store = new_store();
        let path = Path::new("hot-reader.db");
        let original;
        {
            let vfs = Arc::new(MemVfs::from_store(store.clone()));
            let mut pager = create_pager(vfs, path);
            pager.begin_write().expect("begin");
            let page = pager.alloc_page().expect("alloc");
            init_page(&mut pager, page, PageType::TableLeaf).expect("init");
            pager.commit().expect("commit");
            original = pager.page(page).expect("page").data().to_vec();
        }
        let vfs = Arc::new(MemVfs::from_store(store.clone()));
        let mut reader = Pager::open(vfs, path, OpenMode::ReadWrite).expect("reader");
        assert_eq!(reader.page(2).expect("page").data(), original.as_slice());

        // A second handle crashes mid-commit after writing database pages.
        {
            let vfs = MemVfs::from_store(store.clone());
            let shared: Arc<dyn Vfs> = Arc::new(vfs.clone());
            let mut writer = Pager::open(shared, path, OpenMode::ReadWrite).expect("writer");
            writer.begin_write().expect("begin");
            {
                let data = writer.page_mut(2).expect("mutate");
                data[100] = 0xEE;
                write_page_checksum(data);
            }
            vfs.inject_fault(FaultConfig {
                fail_after_ops: Some(6),
                torn_write: false,
            });
            writer.commit().expect_err("crash");
        }

        // The reader detects the hot journal and rolls it back before reading.
        reader.begin_read().expect("recover");
        assert_eq!(reader.page(2).expect("page").data(), original.as_slice());
        reader.end_read().expect("end read");
    }

    #[test]
    fn read_only_reader_rejects_hot_journal() {
        let store = new_store();
        let path = Path::new("hot-readonly.db");
        {
            let vfs = Arc::new(MemVfs::from_store(store.clone()));
            let mut pager = create_pager(vfs, path);
            pager.begin_write().expect("begin");
            let page = pager.alloc_page().expect("alloc");
            init_page(&mut pager, page, PageType::TableLeaf).expect("init");
            pager.commit().expect("commit");
        }
        let vfs = Arc::new(MemVfs::from_store(store.clone()));
        let mut reader = Pager::open(vfs, path, OpenMode::ReadOnly).expect("reader");
        {
            let vfs = MemVfs::from_store(store.clone());
            let shared: Arc<dyn Vfs> = Arc::new(vfs.clone());
            let mut writer = Pager::open(shared, path, OpenMode::ReadWrite).expect("writer");
            writer.begin_write().expect("begin");
            {
                let data = writer.page_mut(2).expect("mutate");
                data[100] = 0xEE;
                write_page_checksum(data);
            }
            vfs.inject_fault(FaultConfig {
                fail_after_ops: Some(6),
                torn_write: false,
            });
            writer.commit().expect_err("crash");
        }
        let err = reader.begin_read().expect_err("hot journal");
        assert_eq!(err.code(), ErrorCode::ReadOnly);
    }

    #[test]
    fn second_writer_handle_is_busy_until_released() {
        let store = new_store();
        let path = Path::new("one-writer.db");
        {
            let vfs = Arc::new(MemVfs::from_store(store.clone()));
            drop(create_pager(vfs, path));
        }
        let mut a = Pager::open(
            Arc::new(MemVfs::from_store(store.clone())),
            path,
            OpenMode::ReadWrite,
        )
        .expect("a");
        let mut b = Pager::open(
            Arc::new(MemVfs::from_store(store.clone())),
            path,
            OpenMode::ReadWrite,
        )
        .expect("b");
        a.begin_write().expect("a writes");
        let err = b.begin_write().expect_err("b blocked");
        assert_eq!(err.code(), ErrorCode::Busy);
        a.commit().expect("commit");
        b.begin_write().expect("b after commit");
        b.rollback().expect("rollback");
        // Dropping a handle mid-transaction releases the slot.
        a.begin_write().expect("a again");
        drop(a);
        b.begin_write().expect("b after drop");
        b.rollback().expect("rollback");
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        /// A commit interrupted at a random I/O operation (with or without a
        /// torn write) must leave the database at a consistent state: either
        /// every page and the version show the committed prefix, or all of
        /// them show the new transaction. Mixed states are corruption.
        #[test]
        fn interrupted_commits_recover_to_a_consistent_state(
            committed_txns in 1u32..4,
            fail_after in proptest::option::of(0u64..45),
            torn in any::<bool>(),
        ) {
            let store = new_store();
            let path = Path::new("proptest-recovery.db");
            let mut allocated: Vec<(u32, u8)> = Vec::new();
            let result;
            {
                let vfs = MemVfs::from_store(store.clone());
                let shared: Arc<dyn Vfs> = Arc::new(vfs.clone());
                let mut pager = create_pager(shared, path);
                for round in 1..=committed_txns {
                    pager.begin_write().expect("begin");
                    let page = pager.alloc_page().expect("alloc");
                    init_page(&mut pager, page, PageType::TableLeaf).expect("init");
                    {
                        let data = pager.page_mut(page).expect("mutate");
                        data[512] = round as u8;
                        write_page_checksum(data);
                    }
                    pager.update_header(|header| header.user_version = round);
                    pager.commit().expect("commit");
                    allocated.push((page, round as u8));
                }

                pager.begin_write().expect("begin");
                for (page, _) in &allocated {
                    let data = pager.page_mut(*page).expect("mutate");
                    data[512] = data[512].wrapping_add(100);
                    write_page_checksum(data);
                }
                pager.update_header(|header| header.user_version = committed_txns + 1);
                vfs.inject_fault(FaultConfig { fail_after_ops: fail_after, torn_write: torn });
                result = pager.commit();
            }

            let vfs = Arc::new(MemVfs::from_store(store));
            let mut pager = Pager::open(vfs, path, OpenMode::ReadWrite).expect("reopen recovers");
            for page in 2..pager.header().page_count as u32 {
                pager.page(page).expect("every page passes its checksum");
            }

            let version = pager.header().user_version;
            let counter = pager.header().change_counter;
            assert_eq!(counter, version, "version and change counter move together");
            let old_state = version == committed_txns;
            let new_state = version == committed_txns + 1;
            assert!(old_state || new_state, "no mixed state: version {version}");
            if result.is_ok() {
                assert!(new_state, "a successful commit is visible");
            }
            for (page, round) in &allocated {
                let byte = pager.page(*page).expect("page").data()[512];
                let expected = if new_state { round.wrapping_add(100) } else { *round };
                assert_eq!(byte, expected, "page {page} belongs to the same state");
            }
        }
    }
}
