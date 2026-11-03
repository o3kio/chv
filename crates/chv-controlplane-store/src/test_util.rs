use crate::{run_migrations, CredentialEncryption, StorePool};
use sqlx::SqlitePool;

/// Deterministic credential cipher for repository tests that must exercise
/// the encrypt path. Injected explicitly so tests never race on the
/// process-global `CHV_ENCRYPTION_KEY` environment variable.
pub fn test_credential_encryption() -> CredentialEncryption {
    CredentialEncryption::with_key("chv-controlplane-store-test-encryption-key")
}

/// Synthetic users referenced by BFF/control-plane integration-test JWTs.
///
/// Session validation performs a user-existence lookup on every request
/// (#660), so a test token with a fabricated `sub` needs a matching row.
/// Roles here are irrelevant to validation; the claims carry the role.
pub async fn seed_test_users(pool: &StorePool) {
    for (user_id, role) in [
        ("test-admin-id", "admin"),
        ("u-tester", "admin"),
        ("u-ops", "operator"),
        ("u-op", "operator"),
        ("u-viewer", "viewer"),
        ("u-op-import", "operator"),
        ("u-alice", "operator"),
        ("u-bob", "operator"),
        ("u-admin", "admin"),
        ("u-operator", "operator"),
    ] {
        sqlx::query(
            "INSERT OR IGNORE INTO users (user_id, username, password_hash, role) \
             VALUES (?, ?, 'x', ?)",
        )
        .bind(user_id)
        .bind(user_id)
        .bind(role)
        .execute(pool)
        .await
        .unwrap_or_else(|e| panic!("failed to seed test user {user_id}: {e}"));
    }
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
        seed_test_users(&pool).await;
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
    seed_test_users(&pool).await;
    pool
}
