//! Process-global writer exclusion.
//!
//! The VFS locks are advisory `fcntl` locks on Unix, which are per-process:
//! two handles in the same process can both believe they hold the write lock.
//! This registry enforces one writer per database path per process; the VFS
//! lock still coordinates across processes.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use crate::error::{Error, ErrorCode, Result};

/// A claimed writer slot for one database path.
#[derive(Debug, Default)]
pub(crate) struct PathClaim {
    claimed: AtomicBool,
}

type Key = (usize, PathBuf);

fn registry() -> &'static Mutex<HashMap<Key, Arc<PathClaim>>> {
    static REGISTRY: OnceLock<Mutex<HashMap<Key, Arc<PathClaim>>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Normalize a path for the registry; falls back to the literal path for
/// in-memory VFS paths that do not exist on the real filesystem.
fn key(namespace: usize, path: &Path) -> Key {
    (
        namespace,
        std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()),
    )
}

/// Claim the writer slot for `path` in `namespace`, failing with `Busy` when
/// another handle in this process holds it.
pub(crate) fn claim(namespace: usize, path: &Path) -> Result<Arc<PathClaim>> {
    let mut registry = registry()
        .lock()
        .map_err(|_| Error::internal("writer registry poisoned"))?;
    let claim = registry.entry(key(namespace, path)).or_default().clone();
    claim
        .claimed
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .map_err(|_| {
            Error::new(
                ErrorCode::Busy,
                format!(
                    "another connection in this process is writing to {}",
                    path.display()
                ),
            )
        })?;
    Ok(claim)
}

/// Release a previously claimed writer slot.
pub(crate) fn release(claim: &PathClaim) {
    claim.claimed.store(false, Ordering::Release);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claim_is_exclusive_until_released() {
        let path = Path::new("writer-lock-unit.db");
        let first = claim(1, path).expect("first claim");
        let err = claim(1, path).expect_err("second claim");
        assert_eq!(err.code(), ErrorCode::Busy);
        release(&first);
        let _second = claim(1, path).expect("reclaim after release");
    }

    #[test]
    fn distinct_paths_and_namespaces_do_not_conflict() {
        let a = claim(1, Path::new("writer-lock-a.db")).expect("a");
        let b = claim(1, Path::new("writer-lock-b.db")).expect("b");
        let c = claim(2, Path::new("writer-lock-a.db")).expect("other namespace");
        release(&a);
        release(&b);
        release(&c);
    }
}
