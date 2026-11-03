//! Single-instance advisory lock for the control plane.
//!
//! Two `chv-controlplane` processes sharing one SQLite database is never a
//! supported topology: both would run migrations, seed starter topologies, and
//! race every worker. This guard takes an exclusive, non-blocking advisory lock
//! next to the SQLite database file (or in `runtime_dir` for in-memory/unknown
//! URLs) *before* the database is opened or migrated.
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
    ///
    /// Production startup uses [`Self::acquire_for_database`]; this
    /// runtime-dir form exists only for unit tests that need a
    /// directory-keyed guard.
    #[cfg(test)]
    pub fn acquire(runtime_dir: &Path) -> Result<Self, LockError> {
        Self::acquire_at(&runtime_dir.join(LOCK_FILE_NAME))
    }

    /// Acquire the lock that protects `database_url`, independent of which
    /// runtime directory a particular unit/config chooses.
    ///
    /// The invariant that matters is "at most one control plane per SQLite
    /// database". Keying the lock to `runtime_dir` only enforced it while both
    /// processes happened to share that setting; two units with different
    /// `[controlplane] runtime_dir` values (or one with a bind-mounted runtime
    /// dir) could still race migrations and workers against the same file.
    /// For a file-backed SQLite URL the lock lives next to the database; for
    /// in-memory/unknown URLs it falls back to the runtime directory.
    pub fn acquire_for_database(runtime_dir: &Path, database_url: &str) -> Result<Self, LockError> {
        Self::acquire_at(&lock_path_for_database(runtime_dir, database_url))
    }

    fn acquire_at(path: &Path) -> Result<Self, LockError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| LockError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }

        let path = path.to_path_buf();
        // Refuse a symlink or non-regular lock path: `set_permissions` and the
        // post-lock truncate must never be redirected at an attacker-chosen
        // file. The runtime/database directory is service-owned, so this is
        // defence in depth rather than the primary boundary.
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
        let mut options = {
            let mut options = OpenOptions::new();
            options.read(true).write(true).create(true);
            // Only applies on creation; an existing file is tightened below.
            options.mode(LOCK_FILE_MODE);
            options
        };
        // Close the lstat→open TOCTOU window: the pre-check above diagnoses a
        // symlink, but only O_NOFOLLOW makes the open itself refuse one.
        #[cfg(target_os = "linux")]
        options.custom_flags(nix::libc::O_NOFOLLOW);
        let mut file = options.open(&path).map_err(|source| LockError::Io {
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

/// Lock path for a control-plane database URL.
///
/// SQLite URLs accepted by sqlx include `sqlite:/abs/path`, `sqlite:///abs/path`,
/// `sqlite:relative.db`, and the in-memory forms (`sqlite::memory:`,
/// `sqlite:file::memory:?cache=shared`). Only file-backed URLs get a
/// database-adjacent lock; everything else falls back to the runtime directory
/// so the guard still exists.
pub fn lock_path_for_database(runtime_dir: &Path, database_url: &str) -> PathBuf {
    let Some(db_path) = sqlite_url_path(database_url) else {
        return runtime_dir.join(LOCK_FILE_NAME);
    };
    let file_name = db_path
        .file_name()
        .map(|name| name.to_os_string())
        .unwrap_or_else(|| "controlplane.db".into());
    let mut lock_name = std::ffi::OsString::from(".");
    lock_name.push(file_name);
    lock_name.push(".instance.lock");
    db_path.with_file_name(lock_name)
}

fn sqlite_url_path(database_url: &str) -> Option<PathBuf> {
    // Mirror sqlx-sqlite 0.8's `SqliteConnectOptions::from_str`: strip the
    // scheme (and the optional `//` authority marker), split only on `?`, and
    // percent-decode the database component. `#` is a legal filename byte to
    // sqlx, so treating it as a fragment would key the lock to a different
    // path than the database sqlx actually opens.
    let rest = database_url.strip_prefix("sqlite:")?;
    let rest = rest.strip_prefix("//").unwrap_or(rest);
    let database = rest.split('?').next().unwrap_or_default();
    if database.is_empty() {
        return None;
    }

    let decoded = percent_encoding::percent_decode_str(database)
        .decode_utf8()
        .ok()?;
    let decoded = decoded.as_ref();
    if decoded == ":memory:" || decoded.starts_with("file::memory:") {
        return None;
    }

    // SQLite interprets a `file:` prefix as a URI; the lock belongs next to the
    // underlying file, not next to a literal `file:` path component.
    let decoded = decoded.strip_prefix("file:").unwrap_or(decoded);
    if decoded.is_empty() {
        return None;
    }
    Some(PathBuf::from(decoded))
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

    #[test]
    fn sqlite_database_urls_resolve_to_a_database_adjacent_lock() {
        let runtime = Path::new("/run/chv/controlplane");
        assert_eq!(
            lock_path_for_database(runtime, "sqlite:/var/lib/chv/controlplane.db"),
            PathBuf::from("/var/lib/chv/.controlplane.db.instance.lock")
        );
        assert_eq!(
            lock_path_for_database(runtime, "sqlite:///var/lib/chv/controlplane.db"),
            PathBuf::from("/var/lib/chv/.controlplane.db.instance.lock")
        );
        assert_eq!(
            lock_path_for_database(runtime, "sqlite:relative.db?mode=rwc"),
            PathBuf::from(".relative.db.instance.lock")
        );
        // sqlx percent-decodes the database component; the lock must point at
        // the same file the database is opened as.
        assert_eq!(
            lock_path_for_database(runtime, "sqlite:/var/lib/chv/controlplane%20db.sqlite"),
            PathBuf::from("/var/lib/chv/.controlplane db.sqlite.instance.lock")
        );
        // `#` is a filename byte to sqlx, not a fragment delimiter.
        assert_eq!(
            lock_path_for_database(runtime, "sqlite:/var/lib/chv/controlplane#1.db"),
            PathBuf::from("/var/lib/chv/.controlplane#1.db.instance.lock")
        );
        // A percent-encoded `?` stays in the filename instead of starting the
        // query string.
        assert_eq!(
            lock_path_for_database(runtime, "sqlite:/var/lib/chv/controlplane%3F1.db"),
            PathBuf::from("/var/lib/chv/.controlplane?1.db.instance.lock")
        );
        // SQLite URI form: the lock is adjacent to the underlying path.
        assert_eq!(
            lock_path_for_database(runtime, "sqlite:file:/var/lib/chv/controlplane.db?mode=rwc"),
            PathBuf::from("/var/lib/chv/.controlplane.db.instance.lock")
        );
        // Memory and non-SQLite URLs still get a guard, rooted at the runtime
        // directory.
        assert_eq!(
            lock_path_for_database(runtime, "sqlite::memory:"),
            runtime.join(LOCK_FILE_NAME)
        );
        assert_eq!(
            lock_path_for_database(runtime, "sqlite:file::memory:?cache=shared"),
            runtime.join(LOCK_FILE_NAME)
        );
        assert_eq!(
            lock_path_for_database(runtime, "postgres://db.example/chv"),
            runtime.join(LOCK_FILE_NAME)
        );
    }

    /// The lock must key off the same path sqlx will open. Compare directly
    /// against sqlx's own URL parser for every non-memory spelling we accept.
    #[test]
    fn lock_path_matches_sqlx_filename_parsing() {
        use std::str::FromStr;

        for url in [
            "sqlite:/var/lib/chv/controlplane.db",
            "sqlite:///var/lib/chv/controlplane.db",
            "sqlite:relative.db?mode=rwc",
            "sqlite:/var/lib/chv/my%20db.sqlite",
            "sqlite:/var/lib/chv/controlplane%3F1.db",
            "sqlite:/var/lib/chv/controlplane#1.db",
        ] {
            let options = sqlx::sqlite::SqliteConnectOptions::from_str(url)
                .expect("sqlx must accept the URL");
            assert_eq!(
                sqlite_url_path(url).as_deref(),
                Some(options.get_filename()),
                "lock path must match the file sqlx opens for {url}"
            );
        }
    }

    /// Two units configured with different `runtime_dir` values but the same
    /// SQLite file must still contend: the lock is keyed to the database, not
    /// to the runtime directory.
    #[test]
    fn database_lock_is_shared_across_runtime_directories() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("controlplane.db");
        let runtime_a = dir.path().join("run-a");
        let runtime_b = dir.path().join("run-b");
        let url = format!("sqlite:{}", db.display());

        let first = InstanceLock::acquire_for_database(&runtime_a, &url)
            .expect("first acquisition must succeed");
        let err = InstanceLock::acquire_for_database(&runtime_b, &url)
            .expect_err("same database from another runtime_dir must be rejected");
        assert!(matches!(err, LockError::AlreadyHeld { .. }), "got {err:?}");

        drop(first);
        InstanceLock::acquire_for_database(&runtime_b, &url)
            .expect("acquisition after drop must succeed");
    }
}
