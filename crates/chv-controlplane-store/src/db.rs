use serde::Deserialize;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};
use std::borrow::Cow;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;
use thiserror::Error;

pub type StorePool = SqlitePool;

const DEFAULT_MAX_CONNECTIONS: u32 = 16;
const DEFAULT_ACQUIRE_TIMEOUT_SECS: u64 = 5;
const DEFAULT_MIGRATIONS_DIR: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../cmd/chv-controlplane/migrations"
);
const BACKUP_DIR: &str = "/var/lib/chv/backups";
const MAX_BACKUPS: usize = 10;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("database connection error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("migration error: {0}")]
    Migration(#[from] sqlx::migrate::MigrateError),
    #[error("{entity} with id '{id}' not found")]
    NotFound { entity: &'static str, id: String },
    #[error("stale generation for {entity} '{id}': incoming {incoming} not newer than current")]
    StaleGeneration {
        entity: &'static str,
        id: String,
        incoming: i64,
    },
    #[error("stale version for {entity} '{id}': expected {expected}, current {current}")]
    StaleVersion {
        entity: &'static str,
        id: String,
        current: i64,
        expected: i64,
    },
    #[error("conflict on {entity} '{id}': {reason}")]
    Conflict {
        entity: &'static str,
        id: String,
        reason: &'static str,
    },
    #[error("invalid store configuration: {reason}")]
    InvalidConfiguration { reason: String },
    #[error("not implemented: {reason}")]
    NotImplemented { reason: &'static str },
}

#[derive(Debug, Clone, Deserialize)]
pub struct ControlPlaneStoreConfig {
    pub database_url: String,
    #[serde(default = "default_migrations_dir")]
    pub migrations_dir: PathBuf,

    #[serde(default = "default_max_connections")]
    pub max_connections: u32,

    #[serde(default = "default_acquire_timeout_secs")]
    pub acquire_timeout_secs: u64,
}

fn default_max_connections() -> u32 {
    DEFAULT_MAX_CONNECTIONS
}

fn default_acquire_timeout_secs() -> u64 {
    DEFAULT_ACQUIRE_TIMEOUT_SECS
}

fn default_migrations_dir() -> PathBuf {
    PathBuf::from(DEFAULT_MIGRATIONS_DIR)
}

pub fn build_connect_options(
    config: &ControlPlaneStoreConfig,
) -> Result<SqliteConnectOptions, StoreError> {
    Ok(SqliteConnectOptions::from_str(&config.database_url)?
        .create_if_missing(true)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
        .synchronous(sqlx::sqlite::SqliteSynchronous::Normal)
        .pragma("foreign_keys", "ON")
        .pragma("mmap_size", "0")
        .busy_timeout(Duration::from_secs(5)))
}

pub fn build_pool_options(config: &ControlPlaneStoreConfig) -> SqlitePoolOptions {
    SqlitePoolOptions::new()
        .max_connections(config.max_connections)
        .acquire_timeout(Duration::from_secs(config.acquire_timeout_secs))
}

pub async fn connect_pool(config: &ControlPlaneStoreConfig) -> Result<StorePool, StoreError> {
    let connect_options = build_connect_options(config)?;
    let pool = build_pool_options(config)
        .connect_with(connect_options)
        .await?;

    // Run a quick integrity check on startup to detect corruption early
    let result: String = sqlx::query_scalar("PRAGMA integrity_check(1)")
        .fetch_one(&pool)
        .await?;
    if result != "ok" {
        return Err(StoreError::InvalidConfiguration {
            reason: format!("database integrity check failed: {}", result),
        });
    }

    // The sqlite file is created with the process umask (typically 0644 for
    // group/world read): tighten to owner-only. The `-wal` and `-shm`
    // sidecars are tightened too — SQLite creates them on first write with
    // the same umask, and the WAL carries committed data. Best-effort — a
    // memory database, a missing file, or a permission problem must not
    // fail startup, but the failure is logged so a misconfigured deployment
    // is visible.
    if let Some(path) = sqlite_file_path(&config.database_url) {
        let mut targets = vec![path.clone()];
        let mut wal = path.clone().into_os_string();
        wal.push("-wal");
        targets.push(wal.into());
        let mut shm = path.into_os_string();
        shm.push("-shm");
        targets.push(shm.into());
        for target in targets {
            if !target.exists() {
                continue;
            }
            if let Err(error) = tighten_file_mode(&target) {
                tracing::warn!(
                    path = %target.display(),
                    error = %error,
                    "cannot tighten control-plane database file permissions"
                );
            }
        }
    }

    Ok(pool)
}

/// Extracts the on-disk database path from a `sqlite://` URL, if any
/// (memory databases and URLs without a file component return `None`).
fn sqlite_file_path(url: &str) -> Option<PathBuf> {
    let stripped = url
        .strip_prefix("sqlite://")
        .or_else(|| url.strip_prefix("sqlite:"))?;
    let stripped = stripped.split('?').next().unwrap_or("");
    if stripped.is_empty() || stripped == ":memory:" {
        return None;
    }
    Some(PathBuf::from(stripped))
}

#[cfg(unix)]
fn tighten_file_mode(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let metadata = std::fs::metadata(path)?;
    let mut permissions = metadata.permissions();
    permissions.set_mode(permissions.mode() & !0o077);
    std::fs::set_permissions(path, permissions)
}

#[cfg(not(unix))]
fn tighten_file_mode(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

pub fn migrations_path(config: Option<&ControlPlaneStoreConfig>) -> Cow<'_, Path> {
    match config {
        Some(config) => Cow::Borrowed(config.migrations_dir.as_path()),
        None => Cow::Borrowed(Path::new(DEFAULT_MIGRATIONS_DIR)),
    }
}

pub async fn migrator(
    config: Option<&ControlPlaneStoreConfig>,
) -> Result<sqlx::migrate::Migrator, StoreError> {
    Ok(sqlx::migrate::Migrator::new(migrations_path(config).as_ref()).await?)
}

async fn has_pending_migrations(
    pool: &StorePool,
    migrator: &sqlx::migrate::Migrator,
) -> Result<bool, sqlx::Error> {
    let applied: Vec<i64> =
        sqlx::query_scalar::<sqlx::Sqlite, i64>("SELECT version FROM _sqlx_migrations")
            .fetch_all(pool)
            .await
            .unwrap_or_default();

    let applied_set: HashSet<i64> = applied.into_iter().collect();

    for migration in migrator.iter() {
        if migration.migration_type.is_up_migration() && !applied_set.contains(&migration.version) {
            return Ok(true);
        }
    }

    Ok(false)
}

/// Copies the database to `backup_path` as an owner-only (`0600`) file.
///
/// The database contains encrypted (and legacy plaintext) credentials, so a
/// pre-migration backup must never be group/world readable. On Unix the
/// destination is created `0600` and then set explicitly to `0600` because an
/// existing file may carry broader bits from an earlier run. Any failure to
/// enforce the mode removes the partial file and returns the error, so the
/// caller skips the backup instead of leaving a readable copy.
#[cfg(unix)]
fn copy_database_backup(db_path: &Path, backup_path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let mut source = std::fs::File::open(db_path)?;
    let mut backup = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(backup_path)?;

    if let Err(error) = backup.set_permissions(std::fs::Permissions::from_mode(0o600)) {
        drop(backup);
        let _ = std::fs::remove_file(backup_path);
        return Err(error);
    }

    if let Err(error) = std::io::copy(&mut source, &mut backup) {
        drop(backup);
        let _ = std::fs::remove_file(backup_path);
        return Err(error);
    }

    Ok(())
}

#[cfg(not(unix))]
fn copy_database_backup(db_path: &Path, backup_path: &Path) -> std::io::Result<()> {
    std::fs::copy(db_path, backup_path).map(|_| ())
}

fn rotate_backups(backup_dir: &Path, db_filename: &str, keep: usize) -> Result<(), std::io::Error> {
    let prefix = format!("{}.", db_filename);
    let suffix = ".bak";

    let mut entries: Vec<_> = std::fs::read_dir(backup_dir)?
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name.starts_with(&prefix) && name.ends_with(suffix)
        })
        .collect();

    if entries.len() <= keep {
        return Ok(());
    }

    entries.sort_by_key(|a| a.file_name());

    for entry in entries.iter().take(entries.len() - keep) {
        if let Err(e) = std::fs::remove_file(entry.path()) {
            tracing::warn!(
                path = %entry.path().display(),
                error = %e,
                "failed to remove old backup during rotation"
            );
        }
    }

    Ok(())
}

pub async fn run_migrations(
    pool: &StorePool,
    config: Option<&ControlPlaneStoreConfig>,
) -> Result<(), StoreError> {
    let migrator = migrator(config).await?;

    if let Some(config) = config {
        if config.database_url.starts_with("sqlite://") && !config.database_url.contains(":memory:")
        {
            let db_path = &config.database_url["sqlite://".len()..];

            let should_backup = match std::fs::metadata(db_path) {
                Ok(meta) => meta.len() > 0,
                Err(_) => false,
            };

            if should_backup {
                let has_pending = match has_pending_migrations(pool, &migrator).await {
                    Ok(pending) => pending,
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "failed to check for pending migrations, assuming migrations are pending"
                        );
                        true
                    }
                };

                if has_pending {
                    // The database runs in WAL mode: recent commits may only
                    // exist in the `-wal` sidecar, which the file copy below
                    // does not include. Checkpoint them into the main file
                    // first so the backup is a complete snapshot; on failure
                    // continue with a warning rather than skipping the
                    // pre-migration backup entirely. The checkpoint returns a
                    // result ROW (busy, log, checkpointed) rather than an
                    // error when it cannot complete: read it and warn when
                    // frames were left behind, instead of silently copying
                    // a main file that misses recent commits.
                    match sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
                        .fetch_one(pool)
                        .await
                    {
                        Ok(row) => {
                            let busy: i64 = row.try_get(0).unwrap_or(0);
                            if busy != 0 {
                                tracing::warn!(
                                    busy,
                                    "WAL checkpoint was busy before backup; backup may miss recent commits"
                                );
                            }
                        }
                        Err(e) => {
                            tracing::warn!(
                                error = %e,
                                "failed to checkpoint WAL before backup; backup may miss recent commits"
                            );
                        }
                    }
                    let backup_dir = Path::new(BACKUP_DIR);
                    if let Err(e) = std::fs::create_dir_all(backup_dir) {
                        tracing::error!(
                            error = %e,
                            backup_dir = %backup_dir.display(),
                            "failed to create backup directory, continuing without backup"
                        );
                    } else {
                        let timestamp = chrono::Utc::now().format("%Y%m%d%H%M%S");
                        let db_filename = Path::new(db_path)
                            .file_name()
                            .and_then(|n| n.to_str())
                            .unwrap_or("controlplane.db");
                        let backup_path =
                            backup_dir.join(format!("{}.{}.bak", db_filename, timestamp));

                        match copy_database_backup(Path::new(db_path), &backup_path) {
                            Ok(_) => {
                                tracing::info!(
                                    backup_path = %backup_path.display(),
                                    "created pre-migration database backup"
                                );

                                if let Err(e) = rotate_backups(backup_dir, db_filename, MAX_BACKUPS)
                                {
                                    tracing::warn!(
                                        error = %e,
                                        "failed to rotate old backups"
                                    );
                                }
                            }
                            Err(e) => {
                                tracing::error!(
                                    error = %e,
                                    "failed to create pre-migration backup of database, continuing"
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    migrator.run(pool).await?;
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn pre_migration_backup_is_owner_only() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = dir.path().join("controlplane.db");
        std::fs::write(&source, b"encrypted credential rows").expect("write source");
        // A typical SQLite file created under the process umask.
        std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o644))
            .expect("widen source mode");

        let backup = dir.path().join("controlplane.db.20260101000000.bak");
        copy_database_backup(&source, &backup).expect("copy backup");
        assert_eq!(
            std::fs::metadata(&backup)
                .expect("backup metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600,
            "a newly created pre-migration backup must be owner-only"
        );

        // An existing backup with broader bits must be tightened too.
        std::fs::set_permissions(&backup, std::fs::Permissions::from_mode(0o644))
            .expect("widen backup mode");
        copy_database_backup(&source, &backup).expect("overwrite backup");
        assert_eq!(
            std::fs::metadata(&backup)
                .expect("backup metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600,
            "an existing pre-migration backup must be tightened to owner-only"
        );
        assert_eq!(
            std::fs::read(&backup).expect("read backup"),
            b"encrypted credential rows"
        );
    }
}
