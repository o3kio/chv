use crate::{run_migrations, CredentialEncryption, StorePool};
use sqlx::SqlitePool;

/// Deterministic credential cipher for repository tests that must exercise
/// the encrypt path. Injected explicitly so tests never race on the
/// process-global `CHV_ENCRYPTION_KEY` environment variable.
pub fn test_credential_encryption() -> CredentialEncryption {
    CredentialEncryption::with_key("chv-controlplane-store-test-encryption-key")
}

pub struct TestDb {
    pub pool: StorePool,
}

impl TestDb {
    pub async fn new() -> Self {
        let pool = SqlitePool::connect("sqlite::memory:")
            .await
            .expect("failed to create in-memory SQLite pool");
        run_migrations(&pool, None)
            .await
            .expect("failed to run migrations on test pool");
        Self { pool }
    }
}

pub async fn create_test_pool() -> StorePool {
    let pool = SqlitePool::connect("sqlite::memory:")
        .await
        .expect("failed to create in-memory SQLite pool");
    run_migrations(&pool, None)
        .await
        .expect("failed to run migrations on test pool");
    pool
}
