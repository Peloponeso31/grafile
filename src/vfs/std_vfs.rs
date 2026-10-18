//! Real-filesystem VFS implementation backed by `std::fs` and `fs4` locks.

use std::fs::{File as StdFile, OpenOptions};
use std::path::{Path, PathBuf};

use fs4::FileExt as Fs4FileExt;

use super::{File, LockLevel, OpenMode, Vfs};
use crate::error::{Error, ErrorCode, Result};

/// VFS backed by the operating system's filesystem.
///
/// Positioned I/O uses `pread`/`pwrite` on Unix and `seek_read`/`seek_write`
/// on Windows, so handles are usable from multiple threads without a shared
/// seek cursor.
#[derive(Debug, Default, Clone, Copy)]
pub struct StdVfs;

impl Vfs for StdVfs {
    fn open(&self, path: &Path, mode: OpenMode) -> Result<Box<dyn File>> {
        let mut options = OpenOptions::new();
        match mode {
            OpenMode::ReadOnly => {
                options.read(true);
            }
            OpenMode::ReadWrite => {
                options.read(true).write(true);
            }
            OpenMode::Create => {
                options.read(true).write(true).create(true);
            }
        }
        let file = options.open(path)?;
        Ok(Box::new(StdFileHandle {
            file,
            path: path.to_path_buf(),
        }))
    }

    fn remove(&self, path: &Path) -> Result<()> {
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err.into()),
        }
    }

    fn exists(&self, path: &Path) -> Result<bool> {
        Ok(path.exists())
    }

    fn rename(&self, from: &Path, to: &Path) -> Result<()> {
        std::fs::rename(from, to)?;
        Ok(())
    }
}

struct StdFileHandle {
    file: StdFile,
    path: PathBuf,
}

#[cfg(unix)]
fn read_exact_at(file: &StdFile, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
    std::os::unix::fs::FileExt::read_exact_at(file, buf, offset)
}

#[cfg(unix)]
fn write_all_at(file: &StdFile, buf: &[u8], offset: u64) -> std::io::Result<()> {
    std::os::unix::fs::FileExt::write_all_at(file, buf, offset)
}

#[cfg(windows)]
fn read_exact_at(file: &StdFile, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
    use std::os::windows::fs::FileExt;
    let mut read = 0usize;
    while read < buf.len() {
        let n = file.seek_read(&mut buf[read..], offset + read as u64)?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "read past end of file",
            ));
        }
        read += n;
    }
    Ok(())
}

#[cfg(windows)]
fn write_all_at(file: &StdFile, buf: &[u8], offset: u64) -> std::io::Result<()> {
    use std::os::windows::fs::FileExt;
    let mut written = 0usize;
    while written < buf.len() {
        let n = file.seek_write(&buf[written..], offset + written as u64)?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "failed to write buffer",
            ));
        }
        written += n;
    }
    Ok(())
}

/// Flush file data with normal `fsync` semantics.
///
/// `std::fs::File::sync_data` maps to `F_FULLFSYNC` on macOS, which also
/// flushes the drive's write cache and costs milliseconds per call. The
/// journal is flushed with [`std::fs::File::sync_all`] (full durability);
/// database pages and journal invalidation use this cheaper sync, matching
/// SQLite's `SQLITE_SYNC_NORMAL` for the same steps.
#[cfg(unix)]
fn sync_data(file: &StdFile) -> std::io::Result<()> {
    use std::os::unix::io::AsRawFd;
    // SAFETY: the descriptor belongs to `file` and stays open for the call.
    let result = unsafe { libc::fsync(file.as_raw_fd()) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(not(unix))]
fn sync_data(file: &StdFile) -> std::io::Result<()> {
    file.sync_data()
}

impl File for StdFileHandle {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> Result<()> {
        read_exact_at(&self.file, buf, offset)?;
        Ok(())
    }

    fn write_at(&self, buf: &[u8], offset: u64) -> Result<()> {
        write_all_at(&self.file, buf, offset)?;
        Ok(())
    }

    fn size(&self) -> Result<u64> {
        Ok(self.file.metadata()?.len())
    }

    fn set_len(&self, len: u64) -> Result<()> {
        self.file.set_len(len)?;
        Ok(())
    }

    fn sync_data(&self) -> Result<()> {
        sync_data(&self.file)?;
        Ok(())
    }

    fn sync_all(&self) -> Result<()> {
        self.file.sync_all()?;
        Ok(())
    }

    fn lock(&self, level: LockLevel) -> Result<()> {
        let result = match level {
            LockLevel::Shared => Fs4FileExt::lock_shared(&self.file),
            LockLevel::Exclusive => Fs4FileExt::lock(&self.file),
        };
        result.map_err(|err| {
            let code = if err.kind() == std::io::ErrorKind::WouldBlock {
                ErrorCode::Busy
            } else {
                ErrorCode::Io
            };
            Error::new(code, format!("lock {} failed: {err}", self.path.display()))
        })
    }

    fn unlock(&self, _level: LockLevel) -> Result<()> {
        Fs4FileExt::unlock(&self.file).map_err(|err| {
            Error::new(
                ErrorCode::Io,
                format!("unlock {} failed: {err}", self.path.display()),
            )
        })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_write_and_read_roundtrip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("file.db");
        let vfs = StdVfs;
        let file = vfs.open(&path, OpenMode::Create).expect("open");
        file.write_at(b"hello", 0).expect("write");
        assert_eq!(file.size().expect("size"), 5);
        file.sync_all().expect("sync");
        drop(file);

        let file = vfs.open(&path, OpenMode::ReadWrite).expect("reopen");
        let mut buf = [0u8; 5];
        file.read_at(&mut buf, 0).expect("read");
        assert_eq!(&buf, b"hello");
    }

    #[test]
    fn writes_are_positioned_without_a_seek_cursor() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("positioned.db");
        let vfs = StdVfs;
        let file = vfs.open(&path, OpenMode::Create).expect("open");
        file.write_at(b"hello", 4).expect("write");
        assert_eq!(file.size().expect("size"), 9);
        let mut gap = [0u8; 4];
        file.read_at(&mut gap, 0).expect("read gap");
        assert_eq!(gap, [0, 0, 0, 0], "the gap reads as zeros");
        let mut tail = [0u8; 5];
        file.read_at(&mut tail, 4).expect("read tail");
        assert_eq!(&tail, b"hello");
    }

    #[test]
    fn set_len_truncates_and_extends() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("resize.db");
        let vfs = StdVfs;
        let file = vfs.open(&path, OpenMode::Create).expect("open");
        file.write_at(b"abcdefghij", 0).expect("write");
        file.set_len(4).expect("truncate");
        assert_eq!(file.size().expect("size"), 4);
        let mut buf = [0u8; 4];
        file.read_at(&mut buf, 0).expect("read");
        assert_eq!(&buf, b"abcd");
        let mut past_end = [0u8; 1];
        assert!(file.read_at(&mut past_end, 4).is_err(), "read past the end");
        file.set_len(8).expect("extend");
        assert_eq!(file.size().expect("size"), 8);
        file.read_at(&mut past_end, 7).expect("read extended tail");
        assert_eq!(past_end, [0]);
    }

    #[test]
    fn opening_a_missing_file_fails() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("missing.db");
        let vfs = StdVfs;
        assert!(vfs.open(&path, OpenMode::ReadOnly).is_err());
        assert!(vfs.open(&path, OpenMode::ReadWrite).is_err());
        assert!(!vfs.exists(&path).expect("exists"));
    }

    #[test]
    fn remove_is_idempotent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("gone.db");
        let vfs = StdVfs;
        vfs.remove(&path).expect("missing is fine");
        let file = vfs.open(&path, OpenMode::Create).expect("create");
        drop(file);
        assert!(vfs.exists(&path).expect("exists"));
        vfs.remove(&path).expect("remove");
        assert!(!vfs.exists(&path).expect("exists"));
        vfs.remove(&path).expect("second remove is fine");
    }

    #[test]
    fn rename_replaces_the_target() {
        let dir = tempfile::tempdir().expect("tempdir");
        let from = dir.path().join("from.db");
        let to = dir.path().join("to.db");
        let vfs = StdVfs;
        let file = vfs.open(&from, OpenMode::Create).expect("create");
        file.write_at(b"data", 0).expect("write");
        drop(file);

        let stale = vfs.open(&to, OpenMode::Create).expect("create target");
        drop(stale);
        vfs.rename(&from, &to).expect("rename");
        assert!(!vfs.exists(&from).expect("exists"));
        let file = vfs.open(&to, OpenMode::ReadOnly).expect("open target");
        let mut buf = [0u8; 4];
        file.read_at(&mut buf, 0).expect("read");
        assert_eq!(&buf, b"data");
        assert_eq!(file.path(), to.as_path());
    }
}
