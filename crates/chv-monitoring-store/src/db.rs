use crate::config::MonitoringStoreConfig;
use crate::error::MonitoringStoreError;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{Row, SqlSafeStr, SqlitePool};
use std::borrow::Cow;
use std::str::FromStr;
use std::time::Duration;

/// The migration set embedded at compile time — the fallback when the
/// configured `migrations_dir` does not exist or holds no migrations.
/// A default-configuration control plane started from any working
/// directory must not silently degrade monitoring because a relative
/// migrations path resolved nowhere; the embedded copy is byte-ident
/// ical to the packaged files (same checksums, so a database migrated
/// from one source is compatible with the other).
///
/// When a migration file is added under
/// `cmd/chv-controlplane/monitoring-migrations/`, add its embedded
/// entry here — the test at the bottom of this file fails otherwise.
const EMBEDDED_MIGRATIONS: &[(&str, &str)] = &[
    (
        "0001_initial.sql",
        include_str!("../../../cmd/chv-controlplane/monitoring-migrations/0001_initial.sql"),
    ),
    (
        "0002_checks.sql",
        include_str!("../../../cmd/chv-controlplane/monitoring-migrations/0002_checks.sql"),
    ),
];

fn embedded_migrator() -> Result<sqlx::migrate::Migrator, MonitoringStoreError> {
    let mut migrations = Vec::new();
    for (name, sql) in EMBEDDED_MIGRATIONS {
        // Mirror the directory resolver's filename semantics exactly
        // (version prefix, reversible-direction suffix, `_` → ` ` in
        // the description) so embedded and dir-sourced migrations
        // produce identical Migration values — same version, same
        // checksum, same description.
        let (version, description) = name
            .strip_suffix(".sql")
            .and_then(|stem| {
                let (v, d) = stem.split_once('_')?;
                Some((v.parse::<i64>().ok()?, d))
            })
            .ok_or_else(|| MonitoringStoreError::Degraded {
                reason: format!("embedded migration name {name:?} is malformed"),
            })?;
        let migration_type = sqlx::migrate::MigrationType::from_filename(description);
        let description = description
            .trim_end_matches(migration_type.suffix())
            .replace('_', " ");
        migrations.push(sqlx::migrate::Migration::new(
            version,
            Cow::Owned(description),
            migration_type,
            (*sql).into_sql_str(),
            false,
        ));
    }
    // `with_migrations` sorts by version and applies the same defaults the
    // previous struct literal set explicitly (ignore_missing=false,
    // locking=true, no_tx=false).
    Ok(sqlx::migrate::Migrator::with_migrations(migrations))
}

/// The monitoring store's own pool. Deliberately separate from the
/// control-plane store pool (ADR-027): monitoring work can never hold
/// operational DB locks, and this file's failure is monitoring
/// degradation, never lifecycle failure.
#[derive(Debug, Clone)]
pub struct MonitoringStore {
    pub(crate) pool: SqlitePool,
    pub(crate) config: MonitoringStoreConfig,
}

impl MonitoringStore {
    /// Connect, verify integrity, and run migrations. A corrupt or
    /// unwritable monitoring database is reported as an error the
    /// caller turns into a degraded signal — never a startup abort of
    /// the control plane itself.
    pub async fn connect(config: MonitoringStoreConfig) -> Result<Self, MonitoringStoreError> {
        let connect_options = SqliteConnectOptions::from_str(&config.database_url)?
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .pragma("foreign_keys", "ON")
            .pragma("mmap_size", "0")
            .busy_timeout(Duration::from_secs(5));
        let pool = SqlitePoolOptions::new()
            .max_connections(config.max_connections)
            .acquire_timeout(Duration::from_secs(5))
            .connect_with(connect_options)
            .await?;

        let store = MonitoringStore {
            pool,
            config: config.clone(),
        };

        // Quick corruption probe. Unlike the operational store, a failed
        // probe does NOT brick startup: telemetry is disposable, so the
        // caller may drop and recreate the file (see `reset_corrupt`).
        let result: String = sqlx::query_scalar("PRAGMA integrity_check(1)")
            .fetch_one(&store.pool)
            .await?;
        if result != "ok" {
            return Err(MonitoringStoreError::Degraded {
                reason: format!("monitoring db integrity check failed: {result}"),
            });
        }

        // Prefer the configured migrations directory when it exists
        // and holds migrations (packaged installs point at
        // /usr/local/share/chv/monitoring-migrations); otherwise fall
        // back to the embedded copy so a default or relative-path
        // configuration still gets a migrated store instead of a
        // silent degradation.
        let migrator = match sqlx::migrate::Migrator::new(config.migrations_dir.as_path()).await {
            Ok(m) if !m.migrations.is_empty() => m,
            _ => embedded_migrator()?,
        };
        migrator.run(&store.pool).await?;
        // Truncate the WAL after migrations so a fresh file starts small.
        let _ = sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
            .execute(&store.pool)
            .await;
        Ok(store)
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    pub fn config(&self) -> &MonitoringStoreConfig {
        &self.config
    }

    /// Close the pool (used before a reset attempt).
    pub async fn close(&self) {
        self.pool.close().await;
    }

    /// The database file path derived from the configured URL, when the
    /// URL is a plain file path (the only supported shape).
    pub fn db_path(&self) -> Option<std::path::PathBuf> {
        self.config
            .database_url
            .strip_prefix("sqlite://")
            .map(std::path::PathBuf::from)
    }

    /// Current size of the main database file in bytes (WAL excluded —
    /// it is checkpointed by maintenance and bounded separately).
    pub async fn db_size_bytes(&self) -> Result<u64, MonitoringStoreError> {
        let (page_count, page_size): (i64, i64) = sqlx::query(
            "SELECT (SELECT page_count FROM pragma_page_count), (SELECT page_size FROM pragma_page_size)",
        )
        .map(|row: sqlx::sqlite::SqliteRow| (row.get::<i64, _>(0), row.get::<i64, _>(1)))
        .fetch_one(&self.pool)
        .await?;
        Ok(page_count.unsigned_abs() * page_size.unsigned_abs())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The embedded set must list exactly the repo's migration files:
    /// adding a migration without its embedded entry would make
    /// dir-configured and default-configured control planes diverge.
    #[test]
    fn embedded_migrations_match_the_repo_migration_dir() {
        let dir = std::path::Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../cmd/chv-controlplane/monitoring-migrations"
        ));
        let mut on_disk: Vec<String> = std::fs::read_dir(dir)
            .expect("repo migrations dir")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.ends_with(".sql"))
            .collect();
        on_disk.sort();
        let mut embedded: Vec<&str> = EMBEDDED_MIGRATIONS.iter().map(|(n, _)| *n).collect();
        embedded.sort_unstable();
        assert_eq!(
            on_disk, embedded,
            "a migration file under cmd/chv-controlplane/monitoring-migrations has no \
             embedded copy — add it to EMBEDDED_MIGRATIONS in db.rs"
        );
    }

    /// A default or relative `migrations_dir` that resolves nowhere
    /// must still yield a migrated store (embedded fallback), not a
    /// silent monitoring degradation.
    #[tokio::test]
    async fn missing_migrations_dir_falls_back_to_embedded() {
        let dir = tempfile::tempdir().unwrap();
        let store = MonitoringStore::connect(MonitoringStoreConfig {
            database_url: format!("sqlite://{}/monitoring.db", dir.path().display()),
            migrations_dir: std::path::PathBuf::from("/nonexistent-monitoring-migrations"),
            ..MonitoringStoreConfig::default()
        })
        .await
        .expect("connect with embedded migrations");
        // The embedded initial migration created the schema.
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'monitoring_samples'",
        )
        .fetch_one(store.pool())
        .await
        .unwrap();
        assert_eq!(count, 1);
        store.close().await;
    }
}
