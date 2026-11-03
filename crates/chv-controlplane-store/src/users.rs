//! User-account persistence for session revocation (#660).
//!
//! Session JWTs are stateless, so revocation needs a small piece of
//! server-side state: a monotonically increasing `token_version` per user.
//! Every newly issued session token embeds the current version and the
//! auth extractor compares it with this table on every request. Logout
//! and password change call [`UserRepository::bump_token_version`], which
//! makes every previously issued token for that user fail validation.
//!
//! Only credential-adjacent columns live here deliberately — the BFF and
//! the control plane still own login/role policy. Keeping this repository
//! small makes both call sites share the exact same SQL and error type.

use crate::{StoreError, StorePool};

/// Read/write access to the per-user session revocation counter.
#[derive(Clone)]
pub struct UserRepository {
    pool: StorePool,
}

impl UserRepository {
    pub fn new(pool: StorePool) -> Self {
        Self { pool }
    }

    /// Current token version for `user_id`, or `None` when the user does
    /// not exist. Callers must treat `None` as a rejected credential
    /// (fail closed) rather than as version 0.
    pub async fn token_version(&self, user_id: &str) -> Result<Option<u64>, StoreError> {
        let version: Option<i64> =
            sqlx::query_scalar("SELECT token_version FROM users WHERE user_id = ?")
                .bind(user_id)
                .fetch_optional(&self.pool)
                .await?;
        Ok(version.map(|v| v.max(0) as u64))
    }

    /// Atomically bump the counter and return the new version.
    ///
    /// Returns `StoreError::NotFound` when the user row is gone: logout
    /// and password change must not report success for a user that no
    /// longer exists.
    pub async fn bump_token_version(&self, user_id: &str) -> Result<u64, StoreError> {
        let updated = sqlx::query(
            "UPDATE users \
             SET token_version = token_version + 1, \
                 updated_at = strftime('%Y-%m-%dT%H:%M:%SZ','now') \
             WHERE user_id = ?",
        )
        .bind(user_id)
        .execute(&self.pool)
        .await?
        .rows_affected();

        if updated == 0 {
            return Err(StoreError::NotFound {
                entity: "user",
                id: user_id.to_string(),
            });
        }

        Ok(self.token_version(user_id).await?.unwrap_or(0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_pool() -> StorePool {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("connect in-memory sqlite");
        crate::run_migrations(&pool, None)
            .await
            .expect("run migrations");
        pool
    }

    async fn seed_user(pool: &StorePool, user_id: &str) {
        sqlx::query(
            "INSERT INTO users (user_id, username, password_hash, role) \
             VALUES (?, ?, 'x', 'operator')",
        )
        .bind(user_id)
        .bind(format!("user-{user_id}"))
        .execute(pool)
        .await
        .expect("seed user");
    }

    #[tokio::test]
    async fn new_users_start_at_version_zero() {
        let pool = test_pool().await;
        seed_user(&pool, "u-1").await;
        let repo = UserRepository::new(pool);
        assert_eq!(repo.token_version("u-1").await.unwrap(), Some(0));
    }

    #[tokio::test]
    async fn bump_is_monotonic_and_increments_by_one() {
        let pool = test_pool().await;
        seed_user(&pool, "u-1").await;
        let repo = UserRepository::new(pool);

        assert_eq!(repo.bump_token_version("u-1").await.unwrap(), 1);
        assert_eq!(repo.bump_token_version("u-1").await.unwrap(), 2);
        assert_eq!(repo.token_version("u-1").await.unwrap(), Some(2));
    }

    #[tokio::test]
    async fn missing_user_is_not_version_zero() {
        let repo = UserRepository::new(test_pool().await);
        assert_eq!(repo.token_version("nobody").await.unwrap(), None);
        assert!(matches!(
            repo.bump_token_version("nobody").await,
            Err(StoreError::NotFound { .. })
        ));
    }
}
