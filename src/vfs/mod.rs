//! Virtual file system layer (grafile's analogue of SQLite's `os.c`).
//!
//! The pager never talks to `std::fs` directly; it goes through the [`Vfs`]
//! and [`File`] traits. This keeps the engine testable (in-memory files with
//! fault injection) and portable (positioned I/O is implemented per platform
//! in [`std_vfs`]).

pub mod mem;
pub mod std_vfs;

use std::path::{Path, PathBuf};

use crate::error::Result;

pub use mem::{FaultConfig, MemVfs};
pub use std_vfs::StdVfs;

/// How a database file is opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenMode {
    /// Open an existing file for reading only.
    ReadOnly,
    /// Open an existing file for reading and writing.
    ReadWrite,
    /// Create the file (or open it if it exists) for reading and writing.
    Create,
}

impl OpenMode {
    /// Whether writes are permitted through this handle.
    pub const fn is_writable(self) -> bool {
        matches!(self, Self::ReadWrite | Self::Create)
    }
}

/// File lock levels, mirroring SQLite's simplified locking model.
///
/// Readers hold [`LockLevel::Shared`] for the duration of a read snapshot;
/// writers hold [`LockLevel::Exclusive`] while a write transaction commits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LockLevel {
    /// Shared read lock; multiple readers may coexist.
    Shared,
    /// Exclusive lock; no other reader or writer may hold a lock.
    Exclusive,
}

/// An open file handle with positioned (stateless) I/O.
///
/// All methods take `&self` so handles can be shared; implementations provide
/// their own synchronization where needed.
pub trait File: Send + Sync {
    /// Read exactly `buf.len()` bytes starting at `offset`.
    fn read_at(&self, buf: &mut [u8], offset: u64) -> Result<()>;

    /// Write all of `buf` starting at `offset`, extending the file if needed.
    fn write_at(&self, buf: &[u8], offset: u64) -> Result<()>;

    /// Current file length in bytes.
    fn size(&self) -> Result<u64>;

    /// Truncate or extend the file to `len` bytes.
    fn set_len(&self, len: u64) -> Result<()>;

    /// Flush file data to durable storage (metadata flush may be skipped).
    fn sync_data(&self) -> Result<()>;

    /// Flush file data and metadata to durable storage.
    fn sync_all(&self) -> Result<()>;

    /// Acquire a lock, blocking until available.
    fn lock(&self, level: LockLevel) -> Result<()>;

    /// Release a previously acquired lock.
    fn unlock(&self, level: LockLevel) -> Result<()>;

    /// Path this handle was opened from, for diagnostics.
    fn path(&self) -> &Path;
}

/// Factory for [`File`] handles. Implementations need to be shareable across
/// threads; the pager stores one behind an `Arc`.
pub trait Vfs: Send + Sync {
    /// Open `path` according to `mode`.
    fn open(&self, path: &Path, mode: OpenMode) -> Result<Box<dyn File>>;

    /// Remove `path`; succeeds if the file does not exist.
    fn remove(&self, path: &Path) -> Result<()>;

    /// Whether `path` currently exists.
    fn exists(&self, path: &Path) -> Result<bool>;

    /// Create every missing parent directory of `path`.
    fn create_dir_all(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        Ok(())
    }

    /// Rename `from` to `to`, replacing `to` if it exists.
    fn rename(&self, from: &Path, to: &Path) -> Result<()>;
}

/// Path of the rollback journal that accompanies database `path`.
///
/// The journal is transient: it exists only while a write transaction is in
/// flight and is removed on commit. The database file is self-contained at
/// rest.
pub fn journal_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push("-journal");
    PathBuf::from(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn journal_path_appends_suffix() {
        let path = Path::new("/tmp/example.grafile");
        assert_eq!(
            journal_path(path),
            PathBuf::from("/tmp/example.grafile-journal")
        );
    }

    #[test]
    fn open_mode_writability() {
        assert!(!OpenMode::ReadOnly.is_writable());
        assert!(OpenMode::ReadWrite.is_writable());
        assert!(OpenMode::Create.is_writable());
    }
}
