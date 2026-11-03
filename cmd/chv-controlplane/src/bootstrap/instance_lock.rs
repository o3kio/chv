//! Single-instance advisory lock for the control plane.
//!
//! Two `chv-controlplane` processes sharing one SQLite database is never a
//! supported topology: both would run migrations, seed starter topologies, and
//! race every worker. This guard takes an exclusive, non-blocking advisory lock
//! on `${runtime_dir}/controlplane.lock` *before* the database is opened or
//! migrated.
//!
//! Mechanism: `std::fs::File::try_lock` (`flock(LOCK_EX | LOCK_NB)` on Linux).
//! The lock is owned by the open file description and is released by the kernel
//! when the process exits — including SIGTERM, SIGKILL, and crashes. There is no
//! stale-lock recovery path because there is nothing stale to recover: the file
//! may remain on disk, but the lock itself never outlives the holder.
//!
//! The guard is stored in a process-lifetime `OnceLock` by the bootstrap path,
//! so it is not tied to a short-lived function frame. The lock file is created
//! mode 0600 (and tightened if it already existed) because it carries only a
//! diagnostic PID, never credentials.

use std::fmt;
use std::fs::{File, OpenOptions, Permissions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// File name inside the configured runtime directory.
pub const LOCK_FILE_NAME: &str = "controlplane.lock";

/// Stable, operator-greppable message for the rejection path. Runbooks and
/// tests match this exact phrase; do not reword without updating both.
pub const ALREADY_RUNNING_MESSAGE: &str =
    "another chv-controlplane instance holds the database lock";

const LOCK_FILE_MODE: u32 = 0o600;

/// Exclusive advisory lock held for the lifetime of the process.
#[derive(Debug)]
pub struct InstanceLock {
    file: File,
    path: PathBuf,
}

impl InstanceLock {
    /// Acquire the lock in `runtime_dir`, creating the directory and lock file
    /// if needed.
    ///
    /// Returns [`LockError::AlreadyHeld`] when another process (or another
    /// acquisition in this process) owns the lock. Never blocks.
    pub fn acquire(runtime_dir: &Path) -> Result<Self, LockError> {
        std::fs::create_dir_all(runtime_dir).map_err(|source| LockError::Io {
            path: runtime_dir.to_path_buf(),
            source,
        })?;

        let path = runtime_dir.join(LOCK_FILE_NAME);
        // Refuse a symlink or non-regular lock path: `set_permissions` and the
        // post-lock truncate must never be redirected at an attacker-chosen
        // file. The runtime directory is service-owned, so this is defence in
        // depth rather than the primary boundary.
        match std::fs::symlink_metadata(&path) {
            Ok(meta) if !meta.file_type().is_file() => {
                return Err(LockError::Io {
                    path,
                    source: io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "lock path exists but is not a regular file",
                    ),
                });
            }
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(source) => return Err(LockError::Io { path, source }),
        }
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            // Only applies on creation; an existing file is tightened below.
            .mode(LOCK_FILE_MODE)
            .open(&path)
            .map_err(|source| LockError::Io {
                path: path.clone(),
                source,
            })?;

        // `mode` is subject to umask and does not apply to a pre-existing file.
        // Tighten unconditionally so an upgrade from a looser mode is safe.
        file.set_permissions(Permissions::from_mode(LOCK_FILE_MODE))
            .map_err(|source| LockError::Io {
                path: path.clone(),
                source,
            })?;

        match file.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                let holder = read_holder(&mut file);
                return Err(LockError::AlreadyHeld { path, holder });
            }
            Err(std::fs::TryLockError::Error(source)) => {
                return Err(LockError::Io { path, source });
            }
        }

        // Best-effort diagnostic for the next process that loses the race.
        // Failure to write the PID never invalidates the lock.
        let _ = file.set_len(0);
        let _ = file.seek(SeekFrom::Start(0));
        let _ = writeln!(
            file,
            "pid={}\nstarted_at_unix={}",
            std::process::id(),
            unix_now_secs()
        );

        Ok(Self { file, path })
    }

    /// Path of the lock file (for startup logging).
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        // Closing the descriptor releases the flock. Explicit unlock keeps the
        // intent obvious and would still run before `file` drops.
        let _ = self.file.unlock();
    }
}

fn read_holder(file: &mut File) -> Option<String> {
    let mut contents = String::new();
    let _ = file.seek(SeekFrom::Start(0));
    let _ = file.take(256).read_to_string(&mut contents);
    let contents = contents.trim();
    (!contents.is_empty()).then(|| contents.to_string())
}

fn unix_now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Failure to acquire the process-lifetime instance lock.
#[derive(Debug)]
pub enum LockError {
    /// Another live holder owns the lock. `holder` is the diagnostic PID text
    /// written by that holder, when readable.
    AlreadyHeld {
        path: PathBuf,
        holder: Option<String>,
    },
    Io {
        path: PathBuf,
        source: io::Error,
    },
}

impl fmt::Display for LockError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyHeld { path, holder } => {
                write!(f, "{ALREADY_RUNNING_MESSAGE}: {}", path.display())?;
                if let Some(holder) = holder {
                    write!(f, " (holder: {})", holder.replace('\n', " "))?;
                }
                Ok(())
            }
            Self::Io { path, source } => {
                write!(
                    f,
                    "failed to acquire control-plane lock {}: {source}",
                    path.display()
                )
            }
        }
    }
}

impl std::error::Error for LockError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::AlreadyHeld { .. } => None,
            Self::Io { source, .. } => Some(source),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_acquisition_fails_while_first_guard_alive_then_succeeds_after_drop() {
        let dir = tempfile::tempdir().expect("tempdir");
        let first = InstanceLock::acquire(dir.path()).expect("first acquisition must succeed");

        let err = InstanceLock::acquire(dir.path()).expect_err("second acquisition must fail");
        match &err {
            LockError::AlreadyHeld { path, holder } => {
                assert_eq!(path, &dir.path().join(LOCK_FILE_NAME));
                assert!(
                    holder.as_deref().is_some_and(|h| h.contains("pid=")),
                    "holder diagnostics should be readable: {holder:?}"
                );
            }
            other => panic!("expected AlreadyHeld, got {other:?}"),
        }
        assert!(
            err.to_string().contains(ALREADY_RUNNING_MESSAGE),
            "rejection must be greppable: {err}"
        );

        drop(first);
        let second =
            InstanceLock::acquire(dir.path()).expect("acquisition after drop must succeed");
        assert!(second.path().exists());
    }

    #[test]
    fn lock_file_is_created_with_private_permissions() {
        let dir = tempfile::tempdir().expect("tempdir");
        let _lock = InstanceLock::acquire(dir.path()).expect("acquisition must succeed");

        let mode = std::fs::metadata(dir.path().join(LOCK_FILE_NAME))
            .expect("lock metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, LOCK_FILE_MODE, "lock file must be 0600");
    }

    #[test]
    fn missing_runtime_directory_is_created() {
        let dir = tempfile::tempdir().expect("tempdir");
        let nested = dir.path().join("run/chv/controlplane");
        assert!(!nested.exists());

        let _lock = InstanceLock::acquire(&nested).expect("acquisition must create parent dir");
        assert!(nested.join(LOCK_FILE_NAME).exists());
    }

    #[test]
    fn existing_lock_file_is_tightened_to_private_permissions() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(LOCK_FILE_NAME);
        std::fs::write(&path, "stale diagnostic\n").expect("write pre-existing lock file");
        std::fs::set_permissions(&path, Permissions::from_mode(0o644))
            .expect("loosen pre-existing lock file");

        let _lock = InstanceLock::acquire(dir.path()).expect("acquisition must succeed");

        let mode = std::fs::metadata(&path)
            .expect("lock metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, LOCK_FILE_MODE, "pre-existing mode must be tightened");
    }

    #[test]
    fn symlink_lock_path_is_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("not-the-lock");
        std::fs::write(&target, b"").expect("write symlink target");
        std::os::unix::fs::symlink(&target, dir.path().join(LOCK_FILE_NAME))
            .expect("create symlink");

        let err = InstanceLock::acquire(dir.path()).expect_err("symlink must be rejected");
        assert!(matches!(err, LockError::Io { .. }), "got {err:?}");
    }
}
