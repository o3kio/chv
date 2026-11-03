use crate::{StoreError, StorePool};

/// Canonical CHV node-certificate serial length: eight lowercase hex
/// characters, as produced by `chv_common::gen_short_id`.
pub const CHV_CERTIFICATE_SERIAL_HEX_LEN: usize = 8;

/// Normalize operator-supplied certificate serial input.
///
/// CHV node certificates store `chv_common::gen_short_id()` as the DER
/// INTEGER's ASCII bytes, so the canonical value is **exactly eight lowercase
/// hex characters** (for example `0a3f7b2c`). Uppercase input is normalized;
/// anything else (including the 16-hex `openssl x509 -serial` rendering or a
/// pasted node id) is rejected so an operator cannot silently create a
/// denylist row for a serial that does not exist.
pub fn normalize_chv_certificate_serial(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.len() != CHV_CERTIFICATE_SERIAL_HEX_LEN
        || !trimmed.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return None;
    }
    Some(trimmed.to_ascii_lowercase())
}

/// One revoked node certificate serial.
///
/// The row is a durable denylist entry: as long as it exists, the matching
/// mTLS leaf certificate must not authenticate any control-plane RPC. Rows
/// are first-writer-wins so a repeated revocation is idempotent.
#[derive(sqlx::FromRow, Debug, Clone, PartialEq, Eq)]
pub struct NodeCertificateRevocation {
    pub certificate_serial: String,
    pub node_id: String,
    pub reason: String,
    pub revoked_at: String,
    pub revoked_by: Option<String>,
    /// The revoked certificate's own not-after time, if known. Bookkeeping for
    /// pruning only: an expired row still denies the serial.
    pub expires_at: Option<String>,
}

/// Input for [`CertificateRevocationRepository::revoke`].
#[derive(Debug, Clone)]
pub struct RevokeCertificateInput<'a> {
    pub certificate_serial: &'a str,
    pub node_id: &'a str,
    pub reason: &'a str,
    pub revoked_by: Option<&'a str>,
    /// RFC 3339/ISO-8601 text; `None` lets the store stamp "now".
    pub revoked_at: Option<&'a str>,
    pub expires_at: Option<&'a str>,
}

/// Result of a revocation request: the authoritative row plus whether this
/// call inserted it (`false` means an identical revocation already existed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevokeCertificateOutcome {
    pub revocation: NodeCertificateRevocation,
    pub already_revoked: bool,
}

const INSERT_REVOCATION_SQL: &str = r#"
INSERT INTO node_certificate_revocations (
    certificate_serial,
    node_id,
    reason,
    revoked_at,
    revoked_by,
    expires_at
)
VALUES (
    $1,
    $2,
    $3,
    COALESCE($4, strftime('%Y-%m-%dT%H:%M:%SZ','now')),
    $5,
    $6
)
ON CONFLICT (certificate_serial) DO NOTHING
"#;

const SELECT_REVOCATION_SQL: &str = r#"
SELECT
    certificate_serial,
    node_id,
    reason,
    revoked_at,
    revoked_by,
    expires_at
FROM node_certificate_revocations
"#;

#[derive(Clone)]
pub struct CertificateRevocationRepository {
    pool: StorePool,
}

impl CertificateRevocationRepository {
    pub fn new(pool: StorePool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &StorePool {
        &self.pool
    }

    /// Revoke `certificate_serial`, idempotently.
    ///
    /// The first successful insert is authoritative: a subsequent call with
    /// the same serial returns the existing row with `already_revoked = true`
    /// and never rewrites the reason, actor or timestamp. This lets operators
    /// and retrying automation re-run the command safely while preserving the
    /// audit trail of the original decision.
    ///
    /// Returns [`StoreError::Conflict`] when the serial is already revoked for
    /// a *different* node — almost always an operator typo, and recording it
    /// would attribute one node's compromise to another.
    pub async fn revoke(
        &self,
        input: &RevokeCertificateInput<'_>,
    ) -> Result<RevokeCertificateOutcome, StoreError> {
        if input.certificate_serial.trim().is_empty() {
            return Err(StoreError::InvalidConfiguration {
                reason: "certificate serial must not be empty".into(),
            });
        }
        if input.node_id.trim().is_empty() {
            return Err(StoreError::InvalidConfiguration {
                reason: "node_id must not be empty".into(),
            });
        }
        if input.reason.trim().is_empty() {
            return Err(StoreError::InvalidConfiguration {
                reason: "revocation reason must not be empty".into(),
            });
        }

        let result = sqlx::query(INSERT_REVOCATION_SQL)
            .bind(input.certificate_serial)
            .bind(input.node_id)
            .bind(input.reason)
            .bind(input.revoked_at)
            .bind(input.revoked_by)
            .bind(input.expires_at)
            .execute(&self.pool)
            .await?;

        let already_revoked = result.rows_affected() == 0;
        let revocation =
            self.get(input.certificate_serial)
                .await?
                .ok_or_else(|| StoreError::Conflict {
                    entity: "node_certificate_revocation",
                    id: input.certificate_serial.to_string(),
                    reason: "revocation row vanished between insert and read",
                })?;

        if revocation.node_id != input.node_id {
            return Err(StoreError::Conflict {
                entity: "node_certificate_revocation",
                id: input.certificate_serial.to_string(),
                reason: "certificate serial is already revoked for a different node",
            });
        }

        Ok(RevokeCertificateOutcome {
            revocation,
            already_revoked,
        })
    }

    /// Fetch one revoked serial, or `None` when it is not revoked.
    pub async fn get(
        &self,
        certificate_serial: &str,
    ) -> Result<Option<NodeCertificateRevocation>, StoreError> {
        let sql = format!("{SELECT_REVOCATION_SQL} WHERE certificate_serial = ?");
        let row = sqlx::query_as::<_, NodeCertificateRevocation>(&sql)
            .bind(certificate_serial)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row)
    }

    /// Every revoked serial, ordered deterministically.
    ///
    /// The peer-identity interceptor loads this into its in-memory snapshot.
    /// A caller that cannot read it MUST fail closed in production mode.
    pub async fn list_all(&self) -> Result<Vec<NodeCertificateRevocation>, StoreError> {
        let sql = format!("{SELECT_REVOCATION_SQL} ORDER BY revoked_at, certificate_serial");
        let rows = sqlx::query_as::<_, NodeCertificateRevocation>(&sql)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows)
    }

    /// Revocations recorded for one node, newest first. Operator/runbook aid.
    pub async fn list_for_node(
        &self,
        node_id: &str,
    ) -> Result<Vec<NodeCertificateRevocation>, StoreError> {
        let sql = format!("{SELECT_REVOCATION_SQL} WHERE node_id = ? ORDER BY revoked_at DESC");
        let rows = sqlx::query_as::<_, NodeCertificateRevocation>(&sql)
            .bind(node_id)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows)
    }

    /// Whether a non-empty serial is currently denied.
    ///
    /// This is the authoritative database check; the gRPC hot path uses the
    /// in-memory snapshot instead. Rotation uses this to reject stale/revoked
    /// proof-of-possession serials before minting key material.
    pub async fn is_revoked(&self, certificate_serial: &str) -> Result<bool, StoreError> {
        if certificate_serial.is_empty() {
            return Ok(false);
        }
        Ok(self.get(certificate_serial).await?.is_some())
    }
}

#[cfg(test)]
mod tests {
    use super::normalize_chv_certificate_serial;

    #[test]
    fn canonical_serial_is_eight_lowercase_hex() {
        assert_eq!(
            normalize_chv_certificate_serial("0a3f7b2c").as_deref(),
            Some("0a3f7b2c")
        );
        assert_eq!(
            normalize_chv_certificate_serial(" 0A3F7B2C ").as_deref(),
            Some("0a3f7b2c"),
            "uppercase/whitespace is normalized, not rejected"
        );
    }

    #[test]
    fn openssl_hex_and_malformed_serials_are_rejected() {
        for invalid in [
            "0a3f7b2c2f9e1b4d6a", // openssl -serial HEX DER rendering
            "0a:3f:7b:2c",
            "0a3f7b2",   // too short
            "0a3f7b2cc", // too long
            "0a3f7b2g",  // non-hex
            "node-a",
            "",
        ] {
            assert_eq!(
                normalize_chv_certificate_serial(invalid),
                None,
                "{invalid:?} must not be accepted as a CHV serial"
            );
        }
    }
}
