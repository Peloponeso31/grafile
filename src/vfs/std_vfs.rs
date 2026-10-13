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
