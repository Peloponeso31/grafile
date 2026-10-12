//! In-memory VFS with fault injection, used for crash-recovery and fuzzing.
//!
//! The backing store is a plain `HashMap<PathBuf, Vec<u8>>` behind a mutex.
//! Several [`MemVfs`] handles can share one store, which is how tests simulate
//! a process crash: run until an injected fault trips, drop everything, then
//! reopen from the same store and assert recovery behavior.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use super::{File, LockLevel, OpenMode, Vfs};
use crate::error::{Error, ErrorCode, Result};

/// Shared in-memory file store. Clone the `Arc` to reuse it across reopens.
pub type MemStore = Arc<Mutex<HashMap<PathBuf, Vec<u8>>>>;

/// Create an empty store for [`MemVfs::from_store`].
pub fn new_store() -> MemStore {
    Arc::new(Mutex::new(HashMap::new()))
}

/// Fault-injection settings. Once the configured operation count is exceeded,
/// every operation fails, simulating a process or power failure mid-write.
#[derive(Debug, Clone, Copy, Default)]
pub struct FaultConfig {
    /// Number of successful I/O operations before the first failure.
    pub fail_after_ops: Option<u64>,
    /// If set, the first failing write persists only the first half of the
    /// buffer before returning an error (simulates a torn write).
    pub torn_write: bool,
}

#[derive(Debug, Default)]
struct FaultState {
    config: Option<FaultConfig>,
    ops: u64,
    tripped: bool,
}

#[derive(Debug, Default)]
struct LockTable {
    shared: HashMap<u64, u32>,
    exclusive: Option<u64>,
}

/// In-memory [`Vfs`] implementation.
#[derive(Clone)]
pub struct MemVfs {
    store: MemStore,
    faults: Arc<Mutex<FaultState>>,
    locks: Arc<Mutex<LockTable>>,
    next_id: Arc<AtomicU64>,
}

impl Default for MemVfs {
    fn default() -> Self {
        Self::new()
    }
}

impl MemVfs {
    /// Create a VFS with a fresh, private store.
    pub fn new() -> Self {
        Self::from_store(new_store())
    }

    /// Create a VFS over an existing store (for reopen-after-crash tests).
    pub fn from_store(store: MemStore) -> Self {
        Self {
            store,
            faults: Arc::new(Mutex::new(FaultState::default())),
            locks: Arc::new(Mutex::new(LockTable::default())),
            next_id: Arc::new(AtomicU64::new(1)),
        }
    }

    /// The shared store, for snapshotting file contents in assertions.
    pub fn store(&self) -> &MemStore {
        &self.store
    }

    /// Read the whole contents of `path`, if it exists.
    pub fn contents(&self, path: &Path) -> Option<Vec<u8>> {
        let store = self.store.lock().ok()?;
        store.get(path).cloned()
    }

    /// Arm fault injection. `fail_after_ops` counts reads, writes, syncs and
    /// truncations combined.
    pub fn inject_fault(&self, config: FaultConfig) {
        let mut faults = self.faults.lock().expect("fault mutex poisoned");
        *faults = FaultState {
            config: Some(config),
            ops: 0,
            tripped: false,
        };
    }

    /// Remove fault injection and reset the operation counter.
    pub fn clear_fault(&self) {
        let mut faults = self.faults.lock().expect("fault mutex poisoned");
        *faults = FaultState::default();
    }

    /// Number of I/O operations observed since the last fault reset.
    pub fn op_count(&self) -> u64 {
        self.faults
            .lock()
            .expect("fault mutex poisoned")
            .ops
    }

    /// Whether an injected fault has tripped.
    pub fn fault_tripped(&self) -> bool {
        self.faults
            .lock()
            .expect("fault mutex poisoned")
            .tripped
    }

    fn check_fault(&self, is_write: bool) -> Result<Option<u64>> {
        let mut faults = self.faults.lock().expect("fault mutex poisoned");
        if faults.tripped {
            return Err(simulated_failure());
        }
        faults.ops += 1;
        let limit = match faults.config {
            Some(config) => config.fail_after_ops,
            None => None,
        };
        if let Some(limit) = limit {
            if faults.ops > limit {
                faults.tripped = true;
                let torn = is_write
                    && faults
                        .config
                        .map(|config| config.torn_write)
                        .unwrap_or(false);
                return if torn {
                    Ok(Some(0))
                } else {
                    Err(simulated_failure())
                };
            }
        }
        Ok(None)
    }
}

fn simulated_failure() -> Error {
    Error::new(ErrorCode::Io, "simulated crash (fault injection)")
}

impl Vfs for MemVfs {
    fn open(&self, path: &Path, mode: OpenMode) -> Result<Box<dyn File>> {
        {
            let mut store = self.store.lock().expect("store mutex poisoned");
            if mode == OpenMode::Create {
                store.entry(path.to_path_buf()).or_default();
            } else if !store.contains_key(path) {
                return Err(Error::new(
                    ErrorCode::NotFound,
                    format!("no such file: {}", path.display()),
                ));
            }
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        Ok(Box::new(MemFile {
            vfs: self.clone(),
            path: path.to_path_buf(),
            id,
        }))
    }

    fn remove(&self, path: &Path) -> Result<()> {
        let mut store = self.store.lock().expect("store mutex poisoned");
        store.remove(path);
        Ok(())
    }

    fn exists(&self, path: &Path) -> Result<bool> {
        let store = self.store.lock().expect("store mutex poisoned");
        Ok(store.contains_key(path))
    }

    fn rename(&self, from: &Path, to: &Path) -> Result<()> {
        let mut store = self.store.lock().expect("store mutex poisoned");
        if let Some(data) = store.remove(from) {
            store.insert(to.to_path_buf(), data);
        }
        Ok(())
    }
}

struct MemFile {
    vfs: MemVfs,
    path: PathBuf,
    id: u64,
}

impl File for MemFile {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> Result<()> {
        self.vfs.check_fault(false)?;
        let store = self.vfs.store.lock().expect("store mutex poisoned");
        let data = store.get(&self.path).ok_or_else(|| {
            Error::new(
                ErrorCode::NotFound,
                format!("no such file: {}", self.path.display()),
            )
        })?;
        let start = usize::try_from(offset)
            .map_err(|_| Error::new(ErrorCode::Range, "offset overflow"))?;
        let end = start
            .checked_add(buf.len())
            .ok_or_else(|| Error::new(ErrorCode::Range, "read range overflow"))?;
        if end > data.len() {
            return Err(Error::corrupt(format!(
                "short read: {} wants bytes {start}..{end}, file has {}",
                self.path.display(),
                data.len()
            )));
        }
        buf.copy_from_slice(&data[start..end]);
        Ok(())
    }

    fn write_at(&self, buf: &[u8], offset: u64) -> Result<()> {
        let torn = self.vfs.check_fault(true)?;
        let mut store = self.vfs.store.lock().expect("store mutex poisoned");
        let data = store.entry(self.path.clone()).or_default();
        let start = usize::try_from(offset)
            .map_err(|_| Error::new(ErrorCode::Range, "offset overflow"))?;
        let end = start
            .checked_add(buf.len())
            .ok_or_else(|| Error::new(ErrorCode::Range, "write range overflow"))?;
        if data.len() < end {
            data.resize(end, 0);
        }
        let effective = match torn {
            Some(_) => buf.len() / 2,
            None => buf.len(),
        };
        data[start..start + effective].copy_from_slice(&buf[..effective]);
        if torn.is_some() {
            return Err(simulated_failure());
        }
        Ok(())
    }

    fn size(&self) -> Result<u64> {
        let store = self.vfs.store.lock().expect("store mutex poisoned");
        let data = store.get(&self.path).ok_or_else(|| {
            Error::new(
                ErrorCode::NotFound,
                format!("no such file: {}", self.path.display()),
            )
        })?;
        Ok(data.len() as u64)
    }

    fn set_len(&self, len: u64) -> Result<()> {
        self.vfs.check_fault(false)?;
        let mut store = self.vfs.store.lock().expect("store mutex poisoned");
        let data = store.entry(self.path.clone()).or_default();
        data.resize(len as usize, 0);
        Ok(())
    }

    fn sync_data(&self) -> Result<()> {
        self.vfs.check_fault(false)?;
        Ok(())
    }

    fn sync_all(&self) -> Result<()> {
        self.vfs.check_fault(false)?;
        Ok(())
    }

    fn lock(&self, level: LockLevel) -> Result<()> {
        let mut locks = self.vfs.locks.lock().expect("lock mutex poisoned");
        match level {
            LockLevel::Shared => {
                if let Some(owner) = locks.exclusive {
                    if owner != self.id {
                        return Err(Error::new(
                            ErrorCode::Busy,
                            format!("exclusive lock held on {}", self.path.display()),
                        ));
                    }
                }
                *locks.shared.entry(self.id).or_insert(0) += 1;
            }
            LockLevel::Exclusive => {
                if locks.exclusive.is_some() {
                    return Err(Error::new(
                        ErrorCode::Busy,
                        format!("exclusive lock already held on {}", self.path.display()),
                    ));
                }
                let others_reading = locks
                    .shared
                    .iter()
                    .any(|(id, count)| *id != self.id && *count > 0);
                if others_reading {
                    return Err(Error::new(
                        ErrorCode::Busy,
                        format!("readers active on {}", self.path.display()),
                    ));
                }
                locks.exclusive = Some(self.id);
            }
        }
        Ok(())
    }

    fn unlock(&self, level: LockLevel) -> Result<()> {
        let mut locks = self.vfs.locks.lock().expect("lock mutex poisoned");
        match level {
            LockLevel::Shared => {
                if let Some(count) = locks.shared.get_mut(&self.id) {
                    *count = count.saturating_sub(1);
                }
            }
            LockLevel::Exclusive => {
                if locks.exclusive == Some(self.id) {
                    locks.exclusive = None;
                }
            }
        }
        Ok(())
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open(vfs: &MemVfs, path: &Path) -> Box<dyn File> {
        vfs.open(path, OpenMode::Create).expect("open failed")
    }

    #[test]
    fn write_read_roundtrip_and_extend() {
        let vfs = MemVfs::new();
        let path = PathBuf::from("db");
        let file = open(&vfs, &path);
        file.write_at(b"hello", 2).expect("write");
        assert_eq!(file.size().expect("size"), 7);
        let mut buf = [0u8; 5];
        file.read_at(&mut buf, 2).expect("read");
        assert_eq!(&buf, b"hello");
        let mut zeros = [0u8; 2];
        file.read_at(&mut zeros, 0).expect("read zeros");
        assert_eq!(zeros, [0, 0]);
    }

    #[test]
    fn short_read_is_corruption() {
        let vfs = MemVfs::new();
        let file = open(&vfs, Path::new("db"));
        file.write_at(b"abc", 0).expect("write");
        let mut buf = [0u8; 4];
        let err = file.read_at(&mut buf, 0).expect_err("should fail");
        assert_eq!(err.code(), ErrorCode::Corrupt);
    }

    #[test]
    fn fault_injection_trips_and_stays_tripped() {
        let vfs = MemVfs::new();
        let file = open(&vfs, Path::new("db"));
        vfs.inject_fault(FaultConfig {
            fail_after_ops: Some(2),
            torn_write: false,
        });
        file.write_at(b"a", 0).expect("op 1");
        file.write_at(b"b", 1).expect("op 2");
        let err = file.write_at(b"c", 2).expect_err("op 3 fails");
        assert_eq!(err.code(), ErrorCode::Io);
        assert!(vfs.fault_tripped());
        let err = file.read_at(&mut [0u8; 1], 0).expect_err("reads fail too");
        assert_eq!(err.code(), ErrorCode::Io);
    }

    #[test]
    fn torn_write_persists_half_buffer() {
        let vfs = MemVfs::new();
        let file = open(&vfs, Path::new("db"));
        vfs.inject_fault(FaultConfig {
            fail_after_ops: Some(0),
            torn_write: true,
        });
        let err = file.write_at(b"abcdef", 0).expect_err("write fails");
        assert_eq!(err.code(), ErrorCode::Io);
        // The file is extended by the full write, but only the first half of
        // the buffer lands: the rest stays zero (freshly allocated blocks).
        assert_eq!(
            vfs.contents(Path::new("db")).expect("contents"),
            [b'a', b'b', b'c', 0, 0, 0]
        );
    }

    #[test]
    fn shared_locks_coexist_exclusive_conflicts() {
        let vfs = MemVfs::new();
        let path = Path::new("db");
        let reader = open(&vfs, path);
        let writer = open(&vfs, path);
        reader.lock(LockLevel::Shared).expect("shared ok");
        writer.lock(LockLevel::Shared).expect("shared ok");
        let err = writer.lock(LockLevel::Exclusive).expect_err("conflict");
        assert_eq!(err.code(), ErrorCode::Busy);
        reader.unlock(LockLevel::Shared).expect("unlock");
        writer.unlock(LockLevel::Shared).expect("unlock");
        writer.lock(LockLevel::Exclusive).expect("exclusive now ok");
    }

    #[test]
    fn store_shared_across_reopen() {
        let store = new_store();
        {
            let vfs = MemVfs::from_store(store.clone());
            let file = open(&vfs, Path::new("db"));
            file.write_at(b"persisted", 0).expect("write");
            file.sync_all().expect("sync");
        }
        let vfs2 = MemVfs::from_store(store);
        let file = open(&vfs2, Path::new("db"));
        let mut buf = [0u8; 9];
        file.read_at(&mut buf, 0).expect("read");
        assert_eq!(&buf, b"persisted");
    }
}
