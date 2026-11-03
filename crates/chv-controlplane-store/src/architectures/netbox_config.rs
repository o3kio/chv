//! NetBox projection config repository — CRUD for `netbox_projection_config`.
//!
//! The NetBox API token is write-only: [`NetboxProjectionConfigUpsertInput`]
//! carries plaintext, which is encrypted via [`CredentialEncryption`] before
//! persisting and never stored or returned in the clear. The returned
//! [`NetboxProjectionConfig`] has no token field by construction, and
//! [`NetboxProjectionConfigRepository::read_token`] is the only decrypt
//! path (for the PR-4 projection worker).

use crate::architectures::parse_ts;
use crate::credential_crypto::CredentialEncryption;
use crate::{StoreError, StorePool};
use chv_controlplane_types::architecture::{
    ArchitectureId, NetboxProjectionConfig, NetboxRetentionPolicy,
};
use sqlx::Row;

/// Input for creating or updating a NetBox projection config.
///
/// Deliberately does **not** derive `Debug`: the struct carries the
/// plaintext API token, and any `{:?}` formatting (log lines, panic
/// messages, test failures) would dump the secret — mirroring the
/// secret-bearing inputs in `backups.rs` (`BackupScheduleCreateInput`).
#[derive(Clone)]
pub struct NetboxProjectionConfigUpsertInput {
    pub architecture_id: ArchitectureId,
    /// HTTPS base URL of the NetBox instance. Stored verbatim — HTTPS
    /// enforcement is the BFF's accept-time job (`NETBOX_HTTPS_REQUIRED`),
    /// per the component spec; the store does not validate the scheme.
    pub endpoint: String,
    /// Plaintext NetBox API token. `None` (or an empty/whitespace string,
    /// normalized to absent) on update keeps the existing secret;
    /// required when creating the config, since the `token_ciphertext`
    /// column is NOT NULL. Encrypted before persisting; never stored,
    /// returned, or logged in the clear.
    pub token: Option<String>,
    pub token_secret_ref: String,
    pub retention_policy: NetboxRetentionPolicy,
    pub enable_post_apply: bool,
    pub custom_field_prefix: String,
    pub site_name: Option<String>,
}

#[derive(Clone)]
pub struct NetboxProjectionConfigRepository {
    pool: StorePool,
    crypto: CredentialEncryption,
}

impl NetboxProjectionConfigRepository {
    pub fn new(pool: StorePool) -> Self {
        Self {
            pool,
            crypto: CredentialEncryption::new(),
        }
    }

    /// Production constructor keyed from `[secrets] credential_encryption`
    /// (#662), bypassing the environment lookup. Callers must use the same
    /// key to read rows written by this instance.
    pub fn new_with_key(pool: StorePool, key: &str) -> Self {
        Self {
            pool,
            crypto: CredentialEncryption::from_key(key),
        }
    }

    /// Test-support constructor with an injected credential cipher, so tests
    /// can exercise fail-closed writes without mutating process-global env.
    #[cfg(any(test, feature = "test-util"))]
    pub fn with_crypto(pool: StorePool, crypto: CredentialEncryption) -> Self {
        Self { pool, crypto }
    }

    pub fn pool(&self) -> &StorePool {
        &self.pool
    }

    /// Create or update the config for an architecture (upsert on
    /// `architecture_id`). The plaintext token from the input is encrypted
    /// before it touches the database; the returned config contains no
    /// token material.
    ///
    /// `token` is optional on update: `None` keeps the existing secret —
    /// the update-only path omits `token_ciphertext` from the SET clause
    /// entirely, so a token-less edit cannot destroy the stored
    /// credential. On create it is an error: the `token_ciphertext`
    /// column is NOT NULL and a first-time config needs a secret
    /// ([`StoreError::InvalidConfiguration`]).
    pub async fn upsert(
        &self,
        input: NetboxProjectionConfigUpsertInput,
    ) -> Result<NetboxProjectionConfig, StoreError> {
        // An empty/whitespace token is treated as absent (a careless BFF
        // sending "" must not clobber the stored secret); the raw value is
        // encrypted verbatim otherwise.
        match input.token.as_deref() {
            Some(token) if !token.trim().is_empty() => {
                // Encrypt before touching the database: a failed encryption
                // must not insert/update a row, and must never persist the
                // token in plaintext (#655, fail-closed).
                let token_ciphertext =
                    self.crypto
                        .encrypt(token)
                        .map_err(|err| StoreError::InvalidConfiguration {
                            reason: format!(
                                "cannot store netbox API token for architecture {}: \
                             credential encryption is unavailable (fail-closed): {err}",
                                input.architecture_id
                            ),
                        })?;
                let row = sqlx::query(
                    r#"
                    INSERT INTO netbox_projection_config (
                        architecture_id,
                        endpoint,
                        token_secret_ref,
                        token_ciphertext,
                        retention_policy,
                        enable_post_apply,
                        custom_field_prefix,
                        site_name
                    )
                    VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
                    ON CONFLICT (architecture_id) DO UPDATE SET
                        endpoint = excluded.endpoint,
                        token_secret_ref = excluded.token_secret_ref,
                        token_ciphertext = excluded.token_ciphertext,
                        retention_policy = excluded.retention_policy,
                        enable_post_apply = excluded.enable_post_apply,
                        custom_field_prefix = excluded.custom_field_prefix,
                        site_name = excluded.site_name,
                        updated_at = strftime('%Y-%m-%dT%H:%M:%SZ','now')
                    RETURNING *
                    "#,
                )
                .bind(input.architecture_id.as_str())
                .bind(&input.endpoint)
                .bind(&input.token_secret_ref)
                .bind(&token_ciphertext)
                .bind(input.retention_policy.as_str())
                .bind(input.enable_post_apply)
                .bind(&input.custom_field_prefix)
                .bind(&input.site_name)
                .fetch_one(&self.pool)
                .await
                .map_err(|err| match &err {
                    sqlx::Error::Database(db_err) if db_err.is_foreign_key_violation() => {
                        StoreError::NotFound {
                            entity: "architecture_topology_or_version",
                            id: input.architecture_id.to_string(),
                        }
                    }
                    _ => StoreError::from(err),
                })?;
                row_to_config(&row)
            }
            // Token omitted (or empty): update-only. `token_ciphertext`
            // is not part of the SET clause, so the stored secret is
            // preserved. A missing row means this would be a create, and
            // creating requires a token.
            _ => {
                let row = sqlx::query(
                    r#"
                    UPDATE netbox_projection_config SET
                        endpoint = $2,
                        token_secret_ref = $3,
                        retention_policy = $4,
                        enable_post_apply = $5,
                        custom_field_prefix = $6,
                        site_name = $7,
                        updated_at = strftime('%Y-%m-%dT%H:%M:%SZ','now')
                    WHERE architecture_id = $1
                    RETURNING *
                    "#,
                )
                .bind(input.architecture_id.as_str())
                .bind(&input.endpoint)
                .bind(&input.token_secret_ref)
                .bind(input.retention_policy.as_str())
                .bind(input.enable_post_apply)
                .bind(&input.custom_field_prefix)
                .bind(&input.site_name)
                .fetch_optional(&self.pool)
                .await?;
                match row {
                    Some(row) => row_to_config(&row),
                    None => Err(StoreError::InvalidConfiguration {
                        reason: "token is required when creating a netbox projection config"
                            .to_string(),
                    }),
                }
            }
        }
    }

    /// Fetch the config for an architecture. The returned struct carries
    /// no token material — only `token_secret_ref` (the reference).
    pub async fn get(
        &self,
        architecture_id: &ArchitectureId,
    ) -> Result<Option<NetboxProjectionConfig>, StoreError> {
        let row =
            sqlx::query(r#"SELECT * FROM netbox_projection_config WHERE architecture_id = $1"#)
                .bind(architecture_id.as_str())
                .fetch_optional(&self.pool)
                .await?;
        row.as_ref().map(row_to_config).transpose()
    }

    /// Remove the config for an architecture. Returns `true` when a row
    /// was removed, `false` when none existed. NetBox is never touched —
    /// deleting the config performs no cleanup on the projection target.
    pub async fn delete(&self, architecture_id: &ArchitectureId) -> Result<bool, StoreError> {
        let result =
            sqlx::query(r#"DELETE FROM netbox_projection_config WHERE architecture_id = $1"#)
                .bind(architecture_id.as_str())
                .execute(&self.pool)
                .await?;
        Ok(result.rows_affected() > 0)
    }

    /// All configs with the post-apply trigger enabled — the driver of
    /// the projection worker's post-apply sweep (PR 6). Ordered by
    /// `architecture_id` for deterministic sweeps. The returned structs
    /// carry no token material (same construction as [`Self::get`]).
    pub async fn list_post_apply_enabled(&self) -> Result<Vec<NetboxProjectionConfig>, StoreError> {
        let rows = sqlx::query(
            r#"
            SELECT * FROM netbox_projection_config
            WHERE enable_post_apply = 1
            ORDER BY architecture_id ASC
            "#,
        )
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(row_to_config).collect()
    }

    /// Decrypt and return the stored API token — the only decrypt path,
    /// for the projection worker.
    ///
    /// Fail-closed: any decrypt failure (wrong key, tampered ciphertext,
    /// malformed payload, missing key) returns
    /// [`StoreError::InvalidConfiguration`] — never the ciphertext —
    /// mirroring `backups.rs`.
    pub async fn read_token(
        &self,
        architecture_id: &ArchitectureId,
    ) -> Result<Option<String>, StoreError> {
        let row = sqlx::query(
            r#"SELECT token_ciphertext FROM netbox_projection_config WHERE architecture_id = $1"#,
        )
        .bind(architecture_id.as_str())
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let ciphertext: String = row.try_get("token_ciphertext")?;
        match self.crypto.decrypt(&ciphertext) {
            Ok(plaintext) => Ok(Some(plaintext)),
            Err(err) => {
                tracing::error!(
                    architecture_id = %architecture_id,
                    error = %err,
                    "netbox token decrypt failed; refusing to return ciphertext (fail-closed)"
                );
                Err(StoreError::InvalidConfiguration {
                    reason: "netbox token for this architecture cannot be decrypted".to_string(),
                })
            }
        }
    }
}

fn parse_retention_policy(s: &str) -> Result<NetboxRetentionPolicy, StoreError> {
    match s {
        "mark_stale" => Ok(NetboxRetentionPolicy::MarkStale),
        "delete" => Ok(NetboxRetentionPolicy::Delete),
        other => Err(StoreError::InvalidConfiguration {
            reason: format!("unrecognized netbox retention policy: {other}"),
        }),
    }
}

fn row_to_config(row: &sqlx::sqlite::SqliteRow) -> Result<NetboxProjectionConfig, StoreError> {
    let arch_id_str: String = row.try_get("architecture_id")?;
    let architecture_id =
        ArchitectureId::new(arch_id_str).map_err(|err| StoreError::InvalidConfiguration {
            reason: format!("invalid architecture_id in netbox_projection_config row: {err}"),
        })?;
    let retention_str: String = row.try_get("retention_policy")?;
    let created_at: String = row.try_get("created_at")?;
    let updated_at: String = row.try_get("updated_at")?;

    Ok(NetboxProjectionConfig {
        architecture_id,
        endpoint: row.try_get("endpoint")?,
        token_secret_ref: row.try_get("token_secret_ref")?,
        retention_policy: parse_retention_policy(&retention_str)?,
        enable_post_apply: row.try_get("enable_post_apply")?,
        custom_field_prefix: row.try_get("custom_field_prefix")?,
        site_name: row.try_get("site_name")?,
        created_at: parse_ts(&created_at, "created_at")?,
        updated_at: parse_ts(&updated_at, "updated_at")?,
    })
}
